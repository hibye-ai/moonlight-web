import { StatValue } from "../stats.js"

/// Incremental collection of `RTCPeerConnection.getStats()` inbound-rtp reports.
///
/// Browsers report one `inbound-rtp` entry per kind and SSRC (audio, video, and possibly
/// several simulcast layers). Writing all of them into one flat key set made the result
/// depend on Map enumeration order, mixed audio and video packet counters, and stored
/// seconds where the key names promised milliseconds.
///
/// AR-12: keeping per-stream *snapshots* was not enough, because the output keys were still
/// per kind (`webrtcAudioPacketsReceived`), so the last stream written won and a second audio
/// SSRC silently replaced the first. The output is now explicitly two-layered:
///
///   1. `webrtcAudio*` / `webrtcVideo*` describe ALL live streams of that kind, with the
///      aggregation rule stated per field:
///        * packet/frame/sample counters and their interval deltas: SUM over live streams.
///          A stream that has no previous sample (it just appeared, or its counters reset)
///          contributes nothing to the delta; the total still includes it, so the aggregate
///          delta is a lower bound for the interval rather than a wrong number.
///        * `jitter`:                    packet-weighted mean (weight = packetsReceived)
///        * `jitterBuffer*MeanMs`:       emission-weighted mean, i.e.
///                                       sum(delay seconds) / sum(emitted frames) * 1000
///        * `concealmentRatio`:          sum(concealed) / sum(totalSamplesReceived)
///                                      (see `concealmentRatioOf` for the W3C provenance)
///        * `fps`, `videoWidth/Height`:  taken from the ACTIVE layer only (see below)
///   2. `webrtcAudioStream.<id>.<Field>` / `webrtcVideoStream.<id>.<Field>` keep one key set
///      per report, so a specific SSRC stays inspectable. `<id>` is the `id` from getStats(),
///      or `ssrc<ssrc>` when a fixture omits `id`; characters outside [A-Za-z0-9_-] are
///      replaced with `_` and a `#n` suffix disambiguates collisions.
///
/// The per-frame scalars (fps, resolution) have no meaningful sum across simulcast layers, so
/// they are taken from one explicitly chosen stream instead of "whichever was written last":
/// the ACTIVE layer, defined as the stream with the most decoded frames (audio: the most
/// received samples), tie-broken by fps, then frame area, then label for determinism. Its
/// name is published as `webrtcVideoActiveStream` / `webrtcAudioActiveStream`, so the display
/// says which stream those scalars describe.
///
/// Both layers are shown by `streamStatsToText()`, which iterates every transport key, so no
/// UI change is needed to display them.
///
/// A stream that stops being reported is removed from the aggregate and its snapshot is
/// dropped; if it comes back it is treated as a new interval (no bogus delta). A counter
/// decrease on a surviving stream is treated the same way and falls back to its lifetime
/// mean, so a reset never surfaces as a negative delta.
export type RtcStreamKey = "video" | "audio"

type StreamSnapshot = {
    id: string
    kind: RtcStreamKey
    packetsReceived: number | null
    packetsLost: number | null
    framesDropped: number | null
    framesDecoded: number | null
    keyFramesDecoded: number | null
    nackCount: number | null
    jitterBufferDelay: number | null
    jitterBufferEmittedCount: number | null
    jitterBufferTargetDelay: number | null
    jitterBufferMinimumDelay: number | null
    concealedSamples: number | null
    totalSamplesReceived: number | null
    concealmentEvents: number | null
    insertedSamplesForDeceleration: number | null
    removedSamplesForAcceleration: number | null
    packetsDiscarded: number | null
}

/// Values that only grow while the stream lives; a decrease means the counters restarted
/// (reconnect, new SSRC) and the interval delta must not be negative.
type NumericSnapshotField = Exclude<keyof StreamSnapshot, "id" | "kind">

const CUMULATIVE_FIELDS: Array<NumericSnapshotField> = [
    "packetsReceived",
    "packetsLost",
    "framesDropped",
    "framesDecoded",
    "keyFramesDecoded",
    "nackCount",
    "jitterBufferDelay",
    "jitterBufferEmittedCount",
    "jitterBufferTargetDelay",
    "jitterBufferMinimumDelay",
    "concealedSamples",
    "totalSamplesReceived",
    "concealmentEvents",
    "insertedSamplesForDeceleration",
    "removedSamplesForAcceleration",
    "packetsDiscarded",
]

