import { DiagnosticsBus } from "./diagnostics.js"

// F1 connection UX.
//
// The Stream keeps owning the transport and its bounded control-socket retries. This
// controller only observes the Stream lifecycle and adds what the user sees and can do:
// a visible state, cancel, an explicit reconnect, and recovery on network/tab return.
// It never reconnects automatically after a manual stop, a user cancel, an auth denial
// or a host-side termination; the backend stays authoritative for revocation.

export type ConnectionState = "connecting" | "connected" | "reconnecting" | "offline" | "authRequired" | "stalled" | "failed" | "stopped"

export type ConnectionReason = "cancelled" | "exhausted" | "terminated" | "fatal" | "authDenied" | "offline" | "stalled" | "decoderStalled" | "stopped" | null

export type StreamLifecycle =
    { phase: "connecting" } |
    { phase: "connected" } |
    { phase: "socketClosed" } |
    { phase: "retryScheduled", attempt: number, max: number, delayMs: number } |
    { phase: "retryExhausted" } |
    { phase: "terminated", code?: number } |
    { phase: "fatal" } |
    { phase: "stopped" } |
    { phase: "cancelled" }

export type ProgressVerdict = "unknown" | "progressing" | "videoIdle" | "decoderStalled" | "stalled"

export interface ConnectionTarget {
    reconnect(reason: string): Promise<boolean>
    cancelReconnect(): void
    setAutoReconnectEnabled(enabled: boolean): void
    releaseInput(reason: string): void
}

export type AuthProbeResult = "ok" | "denied" | "unreachable"

export type ConnectionDeps = {
    target: ConnectionTarget
    probeAuth: () => Promise<AuthProbeResult>
    isOnline: () => boolean
    isVisible: () => boolean
    diagnostics?: DiagnosticsBus | null
    setTimeout?: (callback: () => void, ms: number) => number
    clearTimeout?: (id: number) => void
    now?: () => number
}

export type ConnectionView = {
    state: ConnectionState
    reason: ConnectionReason
    attempt: number
    maxAttempts: number
    canReconnect: boolean
    canCancel: boolean
    verdict: ProgressVerdict
}

export type ConnectionListener = (view: ConnectionView) => void

/// Automatic recoveries (network back online / tab visible again) per failure episode.
export const MAX_AUTO_RECOVERIES = 3
/// Minimum spacing between two automatic recoveries.
export const AUTO_RECOVERY_SPACING_MS = 5000
/// Delay after the tab becomes visible, so a resuming network has a moment to settle.
export const VISIBLE_RECOVERY_DELAY_MS = 1000

export class ConnectionController {
    private deps: ConnectionDeps
    private setTimer: (callback: () => void, ms: number) => number
    private clearTimer: (id: number) => void
    private now: () => number

    private state: ConnectionState = "connecting"
    private reason: ConnectionReason = null
    private attempt = 0
    private maxAttempts = 0
    private verdict: ProgressVerdict = "unknown"

    private everConnected = false
    private manualStop = false
    private userCancelled = false
    private authDenied = false
    private hostEnded = false

    private probedThisEpisode = false
    private probeInFlight: Promise<AuthProbeResult> | null = null
    private autoRecoveries = 0
    private lastAutoRecoveryAt: number | null = null
    private visibleTimer: number | null = null
    private disposed = false
    private recoveryGeneration = 0

    private listeners: ConnectionListener[] = []

    constructor(deps: ConnectionDeps) {
        this.deps = deps
        this.setTimer = deps.setTimeout ?? ((callback, ms) => window.setTimeout(callback, ms))
        this.clearTimer = deps.clearTimeout ?? (id => window.clearTimeout(id))
        this.now = deps.now ?? (() => Date.now())
    }

    getView(): ConnectionView {
        return {
            state: this.state,
            reason: this.reason,
            attempt: this.attempt,
            maxAttempts: this.maxAttempts,
            canReconnect: this.state !== "stopped",
            canCancel: this.state === "reconnecting",
            verdict: this.verdict,
        }
    }

    subscribe(listener: ConnectionListener): () => void {
        this.listeners.push(listener)
        return () => {
            const index = this.listeners.indexOf(listener)
            if (index !== -1) this.listeners.splice(index, 1)
        }
    }

