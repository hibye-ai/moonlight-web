// MW-08 / AR-06: audio receiver jitter policy.
//
// The low-latency preference (jitterBufferTarget = 0, playoutDelayHint = 0) was applied to
// audio and video alike on every receiver. It may reduce the slack available to absorb
// network variation, but that has NOT been measured, and a hint does not guarantee a zero
// effective jitter buffer. New/missing settings now use browser-managed buffering.
// Explicit "lowLatency" and custom settings remain available and are preserved.
// Compare `webrtcAudio*` stats (see rtc_stats.ts) to assess the latency/stability tradeoff.
//
// Units (AR-06). The two receiver properties do NOT share a unit:
//   * `RTCRtpReceiver.jitterBufferTarget` (and the WebRTC-Stats `jitterBufferTarget` member)
//     is a DOMHighResTimeStamp in MILLISECONDS. https://w3c.github.io/webrtc-stats/#dom-rtcinboundrtpstreamstats-jitterbuffertarget
//   * `RTCRtpReceiver.playoutDelayHint` is in SECONDS (it predates jitterBufferTarget and was
//     kept for compatibility). The blink-dev discussion that shipped jitterBufferTarget says
//     so explicitly: "jitterBuffferTarget is in milliseconds unit while playoutDelayHint is
//     using seconds".
// Converting a millisecond setting with `/1000` and assigning it to both (what this file did
// before AR-06) therefore asks Chromium for a 0.06 ms target when the user picked 60 ms. The
// two fields are now named after their unit and converted independently.
//
// Video policy is deliberately NOT configurable here; the audit recommends holding it fixed
// while the audio variable is isolated.

export type AudioJitterPolicyMode = "lowLatency" | "auto" | "custom"

export type AudioJitterPolicy = {
    mode: AudioJitterPolicyMode
    /// Only used by "custom"; milliseconds of target jitter buffer delay.
    targetMs?: number
}

export const DEFAULT_AUDIO_JITTER_POLICY: AudioJitterPolicy = { mode: "auto" }

/// Upper bound for a custom target. Above this the added delay is not plausibly useful for
/// interactive streaming; the cap only prevents a typo from buffering a second of audio.
export const MAX_AUDIO_JITTER_TARGET_MS = 500

/// A receiver policy with the unit of each field spelled out, so a caller cannot apply a
/// millisecond value to the seconds property (or the reverse) by accident.
export type ResolvedReceiverPolicy = {
    /// Short stable name recorded in the stats so an A/B run is self-describing.
    name: string
    /// MILLISECONDS to assign to `receiver.jitterBufferTarget`, or null to leave it alone.
    /// This is the value the browser would read back from the property.
    jitterBufferTargetMs: number | null
    /// SECONDS to assign to `receiver.playoutDelayHint`, or null to leave it alone.
    playoutDelayHintSeconds: number | null
}

export function resolveAudioReceiverPolicy(policy: AudioJitterPolicy | undefined | null): ResolvedReceiverPolicy {
    const effective = policy ?? DEFAULT_AUDIO_JITTER_POLICY

    switch (effective.mode) {
        case "auto":
            // Let the browser pick. This is the variable to test against "lowLatency".
            return { name: "auto", jitterBufferTargetMs: null, playoutDelayHintSeconds: null }
        case "custom": {
            const targetMs = clampTarget(effective.targetMs)
            return {
                name: `custom:${targetMs}ms`,
                // Milliseconds, unconverted: 60 stays 60.
                jitterBufferTargetMs: targetMs,
                // Seconds, because that is what playoutDelayHint takes: 60 -> 0.06.
                playoutDelayHintSeconds: targetMs / 1000,
            }
        }
        case "lowLatency":
        default:
            // Previous behaviour, byte for byte.
            return { name: "lowLatency", jitterBufferTargetMs: 0, playoutDelayHintSeconds: 0 }
    }
}

export function clampTarget(targetMs: number | undefined): number {
    if (typeof targetMs != "number" || !Number.isFinite(targetMs) || targetMs < 0) {
        return 0
    }
    return Math.min(targetMs, MAX_AUDIO_JITTER_TARGET_MS)
}

/// Apply a resolved policy to anything shaped like an RTCRtpReceiver. Kept separate from
/// the transport so the rules are testable without a peer connection.
export function applyReceiverPolicy(receiver: any, policy: ResolvedReceiverPolicy): void {
    if (policy.jitterBufferTargetMs != null) {
        // Milliseconds in, milliseconds out.
        receiver.jitterBufferTarget = policy.jitterBufferTargetMs
    }
    if (policy.playoutDelayHintSeconds != null && "playoutDelayHint" in receiver) {
        // Seconds in, seconds out. Not derived from the millisecond field at this point:
        // the conversion happens once, in resolveAudioReceiverPolicy().
        receiver.playoutDelayHint = policy.playoutDelayHintSeconds
    }
}

/// Change the policy of a receiver that is already playing (session preset switch).
/// Unlike applyReceiverPolicy(), a null field actively clears an earlier forced value so
/// "auto" really hands control back to the browser. Returns false when the receiver does
/// not expose jitterBufferTarget, in which case nothing is changed and the preset only
/// applies on the next connection.
export function applyReceiverPolicyLive(receiver: any, policy: ResolvedReceiverPolicy): boolean {
    if (!receiver || !("jitterBufferTarget" in receiver)) {
        return false
    }
    try {
        receiver.jitterBufferTarget = policy.jitterBufferTargetMs
        if ("playoutDelayHint" in receiver) {
            receiver.playoutDelayHint = policy.playoutDelayHintSeconds
        }
        return true
    } catch {
        return false
    }
}
