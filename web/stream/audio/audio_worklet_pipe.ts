import { globalObject } from "../../util.js";
import { buildUrl } from "../../config_.js";
import { Logger } from "../log.js";
import { Pipe, PipeInfo } from "../pipeline/index.js";
import { addPipePassthrough } from "../pipeline/pipes.js";
import { AudioPcmUnit, AudioPlayerSetup, NodeAudioPlayer, PcmAudioPlayer } from "./index.js";

const PROCESSOR_NAME = "moonlight-pcm-player"
const MAX_PENDING_FRAMES = 24


export class AudioWorkletPcmPipe implements PcmAudioPlayer {
    static async getInfo(): Promise<PipeInfo> {
        return {
            environmentSupported: "AudioWorkletNode" in globalObject() && "AudioContext" in globalObject()
        }
    }

    static readonly baseType = "audionode"
    static readonly type = "audiopcm"

    readonly implementationName: string

    private base: NodeAudioPlayer
    private logger: Logger | null
    private node: AudioWorkletNode | null = null
    private pending: AudioPcmUnit[] = []
    private generation = 0

    constructor(base: NodeAudioPlayer, logger?: Logger) {
        this.base = base
        this.logger = logger ?? null
        this.implementationName = `audio_worklet_pcm -> ${base.implementationName}`
        addPipePassthrough(this)
    }

    private setupError: Error | null = null

    setup(setup: AudioPlayerSetup) {
        if ("setup" in this.base && typeof this.base.setup == "function") {
            this.base.setup(setup)
        }

        const context = this.base.getAudioContext()
        if (!context.audioWorklet) {
            throw new Error("AudioWorklet is unavailable on this AudioContext")
        }

        const generation = ++this.generation
        const moduleUrl = buildUrl("/stream/audio/pcm_worklet_processor.js")

        // The module load is async, but the result must not be swallowed: callers await
        // this promise so a failed AudioWorklet surfaces instead of silently producing a
        // renderer that never outputs audio.
        return (async () => {
            try {
                await context.audioWorklet.addModule(moduleUrl)
                if (generation !== this.generation) return
                const node = new AudioWorkletNode(context, PROCESSOR_NAME, {
                    numberOfInputs: 0,
                    numberOfOutputs: 1,
                    outputChannelCount: [setup.channels],
                    channelCount: setup.channels,
                    channelCountMode: "explicit",
                    channelInterpretation: setup.channels > 2 ? "discrete" : "speakers",
                })
                this.node = node
                this.setupError = null
                this.base.setSource(node)
                for (const unit of this.pending.splice(0)) {
                    this.postPcm(unit)
                }
            } catch (error: any) {
                const reason = error instanceof Error ? error : new Error(String(error))
                this.setupError = reason
                this.pending = []
                this.logger?.debug(`Failed to initialize AudioWorklet PCM renderer: ${reason}`)

                throw new Error(`Failed to initialize AudioWorklet PCM renderer: ${reason.message}`)
            }
        })()
    }

    /// Non-null once the worklet could not be initialized; the pipe then renders nothing.
    getSetupError(): Error | null {
        return this.setupError
    }

    private cloneUnit(unit: AudioPcmUnit): AudioPcmUnit {
        return {
            timestampMicroseconds: unit.timestampMicroseconds,
            durationMicroseconds: unit.durationMicroseconds,
            channelData: unit.channelData.map(channel => channel.slice())
        }
    }

    private postPcm(unit: AudioPcmUnit) {
        if (!this.node) return
        const channels = unit.channelData.map(channel => channel.slice())
        this.node.port.postMessage({ channels }, channels.map(channel => channel.buffer))
    }

    playPcm(unit: AudioPcmUnit): void {
        if (this.node) {
            this.postPcm(unit)
            return
        }

        this.pending.push(this.cloneUnit(unit))
        if (this.pending.length > MAX_PENDING_FRAMES) {
            this.pending.splice(0, this.pending.length - Math.floor(MAX_PENDING_FRAMES / 2))
        }
    }

    onUserInteraction(): void {
        void this.base.getAudioContext().resume()
        if ("onUserInteraction" in this.base && typeof this.base.onUserInteraction == "function") {
            this.base.onUserInteraction()
        }
    }

    cleanup(): void {
        this.generation++
        this.pending = []
        this.node?.disconnect()
        this.node?.port.close()
        this.node = null
        if ("cleanup" in this.base && typeof this.base.cleanup == "function") {
            this.base.cleanup()
        }
    }

    getBase(): Pipe | null {
        return this.base
    }
}