/// Stable identity of one `getStats()` report, used to de-duplicate the same report when it
/// is visible through several sources (peer connection + receiver).
///
/// The browser's own `id` is authoritative; a fixture without `id` falls back to
/// `type|kind|ssrc`. When even that is missing there is no safe way to say two reports are
/// the same, so `namespace` keeps them apart instead of silently dropping one.
export function statReportIdentity(key: string, report: any, namespace: string | number = ""): string {
    if (report == null || typeof report != "object") {
        return `${namespace}:${key}`
    }
    const type = String(report.type ?? "")
    const id = report.id
    if (typeof id == "string" && id.length > 0) {
        return `${type}|${id}`
    }
    const ssrc = report.ssrc
    if (typeof ssrc == "number" && Number.isFinite(ssrc)) {
        return `${type}|${report.kind ?? report.mediaType ?? ""}|ssrc${ssrc}`
    }
    return `${namespace}:${type}|${key}`
}

function readNumber(report: any, field: string): number | null {
    const value = report[field]
    return typeof value == "number" && Number.isFinite(value) ? value : null
}

function snapshotOf(report: any, kind: RtcStreamKey): StreamSnapshot {
    return {
        id: String(report.id ?? ""),
        kind,
        packetsReceived: readNumber(report, "packetsReceived"),
        packetsLost: readNumber(report, "packetsLost"),
        framesDropped: readNumber(report, "framesDropped"),
        framesDecoded: readNumber(report, "framesDecoded"),
        keyFramesDecoded: readNumber(report, "keyFramesDecoded"),
        nackCount: readNumber(report, "nackCount"),
        jitterBufferDelay: readNumber(report, "jitterBufferDelay"),
        jitterBufferEmittedCount: readNumber(report, "jitterBufferEmittedCount"),
        jitterBufferTargetDelay: readNumber(report, "jitterBufferTargetDelay"),
        jitterBufferMinimumDelay: readNumber(report, "jitterBufferMinimumDelay"),
        concealedSamples: readNumber(report, "concealedSamples"),
        totalSamplesReceived: readNumber(report, "totalSamplesReceived"),
        concealmentEvents: readNumber(report, "concealmentEvents"),
        insertedSamplesForDeceleration: readNumber(report, "insertedSamplesForDeceleration"),
        removedSamplesForAcceleration: readNumber(report, "removedSamplesForAcceleration"),
        packetsDiscarded: readNumber(report, "packetsDiscarded"),
    }
}

/// Interval delta, or null when either side is missing or the counter went backwards.
function delta(current: number | null, previous: number | null): number | null {
    if (current == null) {
        return null
    }
    if (previous == null || current < previous) {
        return null
    }
    return current - previous
}

function sum(values: Array<number | null>): number | null {
    let total: number | null = null
    for (const value of values) {
        if (value == null) {
            continue
        }
        total = (total ?? 0) + value
    }
    return total
}

/// Weighted mean over `(weight, value)` pairs. Falls back to the unweighted mean when no
/// pair carries a positive weight (e.g. a fixture without `packetsReceived`), and to null
/// when there is nothing to average.
function weightedMean(pairs: Array<{ weight: number | null, value: number | null }>): number | null {
    let weighted = 0
    let weightTotal = 0
    let plainTotal = 0
    let plainCount = 0
    for (const pair of pairs) {
        if (pair.value == null) {
            continue
        }
        plainTotal += pair.value
        plainCount += 1
        if (pair.weight != null && pair.weight > 0) {
            weighted += pair.value * pair.weight
            weightTotal += pair.weight
        }
    }
    if (weightTotal > 0) {
        return weighted / weightTotal
    }
    if (plainCount > 0) {
        return plainTotal / plainCount
    }
    return null
}

/// `AUDIO`/`VIDEO` prefix used for the per-kind aggregate keys.
function prefixOf(kind: RtcStreamKey): string {
    return kind == "video" ? "webrtcVideo" : "webrtcAudio"
}

export class RtcStatsCollector {
    private snapshots = new Map<string, StreamSnapshot>()
    private labels = new Map<string, string>()

