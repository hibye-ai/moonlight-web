//! AR-03 / AR-05 transactional mutation controls.
//!
//! These drive the real `App`/`User`/`Host`/`JsonStorage` paths — no copied state
//! model and no failpoints. A real directory in place of the `.tmp` staging path
//! makes the staging write fail on any platform, including as root.
use super::*;
use crate::app::{App, auth::UserAuth};
use common::config::{Config, StorageConfig};
use std::{future::Future, task::Context};

fn unique_dir(label: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-mutation-{label}-{}-{unique}",
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

/// Blocks the real staging write: `create_private_temp_file` cannot create a file
/// where a directory already sits, and the cleanup path cannot remove a non-empty
/// one either. This is a filesystem error, not a failpoint.
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

// ---------------------------------------------------------------- AR-03 ----

/// AR-03: a user creation must not be able to slip in *between* a role-existence
/// check and the insert when a deletion of that role is already under way.
///
/// Both requests are submitted through the real writer (which serializes them), so
/// exactly one linearization wins. The contract is referential integrity of the
/// committed state — plus the durability of whatever was acknowledged — not a
/// particular return status: if the creation wins it is then cascaded away with
/// the role, and if the deletion is already admitted the creation is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_user_racing_admitted_role_delete_leaves_no_orphan_reference() {
    for round in 0..20 {
        let dir = unique_dir(&format!("race-orphan-{round}"));
        let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
            .await
            .expect("storage");
        // A real admin keeps the store administrable, so the cascade is allowed and
        // the race exercises the reference invariant rather than last-admin refusal.
        let admin_role = storage
            .add_role(role_add("Admin", RoleType::Admin))
            .await
            .expect("role")
            .id;
        storage
            .add_user(user_add("fixture-root", admin_role, "fixture-password"))
            .await
            .expect("admin");
        let role = storage
            .add_role(role_add("Racing", RoleType::User))
            .await
            .expect("role");
        storage.persist().await.expect("seed");

        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        // Both requests are submitted through the real writer; the channel
        // serializes them, so one of the two valid linearizations happens. Twenty
        // rounds exercise both orders without asserting a particular one.
        let deleting = {
            let storage = storage.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                storage.remove_role(role.id).await
            })
        };
        let adding = {
            let storage = storage.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                storage
                    .add_user(user_add("racing", role.id, "fixture-password"))
                    .await
            })
        };
        barrier.wait().await;

        let (deleted, added) = tokio::time::timeout(Duration::from_secs(10), async {
            (
                deleting.await.expect("join deletion"),
                adding.await.expect("join creation"),
            )
        })
        .await
        .expect("neither mutation may deadlock");
        let created_id = added.as_ref().ok().map(|user| user.id);
        let created_ok = created_id.is_some();
        assert!(
            deleted.is_ok(),
            "the unused role deletion must commit: {deleted:?}"
        );

        let live = storage.export_backup().await.expect("live");
        let disk = durable(&dir).await;
        let role_exists = storage.get_role(role.id).await.is_ok();
        let orphan_users: Vec<String> = live["users"]
            .as_object()
            .expect("users")
            .values()
            .filter(|user| {
                let role_id = user["role_id"].as_u64().expect("role_id");
                live["roles"].get(role_id.to_string()).is_none()
            })
            .map(|user| user["name"].as_str().unwrap_or_default().to_owned())
            .collect();
        eprintln!(
            "MUTATION_ROLE_RACE round={round} created={created_ok} role_exists={role_exists} \
             orphans={orphan_users:?} live_equals_disk={}",
            live == disk
        );

        assert!(
            orphan_users.is_empty(),
            "round {round}: committed state references a role that no longer exists"
        );
        assert_eq!(
            live, disk,
            "round {round}: the acknowledged outcome must be durable"
        );
        // A reported creation that the cascade then removed is fine; a reported
        // creation that survives with its role gone is not.
        if let Some(created_id) = created_id {
            assert!(
                role_exists || storage.get_user(created_id).await.is_err(),
                "round {round}: creation was acknowledged but survived its deleted role"
            );
        }

        // A real reload must agree with the committed state.
        let reloaded = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
            .await
            .expect("reload");
        assert_eq!(reloaded.export_backup().await.expect("reloaded"), disk);

        fs::remove_dir_all(dir).await.expect("cleanup");
    }
}

