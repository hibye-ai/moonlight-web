use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, OnceLock, Weak as StdWeak,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use actix_web::{
    Error, HttpRequest, HttpResponse, get,
    http::header::{HOST, ORIGIN},
    post, rt as actix_rt,
    web::{Data, Json, Payload},
};
use actix_ws::{Closed, Message, Session};
use common::{
    api_bindings::{
        LogMessageType, PostCancelRequest, PostCancelResponse, StreamClientMessage,
        StreamServerMessage, StreamStartupError, TransportType,
    },
    ipc::{IpcSender, ServerIpcMessage, StreamerConfig, StreamerIpcMessage, create_child_ipc},
    serialize_json,
};
use log::{debug, error, info, warn};
use serde::Serialize;
use tokio::{
    process::Command,
    sync::Mutex,
    time::{interval, timeout},
};
use tracing::{Level, instrument, span};

use crate::app::{
    App, AppError,
    host::{AppId, HostId},
    user::AuthenticatedUser,
};

use super::auth::extract_user_auth;
use authorization::{STREAM_AUTHORIZATION_RECHECK_INTERVAL, revalidate_stream_authorization};

const MIN_VIDEO_FRAME_QUEUE_SIZE: usize = 1;
const MAX_VIDEO_FRAME_QUEUE_SIZE: usize = 120;
const MIN_AUDIO_SAMPLE_QUEUE_SIZE: usize = 1;
const MAX_AUDIO_SAMPLE_QUEUE_SIZE: usize = 500;

const CONTROL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const CONTROL_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(35);
const CONTROL_INBOUND_QUEUE_CAPACITY: usize = 32;
const STREAMER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound on reaping an already-signalled streamer child (MW-03).
const SHUTDOWN_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);
static ACTIVE_STREAMS: AtomicUsize = AtomicUsize::new(0);
static NEXT_STREAM_DIAGNOSTIC_ID: AtomicUsize = AtomicUsize::new(1);
static STREAM_DIAGNOSTICS: OnceLock<StdMutex<HashMap<usize, ActiveStreamDiagnostic>>> =
    OnceLock::new();
/// Per `(user, host)` stream locks, held weakly so idle pairs can be reclaimed.
type StreamSessionLocks = StdMutex<HashMap<(u32, u32), StdWeak<Mutex<()>>>>;
static STREAM_SESSION_LOCKS: OnceLock<StreamSessionLocks> = OnceLock::new();

#[derive(Debug)]
struct ActiveStreamDiagnostic {
    host_id: u32,
    app_id: u32,
    transport: Option<String>,
    started_at: Instant,
}

#[derive(Debug, Serialize)]
pub(crate) struct StreamDiagnosticSnapshot {
    session_id: usize,
    host_id: u32,
    app_id: u32,
    transport: Option<String>,
    uptime_seconds: u64,
}

fn sanitize_stream_queue_sizes(video: usize, audio: usize) -> (usize, usize) {
    (
        video.clamp(MIN_VIDEO_FRAME_QUEUE_SIZE, MAX_VIDEO_FRAME_QUEUE_SIZE),
        audio.clamp(MIN_AUDIO_SAMPLE_QUEUE_SIZE, MAX_AUDIO_SAMPLE_QUEUE_SIZE),
    )
}

async fn acquire_stream_session_lock(
    user_id: u32,
    host_id: u32,
) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let locks = STREAM_SESSION_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
        let mut locks = locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);

        let key = (user_id, host_id);
        if let Some(lock) = locks.get(&key).and_then(StdWeak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };

    match lock.clone().try_lock_owned() {
        Ok(guard) => guard,
        Err(_) => {
            info!(
                "[Stream]: waiting for previous stream session to finish for user {user_id}, host {host_id}"
            );
            lock.lock_owned().await
        }
    }
}

pub(crate) fn active_stream_count() -> usize {
    ACTIVE_STREAMS.load(Ordering::Relaxed)
}

