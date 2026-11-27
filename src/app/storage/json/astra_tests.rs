//! Audit-only regressions. Isolated fixtures, no remote hosts or clipboard.
use super::*;
use crate::app::{App, auth::UserAuth};
use common::config::{Config, StorageConfig};
use std::{
    future::Future,
    task::{Context, Poll},
};
#[path = "astra_bootstrap_tests.rs"]
mod bootstrap;

async fn app_fixture(label: &str) -> (App, PathBuf, crate::app::user::Admin) {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test fixture operation must succeed")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("astra-{label}-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir)
        .await
        .expect("test fixture operation must succeed");
    let mut config = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };
    config.web_server.first_login_create_admin = true;
    let app = App::new(config)
        .await
        .expect("test fixture operation must succeed");
    let user = app
        .try_add_first_login("audit-admin".into(), "fixture-old-password".into())
        .await
        .expect("test fixture operation must succeed");
    let admin = user
        .into_admin()
        .await
        .expect("test fixture operation must succeed");
    app.inner
        .storage
        .persist()
        .await
        .expect("test fixture operation must succeed");
    (app, dir, admin)
}
fn role(name: &str, ty: RoleType) -> StorageRoleAdd {
    StorageRoleAdd {
        name: name.into(),
        ty,
        default_settings: Default::default(),
        permissions: Default::default(),
    }
}
fn new_user(name: &str, role_id: RoleId) -> StorageUserAdd {
    StorageUserAdd {
        name: name.into(),
        role_id,
        password: Some(
            StoragePassword::new("fixture-old-password")
                .expect("test fixture operation must succeed"),
        ),
        client_unique_id: name.into(),
    }
}
fn poll_once<F: Future>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
}

