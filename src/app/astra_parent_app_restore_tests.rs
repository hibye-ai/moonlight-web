use super::*;
use common::config::StorageConfig;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn astra_parent_app_restore_cancellation_must_publish_epoch_before_releasing_gate() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test fixture operation must succeed")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("astra-app-restore-{stamp}"));
    tokio::fs::create_dir_all(&dir)
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
    let app = Arc::new(
        App::new(config)
            .await
            .expect("test fixture operation must succeed"),
    );
    let admin = app
        .try_add_first_login("fixture-admin".into(), "fixture-password".into())
        .await
        .expect("test fixture operation must succeed");
    let id = admin.id();
    let token = admin
        .new_session(Duration::from_secs(3600))
        .await
        .expect("test fixture operation must succeed");
    app.inner
        .storage
        .persist()
        .await
        .expect("test fixture operation must succeed");
    let old_epoch = app.identity_epoch();
    let old_storage_epoch = app.inner.storage.identity_epoch();
    let mut backup = app
        .inner
        .storage
        .export_backup()
        .await
        .expect("test fixture operation must succeed");
    backup["users"][id.0.to_string()]["name"] = Value::String("fixture-restored".into());
    // A real App cache publication boundary, not a copied state machine.
    let cache_guard = app.inner.app_image_cache.write().await;
    let request_app = app.clone();
    let restore =
        tokio::spawn(async move { request_app.import_backup_unchecked_for_test(backup).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if app.inner.storage.identity_epoch() > old_storage_epoch {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("storage restore did not commit");
    assert_eq!(
        app.inner
            .storage
            .get_user(id)
            .await
            .expect("test fixture operation must succeed")
            .name,
        "fixture-restored"
    );
    restore.abort();
    let _ = restore.await;
    let premature_gate =
        tokio::time::timeout(Duration::from_millis(30), app.enter_stream_scope()).await;
    let released_before_publication = premature_gate.is_ok();
    drop(premature_gate);
    drop(cache_guard);
    let epoch_published = tokio::time::timeout(Duration::from_secs(2), async {
        while app.identity_epoch() == old_epoch {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let stale_session_rejected = app.user_by_auth(UserAuth::Session(token)).await.is_err();
    app.inner
        .storage
        .persist()
        .await
        .expect("test fixture operation must succeed");
    eprintln!(
        "ASTRA_APP_RESTORE gate_released_before_publication={released_before_publication} epoch_published={epoch_published} stale_session_rejected={stale_session_rejected}"
    );
    tokio::fs::remove_dir_all(&dir)
        .await
        .expect("test fixture operation must succeed");
    assert!(
        !released_before_publication,
        "maintenance gate reopened before App cache/epoch publication"
    );
    assert!(
        epoch_published,
        "cancelling the request lost the App identity epoch publication"
    );
    assert!(stale_session_rejected);
}
