import { StatValue, StreamStatsData } from "../stream/stats.js"

// F6 diagnostics.
//
// Three pieces, all without timers of their own:
//   * DiagnosticsBus: a bounded ring buffer of allowlisted events. Only known event codes
//     and allowlisted detail keys/values are stored, so free text (log lines, URLs, tokens,
//     clipboard or typed text) can never enter the timeline.
//   * HealthEvaluator: turns successive stats samples into a per-domain level. A domain
//     without evidence is "unknown"; a single elevated proxy metric is only "fair".
//   * buildDiagnosticsReport: the JSON export, assembled from allowlisted fields only.

export const DIAGNOSTIC_EVENT_CODES = [
    "connection.connecting",
    "connection.connected",
    "connection.reconnecting",
    "connection.offline",
    "connection.online",
    "connection.authRequired",
    "connection.stalled",
    "connection.recovered",
    "connection.failed",
    "connection.cancelled",
    "connection.manualReconnect",
    "connection.stopped",
    "media.progress",
    "audio.state",
    "audio.recover",
    "audio.preset",
    "audio.output",
    "quality.recommendation",
    "quality.apply",
    "quality.profile",
    "input.mode",
    "input.releaseAll",
    "input.chord",
    "input.shortcut",
    "clipboard.action",
    "clipboard.session",
    "visibility.hidden",
    "visibility.visible",
    "diagnostics.reset",
] as const

export type DiagnosticEventCode = typeof DIAGNOSTIC_EVENT_CODES[number]

export type DiagnosticDetail = Record<string, number | boolean | string>

export type DiagnosticEvent = {
    t: number
    code: DiagnosticEventCode
    detail: DiagnosticDetail
}

const NUMBER_DETAIL_KEYS = new Set([
    "attempt", "max", "delayMs", "code", "kbps", "targetKbps", "fromKbps", "count", "devices",
])

const BOOLEAN_DETAIL_KEYS = new Set([
    "live", "visible", "online", "active", "manual",
])

const CONNECTION_STATES = ["connecting", "connected", "reconnecting", "offline", "authRequired", "stalled", "failed", "stopped"]
const AUDIO_STATES = ["playing", "muted", "autoplayBlocked", "unavailable", "transportLost", "unknown"]

/// Per-key enumerations for string details. Anything else is dropped.
const STRING_DETAIL_VALUES: Record<string, ReadonlyArray<string>> = {
    state: [...CONNECTION_STATES, ...AUDIO_STATES],
    reason: [
        "cancelled", "exhausted", "terminated", "fatal", "authDenied", "offline", "unreachable",
        "stalled", "decoderStalled", "manual", "online", "visible", "notSupported", "permission",
        "failed", "socketClosed", "stopped", "noPlayer", "trackEnded",
    ],
    trigger: ["button", "shortcut", "blur", "hidden", "disconnect", "gesture", "online", "visible", "auto", "banner"],
    action: [
        "sendText", "receiveText", "sendImage", "receiveImage",
        "clipboardSendText", "clipboardReceiveText", "releaseAllInput", "toggleInputMode",
        "reconnect", "audioRecover", "toggleDiagnostics",
        "ctrlAltDel", "ctrlShiftEsc", "altTab", "altF4", "win", "winD",
    ],
    result: ["done", "unsupported", "busy", "error", "live", "nextConnect", "partial"],
    preset: ["game", "desktop", "custom"],
    mode: ["game", "text"],
    profile: ["default", "lan", "remote"],
    kind: ["none", "lower", "raise", "insufficient"],
    transport: ["webrtc", "websocket", "auto", "unknown"],
    verdict: ["unknown", "progressing", "videoIdle", "decoderStalled", "stalled"],
}

function roundNumber(value: number): number {
    return Math.round(value * 1000) / 1000
}

