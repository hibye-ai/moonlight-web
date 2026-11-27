import { Pipe } from "../stream/pipeline/index.js"
import { AudioJitterPolicyMode } from "../stream/transport/audio_policy.js"
import { DiagnosticsBus } from "./diagnostics.js"

// F3 audio output: presets, observable output state, audio-only recovery and the optional
// standard output-device selector. Recovery only touches the browser-side output (audio
// element / AudioContext). It never stops, restarts or renegotiates the transport, the
// video or the host stream.

export type AudioPreset = "game" | "desktop" | "custom"
export const AUDIO_PRESETS: ReadonlyArray<AudioPreset> = ["game", "desktop", "custom"]

/// Presets are names for the existing receiver policies, not new settings:
/// game = "lowLatency" (historical default), desktop = "auto" (browser decides), custom.
export function presetFromPolicy(mode: AudioJitterPolicyMode | undefined | null): AudioPreset {
    if (mode === "auto") return "desktop"
    if (mode === "custom") return "custom"
    return "game"
}

export function policyFromPreset(preset: AudioPreset): AudioJitterPolicyMode {
    if (preset === "desktop") return "auto"
    if (preset === "custom") return "custom"
    return "lowLatency"
}

export type AudioOutputState = "playing" | "muted" | "autoplayBlocked" | "unavailable" | "transportLost" | "unknown"

export type AudioOutputObservation =
    { kind: "element", muted: boolean, paused: boolean, autoplayRejected: boolean, hasTrack: boolean, trackEnded: boolean } |
    { kind: "context", state: string }

/// Implemented by the audio pipes that own an actual output (AudioElementPlayer,
/// AudioContextBasePipe). Found by walking the pipe chain.
export interface AudioOutputPipe {
    getOutputState(): AudioOutputObservation | null
    recoverOutput(): Promise<void>
    getSinkTarget(): { setSinkId?: (id: string) => Promise<void> } | null
}

function isOutputPipe(pipe: any): pipe is AudioOutputPipe {
    return pipe != null && typeof pipe.getOutputState === "function" && typeof pipe.recoverOutput === "function"
}

export function collectAudioOutputs(player: Pipe | null | undefined): AudioOutputPipe[] {
    const outputs: AudioOutputPipe[] = []
    const seen = new Set<unknown>()
    let current: any = player
    while (current && !seen.has(current)) {
        seen.add(current)
        if (isOutputPipe(current)) {
            outputs.push(current)
        }
        current = typeof current.getBase === "function" ? current.getBase() : null
    }
    return outputs
}

export function classifyAudioOutputs(input: { hasPlayer: boolean, outputs: ReadonlyArray<AudioOutputObservation | null> }): AudioOutputState {
    if (!input.hasPlayer) return "unavailable"
    const outputs = input.outputs.filter((o): o is AudioOutputObservation => o != null)
    if (outputs.length === 0) return "unknown"

    const elements = outputs.filter((o): o is Extract<AudioOutputObservation, { kind: "element" }> => o.kind === "element")
    const contexts = outputs.filter((o): o is Extract<AudioOutputObservation, { kind: "context" }> => o.kind === "context")

    if (elements.some(e => e.trackEnded)) return "transportLost"
    if (contexts.some(c => c.state === "closed")) return "unavailable"
    if (elements.some(e => e.autoplayRejected && e.paused)) return "autoplayBlocked"
    if (contexts.some(c => c.state === "suspended")) return "autoplayBlocked"
    if (elements.some(e => e.muted)) return "muted"

    const elementPlaying = elements.some(e => !e.paused && !e.muted && e.hasTrack)
    const contextRunning = contexts.some(c => c.state === "running")
    if (elementPlaying || contextRunning) return "playing"
    return "unknown"
}

export function observeAudioOutput(player: Pipe | null | undefined): AudioOutputState {
    return classifyAudioOutputs({
        hasPlayer: player != null,
        outputs: collectAudioOutputs(player).map(output => safeObservation(output)),
    })
}

function safeObservation(output: AudioOutputPipe): AudioOutputObservation | null {
    try {
        return output.getOutputState()
    } catch {
        return null
    }
}

