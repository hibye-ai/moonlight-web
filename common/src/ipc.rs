use std::marker::PhantomData;

use bytes::Bytes;
use log::LevelFilter;
use pem::Pem;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, Lines, Stdin, Stdout,
    },
    process::{ChildStderr, ChildStdin, ChildStdout},
    spawn,
    sync::mpsc::{Receiver, Sender, channel},
};
use tracing::{Span, info, trace, warn};

use crate::{
    api_bindings::{StreamClientMessage, StreamPermissions, StreamServerMessage},
    config::WebRtcConfig,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamerConfig {
    pub webrtc: WebRtcConfig,
    pub log_level: LevelFilter,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerIpcMessage {
    Init {
        config: StreamerConfig,
        host_address: String,
        host_http_port: u16,
        client_unique_id: Option<String>,
        client_private_key: Pem,
        client_certificate: Pem,
        server_certificate: Pem,
        app_id: u32,
        video_frame_queue_size: usize,
        audio_sample_queue_size: usize,
        permissions: StreamPermissions,
    },
    WebSocket(StreamClientMessage),
    WebSocketTransport(Bytes),
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum StreamerIpcMessage {
    WebSocket(StreamServerMessage),
    WebSocketTransport(Bytes),
    HostAppLaunchRejected,
    Stop,
}

/// Describes an IPC message for the trace log without serializing its payload.
///
/// MW-21: `Init` embeds the pairing private key, and the WebSocket variants carry
/// clipboard/input text and media bytes. Only the message kind, non-secret routing
/// ids and payload sizes are recorded, so enabling TRACE never writes secrets or
/// user content to disk.
pub(crate) fn log_message<M: Serialize>(label: &str, message: &M) {
    let encoded = serde_json::to_vec(message).unwrap_or_default();
    trace!(
        "[Ipc] {label}: {kind} payload_bytes={size}",
        kind = describe_value(message),
        size = encoded.len(),
    );
}

/// Best-effort, content-free description of an IPC message: the variant name plus
/// explicitly allow-listed non-secret fields.
fn describe_value<M: Serialize>(message: &M) -> String {
    let Ok(value) = serde_json::to_value(message) else {
        return "unserializable".to_string();
    };

    match value {
        serde_json::Value::String(variant) => variant,
        serde_json::Value::Object(map) => {
            // serde's externally tagged enums are `{"Variant": {...}}`.
            let Some((variant, body)) = map.iter().next() else {
                return "empty".to_string();
            };

            let mut out = variant.clone();

            if let serde_json::Value::Object(fields) = body {
                for key in ["host_address", "host_http_port", "app_id", "host_id"] {
                    if let Some(value) = fields.get(key) {
                        out.push_str(&format!(" {key}={value}"));
                    }
                }
            }

            out
        }
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod redaction_tests {
    //! MW-21: the IPC trace line must never contain secret material or user
    //! content. These tests capture the real `tracing` output produced by the
    //! production `log_message` helper used by both IPC directions.

    use super::*;
    use crate::api_bindings::StreamClientMessage;
    use crate::config::WebRtcConfig;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    const SECRET: &str = "SUPERSECRETPAIRINGKEYMATERIAL";
    const USER_CONTENT: &str = "clipboard-secret-text-1234";

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("sink lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture<M: Serialize>(message: &M) -> String {
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .with_writer(sink.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            log_message("test", message);
        });

        let bytes = sink.0.lock().expect("sink lock").clone();
        String::from_utf8_lossy(&bytes).to_string()
    }

    fn pem_with(secret: &str) -> Pem {
        Pem::new("RSA PRIVATE KEY", secret.as_bytes().to_vec())
    }

    fn init_message() -> ServerIpcMessage {
        ServerIpcMessage::Init {
            config: StreamerConfig {
                webrtc: WebRtcConfig::default(),
                log_level: log::LevelFilter::Info,
            },
            host_address: "10.0.0.5".to_string(),
            host_http_port: 47989,
            client_unique_id: Some("unique-id".to_string()),
            client_private_key: pem_with(SECRET),
            client_certificate: pem_with(SECRET),
            server_certificate: pem_with(SECRET),
            app_id: 7,
            video_frame_queue_size: 3,
            audio_sample_queue_size: 20,
            permissions: crate::api_bindings::StreamPermissions {
                allow_add_hosts: true,
                maximum_bitrate_kbps: None,
                allow_codec_h264: true,
                allow_codec_h265: true,
                allow_codec_av1: true,
                allow_hdr: true,
                allow_transport_webrtc: true,
                allow_transport_websockets: true,
            },
        }
    }

    /// The pairing private key embedded in `Init` must not reach the trace line,
    /// while the message kind and non-secret routing ids stay diagnosable.
    ///
    /// Note: `pem::Pem` serializes as the PEM text (base64 body), so the check has
    /// to look for the encoded form, not the raw sentinel bytes.
    #[test]
    fn init_trace_does_not_leak_pairing_key() {
        let message = init_message();
        let line = capture(&message);

        let private_key = match &message {
            ServerIpcMessage::Init {
                client_private_key, ..
            } => pem::encode(client_private_key),
            _ => unreachable!(),
        };
        // The standalone header is enough to prove a key was printed.
        assert!(
            private_key.contains("BEGIN RSA PRIVATE KEY"),
            "test fixture must actually contain a PEM key"
        );

        assert!(!line.is_empty(), "the trace line must still be emitted");
        assert!(
            !line.contains(&private_key) && !line.contains("BEGIN RSA PRIVATE KEY"),
            "pairing key material leaked into the IPC trace: {line}"
        );
        assert!(
            !line.contains("PRIVATE KEY"),
            "no key material may appear in the IPC trace: {line}"
        );
        assert!(
            line.contains("Init"),
            "the trace must name the message kind: {line}"
        );
        assert!(
            line.contains("10.0.0.5") && line.contains("app_id=7"),
            "non-secret routing fields must stay visible: {line}"
        );
        assert!(
            line.contains("payload_bytes"),
            "the payload size must be reported: {line}"
        );
    }

    /// Control payloads (input events, settings) must not be serialized into logs.
    #[test]
    fn websocket_control_payload_is_not_traced() {
        let message = ServerIpcMessage::WebSocket(StreamClientMessage::SetTransport(
            crate::api_bindings::TransportType::WebRTC,
        ));
        let serialized = serde_json::to_string(&message).expect("serialize");
        let line = capture(&message);

        assert!(
            !line.contains(&serialized),
            "the raw control payload must not be traced: {line}"
        );
        assert!(line.contains("WebSocket"), "kind must stay visible: {line}");
    }

    /// Binary media is described by size only.
    #[test]
    fn binary_payload_is_described_by_size_only() {
        let payload = Bytes::from_static(b"raw-media-bytes");
        let message = ServerIpcMessage::WebSocketTransport(payload.clone());
        let line = capture(&message);

        assert!(
            !line.contains("raw-media-bytes"),
            "media bytes must never be traced: {line}"
        );

        // `Bytes` serializes as a JSON int array, so the reported size is the
        // serialized length. Assert the field is present and matches exactly.
        let encoded_len = serde_json::to_vec(&message).expect("serialize").len();
        assert!(
            line.contains(&format!("payload_bytes={encoded_len}")),
            "the payload size must be reported exactly: {line}"
        );
    }

    /// Free-text log messages coming from the streamer are user-visible content and
    /// must not be echoed into our trace either.
    #[test]
    fn streamer_text_is_not_traced_verbatim() {
        let line = capture(&StreamerIpcMessage::WebSocket(
            crate::api_bindings::StreamServerMessage::DebugLog {
                message: USER_CONTENT.to_string(),
                ty: None,
                code: None,
            },
        ));

        assert!(
            !line.contains(USER_CONTENT),
            "streamer text must not be re-traced verbatim: {line}"
        );
    }
}

pub async fn create_child_ipc<Message, ChildMessage>(
    span: Span,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
) -> (IpcSender<Message>, IpcReceiver<ChildMessage>)
where
    Message: Send + Serialize + 'static,
    ChildMessage: DeserializeOwned,
{
    let mut tasks = IpcTasks::default();
    if let Some(stderr) = stderr {
        // This is the log output of the streamer
        let span = span.clone();

        tasks.0.push(spawn(async move {
            let buf_reader = BufReader::new(stderr);
            let mut lines = buf_reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                info!(parent: &span, "{line}");
            }
        }));
    }

    let (sender, receiver) = channel::<Message>(10);

    tasks.0.push(spawn({
        let span = span.clone();

        async move {
            ipc_sender(span.clone(), stdin, receiver).await;
        }
    }));

    (
        IpcSender {
            sender,
            span: span.clone(),
        },
        IpcReceiver {
            errored: false,
            read: create_lines(stdout),
            phantom: Default::default(),
            span,
            tasks,
        },
    )
}

pub async fn create_process_ipc<ParentMessage, Message>(
    span: Span,
    stdin: Stdin,
    stdout: Stdout,
) -> (IpcSender<Message>, IpcReceiver<ParentMessage>)
where
    ParentMessage: DeserializeOwned,
    Message: Send + Serialize + 'static,
{
    let (sender, receiver) = channel::<Message>(10);

    spawn({
        let span = span.clone();

        async move {
            ipc_sender(span.clone(), stdout, receiver).await;
        }
    });

    (
        IpcSender {
            sender,
            span: span.clone(),
        },
        IpcReceiver {
            errored: false,
            read: create_lines(stdin),
            phantom: Default::default(),
            span,
            tasks: IpcTasks::default(),
        },
    )
}
fn create_lines(
    read: impl AsyncRead + Send + Unpin + 'static,
) -> Lines<Box<dyn AsyncBufRead + Send + Unpin + 'static>> {
    (Box::new(BufReader::new(read)) as Box<dyn AsyncBufRead + Send + Unpin + 'static>).lines()
}

async fn ipc_sender<Message>(
    span: Span,
    mut write: impl AsyncWriteExt + Unpin,
    mut receiver: Receiver<Message>,
) where
    Message: Serialize,
{
    while let Some(value) = receiver.recv().await {
        // MW-21: never trace the serialized payload; it can contain the pairing key,
        // clipboard/input text or media bytes.
        log_message("sending", &value);

        let mut json = match serde_json::to_string(&value) {
            Ok(value) => value,
            Err(err) => {
                warn!(parent: &span,"[Ipc]: failed to encode message: {err:?}");
                continue;
            }
        };

        json.push('\n');

        if let Err(err) = write.write_all(json.as_bytes()).await {
            warn!(parent: &span, "failed to write message length: {err:?}");
            return;
        };

        if let Err(err) = write.flush().await {
            warn!(parent: &span, "failed to flush: {err:?}");
            return;
        }
    }
}

#[derive(Debug)]
pub struct IpcSender<Message> {
    sender: Sender<Message>,
    span: Span,
}

impl<Message> Clone for IpcSender<Message> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            span: self.span.clone(),
        }
    }
}

