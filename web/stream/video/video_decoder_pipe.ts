import { globalObject } from "../../util.js";
import { ByteBuffer } from "../buffer.js";
import { Logger } from "../log.js";
import { StatValue } from "../stats.js";
import { Pipe, PipeInfo } from "../pipeline/index.js";
import { addPipePassthrough } from "../pipeline/pipes.js";
import { emptyVideoCodecs, maybeVideoCodecs, VideoCodecSupport } from "../video.js";
import { CodecStreamTranslator, H264StreamVideoTranslator, H265StreamVideoTranslator, VIDEO_DECODER_CODECS_OUT_OF_BAND } from "./annex_b_translator.js";
import { DataVideoRenderer, FrameVideoRenderer, VideoDecodeUnit, VideoRendererSetup } from "./index.js";

export const VIDEO_DECODER_CODECS_IN_BAND: Record<keyof VideoCodecSupport, string> = {
    // avc1 = out of band config, avc3 = in band with sps, pps, idr
    "H264": "avc3.42E01E",
    "H264_HIGH8_444": "avc3.640032",
    // hvc1 = out of band config, hev1 = in band with sps, pps, idr
    "H265": "hev1.1.6.L93.B0",
    "H265_MAIN10": "hev1.2.4.L120.90",
    "H265_REXT8_444": "hev1.6.6.L93.90",
    "H265_REXT10_444": "hev1.6.10.L120.90",
    // av1 doesn't have in band and out of band distinction
    "AV1_MAIN8": "av01.0.04M.08",
    "AV1_MAIN10": "av01.0.04M.10",
    "AV1_HIGH8_444": "av01.0.08M.08",
    "AV1_HIGH10_444": "av01.0.08M.10"
}

async function probeDecoderCodec(codec: string, verifyConfigure: boolean): Promise<boolean> {
    const configs: VideoDecoderConfig[] = [
        { codec, codedWidth: 1920, codedHeight: 1080, hardwareAcceleration: "prefer-hardware", optimizeForLatency: true },
        { codec, codedWidth: 1920, codedHeight: 1080, optimizeForLatency: true },
        { codec, codedWidth: 1280, codedHeight: 720 },
    ]

    for (const config of configs) {
        try {
            const result = await VideoDecoder.isConfigSupported(config)
            if (!result.supported) {
                continue
            }
            if (!verifyConfigure) {
                return true
            }

            // Some engines report support but throw immediately when the
            // decoder is configured. Catch that here before starting a stream.
            const decoder = new VideoDecoder({
                output: frame => frame.close(),
                error: () => { /* runtime decode failures are handled by the real pipeline */ }
            })
            try {
                decoder.configure(result.config ?? config)
                decoder.close()
                return true
            } catch {
                decoder.close()
            }
        } catch {
            // Treat a throwing capability probe as unsupported and continue.
        }
    }

    return false
}

async function detectCodecs(): Promise<VideoCodecSupport> {
    if (!("isConfigSupported" in VideoDecoder)) {
        return maybeVideoCodecs()
    }

    const codecs = emptyVideoCodecs()
    const promises = []

    for (const codec in codecs) {
        promises.push((async () => {
            const codecKey = codec as keyof VideoCodecSupport
            const supportedInBand = await probeDecoderCodec(VIDEO_DECODER_CODECS_IN_BAND[codecKey], true)
            // Out-of-band AVC/HEVC configuration receives codec private data
            // later from the stream translator, so a support probe is the
            // strongest safe check available before receiving SPS/PPS/VPS.
            const supportedOutOfBand = await probeDecoderCodec(VIDEO_DECODER_CODECS_OUT_OF_BAND[codecKey], false)
            codecs[codecKey] = supportedInBand || supportedOutOfBand
        })())
    }
    await Promise.all(promises)

    // High 4:4:4 H.264 is repeatedly misreported by Safari/Firefox and is not
    // useful as a fallback, so keep it disabled unless the pipeline gains a
    // real sample-decode probe in the future.
    codecs.H264_HIGH8_444 = false

    return codecs
}
async function getIfConfigSupported(config: VideoDecoderConfig): Promise<VideoDecoderConfig | null> {
    try {
        const supported = await VideoDecoder.isConfigSupported(config)
        if (supported.supported) return config
    } catch { /* Try the next configuration or translator. */ }
    return null
}

export class VideoDecoderPipe implements DataVideoRenderer {
    static readonly baseType = "videoframe"
    static readonly type = "videodata"

