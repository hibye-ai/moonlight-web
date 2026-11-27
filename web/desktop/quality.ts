import { StreamStatsData } from "../stream/stats.js"

// F4 quality.
//
// Two separate things, labelled separately in the UI:
//   * the existing next-start adaptive bitrate (settings_profiles.ts), which only changes
//     the bitrate requested at the NEXT stream start, now keyed per user-selected network
//     profile;
//   * a live monitor that only RECOMMENDS a session bitrate. Applying it restarts the
//     stream (there is no dynamic bitrate API on this path), always below the configured
//     ceiling.
// The network profile is chosen by the user. Nothing here infers "LAN" from addresses.

export type NetworkProfile = "lan" | "remote"
export const NETWORK_PROFILES: ReadonlyArray<NetworkProfile> = ["lan", "remote"]

export function isNetworkProfile(value: unknown): value is NetworkProfile {
    return value === "lan" || value === "remote"
}

export type QualityThresholds = {
    goodRtt: number
    badRtt: number
    severeRtt: number
    goodLoss: number
    badLoss: number
    severeLoss: number
    badDropped: number
    severeDropped: number
}

const HISTORICAL_THRESHOLDS: QualityThresholds = {
    goodRtt: 80, badRtt: 140, severeRtt: 250,
    goodLoss: 0.01, badLoss: 0.04, severeLoss: 0.10,
    badDropped: 3, severeDropped: 10,
}

export const QUALITY_THRESHOLDS: { default: QualityThresholds, remote: QualityThresholds, lan: QualityThresholds } = {
    // No profile chosen: exactly the rules the next-start advisor always used.
    default: HISTORICAL_THRESHOLDS,
    remote: { ...HISTORICAL_THRESHOLDS },
    // A wired/Wi-Fi LAN should be far below these; the user opted into stricter rules.
    lan: {
        goodRtt: 20, badRtt: 60, severeRtt: 150,
        goodLoss: 0.005, badLoss: 0.02, severeLoss: 0.08,
        badDropped: 3, severeDropped: 10,
    },
}

export function thresholdsFor(profile: NetworkProfile | null | undefined): QualityThresholds {
    return profile ? QUALITY_THRESHOLDS[profile] : QUALITY_THRESHOLDS.default
}

export type QualityMetrics = {
    rtt: number | null
    lossRate: number | null
    droppedDelta: number | null
}

export type QualityClass = "good" | "neutral" | "bad" | "severe" | "insufficient"

export function classifyQualitySample(metrics: QualityMetrics, t: QualityThresholds): QualityClass {
    const { rtt, lossRate, droppedDelta } = metrics
    if (rtt == null && lossRate == null && droppedDelta == null) {
        return "insufficient"
    }
    const dropped = droppedDelta ?? 0
    const severe = (rtt != null && rtt >= t.severeRtt) || (lossRate != null && lossRate >= t.severeLoss) || dropped >= t.severeDropped
    if (severe) return "severe"
    const bad = (rtt != null && rtt >= t.badRtt) || (lossRate != null && lossRate >= t.badLoss) || dropped >= t.badDropped
    if (bad) return "bad"
    const good = (rtt == null || rtt < t.goodRtt) && (lossRate == null || lossRate < t.goodLoss) && dropped === 0
    return good ? "good" : "neutral"
}

function statNumber(value: unknown): number | null {
    return typeof value === "number" && Number.isFinite(value) ? value : null
}

/// Turns cumulative counters into per-sample metrics. A decreasing counter (reconnect,
/// rollover) yields no delta for that sample instead of a negative one.
export class QualityMetricsTracker {
    private lastReceived: number | null = null
    private lastLost: number | null = null
    private lastDropped: number | null = null

    reset() {
        this.lastReceived = null
        this.lastLost = null
        this.lastDropped = null
    }

