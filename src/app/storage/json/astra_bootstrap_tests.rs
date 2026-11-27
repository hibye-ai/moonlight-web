use super::*;
#[tokio::test]
async fn astra_bootstrap_success_must_have_a_durable_account() {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test fixture operation must succeed")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("astra-bootstrap-{n}"));
    fs::create_dir_all(dir.join("data.json.tmp"))
        .await
        .expect("test fixture operation must succeed");
    let mut cfg = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };
    cfg.web_server.first_login_create_admin = true;
    let app = App::new(cfg)
        .await
        .expect("test fixture operation must succeed");
    let first = app
        .try_add_first_login("fixture-admin".into(), "fixture-password".into())
        .await;
    let accepted = first.is_ok();
    let valid = if let Ok(user) = first {
        let token = user
            .new_session(Duration::from_secs(3600))
            .await
            .expect("test fixture operation must succeed");
        app.user_by_auth(UserAuth::Session(token)).await.is_ok()
    } else {
        false
    };
    let persist_failed = app.inner.storage.persist().await.is_err();
    let durable = dir.join("data.json").is_file();
    eprintln!(
        "ASTRA_BOOTSTRAP login_ok={accepted} session_valid={valid} persist_failed={persist_failed} durable_account_file={durable}"
    );
    fs::remove_dir_all(dir)
        .await
        .expect("test fixture operation must succeed");
    assert!(
        !accepted,
        "bootstrap login acknowledged success although storage cannot commit the first account"
    );
}