impl<Message> IpcSender<Message>
where
    Message: Serialize + Send + 'static,
{
    pub async fn send(&mut self, message: Message) {
        if self.sender.send(message).await.is_err() {
            warn!(parent: &self.span, "failed to send message");
        }
    }
    /// Non-blocking send. Returns `false` when the bounded queue is full or the
    /// writer is gone, so teardown never waits on a stalled child (MW-03).
    pub fn try_send(&mut self, message: Message) -> bool {
        match self.sender.try_send(message) {
            Ok(()) => true,
            Err(err) => {
                warn!(parent: &self.span, "failed to send message without blocking: {err}");
                false
            }
        }
    }
    pub fn blocking_send(&mut self, message: Message) {
        if self.sender.blocking_send(message).is_err() {
            warn!(parent: &self.span, "failed to send message");
        }
    }
}

/// Child-pipe workers belong to the receiver, not to detached runtime tasks.
#[derive(Default)]
struct IpcTasks(Vec<tokio::task::JoinHandle<()>>);

impl IpcTasks {
    async fn shutdown(&mut self) {
        for task in &self.0 {
            task.abort();
        }
        for task in self.0.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for IpcTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

pub struct IpcReceiver<Message> {
    errored: bool,
    read: Lines<Box<dyn AsyncBufRead + Send + Unpin>>,
    phantom: PhantomData<Message>,
    span: Span,
    tasks: IpcTasks,
}

impl<Message> IpcReceiver<Message>
where
    Message: DeserializeOwned,
{
    /// Cancel and join this child's IPC workers after graceful shutdown/reaping.
    /// Keep the writer alive until this call so queued Stop can still drain.
    pub async fn shutdown(&mut self) {
        self.tasks.shutdown().await;
    }

    pub async fn recv(&mut self) -> Option<Message> {
        if self.errored {
            return None;
        }

        let line = match self.read.next_line().await {
            Ok(Some(value)) => value,
            Ok(None) => return None,
            Err(err) => {
                self.errored = true;

                warn!(parent: &self.span, "failed to read next line {err:?}");

                return None;
            }
        };

        match serde_json::from_str::<Message>(&line) {
            Ok(value) => {
                // MW-21: the receiver is only bound to `DeserializeOwned`, so it
                // cannot inspect the message kind. Record the fact that a message
                // arrived plus its size, and never the raw line, which may contain
                // secrets or user content.
                trace!("[Ipc] received message payload_bytes={}", line.len());
                Some(value)
            }
            Err(err) => {
                // Do not echo the malformed line: it is still untrusted content.
                warn!(parent: &self.span, "failed to deserialize message: {err:?}");

                None
            }
        }
    }
}

#[cfg(test)]
mod task_lifetime_tests {
    use super::*;
    use std::{future::Future, process::Stdio, task::Poll};

    fn exercise_workers(drop_receiver: bool) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture runtime")
            .block_on(async {
                let mut child = tokio::process::Command::new("sleep")
                    .arg("30")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .expect("fixture child");
                let (mut sender, mut receiver) = create_child_ipc::<String, StreamerIpcMessage>(
                    Span::none(),
                    child.stdin.take().expect("stdin"),
                    child.stdout.take().expect("stdout"),
                    child.stderr.take(),
                )
                .await;
                let workers = receiver
                    .tasks
                    .0
                    .iter()
                    .map(|t| t.abort_handle())
                    .collect::<Vec<_>>();
                assert_eq!(workers.len(), 2, "writer and stderr reader are owned");
                // The first message exceeds pipe capacity. Let the real writer
                // reach write_all, then fill the ten-message queue behind it.
                sender.send("x".repeat(2 * 1024 * 1024)).await;
                tokio::task::yield_now().await;
                for _ in 0..10 {
                    sender.send("queued".into()).await;
                }
                {
                    let send = sender.send("pending".into());
                    let mut send = std::pin::pin!(send);
                    std::future::poll_fn(|cx| {
                        assert!(
                            send.as_mut().poll(cx).is_pending(),
                            "actual IPC send must be blocked"
                        );
                        Poll::Ready(())
                    })
                    .await;
                }
                assert!(workers.iter().all(|t| !t.is_finished()));
                if drop_receiver {
                    drop(receiver);
                    for _ in 0..20 {
                        tokio::task::yield_now().await;
                    }
                } else {
                    receiver.shutdown().await;
                    assert!(receiver.tasks.0.is_empty(), "all JoinHandles were joined");
                }
                assert!(
                    workers.iter().all(|t| t.is_finished()),
                    "no detached pipe tasks"
                );
                assert!(
                    sender.sender.is_closed(),
                    "blocked writer released the control queue"
                );
                assert!(
                    child.try_wait().expect("child state").is_none(),
                    "worker cancellation must not depend on child exit/EOF"
                );
                child.kill().await.expect("kill and reap fixture child");
            });
    }

    #[test]
    fn child_ipc_shutdown_joins_workers_with_full_pipe() {
        exercise_workers(false);
    }

    #[test]
    fn dropping_child_ipc_aborts_workers_with_full_pipe() {
        exercise_workers(true);
    }
}
