//! AR-07 regressions: transport trust is its own explicit, default-deny allow-list.
//!
//! The production readback for this deployment is: `session_cookie_secure = true`, no
//! TLS certificate on the listener, no `forwarded_header`, and the new HTTP exception
//! unset. Behind the real TLS-terminating proxy that combination refuses login (426),
//! because the proxy's `X-Forwarded-Proto: https` is discarded — the transport trust
//! list is empty, and it used to be reachable only by enabling header-based *user*
//! authentication, which is a much bigger permission than trusting the scheme.
//!
//! These tests pin the migration contract:
//!
//! - the flag-only trust list is default-deny for every non-loopback peer;
//! - a single exact address opens exactly that peer and nothing else;
//! - a wildcard-looking entry (`0.0.0.0`) grants nothing;
//! - enabling transport trust does not enable header user authentication.
//!
//! The independent audit fixtures in `astra_deployment_tests.rs` are untouched: they
//! assert the *unchanged* live flags still refuse, which is what a deployment must see
//! before it is migrated.
// `test` is aliased: importing it as `test` would shadow the `#[test]` attribute.
use actix_web::{App as HttpApp, http::StatusCode, test as actix_test};
use common::config::{Config, ForwardedHeaders, StorageConfig};

use super::*;

/// The proxy peer named in the sanitized deployment context supplied for this worker.
/// Only ever used as a test fixture value; nothing here talks to it.
const PROXY_PEER: &str = "10.23.45.150";
/// A peer inside the same LAN that is *not* the proxy.
const OTHER_LAN_PEER: &str = "10.23.45.200";

async fn app_with(configure: impl FnOnce(&mut Config)) -> Data<App> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-ar07-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create dir");

    let mut config = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: std::time::Duration::from_secs(3600),
        },
        ..Default::default()
    };
    configure(&mut config);

    let app = Data::new(App::new(config).await.expect("build app"));

    // The app must create the fixture account on first login; give it one.
    let mut user = app
        .try_add_first_login("fixture-admin".into(), "fixture-password".into())
        .await
        .expect("fixture account");
    let _ = user.role().await.expect("role");

    app
}

/// Posts a login through the real handler and returns status plus `Set-Cookie`.
async fn post_login(
    app: Data<App>,
    peer: &str,
    forwarded_proto: Option<&str>,
    headers: &[(&str, &str)],
) -> (StatusCode, Option<String>) {
    let service = actix_test::init_service(HttpApp::new().app_data(app).service(login)).await;

    let mut request = actix_test::TestRequest::post()
        .uri("/login")
        .peer_addr(peer.parse().expect("peer addr"))
        .set_json(serde_json::json!({
            "name": "fixture-admin",
            "password": "fixture-password",
        }));

    if let Some(proto) = forwarded_proto {
        request = request.insert_header(("X-Forwarded-Proto", proto));
    }
    for (name, value) in headers {
        request = request.insert_header((*name, *value));
    }

    let response = actix_test::call_service(&service, request.to_request()).await;
    let status = response.status();
    let cookie = response
        .headers()
        .get(actix_web::http::header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    (status, cookie)
}

fn cookie_has_secure(cookie: &str) -> bool {
    cookie
        .split(';')
        .any(|part| part.trim().eq_ignore_ascii_case("secure"))
}

/// The deployment flags that are actually live today, with no transport trust entry.
fn live_flags(config: &mut Config) {
    config.web_server.session_cookie_secure = true;
    config.web_server.certificate = None;
    config.web_server.forwarded_header = None;
    config.web_server.allow_insecure_http_login = false;
}

// ---------------------------------------------------------------------------
// Default deny
// ---------------------------------------------------------------------------

/// With no transport trust configured, a non-loopback peer cannot talk the server into
/// "https" — including the real proxy address, which is only trusted once named.
#[actix_web::test]
async fn transport_trust_is_default_deny_for_every_non_loopback_peer() {
    let app = app_with(live_flags).await;

    for peer in [PROXY_PEER, OTHER_LAN_PEER, "203.0.113.5"] {
        let (status, cookie) =
            post_login(app.clone(), &format!("{peer}:41000"), Some("https"), &[]).await;

        assert_eq!(
            status,
            StatusCode::UPGRADE_REQUIRED,
            "peer {peer} must be refused while the transport trust list is empty"
        );
        assert!(
            cookie.is_none(),
            "peer {peer} must not receive a session cookie, got {cookie:?}"
        );
    }
}

/// A wildcard-shaped entry must not widen the list. `0.0.0.0` can never be a peer
/// address, so it matches nothing rather than everything.
#[actix_web::test]
async fn unspecified_address_in_the_trust_list_grants_nothing() {
    let app = app_with(|config| {
        live_flags(config);
        config.web_server.trusted_forwarded_proto_ips =
            vec!["0.0.0.0".parse().expect("ip"), "::".parse().expect("ip")];
    })
    .await;

    let (status, cookie) =
        post_login(app, &format!("{OTHER_LAN_PEER}:41000"), Some("https"), &[]).await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "a non-peer wildcard entry must not grant transport trust"
    );
    assert!(cookie.is_none(), "cookie: {cookie:?}");
}

