//! Credential-generation and transparent-rehash regressions (AR-01, AR-04).
//!
//! These drive the *real* `/login` handler over a real loopback HTTP listener and the
//! real `App`/`JsonStorage`, with fixture-owned accounts only. Nothing here touches a
//! live deployment, a real account or an external service.
//!
//! Determinism: the request that must be invalidated is genuinely inside its
//! credential check when the rotation happens. The app's test-only
//! `CredentialCheckGate` is armed for the victim account, so the real
//! `User::authenticate` path parks *after it read the credential snapshot and before
//! it starts the PBKDF2 check*, and signals the test. The rotation is then committed
//! while that login is provably parked at the exact boundary the regression is about,
//! and only afterwards is the login released. Nothing depends on thread timing,
//! `sleep`, pool saturation or a permissive assertion.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use actix_web::{App as HttpApp, HttpServer};
use common::config::{Config, StorageConfig};
use openssl::hash::MessageDigest;

use super::*;
use crate::app::{
    password::{HASH_ITERATIONS, StoragePassword},
    storage::{StorageRoleAdd, StorageUserAdd, StorageUserModify},
    user::{RoleType, UserId},
};

/// Legacy cost from an older release: verifiable, and what triggers the transparent
/// upgrade path.
const LEGACY_ITERATIONS: u32 = 1_000;

const ADMIN: &str = "audit-admin";
const OLD_PASSWORD: &str = "fixture-old-password";
const NEW_PASSWORD: &str = "fixture-new-password";

struct Fixture {
    app: Arc<App>,
    addr: SocketAddr,
    dir: PathBuf,
    _server: actix_web::dev::ServerHandle,
}

fn hash_with(password: &str, iterations: u32) -> StoragePassword {
    let salt = [7u8; 16];
    let mut hash = [0u8; 32];
    openssl::pkcs5::pbkdf2_hmac(
        password.as_bytes(),
        &salt,
        iterations as usize,
        MessageDigest::sha256(),
        &mut hash,
    )
    .expect("pbkdf2");
    StoragePassword {
        salt,
        hash,
        iterations,
    }
}

fn login_body(name: &str, password: &str) -> String {
    serde_json::json!({ "name": name, "password": password }).to_string()
}

/// One HTTP/1.1 request over a fresh loopback connection.
///
/// Returns `(status_code, mlSession cookie value)`. Blocking on purpose: the caller
/// runs it on `spawn_blocking` so the test can observe the request before it finishes.
fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    session: Option<&str>,
) -> (u16, Option<String>) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("timeout");

    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(session) = session {
        request.push_str(&format!(
            "Cookie: {COOKIE_SESSION_TOKEN_NAME}={session}\r\n"
        ));
    }
    if let Some(body) = body {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }

    stream.write_all(request.as_bytes()).expect("write request");
    stream.flush().expect("flush request");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let text = String::from_utf8_lossy(&response).into_owned();

    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in response: {text:?}"));

    let cookie = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("set-cookie") {
            return None;
        }
        let pair = value.trim().split(';').next()?;
        let (cookie_name, token) = pair.split_once('=')?;
        if cookie_name.trim() != COOKIE_SESSION_TOKEN_NAME || token.is_empty() {
            return None;
        }
        Some(token.to_string())
    });

    (status, cookie)
}

fn bind_loopback() -> (std::net::TcpListener, SocketAddr) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

async fn fixture(label: &str) -> Fixture {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "moonlight-web-ar-auth-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create dir");

    let mut config = Config {
        data_storage: StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: Duration::from_secs(3600),
        },
        ..Default::default()
    };
    config.web_server.first_login_create_admin = true;
    config.web_server.session_cookie_secure = false;

    let app = App::new(config).await.expect("build app");
    app.try_add_first_login(ADMIN.to_string(), OLD_PASSWORD.to_string())
        .await
        .expect("bootstrap admin");

    let app = Arc::new(app);

    // The listener owns its own actix System on a separate thread, so this test can
    // block on socket I/O without starving the server it drives.
    let (listener, addr) = bind_loopback();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<actix_web::dev::ServerHandle>();
    let data = actix_web::web::Data::from(app.clone());

    std::thread::spawn(move || {
        let system = actix_web::rt::System::new();
        system.block_on(async move {
            let server = HttpServer::new(move || {
                HttpApp::new()
                    .app_data(data.clone())
                    .service(login)
                    .service(authenticate)
            })
            .workers(2)
            .listen(listener)
            .expect("listen")
            .run();

            ready_tx.send(server.handle()).expect("ready");
            let _ = server.await;
        });
    });

    let handle = ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("server failed to start");

    Fixture {
        app,
        addr,
        dir,
        _server: handle,
    }
}

