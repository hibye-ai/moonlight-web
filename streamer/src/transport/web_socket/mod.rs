use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use common::{
    api_bindings::{StreamClientMessage, StreamerStatsUpdate, TransportChannelId},
    ipc::{ServerIpcMessage, StreamerIpcMessage},
};
use log::{trace, warn};
use moonlight_common::stream::{
    audio::{AudioConfig, OpusMultistreamConfig},
    video::{DecodeResult, FrameType, VideoDecodeUnit, VideoSetup},
};
use tokio::{
    spawn,
    sync::{
        Mutex,
        mpsc::{Receiver, Sender, channel, error::TrySendError},
    },
    time::sleep,
};

use crate::{
    buffer::ByteBuffer,
    transport::{
        InboundPacket, OutboundPacket, TransportChannel, TransportError, TransportEvent,
        TransportEvents, TransportSender,
    },
};

pub async fn new(
    video_frame_queue_size: usize,
) -> Result<(WebSocketTransportSender, WebSocketTransportEvents), anyhow::Error> {
    // Keep a small amount of headroom for audio/control traffic while allowing
    // the user-selected video queue size to influence how quickly congestion
    // is detected. Video itself is sent with try_send below so stale frames
    // never wait indefinitely behind a slow browser connection.
    let event_capacity = video_frame_queue_size
        .saturating_mul(2)
        .saturating_add(8)
        .clamp(10, 64);
    let (event_sender, event_receiver) = channel::<TransportEvent>(event_capacity);

    let sender = WebSocketTransportSender {
        event_sender,
        rtt: Arc::new(Mutex::new((Instant::now(), 0))),
        needs_idr: AtomicBool::new(false),
        dropped_video_frames: AtomicUsize::new(0),
    };

    // This will start the loop of sending / receiving
    recv_rtt(sender.rtt.clone(), sender.event_sender.clone(), 0).await;

    Ok((sender, WebSocketTransportEvents { event_receiver }))
}

pub struct WebSocketTransportEvents {
    event_receiver: Receiver<TransportEvent>,
}

#[async_trait]
impl TransportEvents for WebSocketTransportEvents {
    async fn poll_event(&mut self) -> Result<TransportEvent, TransportError> {
        trace!("Polling WebSocketEvents");
        self.event_receiver
            .recv()
            .await
            .ok_or(TransportError::Closed)
    }
}

pub struct WebSocketTransportSender {
    event_sender: Sender<TransportEvent>,
    /// Time when it was sent, sequence_number
    rtt: Arc<Mutex<(Instant, u16)>>,
    needs_idr: AtomicBool,
    dropped_video_frames: AtomicUsize,
}

async fn send_packet(
    event_sender: &Sender<TransportEvent>,
    packet: OutboundPacket,
) -> Result<(), TransportError> {
    let mut new_buffer = Vec::new();

    let (id, mut range) = match packet.serialize(&mut new_buffer) {
        Some(packet) => packet,
        None => {
            warn!("Failed to serialize packet: {packet:?}");
            return Ok(());
        }
    };

    if range.start == 0 {
        new_buffer.resize(range.end - range.start + 1, 0);
        new_buffer.copy_within(range.clone(), range.start + 1);
        range.start += 1;
    }
    new_buffer[range.start - 1] = id.0;

    if event_sender
        .send(TransportEvent::SendIpc(
            StreamerIpcMessage::WebSocketTransport(Bytes::from(new_buffer)),
        ))
        .await
        .is_err()
    {
        return Err(TransportError::Closed);
    }

    Ok(())
}