export function sanitizeDetail(detail: unknown): DiagnosticDetail {
    const result: DiagnosticDetail = {}
    if (!detail || typeof detail !== "object") {
        return result
    }
    for (const [key, value] of Object.entries(detail as Record<string, unknown>)) {
        if (NUMBER_DETAIL_KEYS.has(key)) {
            if (typeof value === "number" && Number.isFinite(value)) {
                result[key] = roundNumber(value)
            }
        } else if (BOOLEAN_DETAIL_KEYS.has(key)) {
            if (typeof value === "boolean") {
                result[key] = value
            }
        } else {
            const allowed = STRING_DETAIL_VALUES[key]
            if (allowed && typeof value === "string" && allowed.indexOf(value) !== -1) {
                result[key] = value
            }
        }
    }
    return result
}

export function isDiagnosticEventCode(code: unknown): code is DiagnosticEventCode {
    return typeof code === "string" && (DIAGNOSTIC_EVENT_CODES as ReadonlyArray<string>).indexOf(code) !== -1
}

export type DiagnosticsListener = (event: DiagnosticEvent) => void

export const DEFAULT_DIAGNOSTICS_CAPACITY = 100

export class DiagnosticsBus {
    private capacity: number
    private now: () => number
    private buffer: DiagnosticEvent[] = []
    private dropped = 0
    private listeners: DiagnosticsListener[] = []

    constructor(options?: { capacity?: number, now?: () => number }) {
        const capacity = Math.floor(options?.capacity ?? DEFAULT_DIAGNOSTICS_CAPACITY)
        this.capacity = Math.max(1, Math.min(capacity, 1000))
        this.now = options?.now ?? (() => Date.now())
    }

    record(code: DiagnosticEventCode | string, detail?: unknown): boolean {
        if (!isDiagnosticEventCode(code)) {
            return false
        }
        const event: DiagnosticEvent = { t: this.now(), code, detail: sanitizeDetail(detail) }
        this.buffer.push(event)
        while (this.buffer.length > this.capacity) {
            this.buffer.shift()
            this.dropped += 1
        }
        for (const listener of this.listeners.slice()) {
            try {
                listener(event)
            } catch {
                // A broken listener must not stop recording.
            }
        }
        return true
    }

    events(): DiagnosticEvent[] {
        return this.buffer.map(event => ({ t: event.t, code: event.code, detail: { ...event.detail } }))
    }

    droppedCount(): number {
        return this.dropped
    }

    subscribe(listener: DiagnosticsListener): () => void {
        this.listeners.push(listener)
        return () => {
            const index = this.listeners.indexOf(listener)
            if (index !== -1) {
                this.listeners.splice(index, 1)
            }
        }
    }

    reset() {
        this.buffer = []
        this.dropped = 0
        this.record("diagnostics.reset")
    }
}

// -- Health

export type HealthLevel = "good" | "fair" | "poor" | "unknown"

export type DomainHealth = {
    level: HealthLevel
    reasons: string[]
    counterReset: boolean
}

export type HealthSummary = {
    network: DomainHealth
    decoder: DomainHealth
    audio: DomainHealth
    host: DomainHealth
}

export type HealthExtras = {
    /// Directly observed audio output state (F3), if any.
    audioState?: string
}

function statNumber(value: StatValue | number | null | undefined): number | null {
    return typeof value === "number" && Number.isFinite(value) ? value : null
}

function levelFromSignals(hasEvidence: boolean, signals: string[]): HealthLevel {
    if (!hasEvidence) return "unknown"
    if (signals.length === 0) return "good"
    // One proxy metric can have many causes; only agreeing indicators are "poor".
    if (signals.length === 1) return "fair"
    return "poor"
}

type CounterSnapshot = Record<string, number | null>

