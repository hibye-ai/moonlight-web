//! AR-01/AR-02 restore-boundary regressions added with the fix.
//!
//! These cover the parts of the restore contract the pinned fixtures do not:
//! repeated restores must keep advancing generations and the epoch, a failed
//! restore must leave disk and live state untouched, and the generation state must
//! survive a reload (it is persisted, not merely in memory).
use super::*;
use crate::app::storage::{CredentialVersion, IdentityEpoch, Storage};

async fn storage_in(label: &str) -> (Arc<JsonStorage>, PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-restore-epoch-{label}-{}-{unique}",
        std::process::id()
    ));
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
    let user = storage
        .add_user(StorageUserAdd {
            role_id: role.id,
            name: name.to_string(),
            password: Some(StoragePassword::new("fixture-password").expect("hash")),
            client_unique_id: name.to_string(),
        })
        .await
        .expect("admin");
    storage.persist().await.expect("seed durable");
    user
}

fn backup_for(id: UserId, name: &str) -> serde_json::Value {
    serde_json::json!({
        "version": "3",
        "users": {
            id.0.to_string(): {
                "role_id": 0,
                "name": name,
                "password": {
                    "salt": "00112233445566778899aabbccddeeff",
                    "hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "iterations": 600000
                },
                "client_unique_id": name,
                "credential_version": 0
            }
        },
        "hosts": {},
        "roles": {
            "0": {
                "name": "Admin",
                "ty": "Admin",
                "default_settings": {},
                "permissions": {
                    "allow_add_hosts": true,
                    "maximum_bitrate_kbps": null,
                    "allow_codec_h264": true,
                    "allow_codec_h265": true,
                    "allow_codec_av1": true,
                    "allow_hdr": true,
                    "allow_transport_webrtc": true,
                    "allow_transport_websockets": true
                }
            }
        }
    })
}

