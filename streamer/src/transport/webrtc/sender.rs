use std::{
    collections::VecDeque,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use log::{debug, warn};
use tokio::{
    runtime::Handle,
    sync::{Mutex, Notify},
    task::JoinHandle,
};
use webrtc::{
    api::media_engine::MediaEngine,
    media::Sample,
    peer_connection::RTCPeerConnection,
    rtcp::packet::Packet,
    rtp::{
        self,
        extension::{
            HeaderExtension, abs_send_time_extension::AbsSendTimeExtension,
            playout_delay_extension::PlayoutDelayExtension,
        },
    },
    rtp_transceiver::rtp_codec::{RTCRtpHeaderExtensionCapability, RTPCodecType},
    sdp::extmap::ABS_SEND_TIME_URI,
    track::track_local::{
        TrackLocal, track_local_static_rtp::TrackLocalStaticRTP,
        track_local_static_sample::TrackLocalStaticSample,
    },
};

const PLAYOUT_DELAY_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";
use super::pacing::{VideoPacing, offset};

pub fn epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn register_header_extensions(api_media: &mut MediaEngine) -> Result<(), webrtc::Error> {
    api_media.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: PLAYOUT_DELAY_URI.to_string(),
        },
        RTPCodecType::Video,
        None,
    )?;
    api_media.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: PLAYOUT_DELAY_URI.to_string(),
        },
        RTPCodecType::Audio,
        None,
    )?;

    api_media.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: ABS_SEND_TIME_URI.to_string(),
        },
        RTPCodecType::Video,
        None,
    )?;
    api_media.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: ABS_SEND_TIME_URI.to_string(),
        },
        RTPCodecType::Audio,
        None,
    )?;

    Ok(())
}

/// Content-free send-path counters.
///
/// Nothing here contains media, clipboard text, tokens or peer identifiers, so
/// the values are safe to log. They exist so the queue/clock question ("is the
/// sender dropping audio, and by how much?") can be answered by measurement
/// instead of by picking a different buffer size (MW-10).
#[derive(Debug, Default)]
pub struct SenderMetrics {
    queued_samples: AtomicU64,
    rejected_samples: AtomicU64,
    written_samples: AtomicU64,
    write_errors: AtomicU64,
    queue_depth: AtomicU64,
    max_queue_depth: AtomicU64,
    write_stall_micros: AtomicU64,
    max_write_stall_micros: AtomicU64,
}

impl SenderMetrics {
    #[cfg(test)]
    pub fn queued_samples(&self) -> u64 {
        self.queued_samples.load(Ordering::Relaxed)
    }
    pub fn rejected_samples(&self) -> u64 {
        self.rejected_samples.load(Ordering::Relaxed)
    }
    pub fn written_samples(&self) -> u64 {
        self.written_samples.load(Ordering::Relaxed)
    }
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
    }
    pub fn queue_depth(&self) -> u64 {
        self.queue_depth.load(Ordering::Relaxed)
    }
    pub fn max_queue_depth(&self) -> u64 {
        self.max_queue_depth.load(Ordering::Relaxed)
    }
    pub fn write_stall_micros(&self) -> u64 {
        self.write_stall_micros.load(Ordering::Relaxed)
    }
    pub fn max_write_stall_micros(&self) -> u64 {
        self.max_write_stall_micros.load(Ordering::Relaxed)
    }

    fn observe_depth(&self, depth: u64) {
        self.queue_depth.store(depth, Ordering::Relaxed);
        self.max_queue_depth.fetch_max(depth, Ordering::Relaxed);
    }

    fn observe_write(&self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.write_stall_micros.fetch_add(micros, Ordering::Relaxed);
        self.max_write_stall_micros
            .fetch_max(micros, Ordering::Relaxed);
    }

    /// One content-free summary line, emitted by the streamer on a timer.
    pub fn summary(&self) -> String {
        format!(
            "audio_sender queue_depth={} max_queue_depth={} rejected_samples={} written_samples={} write_errors={} write_stall_ms={} max_write_stall_ms={}",
            self.queue_depth(),
            self.max_queue_depth(),
            self.rejected_samples(),
            self.written_samples(),
            self.write_errors(),
            self.write_stall_micros() / 1000,
            self.max_write_stall_micros() / 1000,
        )
    }
}

pub struct TrackLocalSender<Track>
where
    Track: TrackLike,
{
    runtime: Handle,
    peer: Weak<RTCPeerConnection>,
    channel_queue_size: usize,
    new_samples_notify: Arc<Notify>,
    queue: Arc<Mutex<VecDeque<FrameSamples<Track>>>>,
    metrics: Arc<SenderMetrics>,
    sender_task: Option<JoinHandle<()>>,
    rtcp_task: Option<JoinHandle<()>>,
}

