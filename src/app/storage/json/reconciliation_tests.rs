//! AR-01 x AR-03/AR-05 reconciliation regression.
//!
//! The identity/restore work (intent A) put three persisted marks into the V3
//! snapshot: the identity epoch, the credential-generation high-water mark and the
//! account-id high-water mark. The transactional-mutation work (intent B) rewrote
//! every identity and host mutation as a writer-owned candidate commit that
//! replaces the whole snapshot.
//!
//! Those two intents only compose if a mutation commits a candidate that *carries*
//! the marks, allocates inside the candidate, and publishes the marks only after the
//! durable write. These controls drive the real `JsonStorage`/`App` paths and assert
//! exactly that:
//!
//! * a host-only mutation neither erases nor lowers the identity/id marks;
//! * a reload preserves them, so a reinstated id or generation stays impossible;
//! * a mutation whose durable write fails publishes no counter at all.
//!
//! No failpoints are used: a real directory in place of the `.tmp` staging path
//! makes the staging write fail on every platform, including as root.
use super::*;
use crate::app::{
    App,
    storage::{IdentityEpoch, Storage},
};
use common::config::{Config, StorageConfig};

fn unique_dir(label: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-recon-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("fixture directory");
    dir
}

fn config_for(dir: &Path) -> Config {
    let mut config = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };
    config.web_server.first_login_create_admin = true;
    config
}

fn role_add(name: &str, ty: RoleType) -> StorageRoleAdd {
    StorageRoleAdd {
        name: name.to_owned(),
        ty,
        default_settings: Default::default(),
        permissions: Default::default(),
    }
}

fn user_add(name: &str, role_id: RoleId, password: &str) -> StorageUserAdd {
    StorageUserAdd {
        role_id,
        name: name.to_owned(),
        password: Some(StoragePassword::new(password).expect("hash")),
        client_unique_id: name.to_owned(),
    }
}

fn fixture_host(owner: Option<UserId>, name: &str) -> StorageHostAdd {
    StorageHostAdd {
        owner,
        address: "127.0.0.1".to_owned(),
        http_port: 47989,
        pair_info: None,
        cache: StorageHostCache {
            name: name.to_owned(),
            mac: None,
        },
    }
}

async fn durable(dir: &Path) -> serde_json::Value {
    let bytes = fs::read(dir.join("data.json"))
        .await
        .expect("durable store");
    serde_json::from_slice(&bytes).expect("valid json")
}

/// `(identity_epoch, next_credential_version, next_user_id)` as published live.
fn published(storage: &JsonStorage) -> (IdentityEpoch, u64, u64) {
    (
        storage.identity_epoch.load(AtomicOrdering::Acquire),
        storage
            .next_credential_version
            .load(AtomicOrdering::Acquire),
        storage.next_user_id.load(AtomicOrdering::Acquire),
    )
}

async fn block_staging(dir: &Path) -> PathBuf {
    let blocked = dir.join("data.json.tmp");
    fs::create_dir(&blocked).await.expect("block staging");
    // Keep the directory non-empty so the stale-temp cleanup also fails.
    fs::write(blocked.join("keep"), b"x").await.expect("seed");
    blocked
}

async fn unblock_staging(blocked: PathBuf) {
    fs::remove_dir_all(&blocked).await.expect("unblock");
}

