import { globalObject } from "../../util.js";
import { Pipe, PipeInfo } from "../pipeline/index.js";
import { addPipePassthrough } from "../pipeline/pipes.js";
import { BoundedTrackWriter } from "../pipeline/bounded_track_writer.js";
import { StatValue } from "../stats.js";
import { allVideoCodecs } from "../video.js";
import { FrameVideoRenderer, TrackVideoRenderer } from "./index.js";

export class VideoTrackGeneratorPipe implements FrameVideoRenderer {
    static readonly baseType = "videotrack"
    static readonly type = "videoframe"

    static async getInfo(): Promise<PipeInfo> {
        // https://developer.mozilla.org/en-US/docs/Web/API/VideoTrackGenerator
        return {
            environmentSupported: "VideoTrackGenerator" in globalObject(),
            supportedVideoCodecs: allVideoCodecs()
        }
    }

    readonly implementationName: string

    private base: TrackVideoRenderer

    private trackGenerator: VideoTrackGenerator
    private writer: WritableStreamDefaultWriter<VideoFrame>
    private output: BoundedTrackWriter<VideoFrame>

    constructor(base: TrackVideoRenderer) {
        this.implementationName = `video_track_generator -> ${base.implementationName}`
        this.base = base

        this.trackGenerator = new VideoTrackGenerator()
        this.writer = this.trackGenerator.writable.getWriter()
        this.output = new BoundedTrackWriter(this.writer, "video")

        addPipePassthrough(this)
    }

    private isFirstSample = true
    submitFrame(frame: VideoFrame): void {
        if (this.isFirstSample) {
            this.isFirstSample = false

            this.base.setTrack(this.trackGenerator.track)
        }
        this.output.submit(frame)
    }

    async reportStats(stats: Record<string, StatValue>): Promise<void> {
        await this.base.reportStats?.(stats)
        this.output.reportStats(stats, "videoTrack")
    }

    cleanup() {
        this.output.close()
        this.trackGenerator.track.stop()
        const base = this.base as typeof this.base & { cleanup?(): void }
        base.cleanup?.()
    }

    getBase(): Pipe | null {
        return this.base
    }
}