/// AR-03: a direct user deletion must apply the same owned-host policy as the role
/// cascade. The app's own exported backup must stay importable, and the surviving
/// state must be what a restart loads.
#[tokio::test]
async fn direct_user_delete_clears_owned_hosts_revokes_sessions_and_reloads() {
    let dir = unique_dir("delete-ownership");
    let app = App::new(config_for(&dir)).await.expect("app");
    let admin = app
        .try_add_first_login("fixture-admin".to_owned(), "fixture-password".to_owned())
        .await
        .expect("bootstrap")
        .into_admin()
        .await
        .expect("admin proof");

    let user_role = app
        .inner
        .storage
        .add_role(role_add("Ordinary", RoleType::User))
        .await
        .expect("role")
        .id;
    let victim = app
        .add_user(
            &admin,
            user_add("fixture-victim", user_role, "fixture-password"),
        )
        .await
        .expect("user");
    let victim_id = victim.id();
    let victim_token = victim
        .new_session(Duration::from_secs(3600))
        .await
        .expect("session");
    let owned = app
        .inner
        .storage
        .add_host(fixture_host(Some(victim_id), "victim-desktop"))
        .await
        .expect("host");
    let global = app
        .inner
        .storage
        .add_host(fixture_host(None, "global-desktop"))
        .await
        .expect("host");

    app.user_by_id(victim_id)
        .await
        .expect("handle")
        .delete(&admin)
        .await
        .expect("delete");

    let after_delete = durable(&dir).await;
    let live = app.export_backup(&admin).await.expect("live backup");
    let owned_owner = app
        .inner
        .storage
        .get_host(owned.id)
        .await
        .expect("owned host")
        .owner;
    let session_valid = app
        .user_by_auth(UserAuth::Session(victim_token))
        .await
        .is_ok();
    assert_eq!(
        live, after_delete,
        "the deletion must be durable when acknowledged"
    );

    // The exported backup must be accepted by the same instance's importer.
    let reimport = app.import_backup(&admin, live.clone()).await;
    let reimported_ok = reimport.is_ok();
    eprintln!(
        "MUTATION_DIRECT_DELETE owner={owned_owner:?} reimport_ok={reimported_ok} \
         session_valid={session_valid} live_equals_disk={}",
        live == after_delete
    );

    assert_eq!(
        owned_owner, None,
        "a deleted user must not stay a host owner"
    );
    assert_eq!(
        app.inner
            .storage
            .get_host(global.id)
            .await
            .expect("global host")
            .owner,
        None,
        "an unrelated global host must be untouched"
    );
    assert!(
        reimported_ok,
        "the app exported a backup its own importer rejects: {reimport:?}"
    );
    assert!(
        !session_valid,
        "a deleted account's session must be revoked"
    );

    // Reload from disk (no live cache) and compare against the durable state.
    let disk = durable(&dir).await;
    assert_eq!(
        app.export_backup(&admin).await.expect("live after import"),
        disk
    );
    let reloaded = App::new(config_for(&dir)).await.expect("reload");
    let reloaded_admin = reloaded
        .user_by_auth(UserAuth::UserPassword {
            username: "fixture-admin".to_owned(),
            password: "fixture-password".to_owned(),
        })
        .await
        .expect("admin survives restart")
        .into_admin()
        .await
        .expect("admin proof");
    assert_eq!(
        reloaded
            .export_backup(&reloaded_admin)
            .await
            .expect("reloaded backup"),
        disk,
        "a restart must load exactly what was committed"
    );
    assert!(reloaded.inner.storage.get_user(victim_id).await.is_err());

    fs::remove_dir_all(dir).await.expect("cleanup");
}