const NETWORK_RTT_HIGH_MS = 150
const NETWORK_JITTER_HIGH_MS = 30
const NETWORK_LOSS_HIGH = 0.02
const NETWORK_LOSS_MIN_PACKETS = 50
const DECODER_DROP_HIGH = 0.05
const DECODER_DROP_MIN_FRAMES = 3
const DECODER_SLOW_MS_PER_FRAME = 16
const AUDIO_CONCEALMENT_HIGH = 0.05
const HOST_LATENCY_HIGH_MS = 20
const STREAMER_TIME_HIGH_MS = 10

export class HealthEvaluator {
    private previous: CounterSnapshot | null = null

    sample(stats: StreamStatsData, _now: number, extras?: HealthExtras): HealthSummary {
        const transport = stats.transport ?? {}
        const current: CounterSnapshot = {
            received: statNumber(transport.webrtcPacketsReceived),
            lost: statNumber(transport.webrtcPacketsLost),
            audioReceived: statNumber(transport.webrtcAudioPacketsReceived),
            audioLost: statNumber(transport.webrtcAudioPacketsLost),
            framesDecoded: statNumber(transport.webrtcVideoFramesDecoded),
            framesDropped: statNumber(transport.webrtcFramesDropped),
            decodeMs: statNumber(transport.webrtcVideoTotalDecodeTimeMs),
            concealed: statNumber(transport.webrtcAudioConcealedSamples),
            totalSamples: statNumber(transport.webrtcAudioTotalSamplesReceived),
        }
        const previous = this.previous
        this.previous = current

        // A decreasing counter means a new stream (reconnect) or a rollover: the previous
        // sample describes something else, so no delta is derived from it.
        const resetKeys = new Set<string>()
        if (previous) {
            for (const key of Object.keys(current)) {
                const now = current[key]
                const before = previous[key]
                if (now != null && before != null && now < before) {
                    resetKeys.add(key)
                }
            }
        }
        const delta = (key: string): number | null => {
            if (!previous || resetKeys.has(key)) return null
            const now = current[key]
            const before = previous[key]
            if (now == null || before == null) return null
            return now - before
        }

        return {
            network: this.network(stats, delta, resetKeys),
            decoder: this.decoder(delta, resetKeys),
            audio: this.audio(delta, resetKeys, extras),
            host: this.host(stats),
        }
    }

    private network(stats: StreamStatsData, delta: (key: string) => number | null, resetKeys: Set<string>): DomainHealth {
        const transport = stats.transport ?? {}
        const rtt = statNumber(transport.webrtcRttMs) ?? statNumber(stats.browserRtt) ?? statNumber(stats.streamerRttMs)
        const jitter = statNumber(transport.webrtcJitterMs)
        const reasons: string[] = []

        let lossRate: number | null = null
        const receivedDelta = sumKnown([delta("received"), delta("audioReceived")])
        const lostDelta = sumKnown([delta("lost"), delta("audioLost")])
        if (receivedDelta != null && lostDelta != null) {
            const total = Math.max(0, receivedDelta) + Math.max(0, lostDelta)
            if (total >= NETWORK_LOSS_MIN_PACKETS) {
                lossRate = Math.max(0, lostDelta) / total
            }
        }

        if (rtt != null && rtt >= NETWORK_RTT_HIGH_MS) reasons.push("highRtt")
        if (lossRate != null && lossRate >= NETWORK_LOSS_HIGH) reasons.push("packetLoss")
        if (jitter != null && jitter >= NETWORK_JITTER_HIGH_MS) reasons.push("highJitter")

        const hasEvidence = rtt != null || lossRate != null || jitter != null
        const counterReset = ["received", "lost", "audioReceived", "audioLost"].some(key => resetKeys.has(key))
        return { level: levelFromSignals(hasEvidence, reasons), reasons, counterReset }
    }

