//! MW-19 integration tests: the stream-vs-restore maintenance gate on the real
//! `App`, exercising the production `App::enter_stream_scope` / `App::import_backup`
//! code paths (not a model of them).
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::config::{Config, StorageConfig};

use crate::app::App;

async fn test_app(label: &str) -> (Arc<App>, std::path::PathBuf) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-appgate-{label}-{}-{unique}",
        std::process::id()
    ));
    tokio::fs::create_dir_all(&dir).await.expect("create dir");

    let config = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().to_string(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };

    let app = App::new(config).await.expect("build app");
    (Arc::new(app), dir)
}

/// MW-19: while a stream scope is held, a restore must not complete. The restore
/// waits for the stream to finish instead of swapping identities underneath it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_waits_for_active_stream_scope() {
    let (app, dir) = test_app("waits").await;

    let stream_scope = app.enter_stream_scope().await.expect("stream scope");
    let epoch_before = app.identity_epoch();

    let restore_app = app.clone();
    let restore = tokio::spawn(async move {
        {
            // The admin proof is not needed for the gate itself; the call is the
            // point under test.
            restore_app
                .import_backup_unchecked_for_test(sample_backup())
                .await
        }
    });

    // Give the restore a chance to (incorrectly) finish while the stream is open.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        app.identity_epoch(),
        epoch_before,
        "a restore must not replace identities while a stream scope is held"
    );
    assert!(
        !restore.is_finished(),
        "the restore must still be blocked by the active stream"
    );

    drop(stream_scope);

    restore
        .await
        .expect("restore task")
        .expect("restore must complete once the stream ends");
    assert_eq!(
        app.identity_epoch(),
        epoch_before + 1,
        "the epoch must advance exactly once, after the stream released the gate"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

/// MW-19: a stream that starts while a restore is running must not observe the old
/// identity snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_scope_blocks_until_restore_finishes() {
    let (app, dir) = test_app("blocks").await;

    let restore_app = app.clone();
    let restore = tokio::spawn(async move {
        restore_app
            .import_backup_unchecked_for_test(sample_backup())
            .await
    });

    // Race a stream scope acquire against the in-flight restore.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let scope = app.enter_stream_scope().await.expect("stream scope");
    let epoch_after_scope = app.identity_epoch();

    restore.await.expect("restore task").expect("restore");

    drop(scope);
    assert_eq!(
        app.identity_epoch(),
        epoch_after_scope,
        "the epoch must be published before the stream could start"
    );
    assert!(
        epoch_after_scope >= 1,
        "the stream must have started only after the restore published its epoch"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

fn sample_backup() -> serde_json::Value {
    serde_json::json!({
        "version": "3",
        "users": {
            "7": {
                "role_id": 3,
                "name": "restored",
                "password": {
                    "salt": "00112233445566778899aabbccddeeff",
                    "hash": "0000000000000000000000000000000000000000000000000000000000000000",
                    "iterations": 600000
                },
                "client_unique_id": "restored"
            }
        },
        "hosts": {},
        "roles": {
            "3": {
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
