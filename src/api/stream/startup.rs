//! Bounded startup supervision for `/host/stream` (AR-11).
//!
//! A WebSocket upgrade used to enter the stream *scope* — the read side of the
//! maintenance gate — in the request handler, before the socket had sent anything.
//! The first `recv()` waiting for `Init` then had no deadline and no authorization
//! recheck, so a client could upgrade, keep the socket open (or keep pinging), let
//! its session be revoked, and never send `Init`. The task still held the read guard,
//! and every backup restore waited behind a stream that had never started.
//!
//! This module puts both startup phases under one supervised lifecycle:
//!
//! - waiting for `Init` is bounded by [`STREAM_INIT_DEADLINE`], revalidates the
//!   authorization the upgrade was admitted with every
//!   [`STREAM_AUTHORIZATION_RECHECK_INTERVAL`], and ends on close/EOF/protocol error;
//! - host discovery, the stream lock and the restore gate are bounded by
//!   [`STREAM_STARTUP_DEADLINE`] and cancelled by the same signals;
//! - ordinary frames that arrive during initialization are retained in arrival order
//!   and handed to the control loop, so supervision does not drop client input.
//!
//! The maintenance gate is acquired only after `Init` has been accepted, which is
//! what keeps an uninitialized socket from ever blocking a restore. A stream that
//! *did* initialize still holds the gate for its whole lifetime, exactly as before.
use std::time::Duration;

use common::api_bindings::{LogMessageType, StreamPermissions, StreamServerMessage};
use log::{debug, warn};

use super::*;
use authorization::{STREAM_AUTHORIZATION_RECHECK_INTERVAL, revalidate_stream_authorization};

/// How long a freshly upgraded socket may take to deliver its first `Init`.
///
/// Bounded on purpose: without a deadline, an uninitialized socket is an
/// unauthenticated hold on a reactor task and on everything the startup path takes.
pub(super) const STREAM_INIT_DEADLINE: Duration = Duration::from_secs(10);

/// How long host discovery, the stream lock and the restore gate may take.
///
/// This covers waits the server does not control (a restore in progress, a previous
/// stream on the same host still shutting down), so it is longer than the `Init`
/// window — but still bounded, and cancellable by close/EOF/revocation.
pub(super) const STREAM_STARTUP_DEADLINE: Duration = Duration::from_secs(30);

/// Upper bound on delivering a startup rejection to a client that may not read.
const STARTUP_REJECTION_TIMEOUT: Duration = Duration::from_secs(2);

/// The bounds and recheck cadence a startup phase runs under.
///
/// Passed in rather than read from the constants so tests can exercise the same
/// production code paths with millisecond windows instead of waiting out the real
/// ten- and thirty-second limits.
#[derive(Debug, Clone, Copy)]
pub(super) struct StartupBounds {
    pub(super) init: Duration,
    pub(super) startup: Duration,
    pub(super) recheck: Duration,
}

impl StartupBounds {
    pub(super) const PRODUCTION: Self = Self {
        init: STREAM_INIT_DEADLINE,
        startup: STREAM_STARTUP_DEADLINE,
        recheck: STREAM_AUTHORIZATION_RECHECK_INTERVAL,
    };
}