    private decoder(delta: (key: string) => number | null, resetKeys: Set<string>): DomainHealth {
        const decoded = delta("framesDecoded")
        const dropped = delta("framesDropped")
        const decodeMs = delta("decodeMs")
        const reasons: string[] = []

        if (decoded != null && dropped != null) {
            const total = decoded + dropped
            if (dropped >= DECODER_DROP_MIN_FRAMES && total > 0 && dropped / total >= DECODER_DROP_HIGH) {
                reasons.push("framesDropped")
            }
        }
        if (decoded != null && decoded > 0 && decodeMs != null && decodeMs / decoded >= DECODER_SLOW_MS_PER_FRAME) {
            reasons.push("slowDecode")
        }

        const hasEvidence = decoded != null
        const counterReset = ["framesDecoded", "framesDropped", "decodeMs"].some(key => resetKeys.has(key))
        return { level: levelFromSignals(hasEvidence, reasons), reasons, counterReset }
    }

    private audio(delta: (key: string) => number | null, resetKeys: Set<string>, extras?: HealthExtras): DomainHealth {
        const reasons: string[] = []
        const concealed = delta("concealed")
        const total = delta("totalSamples")
        let hasEvidence = false
        if (concealed != null && total != null && total > 0) {
            hasEvidence = true
            if (concealed / total >= AUDIO_CONCEALMENT_HIGH) reasons.push("audioConcealment")
        }

        const counterReset = ["concealed", "totalSamples"].some(key => resetKeys.has(key))
        const state = extras?.audioState
        // Direct observations of the output itself, not proxies.
        if (state === "unavailable" || state === "transportLost") {
            return { level: "poor", reasons: [state, ...reasons], counterReset }
        }
        if (state === "muted" || state === "autoplayBlocked") {
            return { level: "fair", reasons: [state, ...reasons], counterReset }
        }
        if (state === "playing") {
            hasEvidence = true
        }
        return { level: levelFromSignals(hasEvidence, reasons), reasons, counterReset }
    }

    private host(stats: StreamStatsData): DomainHealth {
        const hostLatency = statNumber(stats.avgHostProcessingLatencyMs)
        const streamerTime = statNumber(stats.avgStreamerProcessingTimeMs)
        const reasons: string[] = []
        if (hostLatency != null && hostLatency >= HOST_LATENCY_HIGH_MS) reasons.push("highHostLatency")
        if (streamerTime != null && streamerTime >= STREAMER_TIME_HIGH_MS) reasons.push("slowStreamer")
        return {
            level: levelFromSignals(hostLatency != null || streamerTime != null, reasons),
            reasons,
            counterReset: false,
        }
    }
}

function sumKnown(values: Array<number | null>): number | null {
    let total: number | null = null
    for (const value of values) {
        if (value != null) {
            total = (total ?? 0) + value
        }
    }
    return total
}

// -- Export

export const DIAGNOSTICS_FORMAT = "moonlight-web-desktop-diagnostics/1"
export const DIAGNOSTICS_BUILD = "transport-optimization-20261027-r4"

export type DiagnosticsSample = { t: number, transport: string, visible: boolean, stats: StreamStatsData }

// Ten minutes in memory, sampled by the existing session timer. No content or
// network identifiers are exported; statsSection is the only serialization path.
export class DiagnosticsHistory {
    private samples: DiagnosticsSample[] = []
    sample(t: number, transport: string, visible: boolean, stats: StreamStatsData) {
        const last = this.samples[this.samples.length - 1]
        if (last && t - last.t < 1000 && t >= last.t) return
        this.samples.push({ t, transport, visible, stats: { ...stats, transport: { ...stats.transport }, video: { ...stats.video }, audio: { ...stats.audio } } })
        while (this.samples.length > 600 || (this.samples.length > 1 && t - this.samples[0].t > 600000)) this.samples.shift()
    }
    snapshot(): DiagnosticsSample[] { return this.samples.slice() }
}

const HEALTH_LEVELS = ["good", "fair", "poor", "unknown"]
const HEALTH_REASONS = [
    "highRtt", "packetLoss", "highJitter", "framesDropped", "slowDecode", "audioConcealment",
    "highHostLatency", "slowStreamer", "unavailable", "transportLost", "muted", "autoplayBlocked",
]

