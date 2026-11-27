import { Pipe, PipeInfo } from "../pipeline/index.js";
import { addPipePassthrough, DataPipe } from "../pipeline/pipes.js";
import { AudioPlayerSetup, DataAudioPlayer } from "./index.js";

export class DepacketizeAudioPipe implements DataPipe {

    static async getInfo(): Promise<PipeInfo> {
        return {
            environmentSupported: true
        }
    }

    static readonly baseType = "audiodata"
    static readonly type = "wsdata"

    readonly implementationName: string

    private base: DataAudioPlayer
    private timestampMicroseconds: number = 0
    private packetDurationMicroseconds: number = 0

    constructor(base: DataAudioPlayer) {
        this.implementationName = `depacketize_audio -> ${base.implementationName}`
        this.base = base

        addPipePassthrough(this)
    }

    setup(setup: AudioPlayerSetup) {
        this.packetDurationMicroseconds = setup.samplesPerFrame * 1_000_000 / setup.sampleRate
        // A (re)connect restarts the audio timeline; a stale offset would make the
        // downstream AudioData timeline jump arbitrarily.
        this.resetClock()

        if ("setup" in this.base && typeof this.base.setup == "function") {
            return this.base.setup(...arguments)
        }
    }

    /// Restart the packet clock. Called by setup() and usable on a reconnect that does
    /// not re-run the whole pipeline setup.
    resetClock() {
        this.timestampMicroseconds = 0
    }

    submitPacket(buffer: ArrayBuffer) {
        const timestampMicroseconds = this.timestampMicroseconds

        this.base.decodeAndPlay({
            data: buffer,
            timestampMicroseconds,
            durationMicroseconds: this.packetDurationMicroseconds,
        })

        this.timestampMicroseconds = timestampMicroseconds + this.packetDurationMicroseconds
    }

    getBase(): Pipe | null {
        return this.base
    }
}