struct FrameSamples<Track>
where
    Track: TrackLike,
{
    important: bool,
    samples: Vec<Track::Sample>,
}

impl<Track> TrackLocalSender<Track>
where
    Track: TrackLike,
{
    pub fn new(runtime: Handle, peer: Weak<RTCPeerConnection>, channel_queue_size: usize) -> Self {
        Self {
            runtime,
            peer,
            channel_queue_size,
            new_samples_notify: Default::default(),
            queue: Default::default(),
            metrics: Default::default(),
            sender_task: None,
            rtcp_task: None,
        }
    }

    pub fn metrics(&self) -> Arc<SenderMetrics> {
        self.metrics.clone()
    }

    pub async fn create_track(
        &mut self,
        track: Track,
        mut on_packet: impl FnMut(Box<dyn Packet + Send + Sync>) + Send + 'static,
    ) -> Result<(), anyhow::Error> {
        let Some(peer) = self.peer.upgrade() else {
            return Err(anyhow!(
                "Failed to create track because of missing webrtc peer!"
            ));
        };

        let track = Arc::new(track);

        let new_samples_notify = self.new_samples_notify.clone();
        let queue = Arc::downgrade(&self.queue);
        let metrics = self.metrics.clone();
        // Own the task handle: an idle or orphaned sender must be cancellable
        // and joinable instead of leaking until the whole runtime shuts down.
        self.sender_task = Some(self.runtime.spawn({
            let track = track.clone();
            async move {
                sample_sender(track, &new_samples_notify, queue, &metrics).await;
            }
        }));

        let track_sender = peer.add_track(track.track()).await?;

        // Read incoming RTCP packets
        // Before these packets are returned they are processed by interceptors. For things
        // like NACK this needs to be called.
        self.rtcp_task = Some(self.runtime.spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            while let Ok((packets, _)) = track_sender.read(&mut rtcp_buf).await {
                for packet in packets {
                    on_packet(packet);
                }
            }
        }));

        Ok(())
    }

    /// Stop both spawned tasks and wait for them to be reaped.
    ///
    /// The consumer task also terminates on its own once the queue allocation is
    /// gone; this makes the "idle but still alive" case explicit and bounded.
    pub async fn shutdown(&mut self) {
        if let Some(task) = self.sender_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.rtcp_task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    /// Returns if the frame will be delivered
    pub async fn send_samples(&self, samples: Vec<Track::Sample>, important: bool) -> bool {
        let mut queue = self.queue.lock().await;

        let result = if important {
            queue.push_front(FrameSamples { important, samples });
            true
        } else if queue.len() >= self.channel_queue_size {
            self.metrics
                .rejected_samples
                .fetch_add(1, Ordering::Relaxed);
            false
        } else {
            queue.push_front(FrameSamples { important, samples });
            true
        };

        if result {
            self.metrics.queued_samples.fetch_add(1, Ordering::Relaxed);
            self.metrics.observe_depth(queue.len() as u64);
        }

        // `notify_one` stores a permit when no waiter is registered, so a
        // sample enqueued in the window between the consumer's empty-queue check
        // and its `notified().await` is not lost (`notify_waiters` would be).
        self.new_samples_notify.notify_one();

        result
    }

    /// Returns if the frame will be delivered
    pub async fn clear_queue(&self, clear_important: bool) {
        let mut queue = self.queue.lock().await;

        if clear_important {
            queue.clear();
        } else {
            queue.retain(|frame| frame.important);
        }
        self.metrics.observe_depth(queue.len() as u64);
    }
}