/// AR-03 negative control: a failed staging write during a direct user deletion
/// must leave the user, its hosts, its sessions and the durable file untouched.
#[tokio::test]
async fn direct_user_delete_write_failure_changes_nothing() {
    let dir = unique_dir("delete-failure");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("storage");
    let admin_role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role")
        .id;
    let user_role = storage
        .add_role(role_add("User", RoleType::User))
        .await
        .expect("role")
        .id;
    storage
        .add_user(user_add("fixture-root", admin_role, "fixture-password"))
        .await
        .expect("admin");
    let victim = storage
        .add_user(user_add("fixture-victim", user_role, "fixture-password"))
        .await
        .expect("user");
    let token = storage
        .create_session_token(victim.id, Duration::from_secs(3600))
        .await
        .expect("session");
    let host = storage
        .add_host(fixture_host(Some(victim.id), "victim-desktop"))
        .await
        .expect("host");
    storage.persist().await.expect("seed");

    let before = storage.export_backup().await.expect("before");
    let primary_before = fs::read(dir.join("data.json")).await.expect("primary");
    let blocked = block_staging(&dir).await;

    let result = storage.remove_user(victim.id).await;
    let failed = result.is_err();
    unblock_staging(blocked).await;

    eprintln!("MUTATION_DELETE_FAILURE failed={failed}");
    assert!(
        failed,
        "a deletion that cannot be committed must not report success"
    );
    assert_eq!(
        storage.export_backup().await.expect("after"),
        before,
        "live state must be unchanged"
    );
    assert_eq!(
        fs::read(dir.join("data.json"))
            .await
            .expect("primary after"),
        primary_before,
        "durable state must be unchanged"
    );
    assert_eq!(
        storage.get_host(host.id).await.expect("host").owner,
        Some(victim.id),
        "a failed deletion must not globalize the host"
    );
    assert!(
        storage.get_user_by_session_token(token).await.is_ok(),
        "a failed deletion must not revoke sessions"
    );
    // Recovery: the writer is still usable and the intended deletion then lands.
    storage
        .remove_user(victim.id)
        .await
        .expect("retry after unblocking");
    assert_eq!(
        storage.get_host(host.id).await.expect("host").owner,
        None,
        "the retried deletion applies the owned-host policy"
    );
    let disk = durable(&dir).await;
    assert_eq!(
        disk,
        storage.export_backup().await.expect("live after retry")
    );

    fs::remove_dir_all(dir).await.expect("cleanup");
}

/// AR-03: the last-admin protection survives the transactional rewrite.
#[tokio::test]
async fn last_admin_delete_is_still_refused_without_touching_disk() {
    let dir = unique_dir("last-admin");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("storage");
    let admin_role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role")
        .id;
    let only_admin = storage
        .add_user(user_add("fixture-root", admin_role, "fixture-password"))
        .await
        .expect("admin");
    storage.persist().await.expect("seed");

    let before = storage.export_backup().await.expect("before");
    let primary_before = fs::read(dir.join("data.json")).await.expect("primary");

    let result = storage.remove_user(only_admin.id).await;
    eprintln!("MUTATION_LAST_ADMIN result={result:?}");
    assert!(matches!(result, Err(AppError::Forbidden)), "got {result:?}");
    assert_eq!(storage.export_backup().await.expect("after"), before);
    assert_eq!(
        fs::read(dir.join("data.json"))
            .await
            .expect("primary after"),
        primary_before
    );
    assert!(storage.get_user(only_admin.id).await.is_ok());

    fs::remove_dir_all(dir).await.expect("cleanup");
}

// ---------------------------------------------------------------- AR-05 ----

