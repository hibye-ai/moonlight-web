//! Exercises the orchestration used by start_host, not the reap helper.
use super::*;
use crate::app::auth::{SessionToken, UserAuth};
use actix_web::{FromRequest, test::TestRequest, web::Bytes};
use common::config::{Config, StorageConfig};
use futures::channel::mpsc;
use std::path::PathBuf;

struct Fixture {
    app: Data<App>,
    token: SessionToken,
    user_id: u32,
    dir: PathBuf,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "stream-control-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::create_dir_all(&dir).await.expect("fixture dir");
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
        let admin = app
            .try_add_first_login("control-fixture".into(), "fixture-password".into())
            .await
            .expect("fixture user");
        let user_id = admin.id().0;
        let token = admin
            .new_session(Duration::from_secs(3600))
            .await
            .expect("fixture session");
        Self {
            app,
            token,
            user_id,
            dir,
        }
    }

    async fn permissions(&self) -> common::api_bindings::StreamPermissions {
        self.app
            .user_by_session(self.token)
            .await
            .expect("user")
            .role()
            .await
            .expect("role")
            .permissions()
            .await
            .expect("permissions")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// Only ever kills this fixture's child, including on a failing regression.
struct ChildCleanup(u32);
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if process_exists(self.0) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &self.0.to_string()])
                .status();
        }
    }
}
fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

async fn websocket() -> (
    HttpResponse,
    Session,
    actix_ws::MessageStream,
    mpsc::UnboundedSender<Result<Bytes, actix_web::error::PayloadError>>,
) {
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
    let (response, session, stream) = actix_ws::handle(&request, payload).expect("ws handshake");
    (response, session, stream, sender)
}

// RFC 6455 client frame with a fixture-owned mask (no client/network session).
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

