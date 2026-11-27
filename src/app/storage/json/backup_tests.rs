//! Backup import transaction regressions (MW-19).
//!
//! The active-stream check and the store replacement must be one transition: a
//! stream that starts after the check must not race the restore, and an identity
//! mutation must not interleave with it.
use super::*;
use crate::app::storage::Storage;

async fn storage_in(label: &str) -> (Arc<JsonStorage>, PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-backup-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).await.expect("create temp dir");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("load storage");
    (storage, dir)
}

fn role_json(ty: &str) -> serde_json::Value {
    serde_json::json!({
        "name": ty,
        "ty": ty,
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
    })
}

fn user_json(role_id: u32, name: &str) -> serde_json::Value {
    serde_json::json!({
        "role_id": role_id,
        "name": name,
        "password": {
            "salt": "00112233445566778899aabbccddeeff",
            "hash": "0000000000000000000000000000000000000000000000000000000000000000",
            "iterations": 600000
        },
        "client_unique_id": name
    })
}

fn valid_backup(name: &str) -> serde_json::Value {
    serde_json::json!({
        "version": "3",
        "users": { "7": user_json(3, name) },
        "hosts": {},
        "roles": { "3": role_json("Admin") }
    })
}

/// MW-19: an import must invalidate every session it replaces, so a request that
/// was authorized against the previous identity snapshot cannot continue.
#[tokio::test]
async fn import_clears_sessions_issued_before_it() {
    let (storage, dir) = storage_in("sessions").await;
    let role = storage
        .add_role(StorageRoleAdd {
            name: "Admin".to_string(),
            ty: RoleType::Admin,
            default_settings: Default::default(),
            permissions: Default::default(),
        })
        .await
        .expect("role");
    let alice = storage
        .add_user(StorageUserAdd {
            role_id: role.id,
            name: "alice".to_string(),
            password: None,
            client_unique_id: "alice".to_string(),
        })
        .await
        .expect("alice");
    let token = storage
        .create_session_token(alice.id, Duration::from_secs(3600))
        .await
        .expect("session");
    assert!(storage.get_user_by_session_token(token).await.is_ok());

    storage
        .import_backup(valid_backup("restored"))
        .await
        .expect("import");

    assert!(
        matches!(
            storage.get_user_by_session_token(token).await,
            Err(AppError::SessionTokenNotFound)
        ),
        "a restore must invalidate sessions authorized against the old snapshot"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-19: the store epoch must advance on restore so pre-authorized requests can
/// detect that their identity snapshot is stale.
#[tokio::test]
async fn import_advances_the_identity_epoch() {
    let (storage, dir) = storage_in("epoch").await;
    let before = storage.identity_epoch();

    storage
        .import_backup(valid_backup("restored"))
        .await
        .expect("import");

    assert!(
        storage.identity_epoch() > before,
        "the identity epoch must advance so stale authorizations can be rejected"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-19: the restore must be durable before the call reports success, otherwise a
/// crash right after a "successful" restore silently reverts to the old identity
/// set.
#[tokio::test]
async fn import_is_durable_before_returning() {
    let (storage, dir) = storage_in("durable").await;

    storage
        .import_backup(valid_backup("restored"))
        .await
        .expect("import");

    let text = fs::read_to_string(dir.join("data.json"))
        .await
        .expect("read primary file");
    assert!(
        text.contains("restored"),
        "the restored identities must be on disk when the call returns"
    );

    let _ = fs::remove_dir_all(&dir).await;
}
