//! Role-cascade admin and transaction regressions on real JSON storage.
use super::*;
use crate::app::{App, auth::UserAuth};
use common::config::{Config, StorageConfig};

#[path = "cancellation_tests.rs"]
mod cancellation_tests;

async fn fixture_dir(label: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-role-cascade-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).await.expect("fixture directory");
    dir
}

fn role_add(name: &str, ty: RoleType) -> StorageRoleAdd {
    StorageRoleAdd {
        name: name.to_owned(),
        ty,
        default_settings: Default::default(),
        permissions: Default::default(),
    }
}

fn user_add(name: &str, role_id: RoleId, password: Option<StoragePassword>) -> StorageUserAdd {
    StorageUserAdd {
        role_id,
        name: name.to_owned(),
        password,
        client_unique_id: name.to_owned(),
    }
}

#[tokio::test]
async fn app_role_cascade_rejects_deleting_both_remaining_admins() {
    let dir = fixture_dir("app-two-admins").await;
    let mut config = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };
    config.web_server.first_login_create_admin = true;
    let app = App::new(config).await.expect("real App");
    let mut alice = app
        .try_add_first_login("fixture-alice".to_owned(), "fixture-password".to_owned())
        .await
        .expect("fixture bootstrap");
    let role_id = alice.role_id().await.expect("admin role");
    let alice_id = alice.id();
    let admin = alice.into_admin().await.expect("real admin proof");
    let bob = app
        .add_user(
            &admin,
            user_add(
                "fixture-bob",
                role_id,
                Some(StoragePassword::new("fixture-password").expect("hash")),
            ),
        )
        .await
        .expect("second admin");
    let alice_token = admin
        .new_session(Duration::from_secs(3600))
        .await
        .expect("alice session");
    let bob_token = bob
        .new_session(Duration::from_secs(3600))
        .await
        .expect("bob session");
    let host = app
        .inner
        .storage
        .add_host(StorageHostAdd {
            owner: Some(alice_id),
            address: "127.0.0.1".to_owned(),
            http_port: 47989,
            pair_info: None,
            cache: StorageHostCache {
                name: "fixture-host".to_owned(),
                mac: None,
            },
        })
        .await
        .expect("fixture host; no network access");
    app.inner
        .storage
        .persist()
        .await
        .expect("seed durable state");
    let before = app.inner.storage.export_backup().await.expect("before");
    let durable_before = fs::read(dir.join("data.json"))
        .await
        .expect("durable before");

    // This is the exact App/Role path called by DELETE /role. No fabricated Admin.
    let result = app
        .role_by_id(role_id)
        .await
        .expect("role")
        .delete(&admin)
        .await;
    assert!(
        matches!(result, Err(AppError::Forbidden)),
        "the whole deletion set must be rejected, got {result:?}"
    );
    assert_eq!(
        app.inner.storage.export_backup().await.expect("after"),
        before
    );
    assert_eq!(
        fs::read(dir.join("data.json"))
            .await
            .expect("durable after"),
        durable_before
    );
    assert_eq!(
        app.inner
            .storage
            .get_host(host.id)
            .await
            .expect("host")
            .owner,
        Some(alice_id)
    );
    for token in [alice_token, bob_token] {
        assert!(
            app.user_by_auth(UserAuth::Session(token)).await.is_ok(),
            "rejection must preserve sessions"
        );
    }
    fs::remove_dir_all(dir).await.expect("cleanup");
}

struct CascadeFixture {
    storage: Arc<JsonStorage>,
    dir: PathBuf,
    removed_role: RoleId,
    survivor_role: RoleId,
    users: Vec<UserId>,
    tokens: Vec<SessionToken>,
}