async fn recv_rtt(
    rtt_mutex: Arc<Mutex<(Instant, u16)>>,
    event_sender: Sender<TransportEvent>,
    recv_sequence_number: u16,
) {
    let (send, mut sequence_number) = {
        let rtt = rtt_mutex.lock().await;
        *rtt
    };

    let now = Instant::now();
    if recv_sequence_number != sequence_number {
        warn!(
            "Expected rtt packet with sequence_number {sequence_number} but got {recv_sequence_number}"
        );
    }

    // Calc rtt
    let rtt = now - send;

    // Send rtt via stats
    if let Err(err) = send_packet(
        &event_sender,
        OutboundPacket::Stats(StreamerStatsUpdate::BrowserRtt {
            rtt_ms: rtt.as_secs_f64() * 1000.0,
        }),
    )
    .await
    {
        warn!("Failed to send rtt stats update for web socket: {err}");
    }

    // Wait a few ms
    sleep(Duration::from_millis(200)).await;

    sequence_number += 1;
    {
        let mut rtt = rtt_mutex.lock().await;
        *rtt = (Instant::now(), sequence_number);
    }

    // Send new rtt packet
    if let Err(err) = send_packet(&event_sender, OutboundPacket::Rtt { sequence_number }).await {
        warn!("Failed to send web socket rtt packet with sequence number {sequence_number}: {err}");
    }
}

/// Wire header for a HOST_VIDEO WebSocket message, before the transport layer
/// prepends the channel byte:
///
/// ```text
/// [0]     frame type   0 = delta, 1 = key
/// [1..5]  timestamp     big-endian u32, microseconds
/// ```
///
/// With the channel byte the message header is 6 bytes; the browser strips the
/// channel byte and its video depacketizer then reads exactly these 5 bytes
/// (web/stream/video/depackitize_pipe.ts) before the codec payload. There is no
/// reserved byte: adding one shifts every payload byte by one and makes the
/// browser consume payload as a timestamp.
///
/// Golden fixtures: tests/wire/video_header_golden.json
pub const VIDEO_HEADER_BYTES: usize = 5;

pub fn encode_video_header(
    frame_type: FrameType,
    timestamp: Duration,
) -> Result<[u8; VIDEO_HEADER_BYTES], TransportError> {
    // The wire field is 32 bits and the streamer's timestamp is the elapsed time
    // since the stream started, so the value is sent modulo 2^32 microseconds
    // (~71.6 min). Wrapping is deliberate: saturating at u32::MAX would freeze the
    // timestamp for every frame after the first wrap. The browser depacketizer
    // unwraps the counter back onto a JS Number timeline, which works because
    // frames are delivered in order and consecutive frames are less than 2^32 us
    // apart (a gap >= 2^32 us is indistinguishable from a whole skipped epoch).
    let micros = (timestamp.as_micros() % (1u128 << 32)) as u32;
    let mut header = [0u8; VIDEO_HEADER_BYTES];

    let mut byte_buffer = ByteBuffer::new(header.as_mut_slice());
    let written = byte_buffer.put_u8(match frame_type {
        FrameType::Idr => 1,
        FrameType::PFrame => 0,
    }) && byte_buffer.put_u32(micros);

    if !written {
        return Err(TransportError::Implementation(anyhow::anyhow!(
            "WebSocket video header does not fit into {VIDEO_HEADER_BYTES} bytes"
        )));
    }

    Ok(header)
}

