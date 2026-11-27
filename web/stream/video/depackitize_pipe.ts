import { ByteBuffer } from "../buffer.js";
import { Logger } from "../log.js";
import { Pipe, PipeInfo } from "../pipeline/index.js";
import { addPipePassthrough, DataPipe } from "../pipeline/pipes.js";
import { allVideoCodecs } from "../video.js";
import { DataVideoRenderer, VideoRendererSetup } from "./index.js";

const U32_RANGE = 2 ** 32

export class DepacketizeVideoPipe implements DataPipe {

    static readonly baseType = "videodata"
    static readonly type = "wsdata"

    static async getInfo(): Promise<PipeInfo> {
        // no link
        return {
            environmentSupported: true,
            supportedVideoCodecs: allVideoCodecs()
        }
    }

    readonly implementationName: string

    private base: DataVideoRenderer

    /// Last raw wire value (u32 microseconds, big endian on the wire) and the
    /// timeline rebuilt from it. They are tracked separately: the wire counter
    /// wraps every ~71.6 min, the Number timeline must not.
    ///
    /// This only works for ordered delivery with inter-frame gaps strictly below
    /// 2^32 us (~71.6 min): a gap >= 2^32 us is indistinguishable from one or more
    /// skipped epochs, so the wire format cannot recover it.
    private lastRawTimestampMicroseconds: number | null = null
    private timestampMicroseconds = 0

    private buffer = new ByteBuffer(5)

    constructor(base: DataVideoRenderer, logger?: Logger) {
        this.implementationName = `depacketize_video -> ${base.implementationName}`
        this.base = base

        addPipePassthrough(this)
    }

    submitPacket(buffer: ArrayBuffer) {
        const array = new Uint8Array(buffer)

        this.buffer.reset()

        this.buffer.putU8Array(array.slice(0, 5))

        this.buffer.flip()

        const frameType = this.buffer.getU8()
        const rawTimestampMicroseconds = this.buffer.getU32()

        // Unsigned modulo 2^32 delta: a wrap of the wire counter yields a small
        // positive step instead of a negative one.
        let durationMicroseconds = 0
        if (this.lastRawTimestampMicroseconds === null) {
            // First frame of a stream: no previous frame, so there is no elapsed
            // time to report. Seed the timeline from the raw value.
            this.timestampMicroseconds = rawTimestampMicroseconds
        } else {
            durationMicroseconds = (rawTimestampMicroseconds - this.lastRawTimestampMicroseconds) >>> 0
            this.timestampMicroseconds += durationMicroseconds
        }
        this.lastRawTimestampMicroseconds = rawTimestampMicroseconds

        this.base.submitDecodeUnit({
            type: frameType == 0 ? "delta" : "key",
            data: array.slice(5).buffer,
            durationMicroseconds,
            timestampMicroseconds: this.timestampMicroseconds,
        })

        addPipePassthrough(this)
    }

    setup(setup: VideoRendererSetup) {
        // A new stream restarts the wire counter and the timeline. Ordinary IDRs
        // inside a running stream are *not* a boundary: resetting there would
        // throw away the accumulated epoch and jump the timeline backwards.
        this.resetClock()

        if ("setup" in this.base && typeof this.base.setup == "function") {
            return this.base.setup(...arguments)
        }
    }

    /// Restart the unfolder state. Called by setup() and usable on a reconnect
    /// that does not re-run the whole pipeline setup.
    resetClock() {
        this.lastRawTimestampMicroseconds = null
        this.timestampMicroseconds = 0
    }

    getBase(): Pipe | null {
        return this.base
    }
}