async fn sample_sender<Track>(
    track: Arc<Track>,
    new_samples_notify: &Notify,
    queue: Weak<Mutex<VecDeque<FrameSamples<Track>>>>,
    metrics: &SenderMetrics,
) where
    Track: TrackLike,
{
    loop {
        // Register interest BEFORE inspecting the queue. Combined with
        // `notify_one`'s stored permit this removes the lost-wakeup window on
        // both sides.
        let notified = new_samples_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let frame = {
            let Some(queue) = queue.upgrade() else {
                // The owning sender is gone. Stop instead of spinning on an
                // await-less `continue` (which was a busy loop).
                debug!("no sample queue available: stopping to submit samples");
                break;
            };

            let mut queue = queue.lock().await;
            let Some(new_frame) = queue.pop_back() else {
                drop(queue); // Important: drop the mutex
                metrics.observe_depth(0);
                notified.await;
                continue;
            };

            metrics.observe_depth(queue.len() as u64);
            new_frame
        };

        let window = track.pacing_window(&frame.samples);
        let total_bytes: usize = frame.samples.iter().map(|s| track.sample_bytes(s)).sum();
        let started = tokio::time::Instant::now();
        let mut bytes_before = 0;
        for sample in frame.samples {
            let due = started + offset(window, bytes_before, total_bytes);
            // Group sub-millisecond deadlines. Never sleep for each packet,
            // nor catch up by scheduling an entire late frame for the future.
            if due.saturating_duration_since(tokio::time::Instant::now()) >= Duration::from_millis(1) {
                tokio::time::sleep_until(due).await;
            }
            bytes_before += track.sample_bytes(&sample);
            // Send-time is per packet, after pacing, rather than frame dequeue.
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
            let abs_send_time = ((now.as_secs_f64() * 262_144.0) as u64) & 0x00ff_ffff;
            let write_started = Instant::now();
            if let Err(err) = track
                .write_with_extensions(
                    sample,
                    &[
                        HeaderExtension::PlayoutDelay(PlayoutDelayExtension::new(0, 0)),
                        HeaderExtension::AbsSendTime(AbsSendTimeExtension {
                            timestamp: abs_send_time,
                        }),
                    ],
                )
                .await
            {
                metrics.write_errors.fetch_add(1, Ordering::Relaxed);
                warn!("[Stream]: track.write_sample failed: {err}");
            } else {
                metrics.written_samples.fetch_add(1, Ordering::Relaxed);
            }
            metrics.observe_write(write_started.elapsed());
        }
    }
}

pub trait TrackLike: Send + Sync + 'static {
    type Sample: Send + 'static;

    fn pacing_window(&self, _samples: &[Self::Sample]) -> Duration { Duration::ZERO }
    fn sample_bytes(&self, _sample: &Self::Sample) -> usize { 1 }

    fn write_with_extensions(
        &self,
        sample: Self::Sample,
        extensions: &[HeaderExtension],
    ) -> impl Future<Output = Result<(), anyhow::Error>> + Send;

    fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static>;
}

impl TrackLike for TrackLocalStaticSample {
    type Sample = Sample;

    async fn write_with_extensions(
        &self,
        sample: Self::Sample,
        extensions: &[HeaderExtension],
    ) -> Result<(), anyhow::Error> {
        self.write_sample_with_extensions(&sample, extensions)
            .await
            .map_err(anyhow::Error::from)
    }

    fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static> {
        self
    }
}

pub struct SequencedTrackLocalStaticRTP {
    track: Arc<TrackLocalStaticRTP>,
    sequence_number: Mutex<u16>,
    pacing: Arc<VideoPacing>,
}

impl From<TrackLocalStaticRTP> for SequencedTrackLocalStaticRTP {
    fn from(value: TrackLocalStaticRTP) -> Self {
        Self {
            track: Arc::new(value),
            sequence_number: Mutex::new(0),
            pacing: Arc::new(VideoPacing::default()),
        }
    }
}

impl SequencedTrackLocalStaticRTP {
    pub fn with_pacing(track: TrackLocalStaticRTP, pacing: Arc<VideoPacing>) -> Self {
        Self { track: Arc::new(track), sequence_number: Mutex::new(0), pacing }
    }
}

impl TrackLike for SequencedTrackLocalStaticRTP {
    type Sample = rtp::packet::Packet;

    fn sample_bytes(&self, sample: &Self::Sample) -> usize { sample.payload.len() + 40 }
    fn pacing_window(&self, samples: &[Self::Sample]) -> Duration {
        self.pacing.window(samples.iter().map(|s| self.sample_bytes(s)).sum(), epoch_ms())
    }

    async fn write_with_extensions(
        &self,
        mut sample: Self::Sample,
        extensions: &[HeaderExtension],
    ) -> Result<(), anyhow::Error> {
        let (any_paused, all_paused) = (
            self.track.any_binding_paused().await,
            self.track.all_binding_paused().await,
        );

        if all_paused {
            // Abort already here to not increment sequence numbers.
            return Ok(());
        }
        if any_paused {
            warn!("WebRTC: not all paused but any paused");
        }

        let mut sequence_number = self.sequence_number.lock().await;
        sample.header.sequence_number = *sequence_number;
        *sequence_number = sequence_number.wrapping_add(1);

        self.track
            .write_rtp_with_extensions(&sample, extensions)
            .await
            .map_err(anyhow::Error::from)
            .map(|_| ())
    }

    fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static> {
        self.track.clone()
    }
}

