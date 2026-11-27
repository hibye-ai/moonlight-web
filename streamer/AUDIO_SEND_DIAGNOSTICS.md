# WebRTC audio send path: diagnostics and the explicit default

Status: **diagnostics added, default policy deliberately unchanged.**
The 1-second audio stutter is **not solved** by this change and this document does
not claim it is.

## What changed

The RTP sender now counts, without recording any media content:

| counter | meaning |
| --- | --- |
| `queue_depth` / `max_queue_depth` | samples waiting in the per-track send queue |
| `rejected_samples` | samples dropped because the queue was at `audio_sample_queue_size` |
| `written_samples` | samples handed to the RTP track successfully |
| `write_errors` | track writes that returned an error |
| `write_stall_micros` / `max_write_stall_micros` | time spent inside a track write |

`WebRtcAudio` emits one summary line at most every `METRICS_LOG_INTERVAL`
(10 s) at INFO level:

```
[Stream] audio sender (content-free): written_delta=… rejected_delta=… audio_sender queue_depth=… max_queue_depth=… rejected_samples=… written_samples=… write_errors=… write_stall_ms=… max_write_stall_ms=…
```

No sample data, no Opus payload, no peer or token identifiers, and no clipboard
values appear in this line. The summary is length-bounded and covered by a unit
test (`metrics_summary_is_bounded_and_has_no_payload`).

## What did *not* change, on purpose

* `audio_sample_queue_size` is still exactly what the browser sends in
  `StreamSettings` (`web/stream/index.ts` → `Init` → `webrtc/mod.rs` →
  `TrackLocalSender::channel_queue_size`). The existing default of 20 was not retuned
  by this change.
* The Opus sample duration is still `samples_per_frame / sample_rate`; the fixed
  frame duration and therefore the RTP clock progression are unchanged.
* The playout-delay header extension is still `(0, 0)` for audio and video.
* Nothing here is coupled to the WebSocket video/audio framing repairs
  (MW-04/MW-11). Those are separate code paths that do not run for a successful
  WebRTC session.

Picking a different buffer size, a drop policy (drop-oldest vs drop-newest), or a
timestamp/PLC rule without the numbers above would be an unmeasured change. The
default therefore stays explicit and documented until the A/B below is approved
and run.

## Why the timestamp question is still open

If the sender's RTP clock advances only by the samples it actually forwarded, a
burst of rejected samples would make the egress clock fall behind wall-clock.
Whether that happens depends on the packetizer and RTCP behaviour inside the
`webrtc` dependency, which this snapshot does not contain. `rejected_samples`
plus the browser's `concealedSamples` / `totalSamplesReceived` deltas are the
measurements that would settle it. None of those numbers were collected here —
no stream was started, no audio was captured.

## The A/B this prepares (not performed here)

1. Baseline session: record `transport`, `audioPipeline`, codec, channels and
   sample rate, and pin the audio `inbound-rtp` SSRC.
2. Read `rejected_samples` / `queue_depth` / `max_write_stall_ms` from the
   streamer log alongside the browser's receiver stats over the same window.
3. If `rejected_samples` stays 0 and `max_write_stall_ms` stays small, the
   server send queue is not the cause and the receiver-side policy (MW-08) is the
   next single variable to test.
4. If `rejected_samples` grows with audible gaps, decide the drop policy
   explicitly, with the measured loss rate as the justification.

Steps 1–2 need an approved stream session; step 3 is the only one this change
makes possible without changing behaviour.

## Lifecycle notes (MW-09)

The per-track consumer task now exits when its queue is dropped instead of
spinning, registers its waiter before inspecting the queue, and uses
`notify_one` so a push in the gap between the empty check and the wait is not
lost. Both spawned tasks (sample sender, RTCP reader) are owned and cancelled in
`TransportSender::close`, so a closed stream does not leave tasks writing into a
dead peer.