/// AR-01 x AR-03/AR-05: a host mutation must carry the persisted identity marks
/// through its candidate commit instead of replacing the snapshot with one that
/// starts them over at zero, and a reload must agree with the live store.
#[tokio::test]
async fn host_mutations_preserve_and_reload_identity_high_water_marks() {
    let dir = unique_dir("host-marks");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("storage");

    let admin_role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role")
        .id;
    let admin = storage
        .add_user(user_add("fixture-root", admin_role, "fixture-password"))
        .await
        .expect("admin");
    let second = storage
        .add_user(user_add("fixture-bob", admin_role, "fixture-password"))
        .await
        .expect("second");
    storage.persist().await.expect("seed");

    // A real rotation pushes the credential high-water mark above the accounts.
    storage
        .modify_user(
            second.id,
            StorageUserModify {
                password: Some(Some(StoragePassword::new("fixture-rotated").expect("hash"))),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("rotation");

    let before = published(&storage);
    assert!(
        before.2 >= 2,
        "the id allocator must have advanced past both accounts: {before:?}"
    );
    assert!(
        before.1 > 0,
        "a committed rotation must reserve a generation: {before:?}"
    );

    // Host-only mutations: add, modify, remove. None of them is an identity change.
    let host = storage
        .add_host(fixture_host(Some(admin.id), "recon-host"))
        .await
        .expect("host add");
    storage
        .modify_host(
            host.id,
            StorageHostModify {
                cache_name: Some("recon-renamed".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("host modify");
    storage.remove_host(host.id).await.expect("host remove");

    let after = published(&storage);
    assert_eq!(
        (after.0, after.2),
        (before.0, before.2),
        "a host mutation changed the identity epoch or lowered/dropped the id \
         high-water mark: before={before:?} after={after:?}"
    );
    assert!(
        after.1 >= before.1,
        "a host mutation must not lower the credential high-water mark: \
         before={before:?} after={after:?}"
    );

    // The candidate that was committed must itself carry the marks, not just the
    // live atomics — otherwise the next reload restarts them from zero.
    let json = storage.export_backup().await.expect("live backup");
    assert_eq!(
        json["next_user_id"].as_u64().unwrap_or(0),
        after.2,
        "the committed host mutation dropped next_user_id"
    );
    assert_eq!(
        json["identity_epoch"].as_u64().unwrap_or(0),
        after.0,
        "the committed host mutation dropped identity_epoch"
    );
    assert!(
        json["next_credential_version"].as_u64().unwrap_or(0) >= after.1,
        "the committed host mutation dropped next_credential_version"
    );

    // Reload: the marks are persisted state, so a restart keeps them.
    let reloaded = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("reload");
    let reloaded_marks = published(&reloaded);
    assert!(
        reloaded_marks.1 >= after.1 && reloaded_marks.2 >= after.2,
        "a reload must not lower the persisted marks: live={after:?} reloaded={reloaded_marks:?}"
    );

    // And the consequence that matters: the next account never reuses a used id.
    let role = reloaded
        .add_role(role_add("Users", RoleType::User))
        .await
        .expect("role")
        .id;
    let created = reloaded
        .add_user(user_add("fixture-carol", role, "fixture-password"))
        .await
        .expect("third account");
    assert!(
        u64::from(created.id.0) >= before.2,
        "a reloaded store reinstated an id it had already handed out: \
         created={} mark={}",
        created.id.0,
        before.2
    );

    fs::remove_dir_all(dir).await.expect("cleanup");
}

/// The persisted marks as an outside observer sees them: `export_backup` serializes
/// the *live* atomics, so this is the published state, not a private field read.
fn marks_of(json: &serde_json::Value) -> (u64, u64, u64) {
    (
        json["identity_epoch"].as_u64().unwrap_or(0),
        json["next_credential_version"].as_u64().unwrap_or(0),
        json["next_user_id"].as_u64().unwrap_or(0),
    )
}

/// AR-05 x AR-01: a mutation whose durable write fails must not advance any
/// published counter — otherwise a later account would be told its generation or id
/// was reserved while nothing was ever committed.
#[tokio::test]
async fn failed_mutations_do_not_advance_published_counters() {
    let dir = unique_dir("failed-marks");
    let app = App::new(config_for(&dir)).await.expect("app");
    let admin = app
        .try_add_first_login("fixture-admin".to_owned(), "fixture-password".to_owned())
        .await
        .expect("bootstrap")
        .into_admin()
        .await
        .expect("admin proof");
    let admin_id = admin.id;
    let admin_role = app
        .inner
        .storage
        .get_user(admin_id)
        .await
        .expect("admin row")
        .role_id;
    let storage = app.inner.storage.clone();
    storage.persist().await.expect("seed");

    let before = marks_of(&storage.export_backup().await.expect("backup before"));
    let version_before = storage
        .get_user(admin_id)
        .await
        .expect("admin row")
        .credential_version;
    let disk_before = fs::read(dir.join("data.json")).await.expect("primary");

    let blocked = block_staging(&dir).await;
    let failed_add_user = storage
        .add_user(user_add("ghost-user", admin_role, "fixture-password"))
        .await;
    let failed_add_host = storage.add_host(fixture_host(None, "ghost-host")).await;
    let failed_rotate = storage
        .modify_user(
            admin_id,
            StorageUserModify {
                password: Some(Some(
                    StoragePassword::new("fixture-must-not-land").expect("hash"),
                )),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await;
    unblock_staging(blocked).await;

    assert!(
        failed_add_user.is_err(),
        "a user creation that cannot commit must not report success"
    );
    assert!(
        failed_add_host.is_err(),
        "a host add that cannot commit must not report success"
    );
    assert!(
        failed_rotate.is_err(),
        "a rotation that cannot commit must not report success"
    );

    let after = marks_of(&storage.export_backup().await.expect("backup after"));
    assert_eq!(
        after, before,
        "a failed mutation advanced a published counter: before={before:?} after={after:?}"
    );
    assert_eq!(
        storage
            .get_user(admin_id)
            .await
            .expect("admin row after failure")
            .credential_version,
        version_before,
        "a failed rotation published a new generation to the live row"
    );
    assert_eq!(
        fs::read(dir.join("data.json"))
            .await
            .expect("primary after"),
        disk_before,
        "a failed mutation changed the durable snapshot"
    );
    assert_eq!(
        durable(&dir).await["next_user_id"].as_u64().unwrap_or(0),
        before.2,
        "a failed mutation advanced the durable id high-water mark"
    );

    fs::remove_dir_all(dir).await.expect("cleanup");
}