    next(stats: StreamStatsData): { metrics: QualityMetrics, counterReset: boolean } {
        const transport = stats.transport ?? {}
        const received = statNumber(transport.webrtcPacketsReceived)
        const lost = statNumber(transport.webrtcPacketsLost)
        const dropped = statNumber(transport.webrtcFramesDropped)
        const rtt = statNumber(transport.webrtcRttMs) ?? statNumber(stats.browserRtt) ?? statNumber(stats.streamerRttMs)

        const counterReset =
            (received != null && this.lastReceived != null && received < this.lastReceived) ||
            (lost != null && this.lastLost != null && lost < this.lastLost) ||
            (dropped != null && this.lastDropped != null && dropped < this.lastDropped)

        let lossRate: number | null = null
        let droppedDelta: number | null = null
        if (!counterReset) {
            if (received != null && lost != null && this.lastReceived != null && this.lastLost != null) {
                const receivedDelta = received - this.lastReceived
                const lostDelta = lost - this.lastLost
                if (receivedDelta + lostDelta >= 30) lossRate = lostDelta / (receivedDelta + lostDelta)
            }
            if (dropped != null && this.lastDropped != null) {
                droppedDelta = dropped - this.lastDropped
            }
        }

        this.lastReceived = received
        this.lastLost = lost
        this.lastDropped = dropped

        return { metrics: { rtt, lossRate, droppedDelta }, counterReset }
    }
}

export type QualityRecommendationKind = "none" | "lower" | "raise" | "insufficient"

export type QualityRecommendation = {
    kind: QualityRecommendationKind
    targetKbps: number | null
    currentKbps: number
    ceilingKbps: number
    manual: boolean
    reasons: string[]
}

export type LiveQualityOptions = {
    ceilingKbps: number
    currentKbps: number
    profile: NetworkProfile | null
    now?: () => number
    /// After a change (apply/reconnect) ignore samples this long.
    settleMs?: number
    /// No raise until this long after the last bad sample.
    raiseCooldownMs?: number
    /// Consecutive bad samples before suggesting a step down.
    lowerAfter?: number
    /// Consecutive good samples before suggesting a step up.
    raiseAfter?: number
    floorKbps?: number
}

const STEP_DOWN = 0.75
const STEP_DOWN_SEVERE = 0.6
const STEP_UP = 1.15
const ROUND_KBPS = 500

function roundDown(kbps: number): number {
    return Math.floor(kbps / ROUND_KBPS) * ROUND_KBPS
}
function roundUp(kbps: number): number {
    return Math.ceil(kbps / ROUND_KBPS) * ROUND_KBPS
}

/// Live, advisory session bitrate monitor. Bounded state (counters only, no history),
/// slow to raise, quick only to suggest lowering after sustained trouble, with hysteresis:
/// a raise is withdrawn by any bad sample, a "lower" is withdrawn only after recovery.
export class LiveQualityMonitor {
    private ceiling: number
    private current: number
    private profile: NetworkProfile | null
    private now: () => number
    private settleMs: number
    private raiseCooldownMs: number
    private lowerAfter: number
    private raiseAfter: number
    private floor: number

    private tracker = new QualityMetricsTracker()
    private badStreak = 0
    private goodStreak = 0
    private sawSevere = false
    private lastBadAt: number | null = null
    private lastChangeAt: number
    private manual = false
    private recommendation: QualityRecommendation

    constructor(options: LiveQualityOptions) {
        this.ceiling = Math.max(1, Math.round(options.ceilingKbps))
        this.current = Math.min(this.ceiling, Math.max(1, Math.round(options.currentKbps)))
        this.profile = options.profile
        this.now = options.now ?? (() => Date.now())
        this.settleMs = options.settleMs ?? 20000
        this.raiseCooldownMs = options.raiseCooldownMs ?? 120000
        this.lowerAfter = Math.max(1, options.lowerAfter ?? 3)
        this.raiseAfter = Math.max(1, options.raiseAfter ?? 15)
        this.floor = Math.min(options.floorKbps ?? 500, this.ceiling)
        this.lastChangeAt = this.now()
        this.recommendation = this.make("insufficient", null, [])
    }

    private make(kind: QualityRecommendationKind, targetKbps: number | null, reasons: string[]): QualityRecommendation {
        return { kind, targetKbps, currentKbps: this.current, ceilingKbps: this.ceiling, manual: this.manual, reasons }
    }

