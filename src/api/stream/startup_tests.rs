//! AR-11 regressions: bounded, cancelable, revalidated stream startup.
//!
//! These drive the production `startup::await_init` / `startup::supervise_startup`
//! supervisors against a real `actix_ws` handshake and the real `App`, with the
//! phase bounds shortened to milliseconds so the *same* code paths can be observed
//! without waiting out the ten- and thirty-second production windows. The
//! unshortened audit fixture (`astra_startup_tests`) remains the end-to-end proof.
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::{FromRequest, test::TestRequest, web::Bytes};
use common::api_bindings::{StreamClientMessage, StreamPermissions};
use common::config::{Config, StorageConfig};
use futures::channel::mpsc;

use super::startup::{self, StartupBounds, StartupError};
use super::*;
use crate::app::auth::{SessionToken, UserAuth};

/// Short bounds: the supervisor logic under test is bound-driven, not clock-driven.
const FAST: StartupBounds = StartupBounds {
    init: Duration::from_millis(300),
    startup: Duration::from_millis(300),
    recheck: Duration::from_millis(100),
};

struct Fixture {
    app: Data<App>,
    token: SessionToken,
    permissions: StreamPermissions,
    dir: std::path::PathBuf,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "stream-startup-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)
            .expect("fixture temp dir: create unique test-owned directory before App::new");
        let app = Data::new(
            App::new(Config {
                data_storage: StorageConfig::Json {
                    path: dir.join("data.json").to_string_lossy().into_owned(),
                    session_expiration_check_interval: Duration::from_secs(3600),
                },
                ..Default::default()
            })
            .await
            .expect("fixture app"),
        );
        let mut admin = app
            .try_add_first_login("startup-fixture".into(), "fixture-password".into())
            .await
            .expect("fixture user");
        let token = admin
            .new_session(Duration::from_secs(3600))
            .await
            .expect("fixture session");
        let permissions = admin
            .role()
            .await
            .expect("role")
            .permissions()
            .await
            .expect("permissions");
        Self {
            app,
            token,
            permissions,
            dir,
        }
    }

    fn auth(&self) -> UserAuth {
        UserAuth::Session(self.token)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type Frames = mpsc::UnboundedSender<Result<Bytes, actix_web::error::PayloadError>>;

/// A real websocket handshake plus the client side of the payload.
async fn websocket() -> (actix_ws::Session, actix_ws::MessageStream, Frames) {
    let request = TestRequest::get()
        .uri("/host/stream")
        .insert_header(("upgrade", "websocket"))
        .insert_header(("connection", "upgrade"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_request();
    let (sender, receiver) = mpsc::unbounded();
    let mut payload = actix_web::dev::Payload::from(Box::pin(receiver)
        as std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<Bytes, actix_web::error::PayloadError>>>,
        >);
    let payload = Payload::from_request(&request, &mut payload)
        .await
        .expect("payload");
    let (_response, session, stream) = actix_ws::handle(&request, payload).expect("ws handshake");
    (session, stream, sender)
}

/// RFC 6455 client frame with a fixture-owned mask (no client/network session).
fn client_frame(opcode: u8, body: &[u8]) -> Bytes {
    let mut bytes = vec![0x80 | opcode];
    if body.len() < 126 {
        bytes.push(0x80 | body.len() as u8);
    } else {
        assert!(body.len() <= u16::MAX as usize);
        bytes.push(0x80 | 126);
        bytes.extend_from_slice(&(body.len() as u16).to_be_bytes());
    }
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(body);
    Bytes::from(bytes)
}

fn init_json() -> Bytes {
    client_frame(
        1,
        serde_json::to_vec(&StreamClientMessage::Init {
            host_id: 1,
            app_id: 2,
            video_frame_queue_size: 3,
            audio_sample_queue_size: 4,
        })
        .expect("init json")
        .as_slice(),
    )
}

// ---------------------------------------------------------------------------
// Init window: bounded, and not extended by a chatty client
// ---------------------------------------------------------------------------

/// A client that upgrades and never sends `Init` cannot hold the startup task: the
/// window closes on its own.
#[actix_web::test]
async fn silent_client_without_init_is_bounded() {
    let fixture = Fixture::new("silent").await;
    let (_session, mut stream, _frames) = websocket().await;

    let started = Instant::now();
    let result = startup::await_init(
        &fixture.app,
        &fixture.auth(),
        &fixture.permissions,
        &mut stream,
        FAST,
    )
    .await;

    assert!(
        matches!(result, Err(StartupError::Deadline(_))),
        "a socket that never sends Init must hit the Init deadline, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the Init window must be bounded, took {:?}",
        started.elapsed()
    );
}

/// Ping/Pong traffic does not count as progress: a client that keeps the socket
/// busy while withholding `Init` is still bounded.
#[actix_web::test]
async fn continuous_ping_does_not_extend_the_init_window() {
    let fixture = Fixture::new("pinging").await;
    let (_session, mut stream, frames) = websocket().await;

    // A ping every 25 ms, for well past the 300 ms window.
    let pinger = tokio::spawn(async move {
        for _ in 0..60 {
            if frames
                .unbounded_send(Ok(client_frame(9, b"keepalive")))
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        frames
    });

    let started = Instant::now();
    let result = startup::await_init(
        &fixture.app,
        &fixture.auth(),
        &fixture.permissions,
        &mut stream,
        FAST,
    )
    .await;
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(StartupError::Deadline(_))),
        "pings must not substitute for Init, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "keepalive traffic must not extend the deadline, took {elapsed:?}"
    );

    pinger.abort();
}

/// Dropping the payload (EOF, as a closing browser produces) ends the wait at once
/// instead of after the deadline.
#[actix_web::test]
async fn eof_before_init_ends_startup_immediately() {
    let fixture = Fixture::new("eof").await;
    let (_session, mut stream, frames) = websocket().await;
    drop(frames);

    let started = Instant::now();
    let result = startup::await_init(
        &fixture.app,
        &fixture.auth(),
        &fixture.permissions,
        &mut stream,
        StartupBounds {
            init: Duration::from_secs(30),
            ..FAST
        },
    )
    .await;

    assert!(
        matches!(result, Err(StartupError::Closed(_))),
        "EOF must end startup, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "EOF must not wait for the deadline, took {:?}",
        started.elapsed()
    );
}

/// A well-formed `Init` is still accepted — the deadline must not reject healthy
/// clients.
#[actix_web::test]
async fn valid_init_is_accepted() {
    let fixture = Fixture::new("valid").await;
    let (_session, mut stream, frames) = websocket().await;
    frames.unbounded_send(Ok(init_json())).expect("init frame");

    let init = startup::await_init(
        &fixture.app,
        &fixture.auth(),
        &fixture.permissions,
        &mut stream,
        FAST,
    )
    .await
    .expect("valid Init must be accepted");

    assert_eq!(init.host_id, 1);
    assert_eq!(init.app_id, 2);
    assert_eq!(init.video_frame_queue_size, 3);
    assert_eq!(init.audio_sample_queue_size, 4);
}

// ---------------------------------------------------------------------------
// Authorization revalidation inside the Init window
// ---------------------------------------------------------------------------

/// Revoking the session while the client withholds `Init` ends startup at the next
/// recheck, not at the deadline: a revoked socket must not linger for the whole
/// window.
#[actix_web::test]
async fn revoked_session_ends_init_wait_before_the_deadline() {
    let fixture = Fixture::new("revoked").await;
    let (_session, mut stream, _frames) = websocket().await;

    let revoke_app = fixture.app.clone();
    let revoke_token = fixture.token;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        revoke_app
            .delete_session(revoke_token)
            .await
            .expect("revoke fixture session");
    });

    let started = Instant::now();
    let result = startup::await_init(
        &fixture.app,
        &fixture.auth(),
        &fixture.permissions,
        &mut stream,
        StartupBounds {
            // A long Init window proves the *recheck*, not the deadline, ended it.
            init: Duration::from_secs(30),
            startup: Duration::from_secs(30),
            recheck: Duration::from_millis(100),
        },
    )
    .await;

    assert!(
        matches!(result, Err(StartupError::Revoked)),
        "a revoked session must end the Init wait, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "revocation must not wait for the Init deadline, took {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Weak references: the stream App must not be retained past startup
// ---------------------------------------------------------------------------

/// The supervisor must not retain the `App` (and therefore storage) after it
/// returns, on either a success or a failure path.
#[actix_web::test]
async fn startup_supervision_does_not_retain_the_app() {
    let fixture = Fixture::new("retains").await;
    let references = Arc::strong_count(&fixture.app);

    let (_session, mut stream, _frames) = websocket().await;
    let _ = startup::await_init(
        &fixture.app,
        &fixture.auth(),
        &fixture.permissions,
        &mut stream,
        FAST,
    )
    .await;

    assert_eq!(
        Arc::strong_count(&fixture.app),
        references,
        "startup supervision must not leak an App reference"
    );
}