#[async_trait]
impl TransportSender for WebSocketTransportSender {
    async fn setup_video(&self, _setup: VideoSetup) -> i32 {
        // empty
        0
    }
    async fn send_video_unit<'a>(
        &'a self,
        unit: VideoDecodeUnit<&'a [u8]>,
    ) -> Result<DecodeResult, TransportError> {
        let header = encode_video_header(unit.frame_type, unit.timestamp)?;

        // channel byte + 5 byte video header, then the codec payload
        let mut new_buffer = Vec::with_capacity(1 + VIDEO_HEADER_BYTES);
        new_buffer.push(TransportChannelId::HOST_VIDEO);
        new_buffer.extend_from_slice(&header);
        for buffer in &unit.buffers {
            new_buffer.extend_from_slice(buffer.data);
        }
        // TODO: ignore h264/h265 fillerdata?
        let event = TransportEvent::SendIpc(StreamerIpcMessage::WebSocketTransport(Bytes::from(
            new_buffer,
        )));
        match self.event_sender.try_send(event) {
            Ok(()) => {
                if matches!(unit.frame_type, FrameType::Idr) {
                    self.dropped_video_frames.store(0, Ordering::Relaxed);
                }
            }
            Err(TrySendError::Full(_)) => {
                let dropped = self.dropped_video_frames.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped.is_multiple_of(60) {
                    warn!(
                        "WebSocket video queue congested; dropped {dropped} frame(s) and requested an IDR"
                    );
                }
                self.needs_idr.store(true, Ordering::Release);
                return Ok(DecodeResult::NeedIdr);
            }
            Err(TrySendError::Closed(_)) => return Err(TransportError::Closed),
        }

        if self
            .needs_idr
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(DecodeResult::NeedIdr);
        }

        Ok(DecodeResult::Ok)
    }

    async fn setup_audio(
        &self,
        _audio_config: AudioConfig,
        _stream_config: OpusMultistreamConfig,
    ) -> i32 {
        // empty
        0
    }
    async fn send_audio_sample(&self, data: &[u8]) -> Result<(), TransportError> {
        let mut new_buffer = vec![0];

        let mut byte_buffer = ByteBuffer::new(new_buffer.as_mut_slice());
        byte_buffer.put_u8(TransportChannelId::HOST_AUDIO);

        new_buffer.extend_from_slice(data);

        if self
            .event_sender
            .send(TransportEvent::SendIpc(
                StreamerIpcMessage::WebSocketTransport(Bytes::from(new_buffer)),
            ))
            .await
            .is_err()
        {
            return Err(TransportError::Closed);
        }

        Ok(())
    }

    async fn send(&self, packet: OutboundPacket) -> Result<(), TransportError> {
        send_packet(&self.event_sender, packet).await
    }

    async fn on_ipc_message(&self, message: ServerIpcMessage) -> Result<(), TransportError> {
        match message {
            ServerIpcMessage::WebSocketTransport(message) => {
                if message.is_empty() {
                    warn!("Empty packet received!");
                    return Ok(());
                }

                let channel_id = message[0];

                let Some(packet) =
                    InboundPacket::deserialize(TransportChannel(channel_id), &message[1..])
                else {
                    warn!("Failed to receive packet on channel {channel_id}");
                    return Ok(());
                };

                if let InboundPacket::RequestVideoIdr = packet {
                    self.needs_idr.store(true, Ordering::Release);
                }

                if let InboundPacket::Rtt { sequence_number } = packet {
                    spawn(recv_rtt(
                        self.rtt.clone(),
                        self.event_sender.clone(),
                        sequence_number,
                    ));
                }

                if self
                    .event_sender
                    .send(TransportEvent::RecvPacket(packet))
                    .await
                    .is_err()
                {
                    return Err(TransportError::Closed);
                }
            }
            #[allow(clippy::collapsible_match)]
            ServerIpcMessage::WebSocket(StreamClientMessage::StartStream { settings }) => {
                if self
                    .event_sender
                    .send(TransportEvent::StartStream { settings })
                    .await
                    .is_err()
                {
                    warn!("Failed to send start stream event");
                    return Err(TransportError::Closed);
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_setup_complete(&self) {
        // empty
    }

    async fn close(&self) -> Result<(), TransportError> {
        // emtpy
        Ok(())
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    type GoldenRow = (
        &'static str,
        FrameType,
        u64,
        u32,
        &'static [u8],
        &'static str,
    );

    /// Mirrors tests/wire/video_header_golden.json. Keep both in sync.
    const GOLDEN: &[GoldenRow] = &[
        (
            "key_frame_at_zero",
            FrameType::Idr,
            0,
            0,
            &[0x00, 0x00, 0x00, 0x01, 0x67],
            "0201000000000000000167",
        ),
        (
            "delta_frame_small",
            FrameType::PFrame,
            16_667,
            16_667,
            &[0x00, 0x00, 0x00, 0x01, 0x41, 0xaa, 0xbb],
            "02000000411b0000000141aabb",
        ),
        (
            "key_frame_answer_byte_high_bit",
            FrameType::Idr,
            u32::MAX as u64,
            u32::MAX,
            &[0xff, 0x00, 0xff, 0x00],
            "0201ffffffffff00ff00",
        ),
        (
            "delta_frame_wrapped_clock",
            FrameType::PFrame,
            305_419_896,
            305_419_896,
            &[0xde, 0xad, 0xbe, 0xef],
            "020012345678deadbeef",
        ),
        (
            "empty_payload",
            FrameType::PFrame,
            1_000,
            1_000,
            &[],
            "0200000003e8",
        ),
    ];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn from_hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    /// Full wire frame as produced by `send_video_unit`: channel byte, then the
    /// 5 byte video header, then the payload.
    fn frame(frame_type: FrameType, timestamp: Duration, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![TransportChannelId::HOST_VIDEO];
        out.extend_from_slice(&encode_video_header(frame_type, timestamp).expect("header fits"));
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn websocket_video_header_matches_golden_fixtures() {
        for (name, frame_type, micros, _wire_ts, payload, wire_hex) in GOLDEN {
            let produced = frame(*frame_type, Duration::from_micros(*micros), payload);
            assert_eq!(hex(&produced), *wire_hex, "golden mismatch for case {name}");
        }
    }

    #[test]
    fn websocket_video_header_is_six_bytes_before_payload() {
        for (name, frame_type, micros, _wire_ts, _payload, wire_hex) in GOLDEN {
            let produced = frame(*frame_type, Duration::from_micros(*micros), &[]);
            assert_eq!(produced.len(), 1 + VIDEO_HEADER_BYTES, "case {name}");
            assert_eq!(from_hex(wire_hex)[..1 + VIDEO_HEADER_BYTES], produced[..]);
        }
    }

    /// The browser transport removes the channel byte, then the video
    /// depacketizer reads 1 type byte + 4 timestamp bytes and keeps the rest.
    #[test]
    fn websocket_video_consumer_view_is_five_header_bytes() {
        for (name, frame_type, micros, wire_ts, payload, _wire_hex) in GOLDEN {
            let produced = frame(*frame_type, Duration::from_micros(*micros), payload);
            let consumer = &produced[1..];

            assert_eq!(consumer.len(), 5 + payload.len(), "case {name}");
            assert_eq!(
                consumer[0] == 1,
                matches!(*frame_type, FrameType::Idr),
                "case {name}"
            );
            assert_eq!(
                u32::from_be_bytes([consumer[1], consumer[2], consumer[3], consumer[4]]),
                *wire_ts,
                "case {name}"
            );
            assert_eq!(&consumer[5..], *payload, "payload shifted in case {name}");
        }
    }

    /// The streamer sends elapsed microseconds modulo 2^32 instead of saturating at
    /// u32::MAX, which would freeze the timestamp after the first wrap (~71.6 min).
    #[test]
    fn websocket_video_timestamp_wraps_modulo_u32_instead_of_panicking() {
        const WRAP: u64 = 1 << 32;
        let cases: &[(u64, u32)] = &[
            (u64::from(u32::MAX) - 1, u32::MAX - 1),
            (u64::from(u32::MAX), u32::MAX),
            (u64::from(u32::MAX) + 1, 0), // == 2^32
            (u64::from(u32::MAX) + 16_667, 16_666),
            (WRAP + 16_667, 16_667),
            (2 * WRAP + 16_667, 16_667),
        ];

        for (micros, expected) in cases {
            let header = encode_video_header(FrameType::PFrame, Duration::from_micros(*micros))
                .expect("a u32 timestamp always fits the header");
            assert_eq!(
                u32::from_be_bytes([header[1], header[2], header[3], header[4]]),
                *expected,
                "wire timestamp for {micros} us"
            );
            assert_eq!(header[0], 0, "delta frame type byte for {micros} us");
        }
    }

    /// A frame exactly one wrap after the previous one is indistinguishable from a
    /// zero-length gap; both encode to the same wire value by construction.
    #[test]
    fn websocket_video_timestamp_is_periodic_with_u32_range() {
        let one_frame = Duration::from_micros(16_667);
        let wrap = Duration::from_micros(1 << 32);
        let base = encode_video_header(FrameType::Idr, one_frame).expect("header fits");
        let wrapped = encode_video_header(FrameType::Idr, one_frame + wrap).expect("header fits");

        assert_eq!(base, wrapped);
        assert_eq!(
            u32::from_be_bytes([wrapped[1], wrapped[2], wrapped[3], wrapped[4]]),
            16_667
        );
    }
}