    /// Feed one `getStats()` map; returns the transport stats contributed by inbound-rtp.
    collect(reports: Iterable<[string, any]>): Record<string, StatValue> {
        const stats: Record<string, StatValue> = {}

        // Sort by report id so the outcome cannot depend on Map enumeration order.
        const inbound: Array<{ key: string, report: any, kind: RtcStreamKey }> = []
        for (const [key, report] of reports) {
            if (!report || report.type != "inbound-rtp") {
                continue
            }
            const kind = report.kind ?? report.mediaType
            if (kind != "video" && kind != "audio") {
                continue
            }
            inbound.push({ key, report, kind })
        }
        inbound.sort((a, b) => a.kind.localeCompare(b.kind) || a.key.localeCompare(b.key))

        const samples: StreamSample[] = []
        const live = new Set<string>()

        for (const entry of inbound) {
            const storeKey = this.storeKeyOf(entry.report, entry.key)
            live.add(storeKey)
            const label = this.labelOf(storeKey, entry.report)
            samples.push(this.sampleOf(entry.report, entry.kind, storeKey, label))
        }

        // Drop snapshots for streams that disappeared (unpublished track / closed peer).
        for (const key of [...this.snapshots.keys()]) {
            if (!live.has(key)) {
                this.snapshots.delete(key)
            }
        }
        for (const key of [...this.labels.keys()]) {
            if (!live.has(key)) {
                this.labels.delete(key)
            }
        }

        // Fixed emission order (audio, then video) so the key order is reproducible.
        this.emitKind("audio", samples.filter(sample => sample.kind == "audio"), stats)
        this.emitKind("video", samples.filter(sample => sample.kind == "video"), stats)

        return stats
    }

    /// Forget all history; the next sample starts a fresh interval (used on reconnect).
    reset(): void {
        this.snapshots.clear()
        this.labels.clear()
    }

    private storeKeyOf(report: any, fallback: string): string {
        const id = String(report.id ?? "")
        const kind = report.kind ?? report.mediaType
        return `${kind}:${id.length > 0 ? id : fallback}`
    }

    /// A stable, display-safe name for one stream. Reuses the assignment from earlier
    /// samples so a stream does not change name mid-call.
    private labelOf(storeKey: string, report: any): string {
        const existing = this.labels.get(storeKey)
        if (existing != null) {
            return existing
        }

        const id = String(report.id ?? "")
        const ssrc = readNumber(report, "ssrc")
        const base = id.length > 0 ? id : ssrc != null ? `ssrc${ssrc}` : storeKey.split(":").slice(1).join(":") || "unnamed"
        let candidate = base.replace(/[^A-Za-z0-9_-]/g, "_")
        if (candidate.length == 0) {
            candidate = "unnamed"
        }

        // Disambiguate: the same label must never address two live streams.
        const taken = new Set(this.labels.values())
        if (taken.has(candidate)) {
            let index = 2
            while (taken.has(`${candidate}#${index}`)) {
                index += 1
            }
            candidate = `${candidate}#${index}`
        }

        this.labels.set(storeKey, candidate)
        return candidate
    }

    private sampleOf(report: any, kind: RtcStreamKey, storeKey: string, label: string): StreamSample {
        const current = snapshotOf(report, kind)
        const previous = this.snapshots.get(storeKey) ?? null
        this.snapshots.set(storeKey, current)

        // A decrease means the stream restarted (new SSRC after reconnect): the previous
        // sample describes a different stream, so fall back to the lifetime mean.
        const countersReset =
            previous != null && (
                (current.jitterBufferEmittedCount != null && previous.jitterBufferEmittedCount != null && current.jitterBufferEmittedCount < previous.jitterBufferEmittedCount) ||
                (current.packetsReceived != null && previous.packetsReceived != null && current.packetsReceived < previous.packetsReceived)
            )

        const interval = previous != null && !countersReset

        // Buffer delay totals are summed in seconds over the emitted frame count, so the
        // aggregate can be weighted by actual frames; see the class comment.
        const delaySeconds = interval ? delta(current.jitterBufferDelay, previous.jitterBufferDelay) : current.jitterBufferDelay
        const targetSeconds = interval ? delta(current.jitterBufferTargetDelay, previous.jitterBufferTargetDelay) : current.jitterBufferTargetDelay
        const minimumSeconds = interval ? delta(current.jitterBufferMinimumDelay, previous.jitterBufferMinimumDelay) : current.jitterBufferMinimumDelay
        const emittedFrames = interval ? delta(current.jitterBufferEmittedCount, previous.jitterBufferEmittedCount) : current.jitterBufferEmittedCount

        return {
            kind,
            label,
            current,
            previous,
            countersReset,
            jitterMs: (() => {
                const seconds = readNumber(report, "jitter")
                return seconds == null ? null : seconds * 1000
            })(),
            delaySeconds,
            emittedFrames,
            targetSeconds,
            minimumSeconds,
            framesPerSecond: readNumber(report, "framesPerSecond"),
            frameWidth: readNumber(report, "frameWidth"),
            frameHeight: readNumber(report, "frameHeight"),
            totalDecodeSeconds: readNumber(report, "totalDecodeTime"),
            totalAssemblySeconds: readNumber(report, "totalAssemblyTime"),
            totalProcessingSeconds: readNumber(report, "totalProcessingDelay"),
            decoderImplementation: typeof report.decoderImplementation == "string" ? report.decoderImplementation : null,
        }
    }

