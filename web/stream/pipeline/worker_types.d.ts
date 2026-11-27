import { LogMessageInfo } from "../log.js"
import { VideoDecodeUnit, VideoRendererSetup } from "../video/index.js"
import { PipeInfo, Pipeline } from "./index.js"
import { StatValue } from "../stats.js"

export type ToWorkerMessage =
    { checkSupport: Pipeline } |
    { createPipeline: Pipeline, rendererOptions?: unknown } |
    { inspect: true } |
    { input: WorkerMessage }

export type WorkerMessage =
    { call: "cleanup" } |
    { videoSetup: VideoRendererSetup } |
    // VideoFrame is a transferable object
    { videoFrame: VideoFrame } |
    // MediaStreamTrack is a transferable object when using the transfer parameter
    { track: MediaStreamTrack } |
    { data: ArrayBuffer } |
    { videoData: VideoDecodeUnit } |
    // Canvas stuff
    { canvas: OffscreenCanvas }

export type ToMainMessage =
    { checkSupport: PipeInfo } |
    { videoAccepted: true } |
    { inspection: { stats: Record<string, StatValue>, requestIdr: boolean } } |
    { log: string, info: LogMessageInfo } |
    { output: WorkerMessage }