/// AR-05: a first-login that cannot commit must not report success, must not
/// issue a usable session, and must not leave a ghost account that blocks the
/// retry. After the staging path is unblocked the same bootstrap must succeed.
#[tokio::test]
async fn bootstrap_write_failure_leaves_no_ghost_account_and_recovers() {
    let dir = unique_dir("bootstrap-ghost");
    let app = App::new(config_for(&dir)).await.expect("app");

    let blocked = block_staging(&dir).await;
    let first = app
        .try_add_first_login("fixture-admin".to_owned(), "fixture-password".to_owned())
        .await;
    let accepted = first.is_ok();
    let session_valid = if let Ok(user) = first {
        match user.new_session(Duration::from_secs(3600)).await {
            Ok(token) => app.user_by_auth(UserAuth::Session(token)).await.is_ok(),
            Err(_) => false,
        }
    } else {
        false
    };
    let user_rows_after_failure = app
        .inner
        .storage
        .list_users()
        .await
        .expect("users")
        .len_checked();
    let durable_after_failure = dir.join("data.json").is_file();

    unblock_staging(blocked).await;

    eprintln!(
        "MUTATION_BOOTSTRAP_GHOST accepted={accepted} session_valid={session_valid} \
         user_rows={user_rows_after_failure} durable={durable_after_failure}"
    );
    assert!(
        !accepted,
        "bootstrap acknowledged success although the account could not be committed"
    );
    assert!(
        !session_valid,
        "a failed bootstrap must not mint a usable session"
    );
    assert_eq!(
        user_rows_after_failure, 0,
        "a failed bootstrap must not leave a ghost account"
    );
    assert!(
        !durable_after_failure,
        "nothing may have been published to disk"
    );

    // Recovery: the real bootstrap path still works once storage can commit, and
    // it is durable before it is acknowledged.
    let admin = app
        .try_add_first_login("fixture-admin".to_owned(), "fixture-password".to_owned())
        .await
        .expect("recovered bootstrap")
        .into_admin()
        .await
        .expect("admin proof");
    let live = app.export_backup(&admin).await.expect("live");
    let disk = durable(&dir).await;
    eprintln!(
        "MUTATION_BOOTSTRAP_RECOVERY live_equals_disk={}",
        live == disk
    );
    assert_eq!(live, disk, "a successful bootstrap must already be durable");

    // A restart must find the very same single account.
    let reloaded = App::new(config_for(&dir)).await.expect("reload");
    assert!(
        reloaded
            .user_by_auth(UserAuth::UserPassword {
                username: "fixture-admin".to_owned(),
                password: "fixture-password".to_owned(),
            })
            .await
            .is_ok(),
        "the bootstrapped admin must survive a restart"
    );

    fs::remove_dir_all(dir).await.expect("cleanup");
}

