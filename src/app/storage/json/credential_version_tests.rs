//! Credential-generation compare-and-swap controls (AR-01, AR-04) at the storage
//! level. Every case runs against the real `JsonStorage` on a private temp file.
use super::*;
use crate::app::storage::Storage;

fn storage_dir(label: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "moonlight-web-credver-{label}-{}-{unique}",
        std::process::id()
    ))
}

async fn storage_in(label: &str) -> (Arc<JsonStorage>, PathBuf) {
    let dir = storage_dir(label);
    fs::create_dir_all(&dir).await.expect("create temp dir");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("load storage");
    (storage, dir)
}

async fn seed_admin(storage: &JsonStorage, name: &str) -> StorageUser {
    let role = storage
        .add_role(StorageRoleAdd {
            name: "Admin".to_string(),
            ty: RoleType::Admin,
            default_settings: Default::default(),
            permissions: Default::default(),
        })
        .await
        .expect("role");
    storage
        .add_user(StorageUserAdd {
            role_id: role.id,
            name: name.to_string(),
            password: Some(StoragePassword::new("fixture-password").expect("hash")),
            client_unique_id: name.to_string(),
        })
        .await
        .expect("admin")
}

/// AR-01: a new account starts at generation zero and a session records that
/// generation, so the token can be invalidated by a later rotation.
#[tokio::test]
async fn new_account_starts_at_generation_zero() {
    let (storage, dir) = storage_in("initial").await;
    let admin = seed_admin(&storage, "alice").await;

    assert_eq!(admin.credential_version, CredentialVersion(0));
    assert_eq!(
        storage
            .get_user(admin.id)
            .await
            .expect("stored")
            .credential_version,
        CredentialVersion(0)
    );

    let token = storage
        .create_session_token_checked(
            admin.id,
            Duration::from_secs(3600),
            Some(CredentialVersion(0)),
        )
        .await
        .expect("session at the current generation");
    assert!(storage.get_user_by_session_token(token).await.is_ok());

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-01: a session may only be minted for the generation the credential check
/// actually verified. A snapshot from before a rotation cannot obtain a usable token,
/// and if one is inserted anyway it is not honoured at authentication time.
#[tokio::test]
async fn session_issuance_is_pinned_to_the_verified_generation() {
    let (storage, dir) = storage_in("pinned").await;
    let admin = seed_admin(&storage, "alice").await;
    let stale = storage.get_user(admin.id).await.expect("stale snapshot");

    // A real rotation between "credentials verified" and "issue the session".
    storage
        .modify_user(
            admin.id,
            StorageUserModify {
                password: Some(Some(StoragePassword::new("rotated").expect("hash"))),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("rotation");

    let err = storage
        .create_session_token_checked(
            admin.id,
            Duration::from_secs(3600),
            Some(stale.credential_version),
        )
        .await
        .expect_err("a session must not be minted for a superseded credential generation");
    assert!(
        matches!(err, AppError::CredentialsWrong),
        "expected CredentialsWrong, got {err}"
    );

    // Unpinned issuance still works for the ordinary "user just logged in" path.
    let current = storage.get_user(admin.id).await.expect("current");
    let token = storage
        .create_session_token(admin.id, Duration::from_secs(3600))
        .await
        .expect("unpinned session");
    assert!(
        storage.get_user_by_session_token(token).await.is_ok(),
        "a session at the current generation must authenticate"
    );
    assert_eq!(current.credential_version, CredentialVersion(1));

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-01: deleting the account must make session issuance fail even for a handle that
/// still holds a pre-deletion snapshot.
#[tokio::test]
async fn deleted_account_cannot_be_issued_a_session() {
    let (storage, dir) = storage_in("deleted").await;
    let admin = seed_admin(&storage, "alice").await;
    // A second login-capable admin, so the last-admin guard does not mask the
    // property under test.
    storage
        .add_user(StorageUserAdd {
            role_id: admin.role_id,
            name: "bob".to_string(),
            password: Some(StoragePassword::new("fixture-password").expect("hash")),
            client_unique_id: "bob".to_string(),
        })
        .await
        .expect("second admin");
    let snapshot = storage.get_user(admin.id).await.expect("snapshot");

    storage.remove_user(admin.id).await.expect("delete");

    let err = storage
        .create_session_token_checked(
            admin.id,
            Duration::from_secs(3600),
            Some(snapshot.credential_version),
        )
        .await
        .expect_err("a deleted account must not be issued a session");
    assert!(
        matches!(err, AppError::SessionTokenNotFound),
        "expected SessionTokenNotFound, got {err}"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-04: a stale rehash (candidate hash derived from a superseded generation) is
/// refused and never overwrites the newest password.
#[tokio::test]
async fn stale_rehash_is_rejected_and_preserves_the_new_password() {
    let (storage, dir) = storage_in("stale-rehash").await;
    let admin = seed_admin(&storage, "alice").await;
    let stale = storage.get_user(admin.id).await.expect("stale snapshot");

    storage
        .modify_user(
            admin.id,
            StorageUserModify {
                password: Some(Some(StoragePassword::new("rotated").expect("hash"))),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("rotation");

    let candidate = StoragePassword::new("fixture-password").expect("rehash candidate");
    let err = storage
        .rehash_password(admin.id, stale.credential_version, candidate)
        .await
        .expect_err("a rehash from a superseded generation must be refused");
    assert!(
        matches!(err, AppError::CredentialsWrong),
        "expected CredentialsWrong, got {err}"
    );

    let stored = storage.get_user(admin.id).await.expect("stored");
    let password = stored.password.expect("password");
    assert!(
        password.verify("rotated").expect("verify new"),
        "the stale rehash must not have overwritten the rotated password"
    );
    assert!(
        !password.verify("fixture-password").expect("verify old"),
        "the stale rehash must not have resurrected the old password"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-04: a rehash at the current generation applies the upgrade, keeps the
/// generation and therefore keeps every existing session valid.
#[tokio::test]
async fn current_generation_rehash_upgrades_the_hash_without_revoking_sessions() {
    let (storage, dir) = storage_in("fresh-rehash").await;
    let admin = seed_admin(&storage, "alice").await;
    let token = storage
        .create_session_token(admin.id, Duration::from_secs(3600))
        .await
        .expect("session");
    let before = storage.get_user(admin.id).await.expect("before");

    let upgraded = StoragePassword::new("fixture-password").expect("upgrade");
    storage
        .rehash_password(admin.id, before.credential_version, upgraded)
        .await
        .expect("current-generation rehash");

    let after = storage.get_user(admin.id).await.expect("after");
    assert_eq!(
        after.credential_version, before.credential_version,
        "a rehash is not a credential change"
    );
    assert!(
        after
            .password
            .expect("password")
            .verify("fixture-password")
            .expect("verify"),
        "the upgraded hash must still verify the same password"
    );
    assert!(
        storage.get_user_by_session_token(token).await.is_ok(),
        "a transparent rehash must keep other sessions"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-04: only a genuine rotation bumps the generation, and it revokes sessions at the
/// same time. This pins the difference between the two paths.
#[tokio::test]
async fn rotation_advances_the_generation_and_revokes_sessions() {
    let (storage, dir) = storage_in("rotation-bumps").await;
    let admin = seed_admin(&storage, "alice").await;
    let token = storage
        .create_session_token(admin.id, Duration::from_secs(3600))
        .await
        .expect("session");
    let before = storage.get_user(admin.id).await.expect("before");

    storage
        .modify_user(
            admin.id,
            StorageUserModify {
                password: Some(Some(StoragePassword::new("rotated").expect("hash"))),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("rotation");

    let after = storage.get_user(admin.id).await.expect("after");
    assert_eq!(
        after.credential_version,
        CredentialVersion(before.credential_version.0 + 1),
        "a rotation must advance the credential generation"
    );
    assert!(
        matches!(
            storage.get_user_by_session_token(token).await,
            Err(AppError::SessionTokenNotFound)
        ),
        "a rotation must revoke existing sessions"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-01: a restore replaces the identity snapshot, so every credential generation
/// advances and a session issued before it cannot survive — even if the restored hash
/// for that account is byte-identical.
#[tokio::test]
async fn restore_advances_every_credential_generation() {
    let (storage, dir) = storage_in("restore-bumps").await;
    let admin = seed_admin(&storage, "alice").await;
    let token = storage
        .create_session_token(admin.id, Duration::from_secs(3600))
        .await
        .expect("session");
    let backup = storage.export_backup().await.expect("backup");
    let before = storage.get_user(admin.id).await.expect("before");

    storage.import_backup(backup).await.expect("restore");

    let after = storage.get_user(admin.id).await.expect("after");
    assert_eq!(
        after.credential_version,
        CredentialVersion(before.credential_version.0 + 1),
        "a restore must advance the credential generation"
    );
    assert!(
        matches!(
            storage.get_user_by_session_token(token).await,
            Err(AppError::SessionTokenNotFound)
        ),
        "a session from before the restore must not authenticate"
    );

    let _ = fs::remove_dir_all(&dir).await;
}
