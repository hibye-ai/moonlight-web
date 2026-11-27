import { globalObject } from "../../util.js";
import { Pipe, PipeInfo } from "../pipeline/index.js";
import { addPipePassthrough } from "../pipeline/pipes.js";
import { BoundedTrackWriter } from "../pipeline/bounded_track_writer.js";
import { StatValue } from "../stats.js";
import { SampleAudioPlayer, TrackAudioPlayer } from "./index.js";

export class AudioMediaStreamTrackGeneratorPipe implements SampleAudioPlayer {

    static readonly baseType = "audiotrack"
    static readonly type = "audiosample"

    static async getInfo(): Promise<PipeInfo> {
        return {
            environmentSupported: "MediaStreamTrackGenerator" in globalObject()
        }
    }

    implementationName: string

    private base: TrackAudioPlayer

    private trackGenerator: MediaStreamTrackGenerator
    private writer: WritableStreamDefaultWriter<AudioData>
    private output: BoundedTrackWriter<AudioData>

    constructor(base: TrackAudioPlayer) {
        this.implementationName = `audio_media_stream_track_generator -> ${base.implementationName}`
        this.base = base

        this.trackGenerator = new MediaStreamTrackGenerator({ kind: "audio" })
        this.writer = this.trackGenerator.writable.getWriter()
        this.output = new BoundedTrackWriter(this.writer, "audio")

        addPipePassthrough(this)
    }

    private isFirstSample = true
    submitSample(sample: AudioData): void {
        if (this.isFirstSample) {
            this.isFirstSample = false

            this.base.setTrack(this.trackGenerator)
        }
        this.output.submit(sample)
    }

    async reportStats(stats: Record<string, StatValue>): Promise<void> {
        await this.base.reportStats?.(stats)
        this.output.reportStats(stats, "audioTrack")
    }

    cleanup() {
        this.output.close()
        this.trackGenerator.stop()
        const base = this.base as typeof this.base & { cleanup?(): void }
        base.cleanup?.()
    }

    getBase(): Pipe | null {
        return this.base
    }

}