    getRecommendation(): QualityRecommendation {
        return { ...this.recommendation, reasons: this.recommendation.reasons.slice() }
    }

    setProfile(profile: NetworkProfile | null) {
        this.profile = profile
        this.badStreak = 0
        this.goodStreak = 0
        this.sawSevere = false
        this.lastBadAt = null
        this.lastChangeAt = this.now()
        this.tracker.reset()
        this.recommendation = this.make("insufficient", null, [])
    }

    /// The session bitrate changed (applied recommendation or manual override + reconnect).
    setCurrent(kbps: number, options: { manual: boolean }) {
        this.current = Math.min(this.ceiling, Math.max(this.floor, Math.round(kbps)))
        this.manual = options.manual
        this.lastChangeAt = this.now()
        this.badStreak = 0
        this.goodStreak = 0
        this.sawSevere = false
        this.tracker.reset()
        this.recommendation = this.make("none", null, [])
    }

    sample(stats: StreamStatsData): QualityRecommendation {
        const { metrics, counterReset } = this.tracker.next(stats)
        const now = this.now()

        if (counterReset) {
            this.badStreak = 0
            this.goodStreak = 0
            this.recommendation = this.make(this.recommendation.kind === "insufficient" ? "insufficient" : "none", null, [])
            return this.getRecommendation()
        }

        if (now - this.lastChangeAt < this.settleMs) {
            this.badStreak = 0
            this.goodStreak = 0
            this.recommendation = this.make("none", null, [])
            return this.getRecommendation()
        }

        const quality = classifyQualitySample(metrics, thresholdsFor(this.profile))
        const reasons = this.reasonsFor(metrics)

        if (quality === "insufficient") {
            // No evidence is neither good nor bad: keep streaks and the current advice.
            return this.getRecommendation()
        }

        if (quality === "bad" || quality === "severe") {
            this.badStreak += 1
            this.goodStreak = 0
            this.sawSevere = this.sawSevere || quality === "severe"
            this.lastBadAt = now
        } else if (quality === "good") {
            this.goodStreak += 1
            this.badStreak = 0
            this.sawSevere = false
        } else {
            this.goodStreak = 0
            this.badStreak = 0
            this.sawSevere = false
        }

        const previous = this.recommendation.kind

        if (this.badStreak >= this.lowerAfter) {
            const factor = this.sawSevere ? STEP_DOWN_SEVERE : STEP_DOWN
            const target = Math.max(this.floor, roundDown(this.current * factor))
            if (target < this.current) {
                this.recommendation = this.make("lower", target, reasons)
                return this.getRecommendation()
            }
        }

        if (previous === "lower" && this.goodStreak < this.lowerAfter) {
            // Keep the advice until the trouble has clearly passed (hysteresis).
            this.recommendation = { ...this.recommendation, currentKbps: this.current, manual: this.manual }
            return this.getRecommendation()
        }

        const cooledDown = this.lastBadAt == null || now - this.lastBadAt >= this.raiseCooldownMs
        if (this.goodStreak >= this.raiseAfter && cooledDown && this.current < this.ceiling) {
            let target = Math.min(this.ceiling, roundUp(this.current * STEP_UP))
            if (target <= this.current) target = Math.min(this.ceiling, this.current + ROUND_KBPS)
            this.recommendation = this.make("raise", target, [])
            return this.getRecommendation()
        }

        this.recommendation = this.make("none", null, quality === "bad" || quality === "severe" ? reasons : [])
        return this.getRecommendation()
    }

    private reasonsFor(metrics: QualityMetrics): string[] {
        const t = thresholdsFor(this.profile)
        const reasons: string[] = []
        if (metrics.rtt != null && metrics.rtt >= t.badRtt) reasons.push("highRtt")
        if (metrics.lossRate != null && metrics.lossRate >= t.badLoss) reasons.push("packetLoss")
        if (metrics.droppedDelta != null && metrics.droppedDelta >= t.badDropped) reasons.push("framesDropped")
        return reasons
    }
}