#[tokio::test]
async fn astra_password_rotation_must_reject_stale_authentication_snapshot() {
    let (app, dir, admin) = app_fixture("stale-auth").await;
    let stale = app
        .user_by_name("audit-admin")
        .await
        .expect("test fixture operation must succeed");
    // Same snapshot loaded by user_by_auth before the async PBKDF2 wait.
    let mut changing = app
        .user_by_id(admin.id())
        .await
        .expect("test fixture operation must succeed");
    changing
        .modify(
            &admin,
            StorageUserModify {
                password: Some(Some(
                    StoragePassword::new("fixture-new-password")
                        .expect("test fixture operation must succeed"),
                )),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("test fixture operation must succeed");
    assert!(
        app.user_by_auth(UserAuth::UserPassword {
            username: "audit-admin".into(),
            password: "fixture-old-password".into()
        })
        .await
        .is_err(),
        "fresh old-password lookup must be rejected"
    );
    let old_auth = stale
        .authenticate(&UserAuth::UserPassword {
            username: "audit-admin".into(),
            password: "fixture-old-password".into(),
        })
        .await;
    let accepted_session = match old_auth {
        Ok(user) => {
            let token = user
                .new_session(Duration::from_secs(3600))
                .await
                .expect("test fixture operation must succeed");
            app.user_by_auth(UserAuth::Session(token)).await.is_ok()
        }
        Err(_) => false,
    };
    assert!(
        app.user_by_auth(UserAuth::UserPassword {
            username: "audit-admin".into(),
            password: "fixture-new-password".into()
        })
        .await
        .is_ok(),
        "control: rotated password remains usable"
    );
    eprintln!(
        "ASTRA_STALE_AUTH fresh_old_password_rejected=true fresh_new_password_accepted=true stale_old_password_session_valid={accepted_session}"
    );
    fs::remove_dir_all(dir)
        .await
        .expect("test fixture operation must succeed");
    assert!(
        !accepted_session,
        "password rotation allowed a stale credential check to mint a usable new session"
    );
}

#[tokio::test]
async fn astra_legacy_rehash_must_keep_other_sessions() {
    let (app, dir, admin) = app_fixture("legacy-rehash").await;
    let mut hash = [0u8; 32];
    let salt = [9u8; 16];
    openssl::pkcs5::pbkdf2_hmac(
        b"fixture-old-password",
        &salt,
        1000,
        openssl::hash::MessageDigest::sha256(),
        &mut hash,
    )
    .expect("test fixture operation must succeed");
    app.inner
        .storage
        .modify_user(
            admin.id(),
            StorageUserModify {
                password: Some(Some(StoragePassword {
                    salt,
                    hash,
                    iterations: 1000,
                })),
                ..Default::default()
            },
        )
        .await
        .expect("test fixture operation must succeed");
    app.inner
        .storage
        .persist()
        .await
        .expect("test fixture operation must succeed");
    let previous = admin
        .new_session(Duration::from_secs(3600))
        .await
        .expect("test fixture operation must succeed");
    app.user_by_auth(UserAuth::UserPassword {
        username: "audit-admin".into(),
        password: "fixture-old-password".into(),
    })
    .await
    .expect("test fixture operation must succeed");
    let kept = app.user_by_auth(UserAuth::Session(previous)).await.is_ok();
    eprintln!("ASTRA_LEGACY_REHASH previous_session_valid={kept}");
    fs::remove_dir_all(dir)
        .await
        .expect("test fixture operation must succeed");
    assert!(
        kept,
        "transparent legacy rehash used explicit password-rotation revocation path"
    );
}

#[tokio::test]
async fn astra_cancelled_backup_restore_must_not_split_disk_and_live_state() {
    let (app, dir, admin) = app_fixture("restore-cancel").await;
    let token = admin
        .new_session(Duration::from_secs(3600))
        .await
        .expect("test fixture operation must succeed");
    let before = app
        .export_backup(&admin)
        .await
        .expect("test fixture operation must succeed");
    let mut candidate = before.clone();
    for user in candidate["users"]
        .as_object_mut()
        .expect("test fixture operation must succeed")
        .values_mut()
    {
        user["name"] = "restored-admin".into();
    }
    let primary_before =
        std::fs::read(dir.join("data.json")).expect("test fixture operation must succeed");
    let mut restore = Box::pin(app.import_backup(&admin, candidate));
    let mut committed = false;
    for _ in 0..5000 {
        if std::fs::read(dir.join("data.json")).expect("test fixture operation must succeed")
            != primary_before
        {
            committed = true;
            break;
        }
        if let Poll::Ready(result) = poll_once(restore.as_mut()) {
            panic!("restore ended before selected cancellation boundary: {result:?}");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(committed, "must observe actual primary rename");
    drop(restore);
    let durable: serde_json::Value = serde_json::from_slice(
        &fs::read(dir.join("data.json"))
            .await
            .expect("test fixture operation must succeed"),
    )
    .expect("test fixture operation must succeed");
    let live = app
        .export_backup(&admin)
        .await
        .expect("test fixture operation must succeed");
    let old_session_valid = app.user_by_auth(UserAuth::Session(token)).await.is_ok();
    app.inner
        .storage
        .persist()
        .await
        .expect("test fixture operation must succeed");
    let later: serde_json::Value = serde_json::from_slice(
        &fs::read(dir.join("data.json"))
            .await
            .expect("test fixture operation must succeed"),
    )
    .expect("test fixture operation must succeed");
    eprintln!(
        "ASTRA_RESTORE durable_equals_live={} old_session_valid={} later_persist_restores_old={}",
        durable == live,
        old_session_valid,
        later == before
    );
    fs::remove_dir_all(dir)
        .await
        .expect("test fixture operation must succeed");
    assert_eq!(
        live, durable,
        "cancelled backup restore committed disk but not live identities/session revocation"
    );
    assert_eq!(
        later, durable,
        "subsequent persist undid the committed restore"
    );
    assert!(!old_session_valid);
}

#[tokio::test]
async fn astra_direct_user_delete_must_leave_reimportable_ownership() {
    let (app, dir, admin) = app_fixture("delete-host").await;
    let role_id = app
        .inner
        .storage
        .add_role(role("User", RoleType::User))
        .await
        .expect("test fixture operation must succeed")
        .id;
    let user = app
        .add_user(&admin, new_user("fixture-ordinary", role_id))
        .await
        .expect("test fixture operation must succeed");
    let id = user.id();
    let host = app
        .inner
        .storage
        .add_host(StorageHostAdd {
            owner: Some(id),
            address: "127.0.0.1".into(),
            http_port: 47989,
            pair_info: None,
            cache: StorageHostCache {
                name: "offline-fixture".into(),
                mac: None,
            },
        })
        .await
        .expect("test fixture operation must succeed");
    app.user_by_id(id)
        .await
        .expect("test fixture operation must succeed")
        .delete(&admin)
        .await
        .expect("test fixture operation must succeed");
    let owner = app
        .inner
        .storage
        .get_host(host.id)
        .await
        .expect("test fixture operation must succeed")
        .owner;
    let backup = app
        .export_backup(&admin)
        .await
        .expect("test fixture operation must succeed");
    let reimport = app.import_backup(&admin, backup).await;
    eprintln!(
        "ASTRA_DELETE deleted_user_still_owner={} own_backup_reimport_ok={}",
        owner == Some(id),
        reimport.is_ok()
    );
    fs::remove_dir_all(dir)
        .await
        .expect("test fixture operation must succeed");
    assert!(
        reimport.is_ok(),
        "normal user deletion exported a backup the same importer rejects due to orphaned ownership"
    );
}

#[tokio::test]
async fn astra_add_user_racing_role_delete_must_not_create_orphan() {
    let (app, dir, admin) = app_fixture("orphan-race-seed").await;
    let role_id = app
        .inner
        .storage
        .add_role(role("Unused", RoleType::User))
        .await
        .expect("test fixture operation must succeed")
        .id;
    app.inner
        .storage
        .persist()
        .await
        .expect("test fixture operation must succeed");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("test fixture operation must succeed");
    let users_gate = storage.users.write().await;
    let mut adding = Box::pin(storage.add_user(new_user("racing", role_id)));
    assert!(
        poll_once(adding.as_mut()).is_pending(),
        "role lookup passed; user-name lookup waits on users gate"
    );
    let mut deleting = Box::pin(storage.remove_role(role_id));
    assert!(poll_once(deleting.as_mut()).is_pending());
    tokio::task::yield_now().await; // writer now holds store_lock and waits for users.
    assert!(
        storage.store_lock.try_lock().is_err(),
        "deletion writer acquired commit lock"
    );
    drop(users_gate);
    assert!(
        poll_once(adding.as_mut()).is_pending(),
        "after username check insertion must queue behind deletion"
    );
    tokio::time::timeout(Duration::from_secs(3), deleting)
        .await
        .expect("test fixture operation must succeed")
        .expect("test fixture operation must succeed");
    let created = tokio::time::timeout(Duration::from_secs(3), adding)
        .await
        .expect("test fixture operation must succeed");
    let role_exists = storage.get_role(role_id).await.is_ok();
    eprintln!(
        "ASTRA_ROLE_RACE user_created={} referenced_role_exists={role_exists}",
        created.is_ok()
    );
    let orphan = created.is_ok() && !role_exists;
    drop(admin);
    drop(app);
    fs::remove_dir_all(dir)
        .await
        .expect("test fixture operation must succeed");
    assert!(
        !orphan,
        "role validation ran outside the user insertion transaction"
    );
}
