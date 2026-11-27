import { OpusMultistreamDecoder } from "../../libopus/index.js";
import loadOpus from "../../libopus/libopus.js";
import { MainModule as OpusModule } from "../../libopus/libopus.js";
import { Logger } from "../log.js";
import { Pipe, PipeInfo } from "../pipeline/index.js";
import { addPipePassthrough } from "../pipeline/pipes.js";
import { AudioDecodeUnit, AudioPlayerSetup, DataAudioPlayer, PcmAudioPlayer } from "./index.js";

// TODO: use AudioWorklets? https://developer.mozilla.org/en-US/docs/Web/API/Web_Audio_API/Using_AudioWorklet

export class OpusAudioDecoderPipe implements DataAudioPlayer {

    static async getInfo(): Promise<PipeInfo> {
        return {
            environmentSupported: true
        }
    }

    static readonly baseType = "audiopcm"
    static readonly type = "audiodata"

    readonly implementationName: string

    private logger: Logger | null = null

    private base: PcmAudioPlayer

    private errored: boolean = false

    private decoder: OpusMultistreamDecoder | null = null

    private opusModule: OpusModule | null = null
    private setupData: AudioPlayerSetup | null = null

    private buffer: Float32Array = new Float32Array([])
    private channelBuffers: Array<Float32Array> = []
    private channelBufferCapacity: number = 0

    constructor(base: PcmAudioPlayer, logger?: Logger) {
        loadOpus().then(module => this.opusModule = module)

        this.logger = logger ?? null

        this.implementationName = `opus_decode -> ${base.implementationName}`
        this.base = base

        addPipePassthrough(this)
    }

    setup(setup: AudioPlayerSetup) {
        this.setupData = setup

        if ("setup" in this.base && typeof this.base.setup == "function") {
            return this.base.setup(...arguments)
        }
    }

    decodeAndPlay(unit: AudioDecodeUnit): void {
        if (this.errored) {
            return
        }

        if (!this.setupData) {
            this.errored = true
            this.logger?.debug("Failed to play audio sample because audio player is not initialized")
            return
        }

        if (!this.decoder) {
            if (!this.opusModule) {
                return
            }

            try {
                this.decoder = new OpusMultistreamDecoder(this.opusModule, this.setupData.sampleRate, this.setupData.channels, this.setupData.streams, this.setupData.coupledStreams, this.setupData.mapping)
            } catch (e: any) {
                this.errored = true

                const message = `Failed to initialize opus decoder: ${"toString" in e && typeof e.toString == "function" ? e.toString() : e}`
                this.logger?.debug(message, { type: "informError" })

                return
            }
            this.buffer = new Float32Array(this.setupData.samplesPerFrame * this.setupData.channels)
        }

        // -- Decode samples
        let samplesDecoded
        try {
            samplesDecoded = this.decoder.decodeFloat(unit.data, this.buffer, this.setupData.samplesPerFrame, false)
        } catch (e: any) {
            this.errored = true

            const message = `Failed to decode audio sample: ${"toString" in e && typeof e.toString == "function" ? e.toString() : e}`
            this.logger?.debug(message, { type: "informError" })

            return
        }

        // -- De-interleave interleaved PCM

        // Initialize channel arrays
        const channels = this.setupData.channels

        // Capacity must be measured in *elements* (sample count), not byteLength:
        // byteLength is 4x the element count, so comparing it against a sample count
        // silently skipped the grow path and forced the short path to reuse a buffer
        // of the wrong size.
        if (this.channelBuffers.length != channels || this.channelBufferCapacity < samplesDecoded) {
            this.channelBuffers = new Array(channels)
            this.channelBufferCapacity = samplesDecoded

            for (let channelIndex = 0; channelIndex < channels; channelIndex++) {
                this.channelBuffers[channelIndex] = new Float32Array(samplesDecoded)
            }
        }

        for (let channelIndex = 0; channelIndex < channels; channelIndex++) {
            const channelBuffer = this.channelBuffers[channelIndex]

            for (let sample = 0; sample < samplesDecoded; sample++) {
                channelBuffer[sample] = this.buffer[(sample * channels) + channelIndex]
            }
        }

        // -- Pass data to next decoder

        // Only the decoded samples are valid. Handing over the whole reused array would
        // replay stale samples from the previous (possibly longer) frame. The unit is
        // documented as borrowed, so downstream pipes must not retain it past the call;
        // AudioWorkletPcmPipe copies before posting.
        this.base.playPcm({
            durationMicroseconds: unit.durationMicroseconds,
            timestampMicroseconds: unit.timestampMicroseconds,
            channelData: this.channelBuffers.map(channelBuffer => channelBuffer.subarray(0, samplesDecoded))
        })
    }

    cleanup() {
        this.decoder?.destroy()

        if ("cleanup" in this.base && typeof this.base.cleanup == "function") {
            return this.base.cleanup(...arguments)
        }
    }

    getBase(): Pipe | null {
        return this.base
    }
}