/// Why a startup phase ended before a stream could run.
#[derive(Debug)]
pub(super) enum StartupError {
    /// A lookup or lock acquisition failed; the client is told why.
    Rejected { message: &'static str, log: String },
    /// The phase exceeded its bounded window.
    Deadline(&'static str),
    /// The upgrade authorization no longer holds.
    Revoked,
    /// The client closed, the payload ended, or the peer violated the protocol.
    Closed(&'static str),
    /// The client pipelined more control frames than the control queue admits.
    Overloaded,
}

impl StartupError {
    /// The client-facing explanation, matching the pre-existing stream errors.
    fn client_message(&self) -> &'static str {
        match self {
            Self::Rejected { message, .. } => message,
            Self::Deadline(_) => {
                "Failed to start stream because the server took too long to initialize it"
            }
            Self::Revoked => "Failed to start stream because the authorization was revoked",
            Self::Closed(_) => "Failed to start stream because the connection was closed",
            Self::Overloaded => {
                "Failed to start stream because too many messages arrived before initialization finished"
            }
        }
    }
}

/// The `Init` fields, already extracted from the first client message.
#[derive(Debug, Clone, Copy)]
pub(super) struct InitRequest {
    pub(super) host_id: u32,
    pub(super) app_id: u32,
    pub(super) video_frame_queue_size: usize,
    pub(super) audio_sample_queue_size: usize,
}

/// Ordinary frames read while startup was still running.
///
/// Handed to [`run_stream_control`] so a client that pipelined control messages
/// behind `Init` keeps them. Pings and other control frames are not replayed: they
/// were already handled by the websocket layer.
#[derive(Debug, Default)]
pub(super) struct Prefetched(Vec<Message>);

impl Prefetched {
    fn push(&mut self, message: Message) -> Result<(), StartupError> {
        match message {
            Message::Close(_) => Err(StartupError::Closed("client closed during startup")),
            Message::Text(_) | Message::Binary(_) => {
                if self.0.len() >= CONTROL_INBOUND_QUEUE_CAPACITY {
                    // Same limit as the running stream's inbound queue: a peer that
                    // outruns the control loop is closed instead of buffered forever.
                    return Err(StartupError::Overloaded);
                }
                self.0.push(message);
                Ok(())
            }
            // Ping/Pong/Continuation/Nop carry no client input of their own.
            Message::Ping(_) | Message::Pong(_) | Message::Continuation(_) | Message::Nop => Ok(()),
        }
    }

    pub(super) fn into_messages(self) -> Vec<Message> {
        self.0
    }
}

/// Tells the client why startup failed and closes the socket, bounded.
///
/// Every wait here is on a peer that may already be gone or not reading, so the
/// notification and the close share one short deadline; a startup rejection must
/// never itself become an unbounded hold. `Session::close` consumes the session, so
/// this takes ownership — the caller has no stream left to keep.
pub(super) async fn reject_startup(session: actix_ws::Session, err: &StartupError) {
    match err {
        StartupError::Closed(reason) => {
            debug!("[Stream]: startup ended because {reason}");
        }
        StartupError::Rejected { message, log } => {
            warn!("[Stream]: refusing stream startup: {log}");
            notify_and_close(session, message).await;
        }
        StartupError::Deadline(reason) => {
            warn!("[Stream]: refusing stream startup: {reason}");
            notify_and_close(session, err.client_message()).await;
        }
        StartupError::Revoked => {
            warn!("[Stream]: refusing stream startup: authorization was revoked");
            notify_and_close(session, err.client_message()).await;
        }
        StartupError::Overloaded => {
            warn!("[Stream]: refusing stream startup: client pipelined too many frames");
            notify_and_close(session, err.client_message()).await;
        }
    }
}

/// Sends the fatal description then closes, under one bounded wait.
async fn notify_and_close(mut session: actix_ws::Session, message: &'static str) {
    let notify = async {
        let _ = session
            .text(
                serde_json::to_string(&StreamServerMessage::DebugLog {
                    message: message.to_string(),
                    ty: Some(LogMessageType::FatalDescription),
                    code: None,
                })
                .unwrap_or_default(),
            )
            .await;
        let _ = session.close(None).await;
    };

    if timeout(STARTUP_REJECTION_TIMEOUT, notify).await.is_err() {
        debug!("[Stream]: startup rejection could not be delivered before the deadline");
    }
}

/// Waits for the first client message, under a deadline and the stream's own
/// authorization lifetime.
///
/// Returns only after a well-formed `Init`. Anything else — a binary first frame, a
/// non-`Init` JSON message, a malformed payload, EOF, or a revoked authorization —
/// is a terminal startup failure. Ping/Pong/Nop frames are consumed but never extend
/// the deadline, which is what makes a permanently pinging client bounded.
pub(super) async fn await_init(
    web_app: &App,
    request_auth: &crate::app::auth::UserAuth,
    authorized_permissions: &StreamPermissions,
    stream: &mut actix_ws::MessageStream,
    bounds: StartupBounds,
) -> Result<InitRequest, StartupError> {
    let deadline = tokio::time::sleep(bounds.init);
    tokio::pin!(deadline);

    let mut revalidation = interval(bounds.recheck);
    // `interval` yields immediately; the first real check is one period from now.
    revalidation.tick().await;

    loop {
        tokio::select! {
            _ = &mut deadline => {
                return Err(StartupError::Deadline("websocket did not send Init in time"));
            }
            _ = revalidation.tick() => {
                if revalidate_stream_authorization(web_app, request_auth, authorized_permissions)
                    .await
                    .is_err()
                {
                    return Err(StartupError::Revoked);
                }
            }
            message = stream.recv() => match message {
                Some(Ok(Message::Text(text))) => {
                    let Ok(message) = serde_json::from_str::<StreamClientMessage>(&text) else {
                        return Err(StartupError::Closed("first message was not valid JSON"));
                    };
                    return match message {
                        StreamClientMessage::Init { host_id, app_id, video_frame_queue_size, audio_sample_queue_size } => {
                            Ok(InitRequest { host_id, app_id, video_frame_queue_size, audio_sample_queue_size })
                        }
                        _ => Err(StartupError::Closed("websocket did not send Init as its first message")),
                    };
                }
                Some(Ok(Message::Binary(_))) => {
                    return Err(StartupError::Closed("websocket sent a binary frame before Init"));
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) => return Err(StartupError::Closed("websocket protocol error before Init")),
                None => return Err(StartupError::Closed("websocket closed before Init")),
            }
        }
    }
}

/// Runs the host-initialization phase under the same supervision as `await_init`.
///
/// `work` owns everything the phase acquires (the maintenance read guard, the host
/// stream lock, the active-stream bookkeeping), so cancelling — deadline, revocation,
/// or a closed socket — drops those acquisitions instead of leaking them into the
/// control loop. On success the acquired state is returned to the caller, which keeps
/// holding it for the stream's lifetime exactly as before.
pub(super) async fn supervise_startup<F, T>(
    web_app: &App,
    request_auth: &crate::app::auth::UserAuth,
    authorized_permissions: &StreamPermissions,
    stream: &mut actix_ws::MessageStream,
    bounds: StartupBounds,
    work: F,
) -> Result<(T, Prefetched), StartupError>
where
    F: Future<Output = Result<T, StartupError>>,
{
    tokio::pin!(work);

    let deadline = tokio::time::sleep(bounds.startup);
    tokio::pin!(deadline);

    let mut revalidation = interval(bounds.recheck);
    revalidation.tick().await;

    let mut prefetched = Prefetched::default();

    loop {
        tokio::select! {
            result = &mut work => return result.map(|state| (state, prefetched)),
            _ = &mut deadline => {
                return Err(StartupError::Deadline("stream initialization did not finish in time"));
            }
            _ = revalidation.tick() => {
                if revalidate_stream_authorization(web_app, request_auth, authorized_permissions)
                    .await
                    .is_err()
                {
                    return Err(StartupError::Revoked);
                }
            }
            message = stream.recv() => match message {
                Some(Ok(message)) => prefetched.push(message)?,
                Some(Err(_)) => return Err(StartupError::Closed("websocket protocol error during startup")),
                None => return Err(StartupError::Closed("websocket closed during startup")),
            }
        }
    }
}