/// The `Forwarded: proto=https` spelling is held to the same trust list as
/// `X-Forwarded-Proto`: an untrusted peer gets nothing from either header.
#[actix_web::test]
async fn forwarded_header_is_held_to_the_same_default_deny() {
    let app = app_with(live_flags).await;

    let (status, cookie) = post_login(
        app,
        &format!("{PROXY_PEER}:41000"),
        None,
        &[("Forwarded", "for=192.0.2.1;proto=https")],
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "an untrusted peer must not assert the scheme through Forwarded either"
    );
    assert!(cookie.is_none(), "cookie: {cookie:?}");
}

/// Loopback stays trusted without any configuration, as before: a local reverse proxy
/// needs no allow-list entry. This is the pre-existing behaviour and must not regress.
#[actix_web::test]
async fn loopback_proxy_needs_no_allow_list_entry() {
    let app = app_with(live_flags).await;

    let (status, cookie) = post_login(app, "127.0.0.1:41000", Some("https"), &[]).await;

    assert!(
        status.is_success(),
        "a loopback proxy must be trusted without configuration, got {status}"
    );
    let cookie = cookie.expect("loopback proxy login must issue a session");
    assert!(
        cookie_has_secure(&cookie),
        "a trusted-proxy login must still mark the cookie Secure: {cookie:?}"
    );
}

// ---------------------------------------------------------------------------
// Exact peer positive
// ---------------------------------------------------------------------------

/// Naming the exact proxy address opens exactly that peer: it can assert https and gets
/// a `Secure` cookie, while every other non-loopback peer stays refused.
#[actix_web::test]
async fn exact_trusted_peer_may_assert_https_and_gets_a_secure_cookie() {
    let app = app_with(|config| {
        live_flags(config);
        config.web_server.trusted_forwarded_proto_ips = vec![PROXY_PEER.parse().expect("ip")];
    })
    .await;

    let (status, cookie) = post_login(
        app.clone(),
        &format!("{PROXY_PEER}:41000"),
        Some("https"),
        &[],
    )
    .await;

    assert!(
        status.is_success(),
        "the exactly-trusted proxy must be able to complete a login, got {status}"
    );
    let cookie = cookie.expect("trusted-proxy login must issue a session");
    assert!(
        cookie_has_secure(&cookie),
        "the migrated path must still set Secure: {cookie:?}"
    );

    // Same configuration, one address over: still refused.
    let (status, cookie) =
        post_login(app, &format!("{OTHER_LAN_PEER}:41000"), Some("https"), &[]).await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "trusting one peer must not trust the rest of the subnet"
    );
    assert!(cookie.is_none(), "cookie: {cookie:?}");
}