    static async getInfo(): Promise<PipeInfo> {
        const supported = "VideoDecoder" in globalObject()

        return {
            environmentSupported: supported,
            supportedVideoCodecs: supported ? await detectCodecs() : emptyVideoCodecs()
        }
    }

    readonly implementationName: string

    private logger: Logger | null

    private base: FrameVideoRenderer

    private fps = 0
    private codedWidth = 0
    private codedHeight = 0
    private cleaned = false
    private recoveryPending = false
    private bufferedBytes = 0
    private bootstrapDropped = 0
    private resets = 0
    private decoded = 0

    private errored = false
    private config: VideoDecoderConfig | null = null
    private translator: CodecStreamTranslator | null = null
    private decoder: VideoDecoder

    constructor(base: FrameVideoRenderer, logger?: Logger) {
        this.implementationName = `video_decoder -> ${base.implementationName}`
        this.logger = logger ?? null

        this.base = base

        this.decoder = new VideoDecoder({
            error: this.onError.bind(this),
            output: this.onOutput.bind(this)
        })

        addPipePassthrough(this)
    }

    private onError(error: any) {
        this.errored = true

        this.logger?.debug(`VideoDecoder has an error ${"toString" in error ? error.toString() : `${error}`}`, { type: "fatal" })
        console.error(error)
    }

    private onOutput(frame: VideoFrame) {
        if (this.cleaned) { frame.close(); return }
        this.decoded++
        this.base.submitFrame(frame)
    }

    private async trySetConfig(codec: string) {
        if (!this.config) {
            this.config = await getIfConfigSupported({
                codec,
                codedWidth: this.codedWidth, codedHeight: this.codedHeight,
                hardwareAcceleration: "prefer-hardware",
                optimizeForLatency: true
            })
        }

        if (!this.config) {
            this.config = await getIfConfigSupported({
                codec,
                codedWidth: this.codedWidth, codedHeight: this.codedHeight,
                optimizeForLatency: true
            })
        }

        if (!this.config) {
            this.config = await getIfConfigSupported({
                codec,
                codedWidth: this.codedWidth, codedHeight: this.codedHeight,
            })
        }
    }
    async setup(setup: VideoRendererSetup): Promise<void> {
        this.fps = setup.fps
        this.codedWidth = setup.width
        this.codedHeight = setup.height

        const codec = VIDEO_DECODER_CODECS_IN_BAND[setup.codec]
        await this.trySetConfig(codec)
        if (this.cleaned) return

        if (!this.config) {
            if (setup.codec == "H264" || setup.codec == "H264_HIGH8_444") {
                this.translator = new H264StreamVideoTranslator(this.logger ?? undefined)

                const codec = VIDEO_DECODER_CODECS_OUT_OF_BAND[setup.codec]
                await this.trySetConfig(codec)
            } else if (setup.codec == "H265" || setup.codec == "H265_MAIN10" || setup.codec == "H265_REXT8_444" || setup.codec == "H265_REXT10_444") {
                this.translator = new H265StreamVideoTranslator(this.logger ?? undefined)

                const codec = VIDEO_DECODER_CODECS_OUT_OF_BAND[setup.codec]
                await this.trySetConfig(codec)
            } else if (setup.codec == "AV1_MAIN8" || setup.codec == "AV1_MAIN10" || setup.codec == "AV1_HIGH8_444" || setup.codec == "AV1_HIGH10_444") {
                this.errored = true
                this.logger?.debug("Av1 stream translator is not implemented currently!", { type: "fatalDescription" })
                return
            } else {
                this.errored = true
                this.logger?.debug(`Failed to find stream translator for codec ${setup.codec}`)
                return
            }
        }

        if (!this.config) {
            this.errored = true
            this.logger?.debug(`Failed to setup VideoDecoder for codec ${setup.codec} because of missing config`)
            return
        }
        this.translator?.setBaseConfig(this.config)
        if (this.cleaned) return

        this.logger?.debug(`VideoDecoder config: ${JSON.stringify(this.config)}`)

        this.reset()

        if ("setup" in this.base && typeof this.base.setup == "function") {
            await this.base.setup(...arguments)
        }
        if (this.cleaned) return
        this.decoderSetupFinished = true
        const units = this.bufferedUnits.splice(0)
        this.bufferedBytes = 0
        for (const unit of units) this.submitDecodeUnit(unit)
    }

    private decoderSetupFinished = false
    private requestedIdr = false
    private needsKeyFrame = true

