use super::*;
use std::{
    future::Future,
    task::{Context, Poll},
};

// Adopted from the independent review's real-App regression. Observe an actual
// primary rename, then drop the caller at a Pending boundary without failpoints.
#[tokio::test]
async fn independent_cancel_after_primary_commit_keeps_disk_and_live_identity_consistent() {
    let f = cascade_fixture("independent-cancel").await;
    let app = App::new(Config {
        data_storage: StorageConfig::Json {
            path: f.storage.file.to_string_lossy().into_owned(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    })
    .await
    .expect("real App");
    let user = app
        .user_by_auth(UserAuth::UserPassword {
            username: "fixture-alice".to_owned(),
            password: "fixture-password".to_owned(),
        })
        .await
        .expect("fixture password authentication");
    let token = user
        .new_session(Duration::from_secs(3600))
        .await
        .expect("session");
    let admin = user.into_admin().await.expect("real admin proof");
    let survivor = app
        .user_by_auth(UserAuth::UserPassword {
            username: "fixture-root".to_owned(),
            password: "fixture-password".to_owned(),
        })
        .await
        .expect("surviving admin authentication");
    let survivor_token = survivor
        .new_session(Duration::from_secs(3600))
        .await
        .expect("survivor session");
    let before = app.inner.storage.export_backup().await.expect("before");
    let primary_before = std::fs::read(&f.storage.file).expect("primary before");
    let role = app.role_by_id(f.removed_role).await.expect("role");
    let mut deletion = Box::pin(role.delete(&admin));
    let mut observed_commit = false;
    for _ in 0..5000 {
        if std::fs::read(&f.storage.file).expect("primary during deletion") != primary_before {
            observed_commit = true;
            break;
        }
        let poll = {
            let mut cx = Context::from_waker(futures::task::noop_waker_ref());
            deletion.as_mut().poll(&mut cx)
        };
        if let Poll::Ready(result) = poll {
            panic!(
                "deletion completed before cancellation boundary could be exercised: {result:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        observed_commit,
        "must exercise a real on-disk primary commit, not a synthetic failpoint"
    );
    drop(deletion);
    let durable: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&f.storage.file).expect("committed primary"))
            .expect("JSON");
    let live = tokio::time::timeout(Duration::from_secs(5), app.inner.storage.export_backup())
        .await
        .expect("owned transition must finish")
        .expect("live after cancellation");
    let session_valid = app.user_by_auth(UserAuth::Session(token)).await.is_ok();
    eprintln!(
        "CANCELLATION_PROOF committed_primary_users={} live_users={} deleted_admin_session_valid={} live_equals_before={}",
        durable["users"].as_object().expect("users").len(),
        live["users"].as_object().expect("users").len(),
        session_valid,
        live == before
    );
    // A later real persist must not resurrect the deleted accounts on disk.
    app.inner
        .storage
        .persist()
        .await
        .expect("real subsequent acknowledged persist");
    let later: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&f.storage.file).expect("later primary"))
            .expect("JSON");
    eprintln!(
        "CANCELLATION_PROOF subsequent_persist_users={}",
        later["users"].as_object().expect("users").len()
    );
    assert_eq!(
        live, durable,
        "cancelled App role deletion left primary and live identities inconsistent after actual primary rename"
    );
    assert_eq!(
        later, durable,
        "subsequent acknowledged persist resurrected committed deleted identities"
    );
    assert!(
        !session_valid,
        "committed role deletion must revoke the removed admin session"
    );
    assert!(
        app.user_by_auth(UserAuth::Session(survivor_token))
            .await
            .is_ok(),
        "cancellation must not revoke the surviving admin session"
    );
    assert_eq!(live["users"].as_object().expect("users").len(), 1);
    assert!(matches!(
        app.inner.storage.get_role(f.removed_role).await,
        Err(AppError::RoleNotFound)
    ));
    for host in live["hosts"].as_object().expect("hosts").values() {
        if host["cache"]["name"] == "fixture-root" {
            assert_eq!(host["owner"], f.users[2].0);
        } else {
            assert!(
                host["owner"].is_null(),
                "committed cascade must clear only removed owners"
            );
        }
    }
    let reloaded = JsonStorage::load(f.storage.file.clone(), Duration::from_secs(3600))
        .await
        .expect("reload");
    assert_eq!(
        reloaded.export_backup().await.expect("reload snapshot"),
        durable
    );
    fs::remove_dir_all(f.dir).await.expect("fixture cleanup");
}

#[tokio::test]
async fn role_cascade_cancel_after_acceptance_before_io_still_commits() {
    let f = cascade_fixture("cancel-accepted").await;
    let gate = f.storage.store_lock.lock().await;
    let mut deletion = Box::pin(f.storage.remove_role(f.removed_role));
    {
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(deletion.as_mut().poll(&mut cx).is_pending());
    }
    // This current-thread runtime has not yielded to the writer. The request
    // occupies the channel, but the real store lock prevents any disk work.
    assert_eq!(
        f.storage.store_sender.capacity(),
        0,
        "deletion must have been accepted"
    );
    drop(deletion);
    drop(gate);
    // This acknowledged persist queues behind the owned deletion.
    tokio::time::timeout(Duration::from_secs(5), f.storage.persist())
        .await
        .expect("writer must finish dropped-caller operation")
        .expect("persist");
    let live = f.storage.export_backup().await.expect("live");
    let durable: serde_json::Value =
        serde_json::from_slice(&fs::read(&f.storage.file).await.expect("disk")).expect("JSON");
    assert_eq!(live, durable);
    assert_eq!(live["users"].as_object().expect("users").len(), 1);
    assert!(matches!(
        f.storage.get_role(f.removed_role).await,
        Err(AppError::RoleNotFound)
    ));
    for token in &f.tokens[..2] {
        assert!(matches!(
            f.storage.get_user_by_session_token(*token).await,
            Err(AppError::SessionTokenNotFound)
        ));
    }
    assert!(
        f.storage
            .get_user_by_session_token(f.tokens[2])
            .await
            .is_ok()
    );
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}

#[tokio::test]
async fn role_cascade_cancel_before_acceptance_has_no_side_effects() {
    let f = cascade_fixture("cancel-not-accepted").await;
    let before = f.storage.export_backup().await.expect("before");
    let gate = f.storage.store_lock.lock().await;
    // Fill the one-slot writer queue without yielding, so deletion cannot hand
    // off ownership. Cancelling the pending send must leave no hidden request.
    f.storage.force_write();
    assert_eq!(f.storage.store_sender.capacity(), 0);
    let mut deletion = Box::pin(f.storage.remove_role(f.removed_role));
    {
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(deletion.as_mut().poll(&mut cx).is_pending());
    }
    drop(deletion);
    drop(gate);
    tokio::time::timeout(Duration::from_secs(5), f.storage.persist())
        .await
        .expect("writer remains usable")
        .expect("persist");
    assert_eq!(f.storage.export_backup().await.expect("after"), before);
    let durable: serde_json::Value =
        serde_json::from_slice(&fs::read(&f.storage.file).await.expect("disk")).expect("JSON");
    assert_eq!(durable, before);
    for token in &f.tokens {
        assert!(
            f.storage.get_user_by_session_token(*token).await.is_ok(),
            "unaccepted cancellation must preserve sessions"
        );
    }
    fs::remove_dir_all(f.dir).await.expect("cleanup");
}
