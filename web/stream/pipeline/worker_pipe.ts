import { globalObject } from "../../util.js";
import { Logger } from "../log.js";
import { OffscreenCanvasRenderer } from "../video/offscreen_canvas.js";
import { getPipe, Pipe, PipeInfo, Pipeline, pipelineToString, PipeStatic } from "./index.js";
import { addPipePassthrough } from "./pipes.js";
import { ToMainMessage, ToWorkerMessage, WorkerMessage } from "./worker_types.js";
import { StatValue } from "../stats.js";
import { VideoDecodeUnit } from "../video/index.js";

export function createPipelineWorker(): Worker | null {
    if (!("Worker" in globalObject())) {
        return null
    }

    return new Worker(new URL("worker.js", import.meta.url), { type: "module" })
}

export interface WorkerReceiver extends Pipe {
    onWorkerMessage(message: WorkerMessage, transferable?: Transferable[]): void
}

// A capability probe must never hang the whole pipeline detection (which also gates
// WebRTC init), so an unresponsive worker is treated as "unsupported" after this long.
const PROBE_TIMEOUT_MS = 3000

export class WorkerPipe implements WorkerReceiver {
    protected static async getInfoInternal(pipeline: Pipeline, options?: { timeoutMs?: number }): Promise<PipeInfo> {
        const worker = createPipelineWorker()
        if (!worker) {
            return {
                environmentSupported: false
            }
        }

        const timeoutMs = options?.timeoutMs ?? PROBE_TIMEOUT_MS

        const info = await new Promise<PipeInfo>((resolve) => {
            let settled = false
            let timer: ReturnType<typeof setTimeout> | null = null

            const settle = (result: PipeInfo) => {
                if (settled) {
                    return
                }
                settled = true
                if (timer != null) {
                    clearTimeout(timer)
                }

                // Always release the probe worker; it only answered a capability
                // question and would otherwise stay alive for the lifetime of the page.
                try {
                    worker.terminate()
                } catch {
                    // terminating an already-failed worker is not an error for us
                }

                resolve(result)
            }

            const unsupported = (reason: string) => {
                this.probeLogger?.debug(`Worker pipeline probe failed: ${reason}`)
                settle({ environmentSupported: false })
            }

            timer = setTimeout(() => unsupported(`no answer within ${timeoutMs}ms`), timeoutMs)

            worker.onmessage = (event) => {
                const message = event.data as ToMainMessage

                if (message && "checkSupport" in message) {
                    settle(message.checkSupport)
                } else if (message && "log" in message) {
                    unsupported(String(message.log))
                } else {
                    unsupported("worker returned an unexpected message")
                }
            }
            worker.onerror = (event: any) => {
                unsupported(String(event?.error ?? event?.message ?? "worker error"))
            }

            const sendMessage: ToWorkerMessage = { checkSupport: pipeline }
            worker.postMessage(sendMessage)
        })

        return info
    }

    private static probeLogger: Logger | null = null

    /// Route probe diagnostics to the stream logger (optional; used once at startup).
    static setProbeLogger(logger: Logger | null) {
        this.probeLogger = logger
    }

    readonly implementationName: string

    private logger: Logger | null

    private worker: Worker
    private base: WorkerReceiver
    private pipeline: Pipeline
    private inspectedStats: Record<string, StatValue> = {}
    private inspectAt = -Infinity
    private requestIdr = false
    private stopped = false
    private videoInFlight = 0
    private pendingVideo: VideoDecodeUnit[] = []
    private pendingVideoBytes = 0
    private waitingForKey = false
    private droppedVideo = 0
    private sendVideo() {
        while (this.videoInFlight < 2 && this.pendingVideo.length) {
            const unit = this.pendingVideo.shift()!
            this.pendingVideoBytes -= unit.data.byteLength
            this.videoInFlight++
            const message: ToWorkerMessage = { input: { videoData: unit } }
            this.worker.postMessage(message)
        }
    }

    async reportStats(stats: Record<string, StatValue>): Promise<void> {
        await this.base.reportStats?.(stats)
        Object.assign(stats, this.inspectedStats)
        stats.videoWorkerPending = this.pendingVideo.length + this.videoInFlight
        stats.videoWorkerDropped = this.droppedVideo
        this.inspectWorker()
    }
    private inspectWorker() {
        if (!this.stopped && performance.now() - this.inspectAt >= 200) {
            this.inspectAt = performance.now()
            const message: ToWorkerMessage = { inspect: true }
            this.worker.postMessage(message)
        }
    }
    pollRequestIdr(): boolean {
        this.inspectWorker()
        const request = this.requestIdr
        this.requestIdr = false
        return request || ((this.base as any).pollRequestIdr?.() ?? false)
    }