/// AR-05: `Host::modify` (the pair-cache write path) must be durable before it
/// reports success, and a failure must leave both disk and live state alone.
#[tokio::test]
async fn host_modify_is_durable_and_reports_write_failure() {
    let dir = unique_dir("host-modify");
    let app = App::new(config_for(&dir)).await.expect("app");
    let admin = app
        .try_add_first_login("fixture-admin".to_owned(), "fixture-password".to_owned())
        .await
        .expect("bootstrap")
        .into_admin()
        .await
        .expect("admin proof");
    let host = app
        .inner
        .storage
        .add_host(fixture_host(None, "before-pair"))
        .await
        .expect("host");
    app.inner.storage.persist().await.expect("seed");
    let _before = app.export_backup(&admin).await.expect("before");

    // Success path: the cached name and mac land on disk before the call returns.
    let mut user = app
        .user_by_auth(UserAuth::UserPassword {
            username: "fixture-admin".to_owned(),
            password: "fixture-password".to_owned(),
        })
        .await
        .expect("real password authentication");
    user.host(host.id)
        .await
        .expect("host handle")
        .modify(
            &mut user,
            StorageHostModify {
                cache_name: Some("after-pair".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("durable modify");
    let disk = durable(&dir).await;
    assert_eq!(
        disk["hosts"][host.id.0.to_string()]["cache"]["name"],
        "after-pair",
        "a successful host modify must already be durable"
    );
    assert_eq!(
        disk,
        app.export_backup(&admin).await.expect("live after success")
    );

    // Failure path: a real staging failure must not publish anything.
    let before_failure = app.export_backup(&admin).await.expect("before failure");
    let primary_before = fs::read(dir.join("data.json")).await.expect("primary");
    let blocked = block_staging(&dir).await;
    let mut user = app
        .user_by_auth(UserAuth::UserPassword {
            username: "fixture-admin".to_owned(),
            password: "fixture-password".to_owned(),
        })
        .await
        .expect("real password authentication");
    let result = user
        .host(host.id)
        .await
        .expect("host handle")
        .modify(
            &mut user,
            StorageHostModify {
                cache_name: Some("must-not-land".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let failed = result.is_err();
    unblock_staging(blocked).await;

    eprintln!("MUTATION_HOST_MODIFY failed={failed}");
    assert!(
        failed,
        "a host modify that cannot commit must not report success"
    );
    assert_eq!(
        app.export_backup(&admin).await.expect("live after failure"),
        before_failure
    );
    assert_eq!(
        fs::read(dir.join("data.json"))
            .await
            .expect("primary after"),
        primary_before
    );
    assert_eq!(
        app.inner
            .storage
            .get_host(host.id)
            .await
            .expect("host")
            .cache
            .name,
        "after-pair",
        "the previous committed value must survive a failed write"
    );

    fs::remove_dir_all(dir).await.expect("cleanup");
}

/// AR-05: host add and remove are durable before they are acknowledged, and an
/// I/O failure publishes neither half of the change.
#[tokio::test]
async fn host_add_and_remove_are_durable_and_fail_closed() {
    let dir = unique_dir("host-add-remove");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("storage");
    let role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role")
        .id;
    storage
        .add_user(user_add("fixture-root", role, "fixture-password"))
        .await
        .expect("admin");

    // Add is durable on success.
    let host = storage
        .add_host(fixture_host(None, "durable-host"))
        .await
        .expect("add");
    assert_eq!(
        durable(&dir).await["hosts"][host.id.0.to_string()]["address"],
        "127.0.0.1",
        "a successful host add must already be durable"
    );

    // Add fails closed.
    let before = storage.export_backup().await.expect("before add failure");
    let blocked = block_staging(&dir).await;
    let failed_add = storage.add_host(fixture_host(None, "ghost-host")).await;
    unblock_staging(blocked).await;
    let add_failed = failed_add.is_err();
    eprintln!("MUTATION_HOST_ADD failed={add_failed}");
    assert!(
        add_failed,
        "a host add that cannot commit must not report success"
    );
    assert_eq!(
        storage.export_backup().await.expect("after add failure"),
        before
    );
    assert!(
        storage
            .export_backup()
            .await
            .expect("hosts")
            .as_object()
            .is_some(),
        "backup remains readable"
    );

    // Remove fails closed.
    let before = storage
        .export_backup()
        .await
        .expect("before remove failure");
    let primary_before = fs::read(dir.join("data.json")).await.expect("primary");
    let blocked = block_staging(&dir).await;
    let failed_remove = storage.remove_host(host.id).await;
    unblock_staging(blocked).await;
    let remove_failed = failed_remove.is_err();
    eprintln!("MUTATION_HOST_REMOVE failed={remove_failed}");
    assert!(
        remove_failed,
        "a host remove that cannot commit must not report success"
    );
    assert_eq!(
        storage.export_backup().await.expect("after remove failure"),
        before
    );
    assert_eq!(
        fs::read(dir.join("data.json"))
            .await
            .expect("primary after"),
        primary_before
    );
    assert_eq!(
        storage.get_host(host.id).await.expect("host").address,
        "127.0.0.1"
    );

    // Remove succeeds durably.
    storage.remove_host(host.id).await.expect("remove");
    let disk = durable(&dir).await;
    assert!(
        disk["hosts"].get(host.id.0.to_string()).is_none(),
        "a successful host remove must already be durable"
    );
    assert_eq!(
        disk,
        storage.export_backup().await.expect("live after remove")
    );

    // A reload sees exactly the committed state.
    let reloaded = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("reload");
    assert_eq!(reloaded.export_backup().await.expect("reloaded"), disk);

    fs::remove_dir_all(dir).await.expect("cleanup");
}

/// AR-05: dropping the caller after the writer accepted the request must not split
/// state. The owned writer finishes the commit, and live then equals disk.
#[tokio::test]
async fn cancelled_mutation_after_acceptance_still_commits_once() {
    let dir = unique_dir("cancel-add-user");
    let storage = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("storage");
    let role = storage
        .add_role(role_add("Admin", RoleType::Admin))
        .await
        .expect("role")
        .id;
    storage
        .add_user(user_add("fixture-root", role, "fixture-password"))
        .await
        .expect("admin");
    storage.persist().await.expect("seed");
    let before = storage.export_backup().await.expect("before");

    // Occupy the commit gate so the creation is accepted but cannot commit yet.
    let gate = storage.store_lock.lock().await;
    let mut creation =
        Box::pin(storage.add_user(user_add("fixture-cancelled", role, "fixture-password")));
    {
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(
            creation.as_mut().poll(&mut cx).is_pending(),
            "creation must wait for the gate"
        );
    }
    assert_eq!(
        storage.store_sender.capacity(),
        0,
        "creation must be accepted by the writer"
    );
    drop(creation);
    drop(gate);

    // The acknowledged persist queues behind the dropped caller's transaction.
    tokio::time::timeout(Duration::from_secs(5), storage.persist())
        .await
        .expect("writer must finish a dropped-caller mutation")
        .expect("persist");

    let live = storage.export_backup().await.expect("live");
    let disk = durable(&dir).await;
    let names: Vec<_> = live["users"]
        .as_object()
        .expect("users")
        .values()
        .map(|user| user["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    eprintln!(
        "MUTATION_CANCEL names={names:?} live_equals_disk={}",
        live == disk
    );

    assert_eq!(
        live, disk,
        "a cancelled caller must not leave live and disk split"
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| *name == "fixture-cancelled")
            .count(),
        1,
        "the accepted mutation must commit exactly once"
    );
    assert_ne!(live, before);

    // Every user still references a live role, and a reload agrees.
    for user in live["users"].as_object().expect("users").values() {
        let role_id = user["role_id"].as_u64().expect("role_id");
        assert!(live["roles"].get(role_id.to_string()).is_some());
    }
    let reloaded = JsonStorage::load(dir.join("data.json"), Duration::from_secs(3600))
        .await
        .expect("reload");
    assert_eq!(reloaded.export_backup().await.expect("reloaded"), live);

    fs::remove_dir_all(dir).await.expect("cleanup");
}

/// AR-05: `Host::delete` invalidates the per-host app-image cache only after the
/// removal committed, so a failed removal cannot leave a cache pointing at a host
/// the store still has (or strip a cache for a host that still exists).
#[tokio::test]
async fn host_delete_invalidates_image_cache_only_after_commit() {
    let dir = unique_dir("host-cache");
    let app = App::new(config_for(&dir)).await.expect("app");
    let admin = app
        .try_add_first_login("fixture-admin".to_owned(), "fixture-password".to_owned())
        .await
        .expect("bootstrap")
        .into_admin()
        .await
        .expect("admin proof");
    let host = app
        .inner
        .storage
        .add_host(fixture_host(None, "cached-host"))
        .await
        .expect("host");

    let cache_key = (admin.id(), host.id, crate::app::host::AppId(7));
    app.inner
        .app_image_cache
        .write()
        .await
        .insert(cache_key, actix_web::web::Bytes::from_static(b"image"));

    // Failed removal: the cache must survive, because the host still exists.
    let blocked = block_staging(&dir).await;
    let mut user = app
        .user_by_auth(UserAuth::UserPassword {
            username: "fixture-admin".to_owned(),
            password: "fixture-password".to_owned(),
        })
        .await
        .expect("real password authentication");
    let failed = user.host_delete(host.id).await.is_err();
    unblock_staging(blocked).await;
    let cached_after_failure = app
        .inner
        .app_image_cache
        .read()
        .await
        .contains_key(&cache_key);

    // Successful removal: the cache entry is dropped with the host.
    let mut user = app
        .user_by_auth(UserAuth::UserPassword {
            username: "fixture-admin".to_owned(),
            password: "fixture-password".to_owned(),
        })
        .await
        .expect("real password authentication");
    user.host_delete(host.id).await.expect("durable delete");
    let cached_after_success = app
        .inner
        .app_image_cache
        .read()
        .await
        .contains_key(&cache_key);

    eprintln!(
        "MUTATION_HOST_CACHE failed={failed} cached_after_failure={cached_after_failure} \
         cached_after_success={cached_after_success}"
    );
    assert!(failed, "the blocked removal must report failure");
    assert!(
        cached_after_failure,
        "a failed removal must not invalidate the cache"
    );
    assert!(
        app.inner.storage.get_host(host.id).await.is_err(),
        "the retried removal committed"
    );
    assert!(
        !cached_after_success,
        "a committed removal must invalidate the cache"
    );

    fs::remove_dir_all(dir).await.expect("cleanup");
}

/// Helper so the assertions above read clearly without depending on `Either`.
trait LenChecked {
    fn len_checked(&self) -> usize;
}

impl LenChecked for Either<Vec<UserId>, Vec<StorageUser>> {
    fn len_checked(&self) -> usize {
        match self {
            Either::Left(ids) => ids.len(),
            Either::Right(users) => users.len(),
        }
    }
}