    private emitKind(kind: RtcStreamKey, samples: StreamSample[], stats: Record<string, StatValue>): void {
        if (samples.length == 0) {
            return
        }

        const prefix = prefixOf(kind)
        const set = (name: string, value: number | null | undefined) => {
            if (value != null) {
                stats[name] = value
            }
        }

        // Per-stream layer first, so any key it writes is never mistaken for an aggregate.
        for (const sample of samples) {
            this.emitStream(sample, `webrtc${kind == "video" ? "Video" : "Audio"}Stream.${sample.label}.`, stats)
        }

        // --- aggregates over all live streams of this kind ---
        const current = (field: NumericSnapshotField) => samples.map(sample => sample.current[field])

        set(`${prefix}PacketsReceived`, sum(current("packetsReceived")))
        set(`${prefix}PacketsLost`, sum(current("packetsLost")))
        set(`${prefix}PacketsReceivedDelta`, sum(samples.map(sample =>
            sample.countersReset ? null : delta(sample.current.packetsReceived, sample.previous?.packetsReceived ?? null))))
        set(`${prefix}PacketsLostDelta`, sum(samples.map(sample =>
            sample.countersReset ? null : delta(sample.current.packetsLost, sample.previous?.packetsLost ?? null))))

        // Packet-weighted mean: a stream that carried more packets has more say in the
        // jitter that the user actually experienced.
        set(`${prefix}JitterMs`, weightedMean(samples.map(sample => ({
            weight: sample.current.packetsReceived,
            value: sample.jitterMs,
        }))))

        // Emission-weighted buffer delay: sum(seconds) / sum(frames) * 1000.
        set(`${prefix}JitterBufferMeanMs`, this.weightedDelayMs(samples, sample => sample.delaySeconds))
        set(`${prefix}JitterBufferTargetMeanMs`, this.weightedDelayMs(samples, sample => sample.targetSeconds))
        set(`${prefix}JitterBufferMinimumMeanMs`, this.weightedDelayMs(samples, sample => sample.minimumSeconds))

        if (kind == "video") {
            const active = activeLayer(samples)
            if (active) {
                set("webrtcFps", active.framesPerSecond)
                set("videoWidth", active.frameWidth)
                set("videoHeight", active.frameHeight)
            }
            stats[`${prefix}ActiveStream`] = (active?.label ?? "")

            set(`${prefix}FramesDecoded`, sum(current("framesDecoded")))
            set(`${prefix}FramesDropped`, sum(current("framesDropped")))
            set(`${prefix}KeyFramesDecoded`, sum(current("keyFramesDecoded")))
            set(`${prefix}NackCount`, sum(current("nackCount")))

            // Lifetime totals, summed across layers; the names keep "Total" and the values
            // are converted from seconds.
            set(`${prefix}TotalDecodeTimeMs`, secondsToMs(sum(samples.map(sample => sample.totalDecodeSeconds))))
            set(`${prefix}TotalAssemblyTimeMs`, secondsToMs(sum(samples.map(sample => sample.totalAssemblySeconds))))
            set(`${prefix}TotalProcessingDelayMs`, secondsToMs(sum(samples.map(sample => sample.totalProcessingSeconds))))

            const decoderImplementation = active?.decoderImplementation ?? samples
                .map(sample => sample.decoderImplementation)
                .find(value => value != null && value.length > 0)
            if (decoderImplementation) {
                stats.decoderImplementation = decoderImplementation
            }

            // Legacy flat keys. They used to be written by whichever report came last; they
            // now always describe the video streams (the adaptive bitrate advisor's intent)
            // and use the same aggregates as the namespaced video keys.
            set("webrtcJitterMs", weightedMean(samples.map(sample => ({
                weight: sample.current.packetsReceived,
                value: sample.jitterMs,
            }))))
            set("webrtcPacketsReceived", sum(current("packetsReceived")))
            set("webrtcPacketsLost", sum(current("packetsLost")))
            set("webrtcFramesDropped", sum(current("framesDropped")))
            set("webrtcKeyFramesDecoded", sum(current("keyFramesDecoded")))
            set("webrtcNackCount", sum(current("nackCount")))
            set("webrtcJitterBufferDelayMs", this.weightedDelayMs(samples, sample => sample.delaySeconds))
            set("webrtcJitterBufferTargetDelayMs", this.weightedDelayMs(samples, sample => sample.targetSeconds))
            set("webrtcJitterBufferMinimumDelayMs", this.weightedDelayMs(samples, sample => sample.minimumSeconds))
        } else {
            stats[`${prefix}ActiveStream`] = (activeLayer(samples)?.label ?? "")

            set(`${prefix}ConcealedSamples`, sum(current("concealedSamples")))
            set(`${prefix}TotalSamplesReceived`, sum(current("totalSamplesReceived")))
            set(`${prefix}ConcealmentEvents`, sum(current("concealmentEvents")))
            set(`${prefix}InsertedSamplesForDeceleration`, sum(current("insertedSamplesForDeceleration")))
            set(`${prefix}RemovedSamplesForAcceleration`, sum(current("removedSamplesForAcceleration")))
            set(`${prefix}PacketsDiscarded`, sum(current("packetsDiscarded")))
            set(`${prefix}ConcealedSamplesDelta`, sum(samples.map(sample =>
                sample.countersReset ? null : delta(sample.current.concealedSamples, sample.previous?.concealedSamples ?? null))))
            set(`${prefix}ConcealmentEventsDelta`, sum(samples.map(sample =>
                sample.countersReset ? null : delta(sample.current.concealmentEvents, sample.previous?.concealmentEvents ?? null))))

            // Concealed / totalSamplesReceived over all live streams: the single number a
            // receiver-policy A/B should compare. The denominator is NOT
            // `totalSamplesReceived + concealedSamples`; the W3C counter already includes
            // the concealed samples (see `concealmentRatioOf`).
            const ratio = ratioOfSums(samples)
            if (ratio != null) {
                stats[`${prefix}ConcealmentRatio`] = ratio
            }
        }
    }