impl Fixture {
    async fn user_id(&self, name: &str) -> UserId {
        self.app
            .user_by_name(name)
            .await
            .expect("fixture user")
            .id()
    }

    async fn admin(&self) -> crate::app::user::Admin {
        self.app
            .user_by_auth(UserAuth::UserPassword {
                username: ADMIN.to_string(),
                password: OLD_PASSWORD.to_string(),
            })
            .await
            .expect("admin")
            .into_admin()
            .await
            .expect("admin proof")
    }

    /// Stores a legacy-cost hash for `password`, the way an older release would have
    /// left it on disk. Deliberately not a session-revoking change, so the account's
    /// credential generation is untouched.
    async fn seed_legacy_hash(&self, id: UserId, password: &str) {
        let admin = self.admin().await;
        let password = {
            let password = password.to_string();
            tokio::task::spawn_blocking(move || hash_with(&password, LEGACY_ITERATIONS))
                .await
                .expect("join")
        };
        self.app
            .user_by_id(id)
            .await
            .expect("seed target")
            .modify(
                &admin,
                StorageUserModify {
                    password: Some(Some(password)),
                    ..Default::default()
                },
            )
            .await
            .expect("seed legacy hash");
        // `User::modify` acknowledges only after `persist`, so the fixture hash is
        // durable when this returns.
    }

    fn cookie(token: &SessionToken) -> String {
        let mut bytes = [0u8; 64];
        token.encode(&mut bytes).to_owned()
    }

    /// Reads the durable store so assertions are about what is actually on disk, not
    /// about an in-memory view of it. The storage writer is asynchronous
    /// (`force_write`), so this waits for the committed value to appear instead of
    /// assuming it is already there.
    async fn durable_user(
        &self,
        id: UserId,
        want: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let path = self.dir.join("data.json");
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
                && let Some(user) = json["users"].get(id.0.to_string())
                && want(user)
            {
                return user.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "durable store never showed the expected state for user {}",
            id.0
        );
    }
}