async fn child(
    script: &str,
) -> (
    tokio::process::Child,
    tokio::process::ChildStdin,
    tokio::process::ChildStdout,
    ChildCleanup,
) {
    let mut child = Command::new("python3")
        .args(["-u", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("fixture child");
    let cleanup = ChildCleanup(child.id().expect("pid"));
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    (child, stdin, stdout, cleanup)
}

#[actix_web::test]
async fn control_close_reaps_hostile_child_retaining_stdout_and_releases_lock() {
    let fixture = Fixture::new("hostile").await;
    let (response, session, stream, frames) = websocket().await;
    // Keep the response body alive, like an open browser connection.
    let _response = response;
    let (child, stdin, stdout, cleanup) = child("import time; time.sleep(30)").await;
    let guard = acquire_stream_session_lock(fixture.user_id, 8001).await;
    frames
        .unbounded_send(Ok(client_frame(8, &[])))
        .expect("close frame");
    let control = run_stream_control(
        fixture.app.clone(),
        UserAuth::Session(fixture.token),
        fixture.permissions().await,
        session,
        stream,
        child,
        stdin,
        stdout,
        ServerIpcMessage::Stop,
        guard,
        StreamDiagnosticGuard::new(8001, 1),
    );
    tokio::pin!(control);
    assert!(
        timeout(Duration::from_millis(200), &mut control)
            .await
            .is_err()
    );
    let next = acquire_stream_session_lock(fixture.user_id, 8001);
    tokio::pin!(next);
    assert!(
        timeout(Duration::from_millis(100), &mut next)
            .await
            .is_err(),
        "reconnect must not acquire the host lock while the previous child is alive"
    );
    assert!(
        process_exists(cleanup.0),
        "hostile child remains alive during grace"
    );
    timeout(Duration::from_secs(5), &mut control)
        .await
        .expect("control shutdown must be bounded without IPC EOF");
    assert!(
        !process_exists(cleanup.0),
        "child must be killed AND reaped before control returns"
    );
    let _next = timeout(Duration::from_millis(250), &mut next)
        .await
        .expect("next stream must acquire host lock");
}

#[actix_web::test]
async fn revocation_interrupts_large_payload_pending_send() {
    let fixture = Fixture::new("saturated").await;
    let (_response, session, stream, frames) = websocket().await;
    let (child, stdin, stdout, cleanup) = child("import time; time.sleep(30)").await;
    let guard = acquire_stream_session_lock(fixture.user_id, 8002).await;
    // Several MiB of serialized data cannot fit in the non-reading child's
    // pipe plus the bounded ten-message IPC queue.
    let frame = client_frame(2, &vec![255; 60 * 1024]);
    for _ in 0..24 {
        frames
            .unbounded_send(Ok(frame.clone()))
            .expect("large frame");
    }
    let diagnostic = StreamDiagnosticGuard::new(8002, 1);
    let diagnostic_id = diagnostic.session_id;
    frames
        .unbounded_send(Ok(client_frame(1, br#"{"SetTransport":"WebRTC"}"#)))
        .expect("marker frame");
    let control = run_stream_control(
        fixture.app.clone(),
        UserAuth::Session(fixture.token),
        fixture.permissions().await,
        session,
        stream,
        child,
        stdin,
        stdout,
        ServerIpcMessage::Stop,
        guard,
        diagnostic,
    );
    tokio::pin!(control);
    assert!(
        timeout(Duration::from_millis(500), &mut control)
            .await
            .is_err()
    );
    let diagnostics = stream_diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|s| s.session_id == diagnostic_id && s.transport.is_none()),
        "the transport marker behind the full send must not be consumed"
    );
    assert!(
        process_exists(cleanup.0),
        "saturated child is still alive before revocation"
    );
    fixture
        .app
        .delete_session(fixture.token)
        .await
        .expect("revoke fixture session");
    timeout(Duration::from_secs(8), &mut control)
        .await
        .expect("revocation must interrupt the actual pending production send");
    assert!(
        !process_exists(cleanup.0),
        "revocation must reap child before returning"
    );
    let _next = timeout(
        Duration::from_millis(250),
        acquire_stream_session_lock(fixture.user_id, 8002),
    )
    .await
    .expect("revoked stream must release its host lock");
}

enum InboundEnd {
    Close,
    Eof,
    PayloadError,
}

fn init_message(permissions: common::api_bindings::StreamPermissions) -> ServerIpcMessage {
    ServerIpcMessage::Init {
        config: StreamerConfig {
            webrtc: common::config::WebRtcConfig::default(),
            log_level: log::LevelFilter::Info,
        },
        host_address: "127.0.0.1".into(),
        host_http_port: 47989,
        client_unique_id: Some("fixture-client".into()),
        client_private_key: pem::Pem::new("PRIVATE KEY", b"fixture-only".to_vec()),
        client_certificate: pem::Pem::new("CERTIFICATE", b"fixture-only".to_vec()),
        server_certificate: pem::Pem::new("CERTIFICATE", b"fixture-only".to_vec()),
        app_id: 1,
        video_frame_queue_size: 3,
        audio_sample_queue_size: 20,
        permissions,
    }
}

async fn inbound_end_interrupts_large_payload_pending_send(end: InboundEnd, host_id: u32) {
    use std::os::fd::AsRawFd;

    let fixture = Fixture::new("inbound-end-saturated").await;
    let references_before = Arc::strong_count(&fixture.app);
    let (_response, session, stream, frames) = websocket().await;
    let (child, stdin, stdout, cleanup) = child("import time; time.sleep(30)").await;
    // These are the actual descriptors owned by the production IPC workers.
    let pipes = [
        stdin.as_raw_fd(),
        stdout.as_raw_fd(),
        child.stderr.as_ref().expect("stderr").as_raw_fd(),
    ]
    .map(|fd| std::fs::read_link(format!("/proc/self/fd/{fd}")).expect("fixture pipe"));
    let guard = acquire_stream_session_lock(fixture.user_id, host_id).await;
    let diagnostic = StreamDiagnosticGuard::new(host_id, 1);
    let diagnostic_id = diagnostic.session_id;
    let frame = client_frame(2, &vec![255; 60 * 1024]);
    for _ in 0..24 {
        frames
            .unbounded_send(Ok(frame.clone()))
            .expect("large frame");
    }
    frames
        .unbounded_send(Ok(client_frame(1, br#"{"SetTransport":"WebRTC"}"#)))
        .expect("marker behind saturated send");
    let permissions = fixture.permissions().await;
    let control = run_stream_control(
        fixture.app.clone(),
        UserAuth::Session(fixture.token),
        permissions.clone(),
        session,
        stream,
        child,
        stdin,
        stdout,
        init_message(permissions),
        guard,
        diagnostic,
    );
    tokio::pin!(control);
    assert!(
        timeout(Duration::from_millis(500), &mut control)
            .await
            .is_err()
    );
    assert!(
        stream_diagnostics()
            .iter()
            .any(|s| s.session_id == diagnostic_id && s.transport.is_none()),
        "ordinary forwarding must be blocked behind the full IPC pipe/queue"
    );
    assert!(
        process_exists(cleanup.0),
        "child is alive before the inbound shutdown signal"
    );
    let next = acquire_stream_session_lock(fixture.user_id, host_id);
    tokio::pin!(next);
    assert!(
        timeout(Duration::from_millis(100), &mut next)
            .await
            .is_err(),
        "host lock must stay held while the saturated child is alive"
    );

    let frames = match end {
        InboundEnd::Close => {
            frames
                .unbounded_send(Ok(client_frame(8, &[])))
                .expect("close");
            Some(frames)
        }
        InboundEnd::Eof => {
            drop(frames);
            None
        }
        InboundEnd::PayloadError => {
            frames
                .unbounded_send(Err(actix_web::error::PayloadError::Incomplete(None)))
                .expect("payload cancellation");
            Some(frames)
        }
    };
    // Less than the five-second authorization recheck: Close/EOF/error, not
    // revocation or the 35-second heartbeat, must start shutdown now.
    timeout(Duration::from_secs(4), &mut control)
        .await
        .expect("inbound termination must interrupt the actual pending IPC send");
    assert!(
        !process_exists(cleanup.0),
        "child must be killed AND reaped before returning"
    );
    if let Some(frames) = frames {
        assert!(
            frames.is_closed(),
            "Close/error must release the reader without an EOF trigger"
        );
    }
    let _next = timeout(Duration::from_millis(250), &mut next)
        .await
        .expect("host lock released");
    assert_eq!(
        Arc::strong_count(&fixture.app),
        references_before,
        "no retained supervisor/revalidator task"
    );
    assert!(
        !stream_diagnostics()
            .iter()
            .any(|s| s.session_id == diagnostic_id),
        "diagnostic guard released"
    );
    for fd in std::fs::read_dir("/proc/self/fd").expect("process descriptors") {
        if let Ok(target) = std::fs::read_link(fd.expect("descriptor").path()) {
            assert!(
                !pipes.contains(&target),
                "IPC worker retained fixture pipe {target:?}"
            );
        }
    }
    fixture
        .app
        .user_by_session(fixture.token)
        .await
        .expect("shutdown must not revoke fixture authorization");
}

#[actix_web::test]
async fn control_close_interrupts_large_payload_pending_send() {
    inbound_end_interrupts_large_payload_pending_send(InboundEnd::Close, 8011).await;
}

#[actix_web::test]
async fn control_eof_interrupts_large_payload_pending_send() {
    inbound_end_interrupts_large_payload_pending_send(InboundEnd::Eof, 8012).await;
}

#[actix_web::test]
async fn control_payload_cancellation_interrupts_large_payload_pending_send() {
    inbound_end_interrupts_large_payload_pending_send(InboundEnd::PayloadError, 8013).await;
}

#[actix_web::test]
async fn saturated_control_preserves_init_and_ordinary_message_order_when_child_resumes() {
    let fixture = Fixture::new("ordered-backlog").await;
    let (_response, session, stream, frames) = websocket().await;
    let resume = fixture.dir.join("resume");
    let record = fixture.dir.join("ordered.json");
    let script = format!(
        "import json, pathlib, sys, time\nwhile not pathlib.Path({:?}).exists():\n time.sleep(0.01)\nmessages = [json.loads(sys.stdin.readline()) for _ in range(26)]\nopen({:?}, 'w').write(json.dumps(messages))\nprint('\"Stop\"', flush=True)",
        resume.to_str().expect("resume path"),
        record.to_str().expect("record path")
    );
    let (child, stdin, stdout, cleanup) = child(&script).await;
    let permissions = fixture.permissions().await;
    let init = init_message(permissions.clone());
    let mut expected = vec![serde_json::to_value(&init).expect("expected Init")];
    for index in 0..24 {
        let bytes = vec![index as u8; 60 * 1024];
        frames
            .unbounded_send(Ok(client_frame(2, &bytes)))
            .expect("ordered binary frame");
        expected.push(serde_json::json!({"WebSocketTransport": bytes}));
    }
    frames
        .unbounded_send(Ok(client_frame(1, br#"{"SetTransport":"WebSocket"}"#)))
        .expect("ordered text frame");
    expected.push(serde_json::json!({"WebSocket": {"SetTransport": "WebSocket"}}));
    let diagnostic = StreamDiagnosticGuard::new(8014, 1);
    let diagnostic_id = diagnostic.session_id;
    let guard = acquire_stream_session_lock(fixture.user_id, 8014).await;
    let control = run_stream_control(
        fixture.app.clone(),
        UserAuth::Session(fixture.token),
        permissions,
        session,
        stream,
        child,
        stdin,
        stdout,
        init,
        guard,
        diagnostic,
    );
    tokio::pin!(control);
    assert!(
        timeout(Duration::from_millis(500), &mut control)
            .await
            .is_err()
    );
    assert!(
        stream_diagnostics()
            .iter()
            .any(|s| s.session_id == diagnostic_id && s.transport.is_none()),
        "text marker must remain behind the blocked binary backlog"
    );
    tokio::fs::write(resume, b"resume fixture reader")
        .await
        .expect("release child reader");
    timeout(Duration::from_secs(5), &mut control)
        .await
        .expect("queued messages drain after child resumes");
    assert!(
        !process_exists(cleanup.0),
        "cooperative reader child reaped"
    );
    let received: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(record).expect("child record")).expect("record JSON");
    assert_eq!(
        received.len(),
        expected.len(),
        "no ordinary message lost or duplicated"
    );
    for (index, (received, expected)) in received.iter().zip(&expected).enumerate() {
        assert!(
            received == expected,
            "message {index} arrived out of order or changed"
        );
    }
}

#[actix_web::test]
async fn inbound_queue_overload_terminates_instead_of_waiting_or_growing() {
    let fixture = Fixture::new("bounded-ingress").await;
    let (_response, session, stream, frames) = websocket().await;
    let (child, stdin, stdout, cleanup) = child("import time; time.sleep(30)").await;
    let frame = client_frame(2, &vec![255; 60 * 1024]);
    // Exceeds the bounded ingress and IPC queues plus their in-flight messages.
    for _ in 0..128 {
        frames
            .unbounded_send(Ok(frame.clone()))
            .expect("overload frame");
    }
    let permissions = fixture.permissions().await;
    let guard = acquire_stream_session_lock(fixture.user_id, 8015).await;
    assert!(
        !frames.is_closed(),
        "the fixture still owns the inbound source"
    );
    timeout(
        Duration::from_secs(4),
        run_stream_control(
            fixture.app.clone(),
            UserAuth::Session(fixture.token),
            permissions.clone(),
            session,
            stream,
            child,
            stdin,
            stdout,
            init_message(permissions),
            guard,
            StreamDiagnosticGuard::new(8015, 1),
        ),
    )
    .await
    .expect("bounded ingress must fail closed without waiting on saturated IPC");
    // Keep frames alive through shutdown: neither EOF, Close, nor revocation
    // triggered it. The product has now dropped its inbound receiver.
    assert!(
        frames.is_closed(),
        "the inbound reader must be released on overload"
    );
    assert!(
        !process_exists(cleanup.0),
        "overload must reap hostile child"
    );
    let _next = timeout(
        Duration::from_millis(250),
        acquire_stream_session_lock(fixture.user_id, 8015),
    )
    .await
    .expect("overloaded stream releases its host lock");
    fixture
        .app
        .user_by_session(fixture.token)
        .await
        .expect("authorization stays valid on overload");
}

#[actix_web::test]
async fn cooperative_control_shutdown_drops_revalidation_app_reference() {
    let fixture = Fixture::new("cooperative").await;
    let references_before = Arc::strong_count(&fixture.app);
    let (_response, session, stream, frames) = websocket().await;
    let stop_record = fixture.dir.join("stop-received");
    let script = format!(
        "import sys\nfor line in sys.stdin:\n if line.strip() == '\"Stop\"':\n  open({:?}, 'w').write('cooperative-stop')\n  sys.exit(0)",
        stop_record.to_str().expect("fixture path")
    );
    let (child, stdin, stdout, cleanup) = child(&script).await;
    let guard = acquire_stream_session_lock(fixture.user_id, 8003).await;
    frames
        .unbounded_send(Ok(client_frame(8, &[])))
        .expect("close frame");
    timeout(
        Duration::from_millis(1500),
        run_stream_control(
            fixture.app.clone(),
            UserAuth::Session(fixture.token),
            fixture.permissions().await,
            session,
            stream,
            child,
            stdin,
            stdout,
            ServerIpcMessage::WebSocket(StreamClientMessage::SetTransport(TransportType::WebRTC)),
            guard,
            StreamDiagnosticGuard::new(8003, 1),
        ),
    )
    .await
    .expect("cooperative control shutdown");
    assert!(!process_exists(cleanup.0), "cooperative child reaped");
    assert_eq!(
        std::fs::read_to_string(stop_record).expect("child Stop acknowledgment"),
        "cooperative-stop"
    );
    assert_eq!(
        Arc::strong_count(&fixture.app),
        references_before,
        "healthy disconnect must cancel/join revalidation, not retain App forever"
    );
}

#[actix_web::test]
async fn control_close_interrupts_backpressured_output_forwarder() {
    let fixture = Fixture::new("blocked-output").await;
    // Never drain the browser-side response: its bounded outgoing queue fills.
    let (_response, session, stream, frames) = websocket().await;
    let (child, stdin, stdout, cleanup) = child(
        "import json, time\nfor _ in range(64):\n print(json.dumps({'WebSocketTransport': [9] * 60000}), flush=True)\ntime.sleep(30)"
    ).await;
    let guard = acquire_stream_session_lock(fixture.user_id, 8004).await;
    let control = run_stream_control(
        fixture.app.clone(),
        UserAuth::Session(fixture.token),
        fixture.permissions().await,
        session,
        stream,
        child,
        stdin,
        stdout,
        ServerIpcMessage::Stop,
        guard,
        StreamDiagnosticGuard::new(8004, 1),
    );
    tokio::pin!(control);
    assert!(
        timeout(Duration::from_millis(500), &mut control)
            .await
            .is_err()
    );
    frames
        .unbounded_send(Ok(client_frame(8, &[])))
        .expect("close while output blocked");
    timeout(Duration::from_secs(5), &mut control)
        .await
        .expect("output backpressure cannot delay reaping");
    assert!(!process_exists(cleanup.0));
}

#[actix_web::test]
async fn healthy_control_forwards_real_frames_and_reconnects_on_the_same_host() {
    let fixture = Fixture::new("reconnect").await;
    let references_before = Arc::strong_count(&fixture.app);
    for attempt in 0..2 {
        let (_response, session, stream, frames) = websocket().await;
        let record = fixture.dir.join(format!("received-{attempt}.json"));
        let script = format!(
            "import json, sys\nmessages = [json.loads(sys.stdin.readline()) for _ in range(3)]\nopen({:?}, 'w').write(json.dumps(messages))\nprint('\"Stop\"', flush=True)",
            record.to_str().expect("fixture path")
        );
        let (child, stdin, stdout, cleanup) = child(&script).await;
        let guard = timeout(
            Duration::from_millis(250),
            acquire_stream_session_lock(fixture.user_id, 8005),
        )
        .await
        .expect("same-host reconnect lock");
        let transport = StreamClientMessage::SetTransport(TransportType::WebSocket);
        frames
            .unbounded_send(Ok(client_frame(
                1,
                &serde_json::to_vec(&transport).expect("control JSON"),
            )))
            .expect("text frame");
        frames
            .unbounded_send(Ok(client_frame(2, &[17, 23, 42])))
            .expect("binary frame");
        timeout(
            Duration::from_secs(3),
            run_stream_control(
                fixture.app.clone(),
                UserAuth::Session(fixture.token),
                fixture.permissions().await,
                session,
                stream,
                child,
                stdin,
                stdout,
                ServerIpcMessage::WebSocket(StreamClientMessage::SetTransport(
                    TransportType::WebRTC,
                )),
                guard,
                StreamDiagnosticGuard::new(8005, 1),
            ),
        )
        .await
        .expect("healthy control orchestration");
        assert!(!process_exists(cleanup.0));
        let received: serde_json::Value =
            serde_json::from_slice(&std::fs::read(record).expect("child record"))
                .expect("record JSON");
        assert_eq!(
            received,
            serde_json::json!([
                {"WebSocket": {"SetTransport": "WebRTC"}},
                {"WebSocket": {"SetTransport": "WebSocket"}},
                {"WebSocketTransport": [17, 23, 42]},
            ])
        );
        assert_eq!(
            Arc::strong_count(&fixture.app),
            references_before,
            "no retained revalidator on reconnect"
        );
        assert!(
            !stream_diagnostics().iter().any(|s| s.host_id == 8005),
            "diagnostic guard released"
        );
    }
}