    /// Emission-weighted mean delay in milliseconds for whichever seconds total is asked for.
    private weightedDelayMs(samples: StreamSample[], select: (sample: StreamSample) => number | null): number | null {
        let seconds = 0
        let frames = 0
        for (const sample of samples) {
            const value = select(sample)
            if (value == null || sample.emittedFrames == null || sample.emittedFrames <= 0) {
                continue
            }
            seconds += value
            frames += sample.emittedFrames
        }
        if (frames <= 0) {
            return null
        }
        return (seconds / frames) * 1000
    }

    private emitStream(sample: StreamSample, prefix: string, stats: Record<string, StatValue>): void {
        const { current, previous, countersReset } = sample
        const set = (name: string, value: number | null | undefined) => {
            if (value != null) {
                stats[`${prefix}${name}`] = value
            }
        }

        set("PacketsReceived", current.packetsReceived)
        set("PacketsLost", current.packetsLost)
        set("PacketsReceivedDelta", countersReset ? null : delta(current.packetsReceived, previous?.packetsReceived ?? null))
        set("PacketsLostDelta", countersReset ? null : delta(current.packetsLost, previous?.packetsLost ?? null))
        set("JitterMs", sample.jitterMs)
        set("JitterBufferMeanMs", meanDelayMs(sample.delaySeconds, sample.emittedFrames))
        set("JitterBufferTargetMeanMs", meanDelayMs(sample.targetSeconds, sample.emittedFrames))
        set("JitterBufferMinimumMeanMs", meanDelayMs(sample.minimumSeconds, sample.emittedFrames))

        if (sample.kind == "video") {
            set("Fps", sample.framesPerSecond)
            set("FrameWidth", sample.frameWidth)
            set("FrameHeight", sample.frameHeight)
            set("FramesDecoded", current.framesDecoded)
            set("FramesDropped", current.framesDropped)
            set("KeyFramesDecoded", current.keyFramesDecoded)
            set("NackCount", current.nackCount)
            set("TotalDecodeTimeMs", secondsToMs(sample.totalDecodeSeconds))
            set("TotalAssemblyTimeMs", secondsToMs(sample.totalAssemblySeconds))
            set("TotalProcessingDelayMs", secondsToMs(sample.totalProcessingSeconds))
        } else {
            set("ConcealedSamples", current.concealedSamples)
            set("TotalSamplesReceived", current.totalSamplesReceived)
            set("ConcealmentEvents", current.concealmentEvents)
            set("InsertedSamplesForDeceleration", current.insertedSamplesForDeceleration)
            set("RemovedSamplesForAcceleration", current.removedSamplesForAcceleration)
            set("PacketsDiscarded", current.packetsDiscarded)
            set("ConcealedSamplesDelta", countersReset ? null : delta(current.concealedSamples, previous?.concealedSamples ?? null))
            set("ConcealmentEventsDelta", countersReset ? null : delta(current.concealmentEvents, previous?.concealmentEvents ?? null))
            // Per-stream ratio uses the same W3C definition as the aggregate (see
            // `concealmentRatioOf`): concealedSamples / totalSamplesReceived.
            set("ConcealmentRatio", concealmentRatioOf(current.concealedSamples, current.totalSamplesReceived))
        }
    }
}