#[cfg(test)]
mod queue_tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::{runtime::Handle, sync::Notify, time::timeout};

    use super::*;

    /// Records what the sender would hand to the RTP layer. No network, no
    /// peer connection, no media.
    struct RecordingTrack {
        written: AtomicUsize,
        fail: AtomicBool,
    }

    impl RecordingTrack {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                written: AtomicUsize::new(0),
                fail: AtomicBool::new(false),
            })
        }
    }

    impl TrackLike for RecordingTrack {
        type Sample = u32;

        async fn write_with_extensions(
            &self,
            _sample: Self::Sample,
            _extensions: &[HeaderExtension],
        ) -> Result<(), anyhow::Error> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(anyhow!("synthetic write failure"));
            }
            self.written.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static> {
            unimplemented!("queue tests never register a real track")
        }
    }

    fn frame(sample: u32) -> FrameSamples<RecordingTrack> {
        FrameSamples {
            important: false,
            samples: vec![sample],
        }
    }

    /// A sender with no peer connection: `send_samples` only touches the queue,
    /// the notify handle and the counters, so this is the real production code
    /// path for admission control.
    fn sender_without_peer(capacity: usize) -> TrackLocalSender<RecordingTrack> {
        TrackLocalSender::new(Handle::current(), Weak::new(), capacity)
    }

    struct PacedRecordingTrack { writes: std::sync::Mutex<Vec<(u32, Instant)>> }
    impl TrackLike for PacedRecordingTrack {
        type Sample = u32;
        fn pacing_window(&self, _: &[u32]) -> Duration { Duration::from_millis(12) }
        async fn write_with_extensions(&self, sample: u32, _: &[HeaderExtension]) -> Result<(), anyhow::Error> {
            self.writes.lock().unwrap().push((sample, Instant::now()));
            Ok(())
        }
        fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static> { unimplemented!() }
    }
    #[tokio::test]
    async fn real_sender_spreads_packets_and_keeps_frame_order() {
        let track = Arc::new(PacedRecordingTrack { writes: Default::default() });
        let queue: Arc<Mutex<VecDeque<FrameSamples<PacedRecordingTrack>>>> = Default::default();
        queue.lock().await.push_front(FrameSamples { important: false, samples: (0..12).collect() });
        let notify = Arc::new(Notify::new());
        let task = tokio::spawn({
            let track = track.clone(); let notify = notify.clone(); let queue = Arc::downgrade(&queue);
            async move { sample_sender(track, &notify, queue, &SenderMetrics::default()).await }
        });
        timeout(Duration::from_secs(2), async {
            while track.writes.lock().unwrap().len() < 12 { tokio::task::yield_now().await; }
        }).await.unwrap();
        let writes = track.writes.lock().unwrap();
        assert_eq!(writes.iter().map(|x| x.0).collect::<Vec<_>>(), (0..12).collect::<Vec<_>>());
        assert!(writes[11].1.duration_since(writes[0].1) >= Duration::from_millis(8));
        drop(writes);
        drop(queue); notify.notify_one();
        timeout(Duration::from_secs(1), task).await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn new_idr_can_clear_older_important_frames() {
        let sender = sender_without_peer(2);
        for i in 0..20 {
            sender.clear_queue(true).await;
            assert!(sender.send_samples(vec![i], true).await);
        }
        assert_eq!(sender.queue.lock().await.len(), 1);
        assert_eq!(sender.metrics().max_queue_depth(), 1);
    }

    /// MW-09: the consumer must stop when the owning queue is gone instead of
    /// spinning in an await-less loop.
    #[tokio::test]
    async fn sample_sender_exits_when_queue_is_dropped() {
        let notify = Arc::new(Notify::new());
        let queue: Arc<Mutex<VecDeque<FrameSamples<RecordingTrack>>>> = Default::default();
        let weak = Arc::downgrade(&queue);
        drop(queue);

        let metrics = SenderMetrics::default();
        let handle = tokio::spawn({
            let notify = notify.clone();
            async move {
                sample_sender(RecordingTrack::new(), &notify, weak, &metrics).await;
            }
        });
        match timeout(Duration::from_millis(500), handle).await {
            Ok(joined) => assert!(joined.is_ok(), "consumer task panicked"),
            Err(_) => panic!("consumer did not terminate after its queue was dropped (busy loop)"),
        }
    }

    /// MW-09: a notification delivered before the consumer starts waiting must
    /// not be lost, otherwise the sample waits for the next push.
    #[tokio::test]
    async fn notification_before_wait_is_not_lost() {
        let notify = Arc::new(Notify::new());
        notify.notify_one();
        assert!(
            timeout(Duration::from_millis(200), notify.notified())
                .await
                .is_ok(),
            "notify_one did not store a permit before the waiter registered"
        );
    }

    /// MW-09 regression control: the old `notify_waiters` call can be lost in
    /// exactly this window, which is why the production code uses notify_one.
    #[tokio::test]
    async fn notify_waiters_would_lose_the_wakeup() {
        let notify = Arc::new(Notify::new());
        notify.notify_waiters();
        assert!(
            timeout(Duration::from_millis(100), notify.notified())
                .await
                .is_err(),
            "notify_waiters unexpectedly stored a permit"
        );
    }

    /// MW-09 + MW-10: overflow is rejected and counted through the real
    /// `send_samples` entry point, and accepted samples are queued.
    #[tokio::test]
    async fn send_samples_rejects_overflow_and_counts_it() {
        let sender = sender_without_peer(2);

        assert!(sender.send_samples(vec![1], false).await);
        assert!(sender.send_samples(vec![2], false).await);
        assert!(
            !sender.send_samples(vec![3], false).await,
            "third sample must be rejected at capacity 2"
        );
        assert!(
            sender.send_samples(vec![4], true).await,
            "important samples bypass the limit"
        );

        let metrics = sender.metrics();
        assert_eq!(
            metrics.queued_samples(),
            3,
            "only admitted samples are counted as queued"
        );
        assert_eq!(metrics.rejected_samples(), 1);
        assert_eq!(metrics.max_queue_depth(), 3);
        assert!(metrics.summary().contains("rejected_samples=1"));
    }

    /// MW-09 + MW-10: the consumer really writes what was admitted, and the
    /// counters describe it.
    #[tokio::test]
    async fn consumer_writes_admitted_samples_and_counts_them() {
        let notify = Arc::new(Notify::new());
        let queue: Arc<Mutex<VecDeque<FrameSamples<RecordingTrack>>>> = Default::default();
        // Share the same Arc the production sender hands to its consumer, so the
        // writer task and this observer read one set of counters.
        let metrics = Arc::new(SenderMetrics::default());
        let track = RecordingTrack::new();

        queue.lock().await.push_front(frame(1));
        queue.lock().await.push_front(frame(2));

        let handle = tokio::spawn({
            let notify = notify.clone();
            let weak = Arc::downgrade(&queue);
            let metrics = metrics.clone();
            let track = track.clone();
            async move {
                sample_sender(track, &notify, weak, &metrics).await;
            }
        });

        timeout(Duration::from_secs(1), async {
            while track.written.load(Ordering::Relaxed) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("consumer did not write the two queued samples");

        assert_eq!(metrics.written_samples(), 2);
        assert_eq!(metrics.rejected_samples(), 0);
        assert_eq!(metrics.write_errors(), 0);

        handle.abort();
        let _ = handle.await;
    }

    /// MW-10: a write failure is counted separately from a successful write and
    /// never blocks the consumer.
    #[tokio::test]
    async fn write_failures_are_counted() {
        let notify = Arc::new(Notify::new());
        let queue: Arc<Mutex<VecDeque<FrameSamples<RecordingTrack>>>> = Default::default();
        let metrics = Arc::new(SenderMetrics::default());
        let track = RecordingTrack::new();
        track.fail.store(true, Ordering::Relaxed);

        queue.lock().await.push_front(frame(7));

        let handle = tokio::spawn({
            let notify = notify.clone();
            let weak = Arc::downgrade(&queue);
            let metrics = metrics.clone();
            let track = track.clone();
            async move {
                sample_sender(track, &notify, weak, &metrics).await;
            }
        });

        timeout(Duration::from_secs(1), async {
            while metrics.write_errors() < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("write failure was not counted");

        assert_eq!(metrics.written_samples(), 0);
        assert!(metrics.summary().contains("write_errors=1"));
        assert!(
            !metrics.summary().contains("opus") && !metrics.summary().contains("token"),
            "summary must stay content-free"
        );

        handle.abort();
        let _ = handle.await;
    }

    /// MW-10: the summary is bounded in size so periodic logging cannot flood.
    #[test]
    fn metrics_summary_is_bounded_and_has_no_payload() {
        let metrics = SenderMetrics::default();
        for _ in 0..1000 {
            metrics.observe_write(Duration::from_micros(250));
        }
        let summary = metrics.summary();
        assert!(
            summary.len() < 256,
            "summary grew to {} bytes",
            summary.len()
        );
        assert!(summary.starts_with("audio_sender "));
        assert_eq!(metrics.write_stall_micros(), 250_000);
        assert_eq!(metrics.max_write_stall_micros(), 250);
    }
}