/// Result of reaping a streamer child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReapOutcome {
    /// The child exited on its own within the grace period.
    Exited,
    /// The child ignored shutdown and had to be killed.
    Killed,
}

/// Reaps the streamer child under a single overall deadline (MW-03).
///
/// Every wait is bounded: if the child does not exit within `grace`, it is killed
/// and then reaped with a bounded wait, so a wedged streamer can never block the
/// per-host stream lock or the next connection.
pub(super) async fn reap_streamer_child(
    child: &mut tokio::process::Child,
    grace: Duration,
) -> ReapOutcome {
    match timeout(grace, child.wait()).await {
        Ok(Ok(status)) => {
            debug!("[Stream]: streamer exited with {status}");
            ReapOutcome::Exited
        }
        Ok(Err(err)) => {
            warn!("[Stream]: failed waiting for streamer: {err}");
            ReapOutcome::Exited
        }
        Err(_) => {
            warn!("[Stream]: streamer did not exit in time; killing it");
            if let Err(err) = child.start_kill() {
                warn!("failed to signal streamer child: {err}");
            }
            // Bounded: a child that cannot even be reaped must not stall cleanup.
            match timeout(SHUTDOWN_KILL_REAP_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => warn!("failed waiting for killed streamer child: {err}"),
                Err(_) => warn!("[Stream]: killed streamer child was not reaped in time"),
            }
            ReapOutcome::Killed
        }
    }
}

/// Sends `Stop` to the streamer without blocking on backpressure (MW-03).
///
/// The control queue is bounded and the child may have stopped reading it. A
/// blocking `send` would hold the teardown path forever, so a full queue is
/// reported instead; the child is then killed by [`reap_streamer_child`].
pub(super) fn try_send_stop(ipc_sender: &mut IpcSender<ServerIpcMessage>) -> bool {
    ipc_sender.try_send(ServerIpcMessage::Stop)
}

pub(crate) fn stream_diagnostics() -> Vec<StreamDiagnosticSnapshot> {
    let diagnostics = STREAM_DIAGNOSTICS.get_or_init(|| StdMutex::new(HashMap::new()));
    let Ok(diagnostics) = diagnostics.lock() else {
        return Vec::new();
    };

    let now = Instant::now();
    let mut snapshots = diagnostics
        .iter()
        .map(|(session_id, stream)| StreamDiagnosticSnapshot {
            session_id: *session_id,
            host_id: stream.host_id,
            app_id: stream.app_id,
            transport: stream.transport.clone(),
            uptime_seconds: now.duration_since(stream.started_at).as_secs(),
        })
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|stream| stream.session_id);
    snapshots
}

struct StreamDiagnosticGuard {
    session_id: usize,
}

impl StreamDiagnosticGuard {
    fn new(host_id: u32, app_id: u32) -> Self {
        let session_id = NEXT_STREAM_DIAGNOSTIC_ID.fetch_add(1, Ordering::Relaxed);
        let diagnostics = STREAM_DIAGNOSTICS.get_or_init(|| StdMutex::new(HashMap::new()));
        if let Ok(mut diagnostics) = diagnostics.lock() {
            diagnostics.insert(
                session_id,
                ActiveStreamDiagnostic {
                    host_id,
                    app_id,
                    transport: None,
                    started_at: Instant::now(),
                },
            );
        }
        Self { session_id }
    }

    fn set_transport(&self, transport: &TransportType) {
        let diagnostics = STREAM_DIAGNOSTICS.get_or_init(|| StdMutex::new(HashMap::new()));
        if let Ok(mut diagnostics) = diagnostics.lock()
            && let Some(stream) = diagnostics.get_mut(&self.session_id)
        {
            stream.transport = Some(
                match transport {
                    TransportType::WebRTC => "webrtc",
                    TransportType::WebSocket => "websocket",
                }
                .to_string(),
            );
        }
    }
}

