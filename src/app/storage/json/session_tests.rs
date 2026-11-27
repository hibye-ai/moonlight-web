//! Session lifetime regressions (MW-01, MW-15). These exercise the real
//! `JsonStorage` implementation, not a model of it.
use super::*;
use crate::app::storage::Storage;

async fn storage_in(label: &str) -> (Arc<JsonStorage>, PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-session-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).await.expect("create temp dir");
    let file = dir.join("data.json");
    let storage = JsonStorage::load(file.clone(), Duration::from_secs(3600))
        .await
        .expect("load storage");
    (storage, dir)
}

fn user_add(name: &str, role_id: u32) -> StorageUserAdd {
    StorageUserAdd {
        role_id: RoleId(role_id),
        name: name.to_string(),
        password: None,
        client_unique_id: name.to_string(),
    }
}

async fn seed_role(storage: &JsonStorage) -> RoleId {
    storage
        .add_role(StorageRoleAdd {
            name: "Admin".to_string(),
            ty: RoleType::Admin,
            default_settings: Default::default(),
            permissions: Default::default(),
        })
        .await
        .expect("add role")
        .id
}

/// MW-15: authentication must reject an expired session immediately, not only
/// after the periodic sweeper runs.
#[tokio::test]
async fn expired_session_is_rejected_at_lookup() {
    let (storage, dir) = storage_in("expiry").await;
    let role = seed_role(&storage).await;
    let user = storage
        .add_user(user_add("alice", role.0))
        .await
        .expect("user");

    let token = storage
        .create_session_token(user.id, Duration::from_millis(30))
        .await
        .expect("session");

    // Valid before expiry.
    assert!(
        matches!(
            storage.get_user_by_session_token(token).await,
            Ok((id, _)) if id == user.id
        ),
        "a live session must authenticate"
    );

    sleep(Duration::from_millis(60)).await;

    assert!(
        matches!(
            storage.get_user_by_session_token(token).await,
            Err(AppError::SessionTokenNotFound)
        ),
        "an expired session must not authenticate"
    );
    // The expired entry must not linger in the session list either.
    assert!(
        storage.list_sessions().await.expect("sessions").is_empty(),
        "expired session must be swept at lookup"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-01: deleting the user must invalidate the sessions that point at it, so
/// a surviving cookie cannot keep reaching owner-scoped resources.
#[tokio::test]
async fn deleting_user_revokes_its_sessions() {
    let (storage, dir) = storage_in("delete").await;
    let role = seed_role(&storage).await;
    let alice = storage
        .add_user(user_add("alice", role.0))
        .await
        .expect("alice");
    let bob = storage
        .add_user(user_add("bob", role.0))
        .await
        .expect("bob");

    let alice_token = storage
        .create_session_token(alice.id, Duration::from_secs(3600))
        .await
        .expect("alice session");
    let bob_token = storage
        .create_session_token(bob.id, Duration::from_secs(3600))
        .await
        .expect("bob session");

    storage.remove_user(alice.id).await.expect("remove alice");

    assert!(
        matches!(
            storage.get_user_by_session_token(alice_token).await,
            Err(AppError::SessionTokenNotFound)
        ),
        "deleted user's session must be rejected"
    );
    assert!(
        matches!(
            storage.get_user_by_session_token(bob_token).await,
            Ok((id, _)) if id == bob.id
        ),
        "unrelated sessions must survive"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-01: a role deletion cascades into user deletion, so those users' sessions
/// must be revoked as part of the same transition.
#[tokio::test]
async fn deleting_role_revokes_cascaded_user_sessions() {
    let (storage, dir) = storage_in("role-delete").await;
    let role = seed_role(&storage).await;
    // The successful-cascade fixture must leave an admin in another role.
    let surviving_role = seed_role(&storage).await;
    let root = storage
        .add_user(user_add("root", surviving_role.0))
        .await
        .expect("surviving admin");
    let root_token = storage
        .create_session_token(root.id, Duration::from_secs(3600))
        .await
        .expect("surviving admin session");
    let alice = storage
        .add_user(user_add("alice", role.0))
        .await
        .expect("alice");
    let token = storage
        .create_session_token(alice.id, Duration::from_secs(3600))
        .await
        .expect("session");

    storage.remove_role(role).await.expect("remove role");

    assert!(
        matches!(
            storage.get_user_by_session_token(token).await,
            Err(AppError::SessionTokenNotFound)
        ),
        "session of a cascaded user must be rejected"
    );
    assert_eq!(
        storage
            .get_user_by_session_token(root_token)
            .await
            .expect("surviving session")
            .0,
        root.id,
        "the admin outside the cascade must keep its session"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-01: an explicitly requested password change revokes existing sessions.
/// A transparent rehash must not, so logins are not kicked out by hashing
/// upgrades.
#[tokio::test]
async fn password_change_revokes_sessions_but_rehash_does_not() {
    let (storage, dir) = storage_in("password").await;
    let role = seed_role(&storage).await;
    let alice = storage
        .add_user(StorageUserAdd {
            password: Some(StoragePassword::new("correct horse").expect("hash")),
            ..user_add("alice", role.0)
        })
        .await
        .expect("alice");
    let token = storage
        .create_session_token(alice.id, Duration::from_secs(3600))
        .await
        .expect("session");

    // Transparent rehash (different iterations, same password) keeps sessions.
    let rehashed = StoragePassword::new("correct horse").expect("rehash");
    storage
        .modify_user(
            alice.id,
            StorageUserModify {
                password: Some(Some(rehashed.clone())),
                ..Default::default()
            },
        )
        .await
        .expect("rehash");
    assert!(
        storage.get_user_by_session_token(token).await.is_ok(),
        "a rehash must not end existing sessions"
    );

    // A user/admin requested password change does.
    storage
        .modify_user(
            alice.id,
            StorageUserModify {
                password: Some(Some(StoragePassword::new("new password").expect("hash"))),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("change password");
    assert!(
        matches!(
            storage.get_user_by_session_token(token).await,
            Err(AppError::SessionTokenNotFound)
        ),
        "a password change must revoke existing sessions"
    );

    let _ = fs::remove_dir_all(&dir).await;
}