/// Browser supplied labels (codec, decoder implementation, jitter policy name) reduced to a
/// short token. Anything that still looks like a URL or a credential is dropped.
export function sanitizeLabel(value: unknown): string | null {
    if (typeof value !== "string") return null
    const cleaned = value.replace(/[^A-Za-z0-9 _.:-]/g, "").replace(/\s+/g, " ").trim().slice(0, 40)
    if (cleaned.length === 0) return null
    if (/https?|token|password|cookie|secret|bearer/i.test(cleaned)) return null
    // Enumeration, not a character filter: arbitrary innocently named content must
    // never become a technical label. Unknown browser implementations stay unknown.
    if (!/^(?:H264|H265|HEVC|AV1|VP8|VP9|H264_HIGH8_420|H265_MAIN8|H265_MAIN10|AV1_MAIN8|AV1_MAIN10|ExternalDecoder(?: D3D11)?|D3D11VideoDecoder|D3D12VideoDecoder|FFmpegVideoDecoder|Dav1dVideoDecoder|VideoToolboxVideoDecoder|MediaCodecVideoDecoder|VDAVideoDecoder|libvpx|libaom|OpenH264|WebCodecs|lowLatency|auto|custom:\d{1,3}ms|unknown)$/i.test(cleaned)) return null
    return cleaned
}

function pickNumber(value: unknown): number | undefined {
    return typeof value === "number" && Number.isFinite(value) ? roundNumber(value) : undefined
}

function pickEnum(value: unknown, allowed: ReadonlyArray<string>): string | undefined {
    return typeof value === "string" && allowed.indexOf(value) !== -1 ? value : undefined
}

function compact<T extends Record<string, unknown>>(record: T): Partial<T> {
    const result: Record<string, unknown> = {}
    for (const [key, value] of Object.entries(record)) {
        if (value !== undefined && value !== null) {
            result[key] = value
        }
    }
    return result as Partial<T>
}

export type DiagnosticsReportInput = {
    generatedAt: number
    connection?: { state?: string, transport?: string, reconnectAttempts?: number, requestedTransport?: string, webRtcFallbackCount?: number }
    samples?: ReadonlyArray<DiagnosticsSample>
    stats?: StreamStatsData | null
    health?: HealthSummary | null
    audio?: { state?: string, preset?: string, outputSelectable?: boolean }
    quality?: { profile?: string, configuredKbps?: number, sessionKbps?: number, recommendation?: string, targetKbps?: number }
    input?: { mode?: string }
    events?: ReadonlyArray<DiagnosticEvent>
    [other: string]: unknown
}