    private setState(state: ConnectionState, reason: ConnectionReason, detail?: Record<string, unknown>) {
        const changed = state !== this.state || reason !== this.reason
        const previous = this.state
        this.state = state
        this.reason = reason
        if (changed) {
            this.recordTransition(previous, state, reason, detail)
        }
        this.emit()
    }

    private recordTransition(previous: ConnectionState, state: ConnectionState, reason: ConnectionReason, detail?: Record<string, unknown>) {
        const diagnostics = this.deps.diagnostics
        if (!diagnostics) return
        if (state === "connected") {
            diagnostics.record(previous === "connecting" ? "connection.connected" : "connection.recovered", { state: previous })
        } else if (state === "failed" && reason === "cancelled") {
            diagnostics.record("connection.cancelled")
        } else {
            diagnostics.record(`connection.${state}`, { ...(detail ?? {}), reason: reason ?? undefined })
        }
    }

    private emit() {
        const view = this.getView()
        for (const listener of this.listeners.slice()) {
            try {
                listener(view)
            } catch {
                // UI listeners must not break the state machine.
            }
        }
    }

    private autoRecoveryBlocked(): boolean {
        return this.disposed || this.manualStop || this.userCancelled || this.authDenied || this.hostEnded
    }

    // -- Stream lifecycle

    onLifecycle(event: StreamLifecycle) {
        if (this.disposed) return
        if (this.manualStop && event.phase !== "stopped") return
        if ((this.authDenied || this.userCancelled || this.hostEnded) && event.phase !== "stopped") return

        switch (event.phase) {
            case "connecting":
                if (this.state === "authRequired") return
                this.setState(this.everConnected ? "reconnecting" : "connecting", null, { attempt: this.attempt })
                return
            case "connected":
                this.everConnected = true
                this.attempt = 0
                this.maxAttempts = 0
                this.verdict = "unknown"
                this.probedThisEpisode = false
                this.autoRecoveries = 0
                this.lastAutoRecoveryAt = null
                this.userCancelled = false
                this.hostEnded = false
                this.setState("connected", null)
                return
            case "socketClosed":
                this.deps.target.releaseInput("disconnect")
                if (this.state === "failed" || this.state === "authRequired" || this.state === "offline") {
                    return
                }
                if (!this.deps.isOnline()) {
                    this.enterOffline()
                    return
                }
                this.setState("reconnecting", null)
                void this.probeAfterFailure()
                return
            case "retryScheduled":
                if (this.state === "authRequired" || this.state === "offline" || this.userCancelled) return
                this.attempt = event.attempt
                this.maxAttempts = event.max
                this.setState("reconnecting", null, { attempt: event.attempt, max: event.max, delayMs: event.delayMs })
                return
            case "retryExhausted":
                if (this.state === "authRequired" || this.state === "offline") return
                this.setState("failed", "exhausted")
                return
            case "terminated":
                this.hostEnded = true
                this.deps.target.releaseInput("disconnect")
                this.setState("failed", "terminated", { code: event.code })
                return
            case "fatal":
                this.hostEnded = true
                this.deps.target.releaseInput("disconnect")
                this.setState("failed", "fatal")
                void this.probeAfterFailure()
                return
            case "cancelled":
                if (this.state !== "failed") this.setState("failed", "cancelled")
                return
            case "stopped":
                this.markManualStop()
                return
        }
    }

    private async probeAfterFailure() {
        if (this.probedThisEpisode || this.probeInFlight) return
        const generation = this.recoveryGeneration
        this.probedThisEpisode = true
        const result = await this.probe()
        if (generation !== this.recoveryGeneration || this.disposed || this.manualStop) return
        if (result === "denied") {
            this.enterAuthRequired()
        } else if (result === "unreachable" && !this.deps.isOnline()) {
            this.enterOffline()
        }
    }

    private async probe(): Promise<AuthProbeResult> {
        if (this.probeInFlight) return this.probeInFlight
        this.probeInFlight = this.deps.probeAuth().catch((): AuthProbeResult => "unreachable")
        try {
            return await this.probeInFlight
        } finally {
            this.probeInFlight = null
        }
    }

