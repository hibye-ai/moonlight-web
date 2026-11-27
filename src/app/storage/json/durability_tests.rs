//! Persistence durability regressions (MW-17).
//!
//! `force_write` only enqueues an intent; the caller used to get a success
//! response before the bytes reached disk. These tests exercise the real writer
//! path against a real file.
use super::*;
use crate::app::storage::Storage;

async fn storage_in(label: &str) -> (Arc<JsonStorage>, PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-durability-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).await.expect("create temp dir");
    let file = dir.join("data.json");
    let storage = JsonStorage::load(file, Duration::from_secs(3600))
        .await
        .expect("load storage");
    (storage, dir)
}

/// MW-17: when the caller asks for a durable mutation, the data must already be
/// on disk (in the primary file) when the call returns.
#[tokio::test]
async fn durable_mutation_is_on_disk_before_returning() {
    let (storage, dir) = storage_in("durable").await;
    let role = storage
        .add_role(StorageRoleAdd {
            name: "Admin".to_string(),
            ty: RoleType::Admin,
            default_settings: Default::default(),
            permissions: Default::default(),
        })
        .await
        .expect("add_role");
    storage.persist().await.expect("durable persist");

    let text = fs::read_to_string(dir.join("data.json"))
        .await
        .expect("read primary storage file");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    assert!(
        value["roles"].get(role.id.0.to_string()).is_some(),
        "the new role must be present in the persisted file, not only in memory"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// MW-17: a failing disk must surface to the caller instead of returning success.
/// The directory is made read-only so the atomic replace cannot land.
#[cfg(unix)]
#[tokio::test]
async fn durable_mutation_reports_write_failure() {
    use std::os::unix::fs::PermissionsExt;

    let (storage, dir) = storage_in("readonly").await;
    // Seed a valid store first, then make the directory unwritable.
    storage
        .add_role(StorageRoleAdd {
            name: "Admin".to_string(),
            ty: RoleType::Admin,
            default_settings: Default::default(),
            permissions: Default::default(),
        })
        .await
        .expect("add_role");
    storage.persist().await.expect("initial durable write");

    fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
        .await
        .expect("chmod dir read-only");

    storage
        .add_role(StorageRoleAdd {
            name: "Second".to_string(),
            ty: RoleType::User,
            default_settings: Default::default(),
            permissions: Default::default(),
        })
        .await
        .expect("in-memory mutation still applies");
    let result = storage.persist().await;

    // Restore permissions before asserting so a panic still allows cleanup.
    fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .await
        .expect("restore chmod");

    assert!(
        result.is_err(),
        "a mutation that could not be persisted must not report success"
    );

    let _ = fs::remove_dir_all(&dir).await;
}

/// The non-durable path must keep working exactly as before, so batch/internal
/// writers are not forced to pay the fsync cost.
#[tokio::test]
async fn regular_mutation_still_succeeds() {
    let (storage, dir) = storage_in("regular").await;
    storage
        .add_role(StorageRoleAdd {
            name: "Admin".to_string(),
            ty: RoleType::Admin,
            default_settings: Default::default(),
            permissions: Default::default(),
        })
        .await
        .expect("regular add_role");

    let _ = fs::remove_dir_all(&dir).await;
}