    private bufferedUnits: Array<VideoDecodeUnit> = []
    submitDecodeUnit(unit: VideoDecodeUnit): void {
        if (this.errored || this.cleaned) return
        if (!this.decoderSetupFinished) {
            if (unit.type === "key" || this.bufferedUnits.length >= 12 || this.bufferedBytes + unit.data.byteLength > 8 * 1024 * 1024) {
                this.bootstrapDropped += this.bufferedUnits.length
                this.bufferedUnits = []
                this.bufferedBytes = 0
                this.recoveryPending = unit.type !== "key"
            }
            if ((!this.bufferedUnits.length && unit.type !== "key") || unit.data.byteLength > 8 * 1024 * 1024) {
                this.bootstrapDropped++
                this.recoveryPending = true
                return
            }
            this.bufferedUnits.push(unit)
            this.bufferedBytes += unit.data.byteLength
            return
        }
        if (this.decoder.decodeQueueSize > Math.max(2, Math.ceil(this.fps * 0.1))) {
            this.reset()
            this.resets++
            this.recoveryPending = true
        }
        if (unit.type !== "key" && this.needsKeyFrame) return

        if (this.translator) {
            const value = this.translator.submitDecodeUnit(unit)
            if (value.error) {
                this.errored = true
                this.logger?.debug("VideoDecoder has errored!")
                return
            }

            const { configure, chunk } = value

            if (!chunk) {
                console.debug("No chunk received!")
                return
            }

            const nextConfig = configure ?? (this.decoder.state === "unconfigured" ? this.translator.getCurrentConfig() : null)
            if (nextConfig) {
                console.debug("Resetting video decoder config with", configure)

                this.decoder.reset()
                this.decoder.configure(nextConfig)

                // This likely is an idr
                this.requestedIdr = false
            }

            const encodedChunk = new EncodedVideoChunk({
                type: unit.type,
                timestamp: unit.timestampMicroseconds,
                duration: unit.durationMicroseconds,
                data: chunk,
            })
            this.decoder.decode(encodedChunk)
        } else {
            if (unit.type != "key" && this.needsKeyFrame) {
                return
            }
            this.needsKeyFrame = false
            this.requestedIdr = false

            const chunk = new EncodedVideoChunk({
                type: unit.type,
                data: unit.data,
                timestamp: unit.timestampMicroseconds,
                duration: unit.durationMicroseconds
            })

            this.decoder.decode(chunk)
        }
        this.needsKeyFrame = false
        if (unit.type === "key") { this.recoveryPending = false; this.requestedIdr = false }
    }

    private reset() {
        this.needsKeyFrame = true
        this.decoder.reset()
        if (!this.translator) {

            if (this.config) {
                this.decoder.configure(this.config)
            } else {
                this.logger?.debug("Failed to configure VideoDecoder because of missing config", { type: "fatal" })
            }
        }
    }

    pollRequestIdr(): boolean {
        if (this.cleaned) return false
        let requestIdr = this.recoveryPending && !this.requestedIdr

        const estimatedQueueDelayMs = this.decoder.decodeQueueSize * 1000 / this.fps
        if (estimatedQueueDelayMs > 200 && this.decoder.decodeQueueSize > 2) {
            // Recover from a decoder backlog instead of playing stale frames.

            if (!this.requestedIdr) {
                requestIdr = true
                this.reset()
                this.resets++
            }
            console.debug(`Requesting idr because of decode queue size(${this.decoder.decodeQueueSize}) and estimated delay of the queue: ${estimatedQueueDelayMs}`)
        }

        if ("pollRequestIdr" in this.base && typeof this.base.pollRequestIdr == "function") {
            if (this.base.pollRequestIdr(...arguments)) {
                requestIdr = true
            }
        }

        if (requestIdr) {
            this.requestedIdr = true
        }

        return requestIdr
    }

    cleanup() {
        this.cleaned = true
        this.bufferedUnits = []
        this.bufferedBytes = 0
        if (this.decoder.state !== "closed") this.decoder.close()

        if ("cleanup" in this.base && typeof this.base.cleanup == "function") {
            return this.base.cleanup(arguments)
        }
    }
    async reportStats(stats: Record<string, StatValue>): Promise<void> {
        await this.base.reportStats?.(stats)
        stats.videoDecoderQueue = this.decoder.decodeQueueSize
        stats.videoDecoderQueueMs = this.decoder.decodeQueueSize * 1000 / Math.max(1, this.fps)
        stats.videoDecoderBootstrapDropped = this.bootstrapDropped
        stats.videoDecoderResets = this.resets
        stats.videoDecoderOutput = this.decoded
        stats.videoDecoderHardwarePreference = this.config?.hardwareAcceleration ?? "no-preference"
    }

    getBase(): Pipe | null {
        return this.base
    }
}
