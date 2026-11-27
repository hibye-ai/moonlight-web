use super::*;
use actix_web::{App as HttpApp, test};
use common::config::{Config, StorageConfig};

async fn check_live_policy_rejection(peer: &str, xfp: Option<&str>) {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test fixture operation must succeed")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("astra-deploy-policy-{n}"));
    std::fs::create_dir_all(&dir).expect("test fixture operation must succeed");
    let mut cfg = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: std::time::Duration::from_secs(3600),
        },
        ..Default::default()
    };
    // Sanitized policy facts independently read from the production config; no credentials.
    cfg.web_server.session_cookie_secure = true;
    cfg.web_server.certificate = None;
    cfg.web_server.forwarded_header = None;
    cfg.web_server.allow_insecure_http_login = false;
    let data = Data::new(
        App::new(cfg)
            .await
            .expect("test fixture operation must succeed"),
    );
    let service = test::init_service(HttpApp::new().app_data(data).service(login)).await;
    let mut request = test::TestRequest::post()
        .uri("/login")
        .peer_addr(peer.parse().expect("test fixture operation must succeed"))
        .set_json(serde_json::json!({"name":"fixture-admin","password":"fixture-password"}));
    if let Some(xfp) = xfp {
        request = request.insert_header(("X-Forwarded-Proto", xfp));
    }
    let response = test::call_service(&service, request.to_request()).await;
    eprintln!(
        "ASTRA_DEPLOY_POLICY peer={peer} xfp={xfp:?} status={}",
        response.status().as_u16()
    );
    assert_eq!(
        response.status().as_u16(),
        426,
        "unchanged live flags must refuse this cleartext/nontrusted ingress"
    );
    assert!(!response.headers().contains_key("set-cookie"));
    std::fs::remove_dir_all(dir).expect("test fixture operation must succeed");
}
#[actix_web::test]
async fn astra_current_live_flags_refuse_direct_lan_login() {
    check_live_policy_rejection("10.23.45.200:40000", None).await;
}
#[actix_web::test]
async fn astra_current_live_flags_refuse_nonloopback_proxy_assertion() {
    check_live_policy_rejection("10.23.45.150:40000", Some("https")).await;
}
