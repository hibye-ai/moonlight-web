//! MW-22 regressions: `session_cookie_secure` must be a real HTTPS-only policy, not a
//! scheme-dependent suggestion.
//!
//! Two defects are covered here:
//!
//! 1. the transport policy used the *request-target* as TLS evidence. A client can send
//!    `POST https://host/api/login HTTP/1.1` over a cleartext socket at will, so any
//!    absolute-form `https` URI was accepted as proof of TLS while a genuine origin-form
//!    request from a TLS listener ("POST /api/login") carried no scheme at all;
//! 2. the policy ran after the first-login account creation, so a refused cleartext
//!    login could still create the first admin and burn the account name.
//!
//! The first case is now decided from the listener's own state (`AppConfig::secure()`),
//! which only `bind_openssl`/`listen_openssl` set. The tests therefore use a real
//! loopback TLS listener for the accepted case and a real cleartext listener for the
//! spoofed case, instead of asserting on `TestRequest` URIs alone.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use actix_web::{App as ActixApp, dev::ServerHandle, http::StatusCode, test, web::Data};
use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    hash::MessageDigest,
    pkey::PKey,
    rsa::Rsa,
    ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode},
    x509::{X509, X509NameBuilder},
};

use super::*;
use crate::app::App;

const LOGIN_BODY: &str = r#"{"name":"alice","password":"password-1"}"#;

/// A private temp directory per test, with the storage file path inside it.
fn temp_data_dir(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-mw22-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    let file = dir.join("data.json");
    (dir, file)
}