async fn cascade_fixture(label: &str) -> CascadeFixture {
    let dir = fixture_dir(label).await;
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("real storage");
    let removed_role = storage
        .add_role(role_add("Removed admins", RoleType::Admin))
        .await
        .expect("role")
        .id;
    let survivor_role = storage
        .add_role(role_add("Surviving admin", RoleType::Admin))
        .await
        .expect("role")
        .id;
    let password = StoragePassword::new("fixture-password").expect("real password hash");
    let mut users = Vec::new();
    let mut tokens = Vec::new();
    for (name, role) in [
        ("fixture-alice", removed_role),
        ("fixture-bob", removed_role),
        ("fixture-root", survivor_role),
    ] {
        let user = storage
            .add_user(user_add(name, role, Some(password.clone())))
            .await
            .expect("user");
        users.push(user.id);
        tokens.push(
            storage
                .create_session_token(user.id, Duration::from_secs(3600))
                .await
                .expect("session"),
        );
        storage
            .add_host(StorageHostAdd {
                owner: Some(user.id),
                address: "127.0.0.1".to_owned(),
                http_port: 47989,
                pair_info: None,
                cache: StorageHostCache {
                    name: name.to_owned(),
                    mac: None,
                },
            })
            .await
            .expect("fixture host; no network access");
    }
    storage.persist().await.expect("seed primary");
    storage.persist().await.expect("seed backup");
    CascadeFixture {
        storage,
        dir,
        removed_role,
        survivor_role,
        users,
        tokens,
    }
}