type StreamSample = {
    kind: RtcStreamKey
    label: string
    current: StreamSnapshot
    previous: StreamSnapshot | null
    countersReset: boolean
    jitterMs: number | null
    /// Seconds of buffered delay that the mean for this sample is computed from.
    delaySeconds: number | null
    targetSeconds: number | null
    minimumSeconds: number | null
    /// Frame count those seconds were accumulated over; also the aggregation weight.
    emittedFrames: number | null
    framesPerSecond: number | null
    frameWidth: number | null
    frameHeight: number | null
    totalDecodeSeconds: number | null
    totalAssemblySeconds: number | null
    totalProcessingSeconds: number | null
    decoderImplementation: string | null
}

/// Mean of a `*Delay` accumulator: value is in seconds of total delay, emitted count is
/// the number of frames it was accumulated over, so the result is a per-frame mean.
function meanDelayMs(totalSeconds: number | null, emitted: number | null): number | null {
    if (totalSeconds == null || emitted == null || emitted <= 0) {
        return null
    }
    return (totalSeconds / emitted) * 1000
}

function secondsToMs(seconds: number | null): number | null {
    return seconds == null ? null : seconds * 1000
}

function ratioOfSums(samples: StreamSample[]): number | null {
    // Only streams that actually reported BOTH counters may contribute: `sum()` would
    // silently treat a missing `concealedSamples` as "0 concealed" while still adding that
    // stream's `totalSamplesReceived`, which would understate the ratio. Missing counters are
    // omitted instead of being invented.
    const reporting = samples.filter(sample =>
        sample.current.concealedSamples != null && sample.current.totalSamplesReceived != null)
    return concealmentRatioOf(
        sum(reporting.map(sample => sample.current.concealedSamples)),
        sum(reporting.map(sample => sample.current.totalSamplesReceived)))
}

/// Concealment ratio as defined by the W3C WebRTC-Stats spec:
/// `concealedSamples` / `totalSamplesReceived`.
///
/// Provenance (https://www.w3.org/TR/webrtc-stats/, RTCInboundRtpStreamStats):
///   * "totalSamplesReceived ... The total number of samples that have been received on this
///     RTP stream. This **includes** concealedSamples."
///   * "concealedSamples ... The total number of samples that are concealed samples."
/// So the total already contains the concealed ones and must NOT be extended by them; adding
/// `concealedSamples` to the denominator would report a ratio that can never reach 1 and
/// understates loss (400 concealed / 1600 total is 0.25, not 0.2).
///
/// A zero or missing total has no defined ratio (division by zero, and no samples means
/// nothing was measured), so it is omitted rather than reported as 0 or NaN.
function concealmentRatioOf(concealed: number | null, total: number | null): number | null {
    if (concealed == null || total == null || total <= 0) {
        return null
    }
    return concealed / total
}

/// The stream whose per-frame scalars are reported for the kind. See the class comment.
/// Deterministic: counters first, then fps, then frame area, then label.
function activeLayer(samples: StreamSample[]): StreamSample | null {
    if (samples.length == 0) {
        return null
    }
    const score = (sample: StreamSample) => sample.current.framesDecoded ?? sample.current.totalSamplesReceived ?? -1
    const area = (sample: StreamSample) => (sample.frameWidth ?? 0) * (sample.frameHeight ?? 0)
    return [...samples].sort((a, b) =>
        score(b) - score(a)
        || (b.framesPerSecond ?? -1) - (a.framesPerSecond ?? -1)
        || area(b) - area(a)
        || a.label.localeCompare(b.label)
    )[0]
}