fn config_for(
    file: &std::path::Path,
    configure: impl FnOnce(&mut common::config::Config),
) -> common::config::Config {
    let mut config = common::config::Config {
        data_storage: common::config::StorageConfig::Json {
            path: file.to_string_lossy().to_string(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };
    configure(&mut config);
    config
}

async fn app_with(configure: impl FnOnce(&mut common::config::Config)) -> Data<App> {
    let (_dir, file) = temp_data_dir("app");
    let config = config_for(&file, configure);
    let app = App::new(config).await.expect("build app");
    Data::new(app)
}

/// Posts a login through the real handler and returns the status plus the raw
/// `Set-Cookie` header, if any.
async fn login_request(
    app: Data<App>,
    uri: &str,
    peer: &str,
    forwarded_proto: Option<&str>,
) -> (StatusCode, Option<String>) {
    let service = test::init_service(ActixApp::new().app_data(app).service(login)).await;

    let mut request = test::TestRequest::post()
        .uri(uri)
        .peer_addr(peer.parse().expect("peer addr"))
        .set_json(serde_json::json!({ "name": "alice", "password": "password-1" }));

    if let Some(proto) = forwarded_proto {
        request = request.insert_header(("X-Forwarded-Proto", proto));
    }

    let response = test::call_service(&service, request.to_request()).await;

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

// ---------------------------------------------------------------------------
// Real loopback listeners
// ---------------------------------------------------------------------------

/// A server running on its own thread and its own actix `System`, so the test client can
/// be a plain blocking socket exactly like a browser would be.
struct LiveServer {
    addr: SocketAddr,
    handle: ServerHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LiveServer {
    async fn stop(mut self) {
        self.handle.stop(true).await;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Builds a throwaway self-signed key pair and certificate in memory, using the openssl
/// dependency the server already links. No files, no downloads, no new dependency.
fn self_signed_acceptor() -> openssl::ssl::SslAcceptorBuilder {
    let key = PKey::from_rsa(Rsa::generate(2048).expect("rsa key")).expect("pkey");

    let mut name = X509NameBuilder::new().expect("name");
    name.append_entry_by_text("CN", "localhost").expect("cn");
    let name = name.build();

    let mut cert = X509::builder().expect("x509 builder");
    cert.set_version(2).expect("version");
    cert.set_subject_name(&name).expect("subject");
    cert.set_issuer_name(&name).expect("issuer");
    cert.set_pubkey(&key).expect("pubkey");
    cert.set_not_before(&Asn1Time::days_from_now(0).expect("not before"))
        .expect("set not before");
    cert.set_not_after(&Asn1Time::days_from_now(1).expect("not after"))
        .expect("set not after");

    let serial = {
        let mut serial = BigNum::new().expect("bn");
        serial
            .rand(128, MsbOption::MAYBE_ZERO, false)
            .expect("rand");
        serial.to_asn1_integer().expect("asn1 int")
    };
    cert.set_serial_number(&serial).expect("serial");
    cert.sign(&key, MessageDigest::sha256()).expect("sign");
    let cert = cert.build();

    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).expect("acceptor");
    builder.set_private_key(&key).expect("set key");
    builder.set_certificate(&cert).expect("set cert");
    builder.check_private_key().expect("key matches cert");
    builder
}

fn bind_loopback() -> (std::net::TcpListener, SocketAddr) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

/// Starts the real `login` handler on a loopback listener; `tls` selects whether the
/// listener actually terminates TLS (which is what sets `AppConfig::secure()`).
fn start_server(config: common::config::Config, tls: bool) -> LiveServer {
    let (listener, addr) = bind_loopback();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<(SocketAddr, ServerHandle)>();

    let thread = std::thread::spawn(move || {
        let system = actix_web::rt::System::new();
        system.block_on(async move {
            let app = Data::new(App::new(config).await.expect("build app"));

            let http = actix_web::HttpServer::new(move || {
                ActixApp::new().app_data(app.clone()).service(login)
            });

            let server = if tls {
                http.listen_openssl(listener, self_signed_acceptor())
                    .expect("listen tls")
                    .run()
            } else {
                http.listen(listener).expect("listen").run()
            };

            ready_tx.send((addr, server.handle())).expect("ready");
            let _ = server.await;
        });
    });

    let (addr, handle) = ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("server failed to start");

    LiveServer {
        addr,
        handle,
        thread: Some(thread),
    }
}

fn raw_request(path: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .expect("write timeout");
    stream
}

fn roundtrip_cleartext(addr: SocketAddr, request: &[u8]) -> String {
    let mut stream = connect(addr);
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    String::from_utf8_lossy(&response).into_owned()
}

fn roundtrip_tls(addr: SocketAddr, request: &[u8]) -> String {
    let mut connector = SslConnector::builder(SslMethod::tls()).expect("connector");
    connector.set_verify(SslVerifyMode::NONE);
    let connector = connector.build();

    let stream = connect(addr);
    let mut stream = connector
        .connect("localhost", stream)
        .expect("tls handshake with the real listener");

    stream.write_all(request).expect("write request");
    stream.flush().expect("flush");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    String::from_utf8_lossy(&response).into_owned()
}

fn set_cookie_of(response: &str) -> Option<String> {
    response.lines().find_map(|line| {
        line.strip_prefix("Set-Cookie: ")
            .or_else(|| line.strip_prefix("set-cookie: "))
            .map(str::to_owned)
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// MW-22: a genuine TLS connection with an ordinary origin-form request target is the
/// case the old implementation could not recognise — it looked for a scheme in the URI.
/// The session must be issued, with `Secure`, from the listener state alone.
#[actix_web::test]
async fn genuine_origin_form_tls_login_issues_a_secure_cookie() {
    let (_dir, file) = temp_data_dir("tls");
    let config = config_for(&file, |config| {
        config.web_server.session_cookie_secure = true;
    });
    let server = start_server(config, true);
    let addr = server.addr;

    let response = tokio::task::spawn_blocking(move || {
        roundtrip_tls(addr, &raw_request("/login", LOGIN_BODY))
    })
    .await
    .expect("client task");

    server.stop().await;

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "a genuine TLS origin-form login must succeed, got:\n{response}"
    );
    let cookie = set_cookie_of(&response).expect("a session cookie must be set over TLS");
    assert!(
        cookie.starts_with("mlSession="),
        "unexpected cookie {cookie:?}"
    );
    assert!(
        cookie_has_secure(&cookie),
        "the session cookie must be Secure over TLS: {cookie:?}"
    );
    assert!(
        cookie.to_ascii_lowercase().contains("httponly"),
        "the session cookie must stay HttpOnly: {cookie:?}"
    );
    assert!(
        file.exists(),
        "the accepted TLS login must persist the first account"
    );
}

/// MW-22: `POST https://host/login` written by a client onto a cleartext socket is a
/// forgery, not evidence of TLS. It must be refused, and the refusal must happen before
/// the first account is created — the storage file must stay untouched.
#[actix_web::test]
async fn cleartext_absolute_form_https_is_refused_and_creates_no_account() {
    let (_dir, file) = temp_data_dir("spoof-wire");
    let config = config_for(&file, |config| {
        config.web_server.session_cookie_secure = true;
        config.web_server.first_login_create_admin = true;
    });
    assert!(!file.exists(), "precondition: no storage yet");

    let server = start_server(config, false);
    let addr = server.addr;

    let response = tokio::task::spawn_blocking(move || {
        roundtrip_cleartext(
            addr,
            &raw_request("https://moonlight.local/login", LOGIN_BODY),
        )
    })
    .await
    .expect("client task");

    server.stop().await;

    assert!(
        response.starts_with("HTTP/1.1 426"),
        "a spoofed https request-target on a cleartext socket must be refused, got:\n{response}"
    );
    assert!(
        set_cookie_of(&response).is_none(),
        "a refused login must not mint a session cookie:\n{response}"
    );
    assert!(
        !file.exists(),
        "a refused anonymous first login must not create the first account ({})",
        file.display()
    );
}

/// MW-22: the direct regression against the old assertion. An actix `TestRequest` with
/// an absolute-form https URI and no TLS listener is exactly the spoof the previous test
/// suite mistook for HTTPS. It must be refused, and the app must still have no user.
#[actix_web::test]
async fn absolute_form_uri_is_not_tls_proof_and_leaves_storage_untouched() {
    let (_dir, file) = temp_data_dir("spoof-testreq");
    let config = config_for(&file, |config| {
        config.web_server.session_cookie_secure = true;
        config.web_server.first_login_create_admin = true;
    });
    let app = Data::new(App::new(config).await.expect("build app"));

    let (status, cookie) = login_request(
        app.clone(),
        "https://moonlight.local/login",
        "127.0.0.1:5000",
        None,
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "an absolute-form https URI over a cleartext request must be refused"
    );
    assert!(
        cookie.is_none(),
        "no session cookie may be issued for a spoofed request-target: {cookie:?}"
    );

    assert!(
        matches!(app.user_by_name("alice").await, Err(AppError::UserNotFound)),
        "the refused login must not have created an account"
    );
    assert!(
        !file.exists(),
        "the refused login must not have written storage ({})",
        file.display()
    );
}

/// MW-22: spoofing the request-target is refused even when the operator disabled the
/// HTTPS-only policy or enabled the development exception — the claim is false either
/// way, and honouring it would be a transport downgrade.
#[actix_web::test]
async fn absolute_form_spoof_is_refused_even_with_dev_exception() {
    let app = app_with(|config| {
        config.web_server.session_cookie_secure = true;
        config.web_server.allow_insecure_http_login = true;
    })
    .await;

    let (status, cookie) =
        login_request(app, "https://moonlight.local/login", "127.0.0.1:5000", None).await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "the dev exception permits cleartext, not a false TLS claim"
    );
    assert!(cookie.is_none(), "cookie: {cookie:?}");
}

/// MW-22: with the policy enabled, a plain-HTTP login must be refused outright instead
/// of minting a session cookie that the browser will happily send in cleartext.
#[actix_web::test]
async fn plain_http_login_is_refused_when_secure_cookie_is_required() {
    let app = app_with(|config| config.web_server.session_cookie_secure = true).await;

    let (status, cookie) =
        login_request(app, "http://moonlight.local/login", "127.0.0.1:5000", None).await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "a plain-HTTP login must be rejected when session_cookie_secure is set"
    );
    assert!(
        cookie.is_none(),
        "no session cookie may be issued over plain HTTP, got {cookie:?}"
    );
}

/// MW-22: the development exception is explicit and opt-in, and it is still a real
/// session (so the local HTTP workflow keeps working).
#[actix_web::test]
async fn explicit_dev_exception_allows_plain_http_without_secure() {
    let app = app_with(|config| {
        config.web_server.session_cookie_secure = true;
        config.web_server.allow_insecure_http_login = true;
    })
    .await;

    let (status, cookie) =
        login_request(app, "http://localhost:8080/login", "127.0.0.1:5000", None).await;

    assert!(
        status.is_success(),
        "the dev exception must allow login, got {status}"
    );
    let cookie = cookie.expect("dev-exception login must still issue a cookie");
    assert!(
        !cookie_has_secure(&cookie),
        "the explicit dev exception is expected to omit Secure: {cookie:?}"
    );
}

/// MW-22: with the policy off, behavior is unchanged (explicit operator choice).
#[actix_web::test]
async fn no_policy_keeps_plain_http_login_working() {
    let app = app_with(|config| config.web_server.session_cookie_secure = false).await;

    let (status, cookie) =
        login_request(app, "http://localhost:8080/login", "127.0.0.1:5000", None).await;

    assert!(
        status.is_success(),
        "login must work with the policy off, got {status}"
    );
    let cookie = cookie.expect("cookie");
    assert!(
        !cookie_has_secure(&cookie),
        "cookie must not claim Secure: {cookie:?}"
    );
}

/// MW-22: an untrusted peer must not be able to talk the server out of (or into) its
/// transport policy with `X-Forwarded-Proto`. A spoofed `https` from an arbitrary
/// address must not make a cleartext login look protected — including when the peer also
/// spoofs an absolute-form https request-target.
#[actix_web::test]
async fn untrusted_forwarded_proto_is_ignored() {
    let app = app_with(|config| config.web_server.session_cookie_secure = true).await;

    let (status, cookie) = login_request(
        app.clone(),
        "http://moonlight.local/login",
        "203.0.113.5:41000",
        Some("https"),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "a spoofed X-Forwarded-Proto from an untrusted peer must not enable login"
    );
    assert!(
        cookie.is_none(),
        "a spoofed forwarded scheme must not produce a session cookie: {cookie:?}"
    );

    let (status, cookie) = login_request(
        app,
        "https://moonlight.local/login",
        "203.0.113.5:41000",
        Some("https"),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UPGRADE_REQUIRED,
        "an untrusted peer cannot combine forwarded headers with an https request-target"
    );
    assert!(cookie.is_none(), "cookie: {cookie:?}");
}

/// MW-22: a *trusted* proxy may assert the scheme, so a TLS-terminating proxy on
/// loopback keeps working without enabling the dev exception.
#[actix_web::test]
async fn trusted_forwarded_proto_is_honoured() {
    let app = app_with(|config| config.web_server.session_cookie_secure = true).await;

    let (status, cookie) = login_request(
        app,
        "http://moonlight.local/login",
        "127.0.0.1:41000",
        Some("https"),
    )
    .await;

    assert!(
        status.is_success(),
        "a trusted proxy asserting https must allow login, got {status}"
    );
    let cookie = cookie.expect("cookie");
    assert!(
        cookie_has_secure(&cookie),
        "a trusted https assertion must still yield a Secure cookie: {cookie:?}"
    );
}

/// MW-22/AR-07: the same trust rule applies to an explicitly configured proxy address,
/// not only to loopback — configured through the *transport* trust list.
///
/// API adaptation: this test previously configured the peer through
/// `ForwardedHeaders::trusted_proxy_ips`, which also enables header-based **user**
/// authentication. Transport trust is now its own list
/// (`trusted_forwarded_proto_ips`), so the assertion — "an explicitly configured proxy
/// address may assert https and get a Secure cookie" — is unchanged while
/// header-auth stays disabled, which is strictly stronger than before.
#[actix_web::test]
async fn configured_trusted_proxy_ip_is_honoured() {
    let app = app_with(|config| {
        config.web_server.session_cookie_secure = true;
        config.web_server.trusted_forwarded_proto_ips = vec!["198.51.100.7".parse().expect("ip")];
        assert!(
            config.web_server.forwarded_header.is_none(),
            "transport trust must not imply header user authentication"
        );
    })
    .await;

    let (status, cookie) = login_request(
        app,
        "http://moonlight.local/login",
        "198.51.100.7:41000",
        Some("https"),
    )
    .await;

    assert!(
        status.is_success(),
        "an explicitly trusted proxy must be able to assert https, got {status}"
    );
    let cookie = cookie.expect("cookie");
    assert!(cookie_has_secure(&cookie), "cookie: {cookie:?}");
}