function statsSection(stats: StreamStatsData | null | undefined) {
    if (!stats) return {}
    const t = stats.transport ?? {}
    const a = stats.audio ?? {}
    const v = stats.video ?? {}
    return compact({
        videoPlayback: /video_?decoder/i.test(stats.videoPipeline ?? "") ? "webcodecs" : /openh264/i.test(stats.videoPipeline ?? "") ? "openh264" : /media_source/.test(stats.videoPipeline ?? "") ? "mediaSource" : stats.videoPipeline ? "trackOrCanvas" : undefined,
        audioPlayback: /audio_decoder/.test(stats.audioPipeline ?? "") ? "webcodecs" : /audio_pcm_buffer/.test(stats.audioPipeline ?? "") ? "pcmBuffer" : /worklet/.test(stats.audioPipeline ?? "") ? "worklet" : stats.audioPipeline ? "trackOrNode" : undefined,
        iceState: pickEnum(t.webrtcIceState, ["new", "checking", "connected", "completed", "failed", "disconnected", "closed"]),
        peerState: pickEnum(t.webrtcConnectionState, ["new", "connecting", "connected", "disconnected", "failed", "closed"]),
        localCandidateType: pickEnum(t.webrtcLocalCandidateType, ["host", "srflx", "prflx", "relay"]),
        remoteCandidateType: pickEnum(t.webrtcRemoteCandidateType, ["host", "srflx", "prflx", "relay"]),
        localProtocol: pickEnum(t.webrtcLocalProtocol, ["udp", "tcp"]),
        remoteProtocol: pickEnum(t.webrtcRemoteProtocol, ["udp", "tcp"]),
        localRelayProtocol: pickEnum(t.webrtcLocalRelayProtocol, ["udp", "tcp", "tls"]),
        websocketReceivedMessages: pickNumber(t.websocketReceivedMessages),
        websocketReceivedBytes: pickNumber(t.websocketReceivedBytes),
        websocketSendBufferedBytes: pickNumber(t.websocketSendBufferedBytes),
        websocketLastReceiveAgeMs: pickNumber(t.websocketLastReceiveAgeMs),
        websocketMaxReceiveGapMs: pickNumber(t.websocketMaxReceiveGapMs),
        inputQueuedMessages: pickNumber(t.webrtcInputQueuedMessages),
        inputBufferedBytes: pickNumber(t.webrtcInputBufferedBytes),
        inputCoalesced: pickNumber(t.webrtcInputCoalesced),
        inputFailures: pickNumber(t.webrtcInputFailures),
        browserLongTaskSupported: pickNumber(t.browserLongTaskSupported),
        browserLongTaskCount: pickNumber(t.browserLongTaskCount),
        browserLongTaskMaxMs: pickNumber(t.browserLongTaskMaxMs),
        browserLongTaskTotalMs: pickNumber(t.browserLongTaskTotalMs),
        videoDecoderQueue: pickNumber(v.videoDecoderQueue),
        videoDecoderQueueMs: pickNumber(v.videoDecoderQueueMs),
        videoDecoderBootstrapDropped: pickNumber(v.videoDecoderBootstrapDropped),
        videoDecoderResets: pickNumber(v.videoDecoderResets),
        videoDecoderOutput: pickNumber(v.videoDecoderOutput),
        videoWorkerPending: pickNumber(v.videoWorkerPending),
        videoWorkerDropped: pickNumber(v.videoWorkerDropped),
        videoDecoderHardwarePreference: pickEnum(v.videoDecoderHardwarePreference, ["prefer-hardware", "prefer-software", "no-preference"]),
        videoRenderedTotal: pickNumber(v.videoRenderedTotal),
        videoRenderedDropped: pickNumber(v.videoRenderedDropped),
        videoReadyState: pickNumber(v.videoReadyState),
        videoPaused: pickNumber(v.videoPaused),
        videoTrackPending: pickNumber(v.videoTrackPending),
        videoTrackWriteAgeMs: pickNumber(v.videoTrackWriteAgeMs),
        videoTrackMaxWriteMs: pickNumber(v.videoTrackMaxWriteMs),
        videoTrackDropped: pickNumber(v.videoTrackDropped),
        videoTrackWritten: pickNumber(v.videoTrackWritten),
        videoTrackErrors: pickNumber(v.videoTrackErrors),
        audioTrackPending: pickNumber(a.audioTrackPending),
        audioTrackPendingMs: pickNumber(a.audioTrackPendingMs),
        audioTrackWriteAgeMs: pickNumber(a.audioTrackWriteAgeMs),
        audioTrackMaxWriteMs: pickNumber(a.audioTrackMaxWriteMs),
        audioTrackDropped: pickNumber(a.audioTrackDropped),
        audioTrackWritten: pickNumber(a.audioTrackWritten),
        audioTrackErrors: pickNumber(a.audioTrackErrors),
        audioPcmScheduled: pickNumber(a.audioPcmScheduled),
        audioPcmPendingMs: pickNumber(a.audioPcmPendingMs),
        audioPcmResyncs: pickNumber(a.audioPcmResyncs),
        audioPcmUnderruns: pickNumber(a.audioPcmUnderruns),
        videoCodec: sanitizeLabel(stats.videoCodec) ?? undefined,
        videoWidth: pickNumber(stats.videoWidth),
        videoHeight: pickNumber(stats.videoHeight),
        videoFps: pickNumber(stats.videoFps),
        hdrEnabled: typeof stats.hdrEnabled === "boolean" ? stats.hdrEnabled : undefined,
        rttMs: pickNumber(t.webrtcRttMs) ?? pickNumber(stats.browserRtt) ?? pickNumber(stats.streamerRttMs),
        streamerRttMs: pickNumber(stats.streamerRttMs),
        streamerRttVarianceMs: pickNumber(stats.streamerRttVarianceMs),
        browserRttMs: pickNumber(stats.browserRtt),
        packetsReceived: pickNumber(t.webrtcPacketsReceived),
        packetsLost: pickNumber(t.webrtcPacketsLost),
        framesDecoded: pickNumber(t.webrtcVideoFramesDecoded),
        totalDecodeTimeMs: pickNumber(t.webrtcVideoTotalDecodeTimeMs),
        totalProcessingDelayMs: pickNumber(t.webrtcVideoTotalProcessingDelayMs),
        framesDropped: pickNumber(t.webrtcFramesDropped),
        keyFramesDecoded: pickNumber(t.webrtcKeyFramesDecoded),
        nackCount: pickNumber(t.webrtcNackCount),
        jitterMs: pickNumber(t.webrtcJitterMs),
        fps: pickNumber(t.webrtcFps),
        jitterBufferDelayMs: pickNumber(t.webrtcJitterBufferDelayMs),
        videoJitterBufferMeanMs: pickNumber(t.webrtcVideoJitterBufferMeanMs),
        videoJitterBufferTargetMeanMs: pickNumber(t.webrtcVideoJitterBufferTargetMeanMs),
        videoJitterBufferMinimumMeanMs: pickNumber(t.webrtcVideoJitterBufferMinimumMeanMs),
        availableIncomingKbps: pickNumber(t.webrtcAvailableIncomingKbps),
        decoderImplementation: sanitizeLabel(t.decoderImplementation) ?? undefined,
        audioPacketsReceived: pickNumber(t.webrtcAudioPacketsReceived),
        audioPacketsLost: pickNumber(t.webrtcAudioPacketsLost),
        audioJitterMs: pickNumber(t.webrtcAudioJitterMs),
        audioJitterBufferMeanMs: pickNumber(t.webrtcAudioJitterBufferMeanMs),
        audioJitterBufferTargetMeanMs: pickNumber(t.webrtcAudioJitterBufferTargetMeanMs),
        audioJitterBufferMinimumMeanMs: pickNumber(t.webrtcAudioJitterBufferMinimumMeanMs),
        audioConcealmentEvents: pickNumber(t.webrtcAudioConcealmentEvents),
        audioPacketsDiscarded: pickNumber(t.webrtcAudioPacketsDiscarded),
        audioConcealedSamples: pickNumber(t.webrtcAudioConcealedSamples),
        audioTotalSamplesReceived: pickNumber(t.webrtcAudioTotalSamplesReceived),
        audioJitterPolicy: sanitizeLabel(t.webrtcAudioJitterPolicy) ?? undefined,
        audioContextBaseLatencyMs: pickNumber(a.audioContextBaseLatencyMs),
        audioContextOutputLatencyMs: pickNumber(a.audioContextOutputLatencyMs),
        avgHostProcessingLatencyMs: pickNumber(stats.avgHostProcessingLatencyMs),
        maxHostProcessingLatencyMs: pickNumber(stats.maxHostProcessingLatencyMs),
        avgStreamerProcessingTimeMs: pickNumber(stats.avgStreamerProcessingTimeMs),
    })
}