/// AR-01 acceptance: the real login request is stopped in the middle of its
/// credential check while the password is rotated underneath it. The stale request
/// must not obtain a session, sessions issued before the rotation must be gone, and
/// the new password must work normally afterwards.
#[actix_web::test]
async fn login_interleaved_with_password_rotation_cannot_mint_a_session() {
    let fixture = fixture("rotation-interleave").await;
    let id = fixture.user_id(ADMIN).await;

    // A session issued before the rotation, exactly like another browser's cookie.
    let prior = fixture
        .admin()
        .await
        .new_session(Duration::from_secs(3600))
        .await
        .expect("prior session");
    let prior_token = Fixture::cookie(&prior);

    fixture.seed_legacy_hash(id, OLD_PASSWORD).await;

    // Park the victim login at the real credential-check boundary: it has already
    // read its credential snapshot, and it will not start (or finish) the PBKDF2
    // check until we release it. This is the exact window AR-01 is about, and it is
    // observed rather than timed.
    let crate::app::ArmedCredentialCheck {
        mut arrived,
        release,
    } = fixture.app.credential_check_gate().arm(ADMIN);

    let addr = fixture.addr;
    let body = login_body(ADMIN, OLD_PASSWORD);
    let victim = tokio::task::spawn_blocking(move || {
        http_request(addr, "POST", "/login", Some(&body), None)
    });

    tokio::time::timeout(Duration::from_secs(30), &mut arrived)
        .await
        .expect("the victim login must reach the credential-check boundary")
        .expect("the victim login must signal the boundary before the rotation");
    // Overlap is proven, not assumed: the request cannot be finished yet because it
    // is parked inside the credential check we control.
    assert!(
        !victim.is_finished(),
        "the stale login must still be in flight when the rotation commits"
    );

    // The rotation lands strictly inside that window, after the snapshot was read and
    // before the password was checked.
    let admin = fixture.admin().await;
    let new_hash = {
        let password = NEW_PASSWORD.to_string();
        tokio::task::spawn_blocking(move || StoragePassword::new(&password))
            .await
            .expect("join")
            .expect("hash")
    };
    fixture
        .app
        .user_by_id(id)
        .await
        .expect("changing user")
        .modify(
            &admin,
            StorageUserModify {
                password: Some(Some(new_hash)),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("rotate password");

    // Release the parked login; it now verifies the *old* password against a
    // generation that no longer exists.
    let _ = release.send(());

    let (status, cookie) = victim.await.expect("victim request");

    assert_eq!(
        status, 401,
        "a login whose credential check predates the rotation must be refused"
    );
    assert!(
        cookie.is_none(),
        "a stale credential check must not be handed a session cookie"
    );

    // Revocation is real: the pre-rotation session is dead.
    let probe_addr = fixture.addr;
    let (probe_status, _) = tokio::task::spawn_blocking(move || {
        http_request(probe_addr, "GET", "/authenticate", None, Some(&prior_token))
    })
    .await
    .expect("probe");
    assert_eq!(
        probe_status, 401,
        "the rotation must also end sessions that were already issued"
    );

    // Control: the new password logs in normally and its cookie really works.
    let control_addr = fixture.addr;
    let control_body = login_body(ADMIN, NEW_PASSWORD);
    let (status, cookie) = tokio::task::spawn_blocking(move || {
        http_request(control_addr, "POST", "/login", Some(&control_body), None)
    })
    .await
    .expect("control login");
    assert_eq!(
        status, 200,
        "control: the rotated password must still log in"
    );
    let token = cookie.expect("control login must set a session cookie");

    let control_addr = fixture.addr;
    let (status, _) = tokio::task::spawn_blocking(move || {
        http_request(control_addr, "GET", "/authenticate", None, Some(&token))
    })
    .await
    .expect("control probe");
    assert_eq!(
        status, 200,
        "control: the freshly issued session must authenticate"
    );

    std::fs::remove_dir_all(&fixture.dir).ok();
}

/// AR-01 revocation variant: deleting the account while its credential check is in
/// flight must not produce a session for a user that no longer exists.
#[actix_web::test]
async fn login_interleaved_with_account_deletion_cannot_mint_a_session() {
    let fixture = fixture("deletion-interleave").await;
    let admin = fixture.admin().await;

    let role = fixture
        .app
        .add_role_no_auth(StorageRoleAdd {
            name: "User".to_string(),
            ty: RoleType::User,
            default_settings: Default::default(),
            permissions: Default::default(),
        })
        .await
        .expect("role")
        .id();
    let victim_id = fixture
        .app
        .add_user(
            &admin,
            StorageUserAdd {
                role_id: role,
                name: "victim".to_string(),
                password: Some(hash_with(OLD_PASSWORD, LEGACY_ITERATIONS)),
                client_unique_id: "victim".to_string(),
            },
        )
        .await
        .expect("victim user")
        .id();

    // Same deterministic boundary as the rotation case: the login is parked after
    // reading its credential snapshot, so the deletion lands strictly inside the
    // window that matters (AR-01).
    let crate::app::ArmedCredentialCheck {
        mut arrived,
        release,
    } = fixture.app.credential_check_gate().arm("victim");

    let addr = fixture.addr;
    let body = login_body("victim", OLD_PASSWORD);
    let victim = tokio::task::spawn_blocking(move || {
        http_request(addr, "POST", "/login", Some(&body), None)
    });

    tokio::time::timeout(Duration::from_secs(30), &mut arrived)
        .await
        .expect("the victim login must reach the credential-check boundary")
        .expect("the victim login must signal the boundary before the deletion");
    assert!(
        !victim.is_finished(),
        "the stale login must still be in flight when the deletion commits"
    );

    fixture
        .app
        .user_by_id(victim_id)
        .await
        .expect("victim handle")
        .delete(&admin)
        .await
        .expect("delete account");

    let _ = release.send(());

    let (status, cookie) = victim.await.expect("victim request");

    assert_eq!(status, 401, "a deleted account must not be able to log in");
    assert!(
        cookie.is_none(),
        "a deleted account must not be issued a session"
    );

    std::fs::remove_dir_all(&fixture.dir).ok();
}

/// AR-04 acceptance over the real login handler: transparently upgrading a legacy hash
/// for a correct password must not end the account's other sessions, and it must
/// actually upgrade the durable hash.
#[actix_web::test]
async fn transparent_rehash_during_login_keeps_other_sessions() {
    let fixture = fixture("rehash-keeps").await;
    let id = fixture.user_id(ADMIN).await;

    let prior = fixture
        .admin()
        .await
        .new_session(Duration::from_secs(3600))
        .await
        .expect("other browser session");
    let prior_token = Fixture::cookie(&prior);

    fixture.seed_legacy_hash(id, OLD_PASSWORD).await;
    assert_eq!(
        fixture
            .durable_user(id, |user| user["password"]["iterations"]
                == LEGACY_ITERATIONS)
            .await["password"]["iterations"],
        LEGACY_ITERATIONS,
        "fixture must start from a legacy cost hash"
    );

    let addr = fixture.addr;
    let body = login_body(ADMIN, OLD_PASSWORD);
    let (status, cookie) = tokio::task::spawn_blocking(move || {
        http_request(addr, "POST", "/login", Some(&body), None)
    })
    .await
    .expect("login");
    assert_eq!(status, 200, "a valid legacy password must still log in");
    assert!(cookie.is_some(), "a successful login must issue a session");

    let durable = fixture
        .durable_user(id, |user| user["password"]["iterations"] == HASH_ITERATIONS)
        .await;
    assert_eq!(
        durable["password"]["iterations"], HASH_ITERATIONS,
        "the login must transparently upgrade the durable hash"
    );
    assert_eq!(
        durable["credential_version"], 0,
        "a transparent rehash is not a credential change"
    );

    let probe_addr = fixture.addr;
    let (status, _) = tokio::task::spawn_blocking(move || {
        http_request(probe_addr, "GET", "/authenticate", None, Some(&prior_token))
    })
    .await
    .expect("other browser probe");
    assert_eq!(
        status, 200,
        "another browser's session must survive a transparent rehash"
    );

    std::fs::remove_dir_all(&fixture.dir).ok();
}

/// AR-04 control: an *explicit* password change still ends the other sessions, so the
/// separation between rehash and rotation is real and not "never revoke".
#[actix_web::test]
async fn explicit_password_change_still_revokes_other_sessions() {
    let fixture = fixture("explicit-revokes").await;
    let id = fixture.user_id(ADMIN).await;
    let admin = fixture.admin().await;

    let other = admin
        .new_session(Duration::from_secs(3600))
        .await
        .expect("other browser session");
    let other_token = Fixture::cookie(&other);

    let new_hash = {
        let password = NEW_PASSWORD.to_string();
        tokio::task::spawn_blocking(move || StoragePassword::new(&password))
            .await
            .expect("join")
            .expect("hash")
    };
    fixture
        .app
        .user_by_id(id)
        .await
        .expect("changing user")
        .modify(
            &admin,
            StorageUserModify {
                password: Some(Some(new_hash)),
                revoke_sessions: true,
                ..Default::default()
            },
        )
        .await
        .expect("explicit change");

    let addr = fixture.addr;
    let (status, _) = tokio::task::spawn_blocking(move || {
        http_request(addr, "GET", "/authenticate", None, Some(&other_token))
    })
    .await
    .expect("other browser probe");
    assert_eq!(
        status, 401,
        "an explicit password change must still end the account's other sessions"
    );

    std::fs::remove_dir_all(&fixture.dir).ok();
}