#[tokio::test]
async fn role_cascade_write_failure_preserves_all_state() {
    let f = cascade_fixture("failure").await;
    let before = f.storage.export_backup().await.expect("before");
    let primary = fs::read(&f.storage.file).await.expect("primary");
    let backup = fs::read(path_with_suffix(&f.storage.file, ".bak"))
        .await
        .expect("backup");
    // An actual filesystem error, including when tests run as root. No failpoint.
    let blocked_temp = path_with_suffix(&f.storage.file, ".tmp");
    fs::create_dir(&blocked_temp)
        .await
        .expect("block staging file");
    let result = f.storage.remove_role(f.removed_role).await;
    assert!(
        result.is_err(),
        "a failed role transaction must return an error, got {result:?}"
    );
    assert_eq!(f.storage.export_backup().await.expect("after"), before);
    assert_eq!(
        fs::read(&f.storage.file).await.expect("primary after"),
        primary
    );
    assert_eq!(
        fs::read(path_with_suffix(&f.storage.file, ".bak"))
            .await
            .expect("backup after"),
        backup
    );
    for (id, token) in f.users.iter().zip(&f.tokens) {
        assert_eq!(
            f.storage
                .get_user_by_session_token(*token)
                .await
                .expect("unchanged session")
                .0,
            *id
        );
    }
    fs::remove_dir(blocked_temp).await.expect("unblock staging");
    f.storage.persist().await.expect("writer remains usable");
    let reloaded = JsonStorage::load(f.storage.file.clone(), Duration::from_secs(3600))
        .await
        .expect("reload");
    assert_eq!(reloaded.export_backup().await.expect("reloaded"), before);
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test]
async fn role_cascade_success_is_durable_and_revokes_only_removed_users() {
    let f = cascade_fixture("success").await;
    let primary_before = fs::read(&f.storage.file).await.expect("primary before");
    f.storage
        .remove_role(f.removed_role)
        .await
        .expect("safe cascade");
    let after = f.storage.export_backup().await.expect("after");
    let durable: serde_json::Value =
        serde_json::from_slice(&fs::read(&f.storage.file).await.expect("durable")).expect("json");
    assert_eq!(
        durable, after,
        "success must already be durable without another persist"
    );
    assert_eq!(
        fs::read(path_with_suffix(&f.storage.file, ".bak"))
            .await
            .expect("backup"),
        primary_before
    );
    assert_eq!(after["users"].as_object().expect("users").len(), 1);
    assert!(matches!(
        f.storage.get_role(f.removed_role).await,
        Err(AppError::RoleNotFound)
    ));
    assert!(f.storage.get_role(f.survivor_role).await.is_ok());
    for token in &f.tokens[..2] {
        assert!(matches!(
            f.storage.get_user_by_session_token(*token).await,
            Err(AppError::SessionTokenNotFound)
        ));
    }
    assert_eq!(
        f.storage
            .get_user_by_session_token(f.tokens[2])
            .await
            .expect("survivor session")
            .0,
        f.users[2]
    );
    for host in after["hosts"].as_object().expect("hosts").values() {
        if host["cache"]["name"] == "fixture-root" {
            assert_eq!(host["owner"], f.users[2].0);
        } else {
            assert!(
                host["owner"].is_null(),
                "only cascaded owners become global"
            );
        }
    }
    let reloaded = JsonStorage::load(f.storage.file.clone(), Duration::from_secs(3600))
        .await
        .expect("reload");
    assert_eq!(reloaded.export_backup().await.expect("reloaded"), after);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            f.storage.file.clone(),
            path_with_suffix(&f.storage.file, ".bak"),
        ] {
            assert_eq!(
                std::fs::metadata(path).expect("mode").permissions().mode() & 0o777,
                0o600
            );
        }
    }
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_cascade_concurrent_deletions_keep_one_admin_role() {
    let f = cascade_fixture("concurrent").await;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for role in [f.removed_role, f.survivor_role] {
        let storage = f.storage.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            storage.remove_role(role).await
        }));
    }
    barrier.wait().await;
    let results = tokio::time::timeout(Duration::from_secs(5), futures::future::join_all(tasks))
        .await
        .expect("no deadlock");
    let results: Vec<_> = results.into_iter().map(|r| r.expect("task")).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(AppError::Forbidden)))
            .count(),
        1
    );
    let after = f.storage.export_backup().await.expect("after");
    assert_eq!(after["roles"].as_object().expect("roles").len(), 1);
    let durable: serde_json::Value =
        serde_json::from_slice(&fs::read(&f.storage.file).await.expect("durable")).expect("json");
    assert_eq!(durable, after);
    for (id, token) in f.users.iter().zip(&f.tokens) {
        if f.storage.get_user(*id).await.is_ok() {
            assert_eq!(
                f.storage
                    .get_user_by_session_token(*token)
                    .await
                    .expect("surviving session")
                    .0,
                *id
            );
        } else {
            assert!(matches!(
                f.storage.get_user_by_session_token(*token).await,
                Err(AppError::SessionTokenNotFound)
            ));
        }
    }
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test]
async fn role_cascade_waits_for_writer_before_mutating() {
    let f = cascade_fixture("writer-gate").await;
    let before = f.storage.export_backup().await.expect("before");
    let gate = f.storage.store_lock.lock().await;
    let deletion = f.storage.remove_role(f.removed_role);
    tokio::pin!(deletion);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut deletion)
            .await
            .is_err(),
        "deletion must wait for the real writer gate"
    );
    assert_eq!(
        f.storage.export_backup().await.expect("still readable"),
        before
    );
    for token in &f.tokens {
        assert!(f.storage.get_user_by_session_token(*token).await.is_ok());
    }
    drop(gate);
    deletion.await.expect("deletion after writer");
    f.storage
        .persist()
        .await
        .expect("acknowledged writer still works");
    let after = f.storage.export_backup().await.expect("after");
    let durable: serde_json::Value =
        serde_json::from_slice(&fs::read(&f.storage.file).await.expect("durable")).expect("json");
    assert_eq!(durable, after);
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test]
async fn role_cascade_requires_password_capable_survivor_in_password_store() {
    let f = cascade_fixture("passwordless-survivor").await;
    f.storage
        .modify_user(
            f.users[2],
            StorageUserModify {
                password: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect("fixture header-only survivor");
    f.storage.persist().await.expect("seed");
    let before = f.storage.export_backup().await.expect("before");
    let durable = fs::read(&f.storage.file).await.expect("durable");
    assert!(matches!(
        f.storage.remove_role(f.removed_role).await,
        Err(AppError::Forbidden)
    ));
    assert_eq!(f.storage.export_backup().await.expect("after"), before);
    assert_eq!(
        fs::read(&f.storage.file).await.expect("after durable"),
        durable
    );
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test]
async fn role_cascade_passwordless_store_keeps_a_valid_admin() {
    let f = cascade_fixture("header-only").await;
    for id in &f.users {
        f.storage
            .modify_user(
                *id,
                StorageUserModify {
                    password: Some(None),
                    ..Default::default()
                },
            )
            .await
            .expect("fixture header-only user");
    }
    f.storage.persist().await.expect("seed");
    f.storage
        .remove_role(f.survivor_role)
        .await
        .expect("two header admins remain");
    let before = f.storage.export_backup().await.expect("before");
    let durable = fs::read(&f.storage.file).await.expect("durable");
    assert!(matches!(
        f.storage.remove_role(f.removed_role).await,
        Err(AppError::Forbidden)
    ));
    assert_eq!(f.storage.export_backup().await.expect("after"), before);
    assert_eq!(
        fs::read(&f.storage.file).await.expect("after durable"),
        durable
    );
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test]
async fn app_role_cascade_write_failure_preserves_live_identity_and_sessions() {
    let f = cascade_fixture("app-failure").await;
    let app = App::new(Config {
        data_storage: StorageConfig::Json {
            path: f.storage.file.to_string_lossy().into_owned(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    })
    .await
    .expect("real App from fixture storage");
    let user = app
        .user_by_auth(UserAuth::UserPassword {
            username: "fixture-alice".to_owned(),
            password: "fixture-password".to_owned(),
        })
        .await
        .expect("real password authentication");
    let token = user
        .new_session(Duration::from_secs(3600))
        .await
        .expect("session");
    let admin = user.into_admin().await.expect("real admin proof");
    let before = app.inner.storage.export_backup().await.expect("before");
    let primary = fs::read(&f.storage.file).await.expect("primary");
    let backup = fs::read(path_with_suffix(&f.storage.file, ".bak"))
        .await
        .expect("backup");
    let blocked_temp = path_with_suffix(&f.storage.file, ".tmp");
    fs::create_dir(&blocked_temp)
        .await
        .expect("real staging failure");
    assert!(
        app.role_by_id(f.removed_role)
            .await
            .expect("role")
            .delete(&admin)
            .await
            .is_err()
    );
    assert_eq!(
        app.inner.storage.export_backup().await.expect("after"),
        before
    );
    assert_eq!(
        fs::read(&f.storage.file).await.expect("primary after"),
        primary
    );
    assert_eq!(
        fs::read(path_with_suffix(&f.storage.file, ".bak"))
            .await
            .expect("backup after"),
        backup
    );
    assert!(app.user_by_auth(UserAuth::Session(token)).await.is_ok());
    fs::remove_dir(blocked_temp).await.expect("unblock");
    app.inner.storage.persist().await.expect("writer recovery");
    let reloaded = JsonStorage::load(f.storage.file.clone(), Duration::from_secs(3600))
        .await
        .expect("reload");
    assert_eq!(reloaded.export_backup().await.expect("reloaded"), before);
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test]
async fn role_cascade_rename_failure_preserves_live_state_and_backup() {
    let f = cascade_fixture("rename-failure").await;
    let before = f.storage.export_backup().await.expect("before");
    let primary = fs::read(&f.storage.file).await.expect("primary");
    let backup = fs::read(path_with_suffix(&f.storage.file, ".bak"))
        .await
        .expect("backup");
    let saved_primary = f.dir.join("saved-primary.json");
    fs::rename(&f.storage.file, &saved_primary)
        .await
        .expect("save fixture primary");
    fs::create_dir(&f.storage.file)
        .await
        .expect("directory blocks final rename, not staging");
    assert!(f.storage.remove_role(f.removed_role).await.is_err());
    assert_eq!(f.storage.export_backup().await.expect("after"), before);
    assert_eq!(
        fs::read(&saved_primary).await.expect("saved primary"),
        primary
    );
    assert_eq!(
        fs::read(path_with_suffix(&f.storage.file, ".bak"))
            .await
            .expect("backup after"),
        backup
    );
    for token in &f.tokens {
        assert!(f.storage.get_user_by_session_token(*token).await.is_ok());
    }
    fs::remove_dir(&f.storage.file).await.expect("unblock");
    fs::rename(saved_primary, &f.storage.file)
        .await
        .expect("restore fixture primary");
    f.storage.persist().await.expect("writer recovery");
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_cascade_and_import_are_serialized_and_durable() {
    let f = cascade_fixture("import-race").await;
    let mut imported = f.storage.export_backup().await.expect("backup");
    imported["users"][f.users[2].0.to_string()]["client_unique_id"] = "imported-root".into();
    let before_epoch = f.storage.identity_epoch();
    let (deletion, restore) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            f.storage.remove_role(f.removed_role),
            f.storage.import_backup(imported)
        )
    })
    .await
    .expect("no deadlock");
    deletion.expect("safe deletion");
    restore.expect("real import");
    assert_eq!(f.storage.identity_epoch(), before_epoch + 1);
    let after = f.storage.export_backup().await.expect("after");
    let durable: serde_json::Value =
        serde_json::from_slice(&fs::read(&f.storage.file).await.expect("durable")).expect("json");
    assert_eq!(
        durable, after,
        "one complete serial outcome must reach disk"
    );
    assert_eq!(
        f.storage
            .get_user(f.users[2])
            .await
            .expect("surviving admin")
            .client_unique_id,
        "imported-root"
    );
    assert!(
        f.storage
            .list_sessions()
            .await
            .expect("sessions")
            .is_empty(),
        "import still revokes every prior session"
    );
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}