function healthSection(health: HealthSummary | null | undefined) {
    const domain = (value: DomainHealth | undefined) => ({
        level: pickEnum(value?.level, HEALTH_LEVELS) ?? "unknown",
        reasons: (value?.reasons ?? []).filter(reason => HEALTH_REASONS.indexOf(reason) !== -1),
    })
    return {
        network: domain(health?.network),
        decoder: domain(health?.decoder),
        audio: domain(health?.audio),
        host: domain(health?.host),
    }
}

export function buildDiagnosticsReport(input: DiagnosticsReportInput) {
    const events = (input.events ?? [])
        .filter(event => isDiagnosticEventCode(event?.code))
        .slice(-DEFAULT_DIAGNOSTICS_CAPACITY)
        .map(event => ({ t: pickNumber(event.t) ?? 0, code: event.code, detail: sanitizeDetail(event.detail) }))

    return {
        format: DIAGNOSTICS_FORMAT,
        build: DIAGNOSTICS_BUILD,
        generatedAt: new Date(pickNumber(input.generatedAt) ?? 0).toISOString(),
        connection: compact({
            state: pickEnum(input.connection?.state, CONNECTION_STATES),
            transport: pickEnum(input.connection?.transport, STRING_DETAIL_VALUES.transport),
            reconnectAttempts: pickNumber(input.connection?.reconnectAttempts),
            requestedTransport: pickEnum(input.connection?.requestedTransport, STRING_DETAIL_VALUES.transport),
            webRtcFallbackCount: pickNumber(input.connection?.webRtcFallbackCount),
        }),
        stats: statsSection(input.stats),
        samples: (input.samples ?? []).slice(-600).map(sample => ({
            t: pickNumber(sample.t) ?? 0,
            transport: pickEnum(sample.transport, STRING_DETAIL_VALUES.transport) ?? "unknown",
            visible: sample.visible === true,
            stats: statsSection(sample.stats),
        })),
        health: healthSection(input.health),
        audio: compact({
            state: pickEnum(input.audio?.state, AUDIO_STATES),
            preset: pickEnum(input.audio?.preset, STRING_DETAIL_VALUES.preset),
            outputSelectable: typeof input.audio?.outputSelectable === "boolean" ? input.audio.outputSelectable : undefined,
        }),
        quality: compact({
            profile: pickEnum(input.quality?.profile, STRING_DETAIL_VALUES.profile),
            configuredKbps: pickNumber(input.quality?.configuredKbps),
            sessionKbps: pickNumber(input.quality?.sessionKbps),
            recommendation: pickEnum(input.quality?.recommendation, STRING_DETAIL_VALUES.kind),
            targetKbps: pickNumber(input.quality?.targetKbps),
        }),
        input: compact({
            mode: pickEnum(input.input?.mode, STRING_DETAIL_VALUES.mode),
        }),
        events,
    }
}

export type DiagnosticsReport = ReturnType<typeof buildDiagnosticsReport>

export function serializeDiagnosticsReport(report: DiagnosticsReport): string {
    return JSON.stringify(report, null, 2)
}

/// Offer the report as a JSON file download. Uses only a Blob URL that is revoked again.
export function downloadDiagnosticsReport(report: DiagnosticsReport, doc: Document = document): string {
    const json = serializeDiagnosticsReport(report)
    const blob = new Blob([json], { type: "application/json" })
    const url = URL.createObjectURL(blob)
    const anchor = doc.createElement("a")
    anchor.href = url
    anchor.download = `moonlight-diagnostics-${report.generatedAt.replace(/[^0-9]/g, "").slice(0, 14)}.json`
    anchor.rel = "noopener"
    anchor.style.display = "none"
    doc.body.appendChild(anchor)
    anchor.click()
    doc.body.removeChild(anchor)
    setTimeout(() => URL.revokeObjectURL(url), 0)
    return anchor.download
}