export type AudioRecoveryResult = {
    state: AudioOutputState
    result: "done" | "partial" | "error" | "unsupported"
    recovered: number
    failed: number
}

/// User-gesture audio recovery. Concurrent clicks share one attempt; repeated clicks are
/// harmless because each step (unmute, reattach the same stream, play, resume) is idempotent.
export class AudioRecovery {
    private getPlayer: () => Pipe | null
    private diagnostics: DiagnosticsBus | null
    private inFlight: Promise<AudioRecoveryResult> | null = null

    constructor(getPlayer: () => Pipe | null, diagnostics?: DiagnosticsBus | null) {
        this.getPlayer = getPlayer
        this.diagnostics = diagnostics ?? null
    }

    recover(trigger: string = "button"): Promise<AudioRecoveryResult> {
        if (this.inFlight) {
            return this.inFlight
        }
        this.inFlight = this.run(trigger).finally(() => {
            this.inFlight = null
        })
        return this.inFlight
    }

    private async run(trigger: string): Promise<AudioRecoveryResult> {
        const player = this.getPlayer()
        let outcome: AudioRecoveryResult
        if (!player) {
            outcome = { state: "unavailable", result: "unsupported", recovered: 0, failed: 0 }
        } else {
            const outputs = collectAudioOutputs(player)
            let recovered = 0
            let failed = 0
            for (const output of outputs) {
                try {
                    await output.recoverOutput()
                    recovered += 1
                } catch {
                    failed += 1
                }
            }
            const state = classifyAudioOutputs({ hasPlayer: true, outputs: outputs.map(output => safeObservation(output)) })
            const result = outputs.length === 0 ? "unsupported" : failed === 0 ? "done" : recovered > 0 ? "partial" : "error"
            outcome = { state, result, recovered, failed }
        }
        this.diagnostics?.record("audio.recover", { trigger, result: outcome.result, state: outcome.state })
        return outcome
    }
}

// -- Output device (optional, standard setSinkId only; no OS-level changes)

export type AudioOutputDevice = { id: string, label: string }

const MAX_OUTPUT_DEVICES = 32

export function supportsOutputSelection(outputs: ReadonlyArray<AudioOutputPipe>): boolean {
    return outputs.some(output => typeof output.getSinkTarget()?.setSinkId === "function")
}

export async function listOutputDevices(
    mediaDevices: { enumerateDevices?: () => Promise<ReadonlyArray<{ kind: string, deviceId: string, label: string }>> } | null | undefined,
    fallbackLabel: (index: number, id: string) => string = (index, id) => id === "default" ? "Default" : `Output ${index + 1}`,
): Promise<AudioOutputDevice[]> {
    if (!mediaDevices || typeof mediaDevices.enumerateDevices !== "function") {
        return []
    }
    let devices: ReadonlyArray<{ kind: string, deviceId: string, label: string }>
    try {
        devices = await mediaDevices.enumerateDevices()
    } catch {
        return []
    }
    return devices
        .filter(device => device.kind === "audiooutput" && typeof device.deviceId === "string")
        .slice(0, MAX_OUTPUT_DEVICES)
        .map((device, index) => ({
            id: device.deviceId,
            label: (typeof device.label === "string" && device.label.trim().length > 0) ? device.label.trim().slice(0, 80) : fallbackLabel(index, device.deviceId),
        }))
}

export async function setOutputDevice(outputs: ReadonlyArray<AudioOutputPipe>, deviceId: string, listed: ReadonlyArray<AudioOutputDevice>): Promise<"done" | "unsupported" | "error"> {
    if (!listed.some(device => device.id === deviceId)) {
        return "unsupported"
    }
    const targets = outputs
        .map(output => output.getSinkTarget())
        .filter((target): target is { setSinkId: (id: string) => Promise<void> } => typeof target?.setSinkId === "function")
    if (targets.length === 0) {
        return "unsupported"
    }
    try {
        for (const target of targets) {
            await target.setSinkId(deviceId)
        }
        return "done"
    } catch {
        return "error"
    }
}