impl Drop for StreamDiagnosticGuard {
    fn drop(&mut self) {
        let diagnostics = STREAM_DIAGNOSTICS.get_or_init(|| StdMutex::new(HashMap::new()));
        if let Ok(mut diagnostics) = diagnostics.lock() {
            diagnostics.remove(&self.session_id);
        }
    }
}

struct ActiveStreamGuard;

impl ActiveStreamGuard {
    fn new() -> Self {
        ACTIVE_STREAMS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        ACTIVE_STREAMS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(super) fn websocket_origin_is_allowed(request: &HttpRequest) -> bool {
    let Some(origin) = request.headers().get(ORIGIN) else {
        // Non-browser websocket clients are allowed to omit Origin.
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(origin_uri) = origin.parse::<actix_web::http::Uri>() else {
        return false;
    };
    let Some(origin_authority) = origin_uri.authority() else {
        return false;
    };
    let Some(host) = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    origin_authority.as_str().eq_ignore_ascii_case(host)
}

#[get("/host/stream")]
#[instrument(name = "start_host", skip(web_app, user, payload), fields(user_id = %user.id()))]
pub async fn start_host(
    web_app: Data<App>,
    mut user: AuthenticatedUser,
    request: HttpRequest,
    payload: Payload,
) -> Result<HttpResponse, Error> {
    if !websocket_origin_is_allowed(&request) {
        warn!(
            "[Stream]: rejected websocket upgrade with mismatched Origin/Host: origin={:?}, host={:?}",
            request.headers().get(ORIGIN),
            request.headers().get(HOST)
        );
        return Ok(HttpResponse::Forbidden().finish());
    }

    // MW-02: keep the authorization that this upgrade was admitted with, so it can be
    // re-checked while the stream is open — and already while it is still starting.
    let request_auth = extract_user_auth(&request)?;

    // Snapshot the authorization *before* the upgrade completes. Every later phase
    // (waiting for Init, host discovery, the running stream) rechecks against this
    // snapshot, so a revocation or permission reduction reaches all three.
    let client_unique_id = user.host_unique_id().await?;
    let permissions = user.role().await?.permissions().await?;
    let authorized_permissions = permissions.clone();

    let user_id = user.id().0;

    let (response, mut session, mut stream) = actix_ws::handle(&request, payload)?;

    let web_app = web_app.clone();
    actix_rt::spawn(async move {
        // AR-11: from here on the socket is supervised as a *startup attempt*, not as a
        // stream. No maintenance read guard is held yet, so an uninitialized socket
        // can never hold a backup restore open.
        let init = match startup::await_init(
            &web_app,
            &request_auth,
            &authorized_permissions,
            &mut stream,
            startup::StartupBounds::PRODUCTION,
        )
        .await
        {
            Ok(init) => init,
            Err(err) => {
                startup::reject_startup(session, &err).await;
                return;
            }
        };

        let original_video_queue_size = init.video_frame_queue_size;
        let original_audio_queue_size = init.audio_sample_queue_size;
        let (video_frame_queue_size, audio_sample_queue_size) =
            sanitize_stream_queue_sizes(original_video_queue_size, original_audio_queue_size);
        if video_frame_queue_size != original_video_queue_size
            || audio_sample_queue_size != original_audio_queue_size
        {
            warn!(
                "[Stream]: clamped client queue sizes video={original_video_queue_size}->{video_frame_queue_size}, audio={original_audio_queue_size}->{audio_sample_queue_size}"
            );
        }

        let host_id = HostId(init.host_id);
        let app_id = AppId(init.app_id);

        // AR-11/MW-19: host discovery, the per-host stream lock and the maintenance
        // gate all happen inside one supervised window. Cancellation (deadline,
        // revocation, close/EOF) drops every acquisition made so far instead of
        // leaking a read guard into a task that never reaches the control loop.
        let started = startup::supervise_startup(
            &web_app,
            &request_auth,
            &authorized_permissions,
            &mut stream,
            startup::StartupBounds::PRODUCTION,
            async {
                let stream_scope = match web_app.enter_stream_scope().await {
                    Ok(scope) => scope,
                    Err(err) => {
                        return Err(startup::StartupError::Rejected {
                            message: "Failed to start stream because of a server error",
                            log: format!("failed to enter the stream scope for host {:?}: {err}", init.host_id),
                        });
                    }
                };

                let stream_session_guard = acquire_stream_session_lock(user_id, init.host_id).await;

                // -- Collect host data
                let mut host = match user.host(host_id).await {
                    Ok(host) => host,
                    Err(AppError::HostNotFound) => {
                        return Err(startup::StartupError::Rejected {
                            message: "Failed to start stream because the host was not found",
                            log: format!("failed to start stream for host {host_id:?} (at host): host not found"),
                        });
                    }
                    Err(err) => {
                        return Err(startup::StartupError::Rejected {
                            message: "Failed to start stream because of a server error",
                            log: format!("failed to start stream for host {host_id:?} (at host): {err}"),
                        });
                    }
                };

                let apps = match host.list_apps(&mut user).await {
                    Ok(apps) => apps,
                    Err(err) => {
                        return Err(startup::StartupError::Rejected {
                            message: "Failed to start stream because of a server error",
                            log: format!("failed to start stream for host {host_id:?} (at list_apps): {err}"),
                        });
                    }
                };

                let Some(app) = apps.into_iter().find(|app| app.id == app_id) else {
                    return Err(startup::StartupError::Rejected {
                        message: "Failed to start stream because the app was not found",
                        log: format!("failed to start stream for host {host_id:?} because the app couldn't be found!"),
                    });
                };

                let (address, http_port) = match host.address_port(&mut user).await {
                    Ok(address_port) => address_port,
                    Err(err) => {
                        return Err(startup::StartupError::Rejected {
                            message: "Failed to start stream because of a server error",
                            log: format!("failed to start stream for host {host_id:?} (at get address_port): {err}"),
                        });
                    }
                };

                let pair_info = match host.pair_info(&mut user).await {
                    Ok(pair_info) => pair_info,
                    Err(err) => {
                        return Err(startup::StartupError::Rejected {
                            message: "Failed to start stream because the host is not paired",
                            log: format!("failed to start stream for host {host_id:?} (at get pair_info): {err}"),
                        });
                    }
                };

                // -- Send App info
                let _ = send_ws_message(
                    &mut session,
                    StreamServerMessage::UpdateApp { app: app.into() },
                )
                .await;

                // -- Starting stage: launch streamer
                let _ = send_ws_message(
                    &mut session,
                    StreamServerMessage::DebugLog {
                        message: "Launching streamer".to_string(),
                        ty: None,
                        code: None,
                    },
                )
                .await;

                let (child, stdin, stdout) =
                    match spawn_streamer(&web_app, &mut session).await {
                        Ok(parts) => parts,
                        Err(err) => return Err(err),
                    };

                let init_message = ServerIpcMessage::Init {
                    config: StreamerConfig {
                        webrtc: web_app.config().webrtc.clone(),
                        log_level: web_app.config().log.level_filter,
                    },
                    host_address: address,
                    host_http_port: http_port,
                    client_unique_id: Some(client_unique_id),
                    client_private_key: pair_info.client_private_key,
                    client_certificate: pair_info.client_certificate,
                    server_certificate: pair_info.server_certificate,
                    app_id: app_id.0,
                    video_frame_queue_size,
                    audio_sample_queue_size,
                    permissions,
                };

                Ok(StartedStream {
                    _stream_scope: stream_scope,
                    _active_stream_guard: ActiveStreamGuard::new(),
                    _stream_diagnostic_guard: StreamDiagnosticGuard::new(init.host_id, init.app_id),
                    stream_session_guard,
                    child,
                    stdin,
                    stdout,
                    init: init_message,
                })
            },
        )
        .await;

        let (started, prefetched) = match started {
            Ok(started) => started,
            Err(err) => {
                startup::reject_startup(session, &err).await;
                return;
            }
        };

        let StartedStream {
            _stream_scope,
            _active_stream_guard,
            _stream_diagnostic_guard,
            stream_session_guard,
            child,
            stdin,
            stdout,
            init,
        } = started;

        run_stream_control_with(
            web_app,
            request_auth,
            authorized_permissions,
            session,
            stream,
            child,
            stdin,
            stdout,
            init,
            stream_session_guard,
            _stream_diagnostic_guard,
            prefetched.into_messages(),
        )
        .await;

        // Hold the restore gate, the active-stream bookkeeping and the diagnostics
        // until the control loop has fully reaped the streamer.
        drop((_stream_scope, _active_stream_guard));
    });

    Ok(response)
}

/// Everything the initialization phase acquired and hands to the control loop.
struct StartedStream {
    _stream_scope: tokio::sync::OwnedRwLockReadGuard<()>,
    _active_stream_guard: ActiveStreamGuard,
    _stream_diagnostic_guard: StreamDiagnosticGuard,
    stream_session_guard: tokio::sync::OwnedMutexGuard<()>,
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    init: ServerIpcMessage,
}

/// Spawns the streamer child, reporting a server error to the client on failure.
async fn spawn_streamer(
    web_app: &App,
    session: &mut Session,
) -> Result<
    (
        tokio::process::Child,
        tokio::process::ChildStdin,
        tokio::process::ChildStdout,
    ),
    startup::StartupError,
> {
    match Command::new(&web_app.config().streamer_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(mut child) => {
            if let Some(stdin) = child.stdin.take()
                && let Some(stdout) = child.stdout.take()
            {
                Ok((child, stdin, stdout))
            } else {
                error!("[Stream]: streamer process didn't include a stdin or stdout");

                let _ = send_ws_message(
                    session,
                    StreamServerMessage::DebugLog {
                        message: "Failed to start stream because of a server error".to_string(),
                        ty: Some(LogMessageType::FatalDescription),
                        code: None,
                    },
                )
                .await;

                if let Err(err) = child.kill().await {
                    warn!("[Stream]: failed to kill child: {err}");
                }

                Err(startup::StartupError::Rejected {
                    message: "Failed to start stream because of a server error",
                    log: "streamer process did not expose stdin/stdout".to_string(),
                })
            }
        }
        Err(err) => {
            error!("[Stream]: failed to spawn streamer process: {err}");

            let _ = send_ws_message(
                session,
                StreamServerMessage::DebugLog {
                    message: "Failed to start stream because of a server error".to_string(),
                    ty: Some(LogMessageType::FatalDescription),
                    code: None,
                },
            )
            .await;

            Err(startup::StartupError::Rejected {
                message: "Failed to start stream because of a server error",
                log: format!("failed to spawn streamer process: {err}"),
            })
        }
    }
}

/// Test-only adapter that keeps the pre-existing control-loop call sites (which pass
/// no prefetched frames) exercising the production orchestration unchanged.
#[cfg(test)]
async fn run_stream_control(
    web_app: Data<App>,
    request_auth: crate::app::auth::UserAuth,
    authorized_permissions: common::api_bindings::StreamPermissions,
    session: Session,
    stream: actix_ws::MessageStream,
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    init: ServerIpcMessage,
    stream_session_guard: tokio::sync::OwnedMutexGuard<()>,
    _stream_diagnostic_guard: StreamDiagnosticGuard,
) {
    run_stream_control_with(
        web_app,
        request_auth,
        authorized_permissions,
        session,
        stream,
        child,
        stdin,
        stdout,
        init,
        stream_session_guard,
        _stream_diagnostic_guard,
        Vec::new(),
    )
    .await
}

async fn run_stream_control_with(
    web_app: Data<App>,
    request_auth: crate::app::auth::UserAuth,
    authorized_permissions: common::api_bindings::StreamPermissions,
    mut session: Session,
    mut stream: actix_ws::MessageStream,
    mut child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    init: ServerIpcMessage,
    stream_session_guard: tokio::sync::OwnedMutexGuard<()>,
    _stream_diagnostic_guard: StreamDiagnosticGuard,
    prefetched: Vec<Message>,
) {
    static CHILD_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = CHILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let span = span!(Level::INFO, "ipc", child_id = id);
    let (mut ipc_sender, mut ipc_receiver) =
        create_child_ipc::<ServerIpcMessage, StreamerIpcMessage>(
            span,
            stdin,
            stdout,
            child.stderr.take(),
        )
        .await;

    // Own the host lock through cancellation and reaping, not in a detached
    // forwarder that might never observe IPC EOF.
    let _stream_session_guard = stream_session_guard;
    let last_client_activity = Arc::new(Mutex::new(Instant::now()));
    let mut heartbeat_session = session.clone();
    let heartbeat_activity = last_client_activity.clone();
    // This is the first send to a fresh, empty IPC queue. Queue Init before any
    // browser messages (or teardown Stop), without putting it in a competing
    // future that an already-buffered Close could cancel before it is polled.
    ipc_sender.send(init).await;
    {
        // Selecting a termination reason drops all other scoped futures,
        // including a pending IPC send or authorization lookup.
        let heartbeat = async move {
            let mut ticker = interval(CONTROL_HEARTBEAT_INTERVAL);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if heartbeat_activity.lock().await.elapsed() >= CONTROL_HEARTBEAT_TIMEOUT {
                    warn!("[Stream]: control WebSocket heartbeat timed out");
                    break;
                }
                if heartbeat_session.ping(b"moonlight-web").await.is_err() {
                    break;
                }
            }
        };

        let output = async {
            while let Some(message) = ipc_receiver.recv().await {
                match message {
                    StreamerIpcMessage::WebSocket(message) => {
                        if let Err(Closed) = send_ws_message(&mut session, message).await {
                            warn!(
                                "[Ipc]: Tried to send a ws message (text) but the socket is already closed"
                            );
                            break;
                        }
                    }
                    StreamerIpcMessage::WebSocketTransport(data) => {
                        if let Err(Closed) = session.binary(data).await {
                            warn!(
                                "[Ipc]: Tried to send a ws message (binary) but the socket is already closed"
                            );
                            break;
                        }
                    }
                    StreamerIpcMessage::HostAppLaunchRejected => {
                        // Queue the fatal notice before closing the control socket. The
                        // streamer waits for our normal Stop/reap path after this message.
                        let _ = send_ws_message(
                            &mut session,
                            StreamServerMessage::DebugLog {
                                message: "The host could not launch the selected app. Check it on the host, then reconnect.".into(),
                                ty: Some(LogMessageType::FatalDescription),
                                code: Some(StreamStartupError::HostAppLaunchRejected),
                            },
                        )
                        .await;
                        break;
                    }
                    StreamerIpcMessage::Stop => {
                        debug!("[Ipc]: ipc receiver stopped by streamer");
                        break;
                    }
                }
            }
            info!("[Ipc]: ipc receiver is closed");
        };

        let revalidation = async {
            let mut ticker = interval(STREAM_AUTHORIZATION_RECHECK_INTERVAL);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if revalidate_stream_authorization(&web_app, &request_auth, &authorized_permissions)
                    .await
                    .is_err()
                {
                    warn!("[Stream]: authorization for this stream was revoked; closing it");
                    break;
                }
            }
        };

        let (inbound_sender, mut inbound_receiver) =
            tokio::sync::mpsc::channel(CONTROL_INBOUND_QUEUE_CAPACITY);
        // AR-11: frames the client pipelined while startup was still running are
        // delivered first, in arrival order, so supervision never drops client input.
        for message in prefetched {
            if inbound_sender.try_send(message).is_err() {
                warn!(
                    "[Stream]: inbound control queue saturated at startup handoff; closing session"
                );
                break;
            }
            *last_client_activity.lock().await = Instant::now();
        }
        let inbound = async {
            loop {
                let Some(Ok(message)) = stream.recv().await else {
                    break;
                };
                *last_client_activity.lock().await = Instant::now();

                match message {
                    Message::Close(_) => break,
                    Message::Text(_) | Message::Binary(_) => {
                        // Never await ordinary-message capacity in the reader:
                        // Close/EOF/error must reach the supervisor even while
                        // IPC is blocked. A saturated ingress terminates the
                        // session rather than dropping input and continuing out
                        // of order or growing an unbounded queue.
                        if inbound_sender.try_send(message).is_err() {
                            warn!("[Stream]: inbound control queue saturated; closing session");
                            break;
                        }
                    }
                    Message::Ping(_)
                    | Message::Pong(_)
                    | Message::Continuation(_)
                    | Message::Nop => {}
                }
            }
        };

        let control = async {
            while let Some(message) = inbound_receiver.recv().await {
                match message {
                    Message::Text(text) => {
                        let Ok(message) = serde_json::from_str::<StreamClientMessage>(&text) else {
                            warn!("[Stream]: failed to deserialize from json");
                            break;
                        };
                        if let StreamClientMessage::SetTransport(ref transport) = message {
                            _stream_diagnostic_guard.set_transport(transport);
                        }
                        ipc_sender.send(ServerIpcMessage::WebSocket(message)).await;
                    }
                    Message::Binary(binary) => {
                        ipc_sender
                            .send(ServerIpcMessage::WebSocketTransport(binary))
                            .await;
                    }
                    _ => unreachable!("only ordinary messages enter the inbound queue"),
                }
            }
        };

        tokio::select! {
            _ = inbound => {},
            _ = control => {},
            _ = output => {},
            _ = revalidation => {},
            _ = heartbeat => {},
        }
    }

    debug!("[Stream]: control session ended; stopping streamer");
    if !try_send_stop(&mut ipc_sender) {
        warn!("[Stream]: could not deliver Stop to the streamer on teardown");
    }

    // Start reaping at the termination signal, independently of stdout EOF
    // and browser backpressure. Keep the writer alive for graceful Stop.
    let (closed, _) = tokio::join!(
        timeout(STREAMER_SHUTDOWN_TIMEOUT, session.close(None)),
        reap_streamer_child(&mut child, STREAMER_SHUTDOWN_TIMEOUT),
    );
    if let Err(err) = closed {
        warn!("[Stream]: closing streamer web socket timed out: {err}");
    }
    ipc_receiver.shutdown().await;
}

async fn send_ws_message(sender: &mut Session, message: StreamServerMessage) -> Result<(), Closed> {
    let Some(json) = serialize_json(&message) else {
        return Ok(());
    };

    sender.text(json).await
}

#[post("/host/cancel")]
pub async fn cancel_host(
    mut user: AuthenticatedUser,
    Json(request): Json<PostCancelRequest>,
) -> Result<Json<PostCancelResponse>, AppError> {
    let host_id = HostId(request.host_id);

    let mut host = user.host(host_id).await?;

    host.cancel_app(&mut user).await?;

    Ok(Json(PostCancelResponse { success: true }))
}

mod authorization;
#[cfg(test)]
mod authorization_tests;
#[cfg(test)]
mod control_tests;
#[cfg(test)]
mod shutdown_tests;
mod startup;
#[cfg(test)]
mod startup_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_queue_sizes_are_bounded() {
        assert_eq!(sanitize_stream_queue_sizes(0, 0), (1, 1));
        assert_eq!(sanitize_stream_queue_sizes(3, 20), (3, 20));
        assert_eq!(
            sanitize_stream_queue_sizes(usize::MAX, usize::MAX),
            (MAX_VIDEO_FRAME_QUEUE_SIZE, MAX_AUDIO_SAMPLE_QUEUE_SIZE)
        );
    }
}

#[cfg(test)]
mod astra_startup_tests;
