use super::*;
use crate::app::{App, auth::UserAuth};
use common::config::{Config, StorageConfig};

async fn fixture(label: &str) -> (App, PathBuf, crate::app::user::Admin) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test fixture operation must succeed")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "astra-parent-{label}-{}-{stamp}",
        std::process::id()
    ));
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
    let admin = app
        .try_add_first_login("parent-admin".into(), "fixture-old-password".into())
        .await
        .expect("test fixture operation must succeed")
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
fn credentials(password: &str) -> UserAuth {
    UserAuth::UserPassword {
        username: "parent-admin".into(),
        password: password.into(),
    }
}

#[tokio::test]
async fn astra_parent_restore_must_not_reuse_an_authenticated_generation() {
    let (app, dir, admin) = fixture("restore-aba").await;
    let backup = app
        .export_backup(&admin)
        .await
        .expect("test fixture operation must succeed");
    app.user_by_id(admin.id())
        .await
        .expect("test fixture operation must succeed")
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
    let old_authenticated = app
        .user_by_auth(credentials("fixture-new-password"))
        .await
        .expect("test fixture operation must succeed");
    app.import_backup(&admin, backup)
        .await
        .expect("test fixture operation must succeed");
    let restored_old_works = app
        .user_by_auth(credentials("fixture-old-password"))
        .await
        .is_ok();
    let fresh_new_rejected = app
        .user_by_auth(credentials("fixture-new-password"))
        .await
        .is_err();
    let accepted = match old_authenticated
        .new_session(Duration::from_secs(3600))
        .await
    {
        Ok(token) => app.user_by_auth(UserAuth::Session(token)).await.is_ok(),
        Err(_) => false,
    };
    eprintln!(
        "ASTRA_PARENT_RESTORE_ABA old_password_restored={restored_old_works} fresh_new_rejected={fresh_new_rejected} stale_authenticated_session_valid={accepted}"
    );
    fs::remove_dir_all(dir)
        .await
        .expect("test fixture operation must succeed");
    assert!(restored_old_works && fresh_new_rejected);
    assert!(
        !accepted,
        "restore reused the authenticated generation and minted a valid stale session"
    );
}

#[tokio::test]
async fn astra_parent_restore_must_reject_stale_legacy_rehash_generation() {
    let (app, dir, admin) = fixture("restore-rehash-aba").await;
    let backup = app
        .export_backup(&admin)
        .await
        .expect("test fixture operation must succeed");
    let salt = [7u8; 16];
    let mut hash = [0u8; 32];
    openssl::pkcs5::pbkdf2_hmac(
        b"fixture-new-password",
        &salt,
        1000,
        openssl::hash::MessageDigest::sha256(),
        &mut hash,
    )
    .expect("test fixture operation must succeed");
    app.user_by_id(admin.id())
        .await
        .expect("test fixture operation must succeed")
        .modify(
            &admin,
            StorageUserModify {
                password: Some(Some(StoragePassword {
                    salt,
                    hash,
                    iterations: 1000,
                })),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("test fixture operation must succeed");
    let stale_snapshot = app
        .user_by_name("parent-admin")
        .await
        .expect("test fixture operation must succeed");
    app.import_backup(&admin, backup)
        .await
        .expect("test fixture operation must succeed");
    let stale_auth_rejected = stale_snapshot
        .authenticate(&credentials("fixture-new-password"))
        .await
        .is_err();
    let restored_password_preserved = app
        .user_by_auth(credentials("fixture-old-password"))
        .await
        .is_ok();
    eprintln!(
        "ASTRA_PARENT_REHASH_ABA stale_auth_rejected={stale_auth_rejected} restored_password_preserved={restored_password_preserved}"
    );
    fs::remove_dir_all(dir)
        .await
        .expect("test fixture operation must succeed");
    assert!(
        stale_auth_rejected,
        "stale pre-restore password verification passed generation CAS"
    );
    assert!(
        restored_password_preserved,
        "stale rehash overwrote the restored password"
    );
}