    constructor(base: WorkerReceiver, pipeline: Pipeline, logger?: Logger, rendererOptions?: unknown) {
        this.implementationName = `worker_pipe [${pipelineToString(pipeline)}] -> ${base.implementationName}`
        this.logger = logger ?? null

        if (getPipe(pipeline.pipes[0])?.type != "workeroutput") {
            logger?.debug("Worker Pipeline doesn't start with a workeroutput!", { type: "fatal" })
            throw "Worker Pipeline doesn't start with a workeroutput!"
        }
        if (getPipe(pipeline.pipes[pipeline.pipes.length - 1])?.baseType != "workerinput") {
            logger?.debug("Worker Pipeline doesn't end with a workerinput!", { type: "fatal" })
            throw "Worker Pipeline doesn't start with a workerinput!"
        }

        this.base = base
        this.pipeline = pipeline

        const worker = createPipelineWorker()
        if (!worker) {
            logger?.debug("Failed to create worker pipeline: Workers not supported!", { type: "fatal" })
            throw "Failed to create worker pipeline: Workers not supported!"
        }
        this.worker = worker

        this.worker.onmessage = this.onReceiveWorkerMessage.bind(this)

        const message: ToWorkerMessage = {
            createPipeline: this.pipeline, rendererOptions
        }
        this.worker.postMessage(message)

        this.worker.onerror = (event) => {
            this.logger?.debug(`Worker errored because of: ${event.error}`)
        }

        addPipePassthrough(this)
    }

    onWorkerMessage(input: WorkerMessage, transfer?: Array<Transferable>): void {
        if (this.stopped) return
        if ("videoData" in input) {
            const unit = input.videoData
            if (unit.type === "key" || this.pendingVideo.length >= 8 || this.pendingVideoBytes + unit.data.byteLength > 8 * 1024 * 1024) {
                this.droppedVideo += this.pendingVideo.length
                this.pendingVideo = []
                this.pendingVideoBytes = 0
                this.waitingForKey = unit.type !== "key"
                if (this.waitingForKey) this.requestIdr = true
            }
            if (this.waitingForKey || unit.data.byteLength > 8 * 1024 * 1024) {
                this.waitingForKey = true
                this.droppedVideo++; this.requestIdr = true; return
            }
            this.pendingVideo.push(unit)
            this.pendingVideoBytes += unit.data.byteLength
            this.sendVideo()
            return
        }
        const message: ToWorkerMessage = { input }

        this.worker.postMessage(message, transfer ?? [])
    }

    private onReceiveWorkerMessage(event: MessageEvent) {
        const data: ToMainMessage = event.data

        if (this.stopped) return
        if ("videoAccepted" in data) {
            this.videoInFlight = Math.max(0, this.videoInFlight - 1)
            this.sendVideo()
        } else if ("output" in data) {
            this.base.onWorkerMessage(data.output)
        } else if ("inspection" in data) {
            this.inspectedStats = data.inspection.stats
            this.requestIdr ||= data.inspection.requestIdr
        } else if ("log" in data) {
            this.logger?.debug(data.log, data.info)
        }
    }

    mount() {
        let result
        if ("mount" in this.base && typeof this.base.mount == "function") {
            result = this.base.mount(...arguments)
        }

        // The OffscreenCanvas needs to transfer it's canvas into the worker, do that here
        if (this.base instanceof OffscreenCanvasRenderer && this.base.offscreen) {
            this.logger?.debug("Transferred OffscreenCanvas into worker")

            const canvas = this.base.offscreen
            this.onWorkerMessage({ canvas }, [canvas])

            this.base.transferred = true
            this.base.offscreen = null
        }

        return result
    }

    cleanup() {
        this.stopped = true
        this.pendingVideo = []
        this.pendingVideoBytes = 0
        this.worker.terminate()

        if ("cleanup" in this.base && typeof this.base.cleanup == "function") {
            return this.base.cleanup(...arguments)
        }
    }

    getBase(): Pipe | null {
        return this.base
    }
}

export function workerPipe(name: string, pipeline: Pipeline): PipeStatic {
    class CustomWorkerPipe extends WorkerPipe {
        static async getInfo(): Promise<PipeInfo> {
            return await this.getInfoInternal(pipeline)
        }

        static readonly baseType = "workeroutput"
        static readonly type = "workerinput"

        constructor(base: any, logger?: Logger, options?: unknown) {
            super(base, pipeline, logger, options)
        }
    }

    Object.defineProperty(CustomWorkerPipe, "name", { value: name })

    return CustomWorkerPipe
}