/// Transport trust must not mint a header identity. With the transport list populated
/// and `forwarded_header` left unset, a request that carries `X-Forwarded-User` is not
/// authenticated by that header.
#[actix_web::test]
async fn transport_trust_does_not_enable_header_user_authentication() {
    let app = app_with(|config| {
        live_flags(config);
        config.web_server.trusted_forwarded_proto_ips = vec![PROXY_PEER.parse().expect("ip")];
        assert!(
            config.web_server.forwarded_header.is_none(),
            "the migration must keep header user auth disabled"
        );
    })
    .await;

    // The header-only request carries no session cookie and no credentials.
    let service = actix_test::init_service(
        HttpApp::new()
            .app_data(app.clone())
            .service(authenticate)
            .service(login),
    )
    .await;

    let header_only = actix_test::TestRequest::get()
        .uri("/authenticate")
        .peer_addr(format!("{PROXY_PEER}:41000").parse().expect("peer"))
        .insert_header(("X-Forwarded-User", "fixture-admin"))
        .to_request();
    let response = actix_test::call_service(&service, header_only).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a trusted transport peer must not authenticate a user from a header"
    );

    // The trusted peer can still log in with real credentials over the trusted TLS path.
    let (status, cookie) =
        post_login(app, &format!("{PROXY_PEER}:41000"), Some("https"), &[]).await;
    assert!(
        status.is_success(),
        "credential login through the trusted proxy must work, got {status}"
    );
    assert!(
        cookie.is_some_and(|cookie| cookie_has_secure(&cookie)),
        "and must still carry Secure"
    );
}

/// Enabling header user authentication *for its own sake* keeps its own exact-peer rule;
/// this ensures the two lists are separate knobs and not aliases of each other.
#[actix_web::test]
async fn header_user_authentication_stays_independent_of_transport_trust() {
    let app = app_with(|config| {
        live_flags(config);
        config.web_server.forwarded_header = Some(ForwardedHeaders {
            username_header: "X-Forwarded-User".to_string(),
            auto_create_missing_user: false,
            trusted_proxy_ips: Vec::new(),
        });
        assert!(
            config.web_server.trusted_forwarded_proto_ips.is_empty(),
            "the fixture must leave transport trust empty"
        );
    })
    .await;

    // Header auth enabled but transport trust empty: the scheme assertion is still
    // refused, so the login cannot be completed over this cleartext leg.
    let (status, _cookie) = post_login(
        app,
        &format!("{PROXY_PEER}:41000"),
        Some("https"),
        &[("X-Forwarded-User", "fixture-admin")],
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "enabling header auth must not implicitly trust the transport"
    );
}

// ---------------------------------------------------------------------------
// Unit-level policy table
// ---------------------------------------------------------------------------

/// The classification itself, without HTTP: exact matches only, default deny.
#[test]
fn transport_peer_matching_is_exact() {
    let mut config = common::config::WebServerConfig::default();
    assert!(
        config.trusted_forwarded_proto_ips.is_empty(),
        "the default must be an empty (deny-all) allow-list"
    );

    config.trusted_forwarded_proto_ips = vec![PROXY_PEER.parse().expect("ip")];

    assert!(
        transport::is_trusted_transport_peer(PROXY_PEER.parse().expect("ip"), &config),
        "the configured address must match itself"
    );
    assert!(
        !transport::is_trusted_transport_peer(OTHER_LAN_PEER.parse().expect("ip"), &config),
        "a neighbouring address must not match"
    );
    assert!(
        !transport::is_trusted_transport_peer("10.23.45.15".parse().expect("ip"), &config),
        "a prefix of the configured address must not match"
    );

    // Wildcards and unspecified addresses never grant trust, even if configured.
    config.trusted_forwarded_proto_ips =
        vec!["0.0.0.0".parse().expect("ip"), "::".parse().expect("ip")];
    assert!(
        !transport::is_trusted_transport_peer("203.0.113.9".parse().expect("ip"), &config),
        "an unspecified entry must not act as a wildcard"
    );
    assert!(
        !transport::is_trusted_transport_peer("0.0.0.0".parse().expect("ip"), &config),
        "the unspecified address is never a real peer, even if listed"
    );
}