    private enterAuthRequired() {
        this.recoveryGeneration += 1
        this.authDenied = true
        this.deps.target.setAutoReconnectEnabled(false)
        this.deps.target.cancelReconnect()
        this.deps.target.releaseInput("disconnect")
        this.setState("authRequired", "authDenied")
    }

    private enterOffline() {
        this.recoveryGeneration += 1
        this.deps.target.releaseInput("offline")
        this.deps.target.setAutoReconnectEnabled(false)
        this.deps.target.cancelReconnect()
        this.setState("offline", "offline")
    }

    // -- User actions

    cancel() {
        if (this.manualStop) return
        this.recoveryGeneration += 1
        this.userCancelled = true
        this.deps.target.setAutoReconnectEnabled(false)
        this.clearVisibleTimer()
        this.deps.target.cancelReconnect()
        this.setState("failed", "cancelled")
    }

    markManualStop() {
        this.recoveryGeneration += 1
        this.manualStop = true
        this.deps.target.setAutoReconnectEnabled(false)
        this.deps.target.releaseInput("disconnect")
        this.clearVisibleTimer()
        this.setState("stopped", "stopped")
    }

    async manualReconnect(trigger: string = "button") {
        if (this.manualStop || this.disposed) return
        const generation = ++this.recoveryGeneration
        this.deps.diagnostics?.record("connection.manualReconnect", { trigger })

        if (this.authDenied) {
            this.setState("reconnecting", null)
            if (generation !== this.recoveryGeneration || this.manualStop || this.disposed) return
            const result = await this.probe()
            if (generation !== this.recoveryGeneration || this.manualStop || this.disposed) return
            if (result !== "ok") {
                this.setState("authRequired", "authDenied")
                return
            }
            this.authDenied = false
        }

        this.userCancelled = false
        this.hostEnded = false
        this.probedThisEpisode = false
        this.autoRecoveries = 0
        this.clearVisibleTimer()
        this.deps.target.setAutoReconnectEnabled(true)
        this.setState("reconnecting", null)
        await this.deps.target.reconnect("manual")
    }

    // -- Environment

    onOnline() {
        this.deps.diagnostics?.record("connection.online", { online: true })
        if (this.autoRecoveryBlocked()) return
        if (this.state === "offline") {
            this.deps.target.setAutoReconnectEnabled(true)
            this.tryAutoRecovery("online")
        } else if (this.state === "failed" && this.reason === "exhausted") {
            this.tryAutoRecovery("online")
        }
    }

    onOffline() {
        if (this.manualStop || this.disposed) return
        if (this.state === "authRequired") return
        this.enterOffline()
    }

    onVisibilityChange(visible: boolean) {
        if (this.disposed) return
        this.deps.diagnostics?.record(visible ? "visibility.visible" : "visibility.hidden", { visible })
        if (!visible) {
            this.clearVisibleTimer()
            return
        }
        if (this.autoRecoveryBlocked()) return
        const recoverable = (this.state === "failed" && this.reason === "exhausted") || this.state === "offline"
        if (!recoverable || this.visibleTimer != null) return
        this.visibleTimer = this.setTimer(() => {
            this.visibleTimer = null
            if (this.autoRecoveryBlocked() || !this.deps.isVisible() || !this.deps.isOnline()) return
            if ((this.state === "failed" && this.reason === "exhausted") || this.state === "offline") {
                this.deps.target.setAutoReconnectEnabled(true)
                this.tryAutoRecovery("visible")
            }
        }, VISIBLE_RECOVERY_DELAY_MS)
    }

    private tryAutoRecovery(trigger: "online" | "visible") {
        if (this.autoRecoveries >= MAX_AUTO_RECOVERIES) return
        const now = this.now()
        if (this.lastAutoRecoveryAt != null && now - this.lastAutoRecoveryAt < AUTO_RECOVERY_SPACING_MS) return
        this.autoRecoveries += 1
        this.lastAutoRecoveryAt = now
        this.probedThisEpisode = false
        this.setState("reconnecting", null, { attempt: this.autoRecoveries })
        void this.deps.target.reconnect(trigger)
    }