/// AR-01: restoring the *same* older backup twice must not hand out a generation
/// twice. Each restore allocates fresh values from the persisted counter, so the
/// second restore's generation is strictly above the first's — a credential pin
/// taken between them is permanently stale instead of becoming valid again.
#[tokio::test]
async fn repeated_restores_never_reuse_a_credential_generation() {
    let (storage, dir) = storage_in("repeated").await;
    let admin = seed_admin(&storage, "alice").await;

    let before = storage.identity_epoch();
    let candidate = backup_for(admin.id, "restored-alice");

    storage
        .import_backup(candidate.clone())
        .await
        .expect("first restore");
    let first = storage.get_user(admin.id).await.expect("after first");
    let first_epoch = storage.identity_epoch();
    assert!(
        first_epoch > before,
        "a restore must advance the identity epoch"
    );

    storage
        .import_backup(candidate)
        .await
        .expect("second restore of the same older backup");
    let second = storage.get_user(admin.id).await.expect("after second");
    let second_epoch = storage.identity_epoch();

    assert!(
        second.credential_version > first.credential_version,
        "restoring the same backup twice reused generation {:?} (first {:?})",
        second.credential_version,
        first.credential_version
    );
    assert!(
        second_epoch > first_epoch,
        "each restore must advance the epoch, not just the first one"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-01: the epoch and generation high-water marks are part of the persisted
/// snapshot, so a restart cannot rewind them. Without that, a pre-restart pin would
/// become valid again after a reload.
#[tokio::test]
async fn restore_epoch_and_generation_survive_a_reload() {
    let (storage, dir) = storage_in("reload").await;
    let admin = seed_admin(&storage, "alice").await;

    storage
        .import_backup(backup_for(admin.id, "restored-alice"))
        .await
        .expect("restore");
    let epoch = storage.identity_epoch();
    let version = storage
        .get_user(admin.id)
        .await
        .expect("after restore")
        .credential_version;
    let durable: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("data.json")).await.expect("read primary"))
            .expect("json");

    drop(storage);
    let reloaded = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("reload");

    assert_eq!(
        reloaded.identity_epoch(),
        epoch,
        "the restored epoch must be persisted, not only in memory"
    );
    assert_eq!(
        reloaded
            .get_user(admin.id)
            .await
            .expect("after reload")
            .credential_version,
        version,
        "the allocated generation must be persisted"
    );
    // The counters are persisted too, so the next allocation cannot repeat one.
    assert!(
        durable["next_credential_version"].as_u64().unwrap_or(0) > version.0,
        "the persisted high-water mark must sit above every live generation"
    );
    assert_eq!(
        durable["identity_epoch"].as_u64().unwrap_or(0),
        epoch,
        "the persisted epoch must match the live one"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-02: a restore that cannot be committed must leave disk *and* live state
/// exactly as they were, including sessions and the epoch. No half-transition.
#[tokio::test]
async fn failed_restore_leaves_disk_and_live_state_unchanged() {
    let (storage, dir) = storage_in("failed").await;
    let admin = seed_admin(&storage, "alice").await;
    let token = storage
        .create_session_token(admin.id, Duration::from_secs(3600))
        .await
        .expect("session");

    let before = storage.export_backup().await.expect("before");
    let primary_before = fs::read(dir.join("data.json")).await.expect("primary");
    let epoch_before = storage.identity_epoch();

    // A real, un-faked staging failure: the atomic write creates `<file>.tmp`, and a
    // directory at that path makes the creation fail before anything is renamed.
    let blocked = path_with_suffix(&dir.join("data.json"), ".tmp");
    fs::create_dir(&blocked).await.expect("block staging");

    let result = storage
        .import_backup(backup_for(admin.id, "restored-alice"))
        .await;
    assert!(
        result.is_err(),
        "a restore that cannot be written must fail"
    );

    assert_eq!(
        storage.export_backup().await.expect("live after"),
        before,
        "a failed restore must not change the live identity snapshot"
    );
    assert_eq!(
        fs::read(dir.join("data.json"))
            .await
            .expect("primary after"),
        primary_before,
        "a failed restore must not change the durable snapshot"
    );
    assert_eq!(
        storage.identity_epoch(),
        epoch_before,
        "a failed restore must not advance the epoch"
    );
    assert!(
        storage.get_user_by_session_token(token).await.is_ok(),
        "a failed restore must not revoke sessions"
    );

    // The writer must still be usable afterwards.
    fs::remove_dir(&blocked).await.expect("unblock");
    storage.persist().await.expect("writer recovery");
    storage
        .import_backup(backup_for(admin.id, "restored-alice"))
        .await
        .expect("restore succeeds once the failure is removed");
    assert!(
        storage.identity_epoch() > epoch_before,
        "the recovered restore must advance the epoch"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-01: a new account never reuses an id, so a credential pin held for a removed
/// account cannot be replayed against the account created after it.
#[tokio::test]
async fn account_ids_are_never_reused_after_removal() {
    let (storage, dir) = storage_in("ids").await;
    let admin = seed_admin(&storage, "alice").await;
    // A second login-capable admin so the last-admin guard does not interfere.
    let second = storage
        .add_user(StorageUserAdd {
            role_id: admin.role_id,
            name: "bob".to_string(),
            password: Some(StoragePassword::new("fixture-password").expect("hash")),
            client_unique_id: "bob".to_string(),
        })
        .await
        .expect("second admin");

    storage.remove_user(second.id).await.expect("remove bob");
    let replacement = storage
        .add_user(StorageUserAdd {
            role_id: admin.role_id,
            name: "carol".to_string(),
            password: Some(StoragePassword::new("fixture-password").expect("hash")),
            client_unique_id: "carol".to_string(),
        })
        .await
        .expect("replacement account");

    assert_ne!(
        replacement.id, second.id,
        "a reinstated account id would let a stale credential pin match the new account"
    );
    assert_ne!(replacement.id, admin.id);

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-01: a restore advances the generation of every installed account above every
/// generation that existed before it, so nothing in the restored snapshot can match
/// a pre-restore pin — this is the property, stated directly on the storage.
#[tokio::test]
async fn restore_generations_exceed_every_pre_restore_generation() {
    let (storage, dir) = storage_in("floor").await;
    let admin = seed_admin(&storage, "alice").await;
    // Push the live generation well above the backup's own (which is 0).
    for _ in 0..3 {
        storage
            .modify_user(
                admin.id,
                StorageUserModify {
                    password: Some(Some(
                        StoragePassword::new("fixture-password").expect("hash"),
                    )),
                    revoke_sessions: true,
                    ..Default::default()
                },
            )
            .await
            .expect("rotation");
    }
    let live_before = storage
        .get_user(admin.id)
        .await
        .expect("before")
        .credential_version;

    storage
        .import_backup(backup_for(admin.id, "restored-alice"))
        .await
        .expect("restore");

    let restored = storage.get_user(admin.id).await.expect("after");
    assert!(
        restored.credential_version > live_before,
        "restored generation {:?} must exceed the pre-restore generation {:?}",
        restored.credential_version,
        live_before
    );
    assert!(
        restored.credential_version > CredentialVersion(0),
        "a restore must not fall back to the backup's own generation"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// AR-01: the epoch is a monotonically increasing counter of snapshot
/// incarnations, so two restores back to back are distinguishable.
#[tokio::test]
async fn epoch_is_strictly_monotonic_across_restores() {
    let (storage, dir) = storage_in("monotonic").await;
    let admin = seed_admin(&storage, "alice").await;
    let candidate = backup_for(admin.id, "restored-alice");

    let mut seen: Vec<IdentityEpoch> = vec![storage.identity_epoch()];
    for _ in 0..4 {
        storage
            .import_backup(candidate.clone())
            .await
            .expect("restore");
        seen.push(storage.identity_epoch());
    }

    for pair in seen.windows(2) {
        assert!(
            pair[1] > pair[0],
            "each restore must advance the epoch: {:?}",
            seen
        );
    }

    let _ = fs::remove_dir_all(&dir).await;
}
