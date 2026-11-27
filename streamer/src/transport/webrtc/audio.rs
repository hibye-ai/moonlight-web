use std::{sync::Weak, time::Duration};

use bytes::Bytes;
use log::{error, info, warn};
use moonlight_common::stream::audio::{AudioConfig, OpusMultistreamConfig};
use std::time::Instant;
use tokio::runtime::Handle;
use webrtc::{
    api::media_engine::{MIME_TYPE_OPUS, MediaEngine},
    media::Sample,
    peer_connection::RTCPeerConnection,
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

use crate::transport::webrtc::{WebRtcInner, sender::TrackLocalSender};

/// How often the content-free audio send metrics are summarised in the log.
const METRICS_LOG_INTERVAL: Duration = Duration::from_secs(10);

pub fn register_audio_codecs(media_engine: &mut MediaEngine) -> Result<(), webrtc::Error> {
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    Ok(())
}

pub struct WebRtcAudio {
    sender: TrackLocalSender<TrackLocalStaticSample>,
    config: Option<OpusMultistreamConfig>,
    metrics_logged_at: Option<Instant>,
    last_logged_written: u64,
    last_logged_rejected: u64,
}

impl WebRtcAudio {
    pub fn new(runtime: Handle, peer: Weak<RTCPeerConnection>, channel_queue_size: usize) -> Self {
        Self {
            sender: TrackLocalSender::new(runtime, peer, channel_queue_size),
            config: None,
            metrics_logged_at: None,
            last_logged_written: 0,
            last_logged_rejected: 0,
        }
    }

    /// Emit a bounded, content-free delta summary at most once per interval.
    ///
    /// This only observes; it does not change the queue size, the packetizer,
    /// the RTP timestamps or the playout-delay extension. Choosing a buffer or
    /// a drop policy without the numbers this logs would be an unmeasured
    /// change, so the default behaviour stays as it is.
    fn maybe_log_metrics(&mut self) {
        let now = Instant::now();
        let due = match self.metrics_logged_at {
            None => true,
            Some(last) => now.duration_since(last) >= METRICS_LOG_INTERVAL,
        };
        if !due {
            return;
        }

        let metrics = self.sender.metrics();
        let written = metrics.written_samples();
        let rejected = metrics.rejected_samples();
        let written_delta = written.saturating_sub(self.last_logged_written);
        let rejected_delta = rejected.saturating_sub(self.last_logged_rejected);

        info!(
            "[Stream] audio sender (content-free): written_delta={} rejected_delta={} {}",
            written_delta,
            rejected_delta,
            metrics.summary()
        );

        self.metrics_logged_at = Some(now);
        self.last_logged_written = written;
        self.last_logged_rejected = rejected;
    }
}

impl WebRtcAudio {
    pub async fn setup(
        &mut self,
        _inner: &WebRtcInner,
        audio_config: AudioConfig,
        stream_config: OpusMultistreamConfig,
    ) -> i32 {
        const SUPPORTED_SAMPLE_RATES: &[u32] = &[80000, 12000, 16000, 24000, 48000];
        if !SUPPORTED_SAMPLE_RATES.contains(&stream_config.sample_rate) {
            warn!(
                "[Stream] Audio could have problems because of the sample rate, Selected: {}, Expected one of: {SUPPORTED_SAMPLE_RATES:?}",
                stream_config.sample_rate
            );
        }
        if audio_config != self.config() {
            warn!(
                "[Stream] A different audio configuration than requested was selected, Expected: {:?}, Found: {audio_config:?}",
                self.config()
            );
        }

        if let Err(err) = self
            .sender
            .create_track(
                TrackLocalStaticSample::new(
                    RTCRtpCodecCapability {
                        mime_type: MIME_TYPE_OPUS.to_string(),
                        ..Default::default()
                    },
                    "audio".to_string(),
                    "moonlight".to_string(),
                ),
                |_| {},
            )
            .await
        {
            error!("Failed to create opus track: {err:?}");
            return -1;
        };

        self.config = Some(stream_config);

        0
    }

    pub async fn send_audio_sample(&mut self, data: &[u8]) {
        let Some(config) = self.config.as_ref() else {
            return;
        };

        // The packetizer derives the RTP clock from this fixed sample duration,
        // so the Opus frame duration handed to it must stay the configured one.
        // `data` is the encoded Opus packet; its byte length is not proportional
        // to the frame duration, so it is not validated here.
        let duration =
            Duration::from_secs_f64(config.samples_per_frame as f64 / config.sample_rate as f64);

        let data = Bytes::copy_from_slice(data);

        let sample = Sample {
            data,
            duration,
            // Time should be set if you want fine-grained sync
            ..Default::default()
        };

        // A rejected sample (queue full) is counted inside the sender; the
        // caller deliberately does not block the host audio callback on it.
        let _ = self.sender.send_samples(vec![sample], false).await;

        self.maybe_log_metrics();
    }

    fn config(&self) -> AudioConfig {
        AudioConfig::STEREO
    }

    /// Stop the audio sample sender and RTCP reader tasks.
    pub async fn shutdown(&mut self) {
        self.sender.shutdown().await;
    }
}