    private clearVisibleTimer() {
        if (this.visibleTimer != null) {
            this.clearTimer(this.visibleTimer)
            this.visibleTimer = null
        }
    }

    // -- Media progress

    onMediaVerdict(verdict: ProgressVerdict) {
        if (this.disposed) return
        const previous = this.verdict
        this.verdict = verdict
        if (verdict !== previous) {
            this.deps.diagnostics?.record("media.progress", { verdict })
        }
        if (this.state === "connected" && (verdict === "stalled" || verdict === "decoderStalled")) {
            this.setState("stalled", verdict)
        } else if (this.state === "stalled" && (verdict === "progressing" || verdict === "videoIdle")) {
            this.setState("connected", null)
        } else if (verdict !== previous) {
            this.emit()
        }
    }

    dispose() {
        this.recoveryGeneration += 1
        this.disposed = true
        this.clearVisibleTimer()
        this.listeners = []
    }
}

// -- Media progress

export type MediaProgressSample = {
    at: number
    videoPackets: number | null
    audioPackets: number | null
    framesDecoded: number | null
    /// Messages from the host that are independent of the picture (RTT probes etc.).
    hostSignals: number | null
}

type SourceState = {
    last: number | null
    lastAdvanceAt: number | null
    everAdvanced: boolean
}

function freshSource(): SourceState {
    return { last: null, lastAdvanceAt: null, everAdvanced: false }
}

/// Classifies media progress from counters that only grow while data actually arrives.
/// A static desktop legitimately sends little or no video, so "video stopped" alone is
/// reported as videoIdle; "stalled" needs every source that has shown life to stop,
/// including at least one source that does not depend on the picture changing.
export class MediaProgressMonitor {
    private stallMs: number
    private decoderStallMs: number
    private video = freshSource()
    private audio = freshSource()
    private host = freshSource()
    private frames = freshSource()

    constructor(options?: { stallMs?: number, decoderStallMs?: number }) {
        this.stallMs = options?.stallMs ?? 8000
        this.decoderStallMs = options?.decoderStallMs ?? 6000
    }

    reset() {
        this.video = freshSource()
        this.audio = freshSource()
        this.host = freshSource()
        this.frames = freshSource()
    }

    sample(sample: MediaProgressSample): ProgressVerdict {
        const pairs: Array<[SourceState, number | null]> = [
            [this.video, sample.videoPackets],
            [this.audio, sample.audioPackets],
            [this.host, sample.hostSignals],
            [this.frames, sample.framesDecoded],
        ]

        const decreased = pairs.some(([source, value]) => value != null && source.last != null && value < source.last)
        if (decreased) {
            // Reconnect or rollover: rebaseline every source on this sample.
            for (const [source, value] of pairs) {
                source.last = value
                source.lastAdvanceAt = value != null ? sample.at : null
                source.everAdvanced = false
            }
            return "unknown"
        }

        for (const [source, value] of pairs) {
            if (value == null) continue
            if (source.last == null) {
                source.last = value
                source.lastAdvanceAt = sample.at
            } else if (value > source.last) {
                source.last = value
                source.lastAdvanceAt = sample.at
                source.everAdvanced = true
            }
        }

        const sources = [this.video, this.audio, this.host]
        const live = sources.filter(source => source.everAdvanced)
        if (live.length === 0) {
            return "unknown"
        }

        const idle = (source: SourceState) => source.lastAdvanceAt != null && sample.at - source.lastAdvanceAt >= this.stallMs
        const nonVideoLive = live.filter(source => source !== this.video)

        if (live.every(idle)) {
            return nonVideoLive.length > 0 ? "stalled" : "videoIdle"
        }

        const videoRecent = this.video.everAdvanced && this.video.lastAdvanceAt != null && sample.at - this.video.lastAdvanceAt < this.decoderStallMs
        if (videoRecent && this.frames.last != null && this.frames.lastAdvanceAt != null && sample.at - this.frames.lastAdvanceAt >= this.decoderStallMs) {
            return "decoderStalled"
        }

        if (this.video.everAdvanced && idle(this.video)) {
            return "videoIdle"
        }

        return "progressing"
    }
}
