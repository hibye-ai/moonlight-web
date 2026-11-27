import { Api } from "../api.js"
import { App, ConnectionStatus, GeneralClientMessage, GeneralServerMessage, StreamCapabilities, StreamClientMessage, StreamPermissions, StreamServerMessage, StreamSettings, TransportChannelId } from "../api_bindings.js"
import { showNotification } from "../component/notification.js"
import { Component } from "../component/index.js"
import { Settings, TransportType } from "../component/settings_menu.js"
import { AudioPlayer } from "./audio/index.js"
import { buildAudioPipeline } from "./audio/pipeline.js"
import { BIG_BUFFER, ByteBuffer } from "./buffer.js"
import { defaultStreamInputConfig, StreamInput } from "./input.js"
import { Logger, LogMessageInfo } from "./log.js"
import { gatherPipeInfo } from "./pipeline/index.js"
import { StreamStats } from "./stats.js"
import { Transport, TransportShutdown } from "./transport/index.js"
import { WebSocketTransport } from "./transport/web_socket.js"
import { WebRTCTransport } from "./transport/webrtc.js"
import { allVideoCodecs, andVideoCodecs, createSupportedVideoFormatsBits, emptyVideoCodecs, getSelectedVideoCodec, hasAnyCodec, VideoCodecSupport } from "./video.js"
import { VideoRenderer } from "./video/index.js"
import { buildVideoPipeline, VideoPipelineOptions } from "./video/pipeline.js"
import { StreamLifecycle } from "../desktop/connection.js"
import { AudioJitterPolicyMode } from "./transport/audio_policy.js"

export type ExecutionEnvironment = {
    main: boolean
    worker: boolean
}

export type InfoEvent = CustomEvent<
    { type: "app", app: App } |
    { type: "serverMessage", message: string } |
    { type: "connectionComplete", capabilities: StreamCapabilities } |
    { type: "videoReady" } |
    { type: "connectionStatus", status: ConnectionStatus } |
    { type: "addDebugLine", line: string, additional?: LogMessageInfo } |
    { type: "lifecycle", lifecycle: StreamLifecycle }
>
export type InfoEventListener = (event: InfoEvent) => void

export function getStreamerSize(settings: Settings, viewerScreenSize: [number, number]): [number, number] {
    let width, height
    if (settings.videoSize == "720p") {
        width = 1280
        height = 720
    } else if (settings.videoSize == "1080p") {
        width = 1920
        height = 1080
    } else if (settings.videoSize == "1440p") {
        width = 2560
        height = 1440
    } else if (settings.videoSize == "4k") {
        width = 3840
        height = 2160
    } else if (settings.videoSize == "custom") {
        width = settings.videoSizeCustom.width
        height = settings.videoSizeCustom.height
    } else { // native
        width = viewerScreenSize[0]
        height = viewerScreenSize[1]
    }
    return [width, height]
}

function getVideoCodecHint(settings: Settings): VideoCodecSupport {
    let videoCodecHint = emptyVideoCodecs()
    if (settings.videoCodec == "h264") {
        videoCodecHint.H264 = true
        videoCodecHint.H264_HIGH8_444 = true
    } else if (settings.videoCodec == "h265") {
        videoCodecHint.H265 = true
        videoCodecHint.H265_MAIN10 = true
        videoCodecHint.H265_REXT8_444 = true
        videoCodecHint.H265_REXT10_444 = true
    } else if (settings.videoCodec == "av1") {
        videoCodecHint.AV1_MAIN8 = true
        videoCodecHint.AV1_MAIN10 = true
        videoCodecHint.AV1_HIGH8_444 = true
        videoCodecHint.AV1_HIGH10_444 = true
    } else if (settings.videoCodec == "auto") {
        videoCodecHint = allVideoCodecs()
    }

    if (isFirefox()) {
        videoCodecHint.AV1_MAIN10 = false
        videoCodecHint.AV1_HIGH10_444 = false
    }

    return videoCodecHint
}

function isFirefox(): boolean {
    return navigator.userAgent.includes("Firefox/")
}

const WEBRTC_CONNECT_TIMEOUT_MS = 15000
const FALLBACK_RECONNECT_DELAY_MS = 500
const CONTROL_RECONNECT_BASE_DELAY_MS = 750
const CONTROL_RECONNECT_MAX_DELAY_MS = 5000
const CONTROL_RECONNECT_MAX_ATTEMPTS = 5
const CONTROL_WS_SEND_BUFFER_MAX = 128
const MIN_SESSION_BITRATE_KBPS = 500

export type StreamOptions = {
    /// The user's configured bitrate. Session overrides never exceed it.
    bitrateCeilingKbps?: number
}

export type StreamMediaCounters = {
    /// Messages received from the host on the RTT channel (independent of the picture).
    hostSignals: number
    /// Packets received on data (WebSocket/data channel) media channels; null for track transports.
    dataVideoPackets: number | null
    dataAudioPackets: number | null
}

export class Stream implements Component {
    private logger: Logger = new Logger()

    private api: Api

    private hostId: number
    private appId: number

    private permissions: StreamPermissions
    private settings: Settings

    private divElement = document.createElement("div")
    private eventTarget = new EventTarget()

    private ws: WebSocket
    private iceServers: Array<RTCIceServer> | null = null
    private transportOverride: TransportType | null = null
    private webRtcFallbackCount = 0
    private controlReconnectAttempts = 0
    private controlReconnectTimer: number | null = null
    private restartingControlSocket = false
    private stopping = false
    private connectionGeneration = 0
    private controlSocketCancelled = false

    private connectionCurrent(generation: number): boolean {
        return generation === this.connectionGeneration && !this.stopping && !this.controlSocketCancelled
    }

    private videoRenderer: VideoRenderer | null = null
    private audioPlayer: AudioPlayer | null = null

    private input: StreamInput
    private stats: StreamStats

    private streamerSize: [number, number]
    private hasConnectionComplete = false
    private hasVideoReady = false
    private hasDispatchedVideoReady = false

    private userStopped = false
    private autoReconnectEnabled = true
    private manualReconnectInFlight: Promise<boolean> | null = null
    private reconnectCancellation = 0
    private bitrateCeilingKbps: number
    private hostSignals = 0
    private dataVideoPackets: number | null = null
    private dataAudioPackets: number | null = null

    constructor(api: Api, hostId: number, appId: number, settings: Settings, viewerScreenSize: [number, number], permissions: StreamPermissions, options?: StreamOptions) {
        this.logger.addInfoListener((info, type) => {
            this.debugLog(info, { type: type ?? undefined })
        })

        this.api = api

        this.hostId = hostId
        this.appId = appId

        this.permissions = permissions
        // Session-only copy: bitrate/audio preset changes made during this session must
        // never leak into the caller's (saved) settings object.
        this.settings = { ...settings }

        let ceiling = Math.max(options?.bitrateCeilingKbps ?? settings.bitrate, settings.bitrate)
        if (permissions.maximum_bitrate_kbps != null) {
            ceiling = Math.min(ceiling, permissions.maximum_bitrate_kbps)
        }
        this.bitrateCeilingKbps = ceiling

        this.streamerSize = getStreamerSize(settings, viewerScreenSize)

        this.ws = this.createControlWebSocket()
        this.sendInitMessage()

        // Stream Input
        const streamInputConfig = defaultStreamInputConfig()
        Object.assign(streamInputConfig, {
            mouseMode: this.settings.mouseMode,
            mouseScrollMode: this.settings.mouseScrollMode,
            touchMode: this.settings.touchMode,
            localCursorSensitivity: this.settings.localCursorSensitivity,
            controllerConfig: this.settings.controllerConfig
        })
        this.input = new StreamInput(streamInputConfig)

        // Stream Stats
        this.stats = new StreamStats(this.logger)
    }

    private debugLog(message: string, additional?: LogMessageInfo) {
        for (const line of message.split("\n")) {
            const event: InfoEvent = new CustomEvent("stream-info", {
                detail: { type: "addDebugLine", line, additional }
            })

            this.eventTarget.dispatchEvent(event)
        }
    }
    private emitLifecycle(lifecycle: StreamLifecycle) {
        const event: InfoEvent = new CustomEvent("stream-info", {
            detail: { type: "lifecycle", lifecycle }
        })
        this.eventTarget.dispatchEvent(event)
    }
    private resetVideoReadyState() {
        this.hasConnectionComplete = false
        this.hasVideoReady = false
        this.hasDispatchedVideoReady = false
    }
    private markConnectionComplete() {
        this.hasConnectionComplete = true
        if (this.controlReconnectAttempts > 0) {
            this.debugLog(`Control WebSocket recovery completed after ${this.controlReconnectAttempts} attempt(s)`)
        }
        this.controlReconnectAttempts = 0
        this.tryDispatchVideoReady()
    }
    private markVideoReady() {
        this.hasVideoReady = true
        this.tryDispatchVideoReady()
    }
    private tryDispatchVideoReady() {
        if (!this.hasConnectionComplete || !this.hasVideoReady || this.hasDispatchedVideoReady) {
            return
        }

        this.hasDispatchedVideoReady = true
        const event: InfoEvent = new CustomEvent("stream-info", {
            detail: { type: "videoReady" }
        })
        this.eventTarget.dispatchEvent(event)
        this.emitLifecycle({ phase: "connected" })
    }

    private async onMessage(message: StreamServerMessage) {
        const generation = this.connectionGeneration
        if (!this.connectionCurrent(generation)) return
        if ("DebugLog" in message) {
            const debugLog = message.DebugLog
            if (debugLog.ty === "fatal" || debugLog.ty === "fatalDescription") {
                this.autoReconnectEnabled = false
                this.clearControlReconnectTimer()
                this.releaseInput()
                this.emitLifecycle({ phase: "fatal" })
            }

            this.debugLog(debugLog.message, {
                type: debugLog.ty ?? undefined
            })
        } else if ("UpdateApp" in message) {
            const event: InfoEvent = new CustomEvent("stream-info", {
                detail: { type: "app", app: message.UpdateApp.app }
            })

            this.eventTarget.dispatchEvent(event)
        } else if ("ConnectionComplete" in message) {
            const capabilities = message.ConnectionComplete.capabilities
            const formatRaw = message.ConnectionComplete.format
            const width = message.ConnectionComplete.width
            const height = message.ConnectionComplete.height
            const fps = message.ConnectionComplete.fps

            const audioSampleRate = message.ConnectionComplete.audio_sample_rate
            const audioChannelCount = message.ConnectionComplete.audio_channel_count
            const audioStreams = message.ConnectionComplete.audio_streams
            const audioCoupledStreams = message.ConnectionComplete.audio_coupled_streams
            const audioSamplesPerFrame = message.ConnectionComplete.audio_samples_per_frame
            const audioMapping = message.ConnectionComplete.audio_mapping

            const format = getSelectedVideoCodec(formatRaw)
            if (format == null) {
                this.debugLog(`Video Format ${formatRaw} was not found! Couldn't start stream!`, { type: "fatal" })
                return
            }

            const event: InfoEvent = new CustomEvent("stream-info", {
                detail: { type: "connectionComplete", capabilities }
            })

            this.eventTarget.dispatchEvent(event)

            this.input.onStreamStart(capabilities, [width, height])

            this.stats.setVideoInfo(format ?? "Unknown", width, height, fps)
            // HDR state will be set when server sends HdrModeUpdate message
            // Don't initialize from settings.hdr because that's just the user's preference,
            // not the actual HDR state (which depends on host support, display, and codec)
            if (this.settings.hdr) {
                this.debugLog("HDR requested by user, waiting for host confirmation...")
            }

            // we should allow streaming without audio
            if (!this.audioPlayer) {
                showNotification("Failed to find supported audio player -> audio is missing.")
            }

            if (!this.videoRenderer || !this.audioPlayer) {
                throw "Video renderer or audio player not initialized!"
            }

            await Promise.all([
                this.videoRenderer.setup({
                    codec: format,
                    fps,
                    width,
                    height,
                }),
                this.audioPlayer.setup({
                    sampleRate: audioSampleRate,
                    channels: audioChannelCount,
                    streams: audioStreams,
                    coupledStreams: audioCoupledStreams,
                    samplesPerFrame: audioSamplesPerFrame,
                    mapping: audioMapping,
                })
            ])

            if (!this.connectionCurrent(generation)) return
            this.markConnectionComplete()
        } else if ("ConnectionTerminated" in message) {
            const code = message.ConnectionTerminated.error_code

            if (this.restartingControlSocket) {
                this.debugLog(`Previous stream session terminated during control-channel recovery with code ${code}`)
            } else {
                this.stopping = true
                this.clearControlReconnectTimer()
                this.input.releaseAll()
                this.debugLog(`ConnectionTerminated with code ${code}`, { type: "fatalDescription" })
                this.emitLifecycle({ phase: "terminated", code })
            }
        }
        // -- WebRTC Config
        else if ("Setup" in message) {
            const iceServers = message.Setup.ice_servers

            this.iceServers = iceServers

            this.debugLog(`window.isSecureContext: ${window.isSecureContext}`)
            this.debugLog(`Using WebRTC Ice Servers: ${createPrettyList(
                iceServers.map(server => server.urls).reduce((list, url) => list.concat(url), [])
            )}`)

            await this.startConnection()
        }
        // -- WebRTC
        else if ("WebRtc" in message) {
            const webrtcMessage = message.WebRtc
            if (this.transport instanceof WebRTCTransport) {
                this.transport.onReceiveMessage(webrtcMessage)
            } else {
                this.debugLog(`Received WebRTC message but transport is currently ${this.transport?.implementationName}`)
            }
        }
    }

    async startConnection() {
        const generation = this.connectionGeneration
        if (!this.connectionCurrent(generation)) return
        this.debugLog(`Permissions: ${JSON.stringify(this.permissions)}`)

        const desiredTransport = this.transportOverride ?? this.settings.dataTransport
        this.debugLog(`Using transport: ${desiredTransport}`)

        if (desiredTransport == "auto") {
            const shutdownReason = await this.tryWebRTCTransport()
            if (generation != this.connectionGeneration || this.stopping) {
                return
            }

            if (shutdownReason == "failednoconnect") {
                this.webRtcFallbackCount++
                this.debugLog("Failed to establish WebRTC connection. Falling back to Web Socket transport.", { type: "ifErrorDescription" })
                await this.restartWithFreshTransportFallback("websocket")
                return
            }
        } else if (desiredTransport == "webrtc") {
            await this.tryWebRTCTransport()
            if (generation != this.connectionGeneration || this.stopping) {
                return
            }
        } else if (desiredTransport == "websocket") {
            await this.tryWebSocketTransport()
            if (generation != this.connectionGeneration || this.stopping) {
                return
            }
        }

        this.debugLog("Tried all configured transport options but no connection was possible", { type: "fatal" })
        this.input.releaseAll()
        this.emitLifecycle({ phase: "fatal" })
    }

    private transport: Transport | null = null

    private createControlWebSocket(): WebSocket {
        this.controlSocketCancelled = false
        const generation = this.connectionGeneration
        const wsApiHost = this.api.host_url.replace(/^http(s)?:/, "ws$1:")
        const ws = new WebSocket(`${wsApiHost}/host/stream`)

        ws.addEventListener("error", (event) => {
            if (this.ws !== ws || !this.connectionCurrent(generation)) {
                return
            }
            this.onError(event)
        })
        ws.addEventListener("open", () => {
            if (this.ws !== ws || !this.connectionCurrent(generation)) {
                return
            }
            this.onWsOpen()
        })
        ws.addEventListener("close", () => {
            if (this.ws !== ws || !this.connectionCurrent(generation)) {
                return
            }
            this.onWsClose()
        })
        ws.addEventListener("message", (event) => {
            if (this.ws !== ws || !this.connectionCurrent(generation)) {
                return
            }
            this.onRawWsMessage(event)
        })

        return ws
    }
    private sendInitMessage() {
        this.sendWsMessage({
            Init: {
                host_id: this.hostId,
                app_id: this.appId,
                video_frame_queue_size: this.settings.videoFrameQueueSize,
                audio_sample_queue_size: this.settings.audioSampleQueueSize,
            }
        })
    }
    private async restartWithFreshTransportFallback(transport: TransportType): Promise<void> {
        const generation = ++this.connectionGeneration
        this.transportOverride = transport
        this.resetVideoReadyState()
        this.restartingControlSocket = true

        try {
            if (this.transport) {
                const previous = this.transport
                this.transport = null
                await previous.close()
            }

            if (!this.connectionCurrent(generation)) return
            this.wsSendBuffer.length = 0
            const oldWs = this.ws

            if (oldWs.readyState == WebSocket.OPEN || oldWs.readyState == WebSocket.CONNECTING) {
                oldWs.close()
                await new Promise<void>((resolve) => {
                    const timeout = window.setTimeout(() => resolve(), 1000)
                    oldWs.addEventListener("close", () => {
                        window.clearTimeout(timeout)
                        resolve()
                    }, { once: true })
                })
            }

            await new Promise((resolve) => window.setTimeout(resolve, FALLBACK_RECONNECT_DELAY_MS))

            if (!this.connectionCurrent(generation)) {
                return
            }
            this.ws = this.createControlWebSocket()
            this.sendInitMessage()
        } finally {
            if (generation === this.connectionGeneration) this.restartingControlSocket = false
        }

        if (!this.stopping && this.ws.readyState == WebSocket.CLOSED) {
            this.scheduleControlReconnect()
        }
    }

    private setTransport(transport: Transport) {
        if (this.transport) {
            this.transport.close()
        }

        this.transport = transport

        this.input.setTransport(this.transport)
        this.stats.setTransport(this.transport)

        const rtt = this.transport.getChannel(TransportChannelId.RTT)
        if (rtt.type == "data") {
            rtt.addReceiveListener((data) => {
                this.hostSignals += 1
                const buffer = new ByteBuffer(data.byteLength)
                buffer.putU8Array(new Uint8Array(data))
                buffer.flip()

                const ty = buffer.getU8()
                if (ty == 0) {
                    rtt.send(data)
                }
            })
        } else {
            this.debugLog("Failed to get rtt as data transport channel. Cannot respond to rtt packets")
        }

        // Setup GENERAL channel listener for HDR mode updates
        const generalChannel = this.transport.getChannel(TransportChannelId.GENERAL)
        this.debugLog(`[GENERAL] Setting up GENERAL channel listener, type=${generalChannel.type}`)
        if (generalChannel.type === "data") {
            generalChannel.addReceiveListener((data: ArrayBuffer) => {
                this.onGeneralChannelMessage(data)
            })
            this.debugLog(`[GENERAL] GENERAL channel listener registered`)
        } else {
            this.debugLog(`[GENERAL] Cannot register listener, channel type is not 'data'`)
        }
    }

    private onGeneralChannelMessage(data: ArrayBuffer) {
        this.debugLog(`[GENERAL] Received message on GENERAL channel, size=${data.byteLength}`)
        const buffer = new Uint8Array(data)
        if (buffer.length < 2) {
            this.debugLog(`[GENERAL] Message too short: ${buffer.length} bytes`)
            return
        }

        const textLength = (buffer[0] << 8) | buffer[1]
        if (buffer.length < 2 + textLength) {
            this.debugLog(`[GENERAL] Message incomplete: expected ${2 + textLength} bytes, got ${buffer.length}`)
            return
        }

        const text = new TextDecoder().decode(buffer.slice(2, 2 + textLength))
        this.debugLog(`[GENERAL] Parsed message: ${text}`)
        try {
            const message: GeneralServerMessage = JSON.parse(text)
            this.handleGeneralMessage(message)
        } catch (err) {
            this.debugLog(`Failed to parse general message: ${err}`)
        }
    }

    private handleGeneralMessage(message: GeneralServerMessage) {
        if ("HdrModeUpdate" in message) {
            const hdrUpdate = message.HdrModeUpdate
            if (hdrUpdate) {
                const enabled = hdrUpdate.enabled
                this.debugLog(`HDR mode ${enabled ? "enabled" : "disabled"}`)
                this.setHdrMode(enabled)
            }
        } else if ("ConnectionStatusUpdate" in message) {
            const statusUpdate = message.ConnectionStatusUpdate
            if (statusUpdate) {
                const status = statusUpdate.status
                const event: InfoEvent = new CustomEvent("stream-info", {
                    detail: { type: "connectionStatus", status }
                })
                this.eventTarget.dispatchEvent(event)
            }
        }
    }

    private setHdrMode(enabled: boolean) {
        this.stats.setHdrEnabled(enabled)
        if (this.videoRenderer) {
            if ("setHdrMode" in this.videoRenderer && typeof this.videoRenderer.setHdrMode === "function") {
                this.videoRenderer.setHdrMode(enabled)
            }
        }
    }

    private sendGeneralMessage(message: GeneralClientMessage): boolean {
        const general = this.transport?.getChannel(TransportChannelId.GENERAL)

        if (!general || general.type != "data") {
            return false
        }

        const text = JSON.stringify(message)

        const buffer = BIG_BUFFER
        buffer.reset()
        buffer.putU16(text.length)
        buffer.putUtf8Raw(text)
        buffer.flip()

        general.send(buffer.getRemainingBuffer().buffer)

        return true
    }

    private async tryWebRTCTransport(): Promise<TransportShutdown> {
        const generation = this.connectionGeneration
        if (!this.connectionCurrent(generation)) return "failednoconnect"
        if (!this.permissions.allow_transport_webrtc) {
            this.debugLog("Not trying WebRTC transport because permissions disallow it")
            return "failednoconnect"
        }

        this.debugLog("Trying WebRTC transport")

        this.sendWsMessage({
            SetTransport: "WebRTC"
        })

        if (!this.iceServers) {
            this.debugLog(`Failed to try WebRTC Transport: no ice servers available`)
            return "failednoconnect"
        }

        const transport = new WebRTCTransport(this.logger)
        transport.onsendmessage = (message) => {
            if (this.connectionCurrent(generation)) this.sendWsMessage({ WebRtc: message })
        }
        // Diagnostic only: the default ("lowLatency") reproduces the previous behaviour.
        transport.audioJitterPolicy = {
            mode: this.settings.audioJitterPolicy ?? "auto",
            targetMs: this.settings.audioJitterTargetMs,
        }
        this.debugLog(`Audio receiver jitter policy: ${transport.getAudioJitterPolicyName()}`)

        transport.initPeer({
            iceServers: this.iceServers
        })
        this.setTransport(transport)

        const videoCodecSupport = await this.createPipelines()
        if (!this.connectionCurrent(generation)) return "failednoconnect"
        if (!videoCodecSupport) {
            this.debugLog("No video pipeline was found for the codec that was specified. If you're unsure which codecs are supported use H264.", { type: "fatalDescription" })

            await transport.close()
            return "failednoconnect"
        }

        // Starting the stream will start negotiation
        await this.startStream(videoCodecSupport)
        if (!this.connectionCurrent(generation)) return "failednoconnect"

        // Wait for negotiation, but don't let a stuck ICE check block fallback forever.
        const result = await new Promise<boolean>((resolve) => {
            const timeout = window.setTimeout(async () => {
                if (!this.connectionCurrent(generation)) { resolve(false); return }
                this.debugLog(`WebRTC negotiation timed out after ${WEBRTC_CONNECT_TIMEOUT_MS}ms`)
                transport.onconnect = null
                transport.onclose = null
                await transport.close()
                resolve(false)
            }, WEBRTC_CONNECT_TIMEOUT_MS)

            transport.onconnect = () => {
                window.clearTimeout(timeout)
                resolve(true)
            }
            transport.onclose = () => {
                window.clearTimeout(timeout)
                resolve(false)
            }
        })
        this.debugLog(`WebRTC negotiation success: ${result}`)

        if (!result) {
            return "failednoconnect"
        }

        return new Promise((resolve) => {
            transport.onclose = (shutdown) => {
                resolve(shutdown)
            }
        })
    }
    private async tryWebSocketTransport() {
        const generation = this.connectionGeneration
        if (!this.connectionCurrent(generation)) return
        if (!this.permissions.allow_transport_websockets) {
            this.debugLog("Not trying WebSocket transport becaues permissions disallow it")
            return
        }

        this.debugLog("Trying Web Socket transport")

        this.sendWsMessage({
            SetTransport: "WebSocket"
        })

        const transport = new WebSocketTransport(this.ws, BIG_BUFFER, this.logger)

        this.setTransport(transport)

        const videoCodecSupport = await this.createPipelines()
        if (!this.connectionCurrent(generation)) return
        if (!videoCodecSupport) {
            this.debugLog("Failed to start stream because no video pipeline with support for the specified codec was found!", { type: "fatalDescription" })
            return
        }

        await this.startStream(videoCodecSupport)
        if (!this.connectionCurrent(generation)) return

        return new Promise((resolve) => {
            transport.onclose = (shutdown) => {
                resolve(shutdown)
            }
        })
    }

    private async createPipelines(): Promise<VideoCodecSupport | null> {
        const generation = this.connectionGeneration
        // Print supported pipes
        const pipesInfo = await gatherPipeInfo()
        if (!this.connectionCurrent(generation)) return null

        this.logger.debug(`Supported Pipes: {`)
        let isFirst = true
        for (const [pipe, info] of pipesInfo) {
            this.logger.debug(`${isFirst ? "" : ","}"${pipe.name}": ${JSON.stringify(info)}`)
            isFirst = false
        }
        this.logger.debug(`}`)

        // Create pipelines
        const [supportedVideoCodecs] = await Promise.all([this.createVideoRenderer(), this.createAudioPlayer()])
        if (!this.connectionCurrent(generation)) return null

        const videoPipelineName = `${this.transport?.getChannel(TransportChannelId.HOST_VIDEO).type} (transport) -> ${this.videoRenderer?.implementationName} (renderer)`
        this.debugLog(`Using video pipeline: ${videoPipelineName}`)

        const audioPipelineName = `${this.transport?.getChannel(TransportChannelId.HOST_AUDIO).type} (transport) -> ${this.audioPlayer?.implementationName} (player)`
        this.debugLog(`Using audio pipeline: ${audioPipelineName}`)

        this.stats.setVideoPipeline(videoPipelineName, this.videoRenderer)
        this.stats.setAudioPipeline(audioPipelineName, this.audioPlayer)

        return supportedVideoCodecs
    }
    private async createVideoRenderer(): Promise<VideoCodecSupport | null> {
        const generation = this.connectionGeneration
        if (this.videoRenderer) {
            this.debugLog("Found an old video renderer -> cleaning it up")

            this.videoRenderer.unmount(this.divElement)
            this.videoRenderer.cleanup()
            this.videoRenderer = null
        }
        if (!this.transport) {
            this.debugLog("Failed to setup video without transport")
            return null
        }

        const codecHint = getVideoCodecHint(this.settings)
        this.debugLog(`Codec Hint by the user: ${JSON.stringify(codecHint)}`)

        if (!hasAnyCodec(codecHint)) {
            this.debugLog("Couldn't find any supported video format. Change the codec option to H264 in the settings if you're unsure which codecs are supported.", { type: "fatalDescription" })
            return null
        }

        const transportCodecSupport = await this.transport.setupHostVideo({
            type: ["videotrack", "data"]
        })
        if (!this.connectionCurrent(generation)) return null
        this.debugLog(`Transport supports these video codecs: ${JSON.stringify(transportCodecSupport)}`)

        const videoSettings: VideoPipelineOptions = {
            supportedVideoCodecs: andVideoCodecs(codecHint, transportCodecSupport),
            canvasRenderer: this.settings.canvasRenderer,
            forceVideoElementRenderer: this.settings.forceVideoElementRenderer,
            canvasVsync: this.settings.canvasVsync
        }

        let pipelineCodecSupport
        const video = this.transport.getChannel(TransportChannelId.HOST_VIDEO)
        if (video.type == "videotrack") {
            const { videoRenderer, supportedCodecs, error } = await buildVideoPipeline("videotrack", videoSettings, this.logger)
            if (!this.connectionCurrent(generation)) { videoRenderer?.cleanup(); return null }

            if (error) {
                return null
            }
            pipelineCodecSupport = supportedCodecs

            videoRenderer.mount(this.divElement)

            video.addTrackListener((track) => {
                if (!this.connectionCurrent(generation)) return
                this.markVideoReady()
                videoRenderer.setTrack(track)
            })

            this.videoRenderer = videoRenderer
        } else if (video.type == "data") {
            const { videoRenderer, supportedCodecs, error } = await buildVideoPipeline("data", videoSettings, this.logger)
            if (!this.connectionCurrent(generation)) { videoRenderer?.cleanup(); return null }

            if (error) {
                return null
            }
            pipelineCodecSupport = supportedCodecs

            videoRenderer.mount(this.divElement)

            this.dataVideoPackets = this.dataVideoPackets ?? 0
            video.addReceiveListener((data) => {
                if (!this.connectionCurrent(generation)) return
                this.dataVideoPackets = (this.dataVideoPackets ?? 0) + 1
                this.markVideoReady()
                videoRenderer.submitPacket(data)

                // data pipeline support requesting idrs over video channel
                if (videoRenderer.pollRequestIdr()) {
                    const buffer = new ByteBuffer(1)

                    buffer.putU8(0)

                    buffer.flip()

                    video.send(buffer.getRemainingBuffer().buffer)
                }
            })

            this.videoRenderer = videoRenderer
        } else {
            this.debugLog(`Failed to create video pipeline with transport channel of type ${video.type} (${this.transport.implementationName})`)
            return null
        }

        return pipelineCodecSupport
    }
    private async createAudioPlayer(): Promise<boolean> {
        const generation = this.connectionGeneration
        if (this.audioPlayer) {
            this.debugLog("Found an old audio player -> cleaning it up")

            this.audioPlayer.unmount(this.divElement)
            this.audioPlayer.cleanup()
            this.audioPlayer = null
        }
        if (!this.transport) {
            this.debugLog("Failed to setup audio without transport")
            return false
        }

        this.transport.setupHostAudio({
            type: ["audiotrack", "data"]
        })

        const audio = this.transport?.getChannel(TransportChannelId.HOST_AUDIO)
        if (audio.type == "audiotrack") {
            const { audioPlayer, error } = await buildAudioPipeline("audiotrack", this.settings, this.logger)
            if (!this.connectionCurrent(generation)) { audioPlayer?.cleanup(); return false }

            if (error) {
                return false
            }

            audioPlayer.mount(this.divElement)

            audio.addTrackListener((track) => { if (this.connectionCurrent(generation)) audioPlayer.setTrack(track) })

            this.audioPlayer = audioPlayer
        } else if (audio.type == "data") {
            const { audioPlayer, error } = await buildAudioPipeline("data", this.settings, this.logger)
            if (!this.connectionCurrent(generation)) { audioPlayer?.cleanup(); return false }

            if (error) {
                return false
            }

            audioPlayer.mount(this.divElement)

            this.dataAudioPackets = this.dataAudioPackets ?? 0
            audio.addReceiveListener((data) => {
                if (!this.connectionCurrent(generation)) return
                this.dataAudioPackets = (this.dataAudioPackets ?? 0) + 1
                audioPlayer.submitPacket(data)
            })

            this.audioPlayer = audioPlayer
        } else {
            this.debugLog(`Cannot find audio pipeline for transport type "${audio.type}"`)
            return false
        }

        return true
    }
    private async startStream(videoCodecSupport: VideoCodecSupport): Promise<void> {
        const settings: StreamSettings = {
            bitrate_kbps: this.settings.bitrate,
            fps: this.settings.fps,
            width: this.streamerSize[0],
            height: this.streamerSize[1],
            play_audio_local: this.settings.playAudioLocal,
            supported_codecs: createSupportedVideoFormatsBits(videoCodecSupport),
            hdr: this.settings.hdr ?? false,
        }

        const message: StreamClientMessage = {
            StartStream: {
                settings
            }
        }
        this.debugLog(`Starting stream with info: ${JSON.stringify(message)}`)
        this.debugLog(`Stream video codec info: ${JSON.stringify(videoCodecSupport)}`)

        // Log HDR requirements if HDR is requested
        if (this.settings.hdr) {
            const hasHdrCodec = videoCodecSupport.H265_MAIN10 || videoCodecSupport.AV1_MAIN10
            if (!hasHdrCodec) {
                this.debugLog(`Warning: HDR requested but no 10-bit codec available. HDR requires H265_MAIN10 or AV1_MAIN10 support.`)
            } else {
                this.debugLog(`HDR codec available: H265_MAIN10=${videoCodecSupport.H265_MAIN10}, AV1_MAIN10=${videoCodecSupport.AV1_MAIN10}`)
            }
        }

        this.sendWsMessage(message)
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.divElement)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.divElement)
    }

    getVideoRenderer(): VideoRenderer | null {
        return this.videoRenderer
    }
    getAudioPlayer(): AudioPlayer | null {
        return this.audioPlayer
    }

    // -- Raw Web Socket stuff
    private wsSendBuffer: Array<string> = []

    private onWsOpen() {
        this.debugLog(`Web Socket Open`)

        for (const raw of this.wsSendBuffer.splice(0)) {
            this.ws.send(raw)
        }
    }
    private onWsClose() {
        this.debugLog(`Web Socket Closed`)
        if (this.stopping || this.restartingControlSocket) {
            return
        }
        // Keys/buttons held on the old connection would otherwise stay "down" locally.
        this.input.releaseAll()
        this.emitLifecycle({ phase: "socketClosed" })
        this.scheduleControlReconnect()
    }

    private clearControlReconnectTimer() {
        if (this.controlReconnectTimer !== null) {
            window.clearTimeout(this.controlReconnectTimer)
            this.controlReconnectTimer = null
        }
    }

    private scheduleControlReconnect() {
        if (this.stopping || this.restartingControlSocket || this.controlReconnectTimer !== null) {
            return
        }
        if (!this.autoReconnectEnabled) {
            this.debugLog(`Control WebSocket disconnected; automatic reconnect is paused`)
            return
        }
        if (this.controlReconnectAttempts >= CONTROL_RECONNECT_MAX_ATTEMPTS) {
            this.debugLog(`Control WebSocket recovery failed after ${CONTROL_RECONNECT_MAX_ATTEMPTS} attempts`, { type: "fatalDescription" })
            this.emitLifecycle({ phase: "retryExhausted" })
            return
        }

        this.controlReconnectAttempts += 1
        const delay = Math.min(
            CONTROL_RECONNECT_BASE_DELAY_MS * (2 ** (this.controlReconnectAttempts - 1)),
            CONTROL_RECONNECT_MAX_DELAY_MS,
        )
        this.debugLog(`Control WebSocket disconnected; reconnecting in ${delay}ms (attempt ${this.controlReconnectAttempts}/${CONTROL_RECONNECT_MAX_ATTEMPTS})`)
        this.controlReconnectTimer = window.setTimeout(() => {
            this.controlReconnectTimer = null
            void this.reconnectControlWebSocket()
        }, delay)
        this.emitLifecycle({ phase: "retryScheduled", attempt: this.controlReconnectAttempts, max: CONTROL_RECONNECT_MAX_ATTEMPTS, delayMs: delay })
    }

    private async reconnectControlWebSocket() {
        if (this.stopping || !this.autoReconnectEnabled) {
            return
        }

        this.restartingControlSocket = true
        const generation = ++this.connectionGeneration
        this.resetVideoReadyState()

        try {
            if (this.transport) {
                const previous = this.transport
                this.transport = null
                await previous.close()
            }

            if (!this.connectionCurrent(generation)) return
            this.wsSendBuffer.length = 0
            this.iceServers = null
            if (this.settings.dataTransport == "auto") {
                this.transportOverride = null
            }

            if (this.stopping || !this.autoReconnectEnabled) return
            this.ws = this.createControlWebSocket()
            this.emitLifecycle({ phase: "connecting" })
            this.sendInitMessage()
        } catch (error) {
            this.debugLog(`Control WebSocket reconnect failed: ${String(error)}`)
        } finally {
            if (generation === this.connectionGeneration) this.restartingControlSocket = false
        }

        if (!this.stopping && this.ws.readyState == WebSocket.CLOSED) {
            this.scheduleControlReconnect()
        }
    }

    /// Explicit, user or recovery initiated reconnect with the same host, app and session
    /// settings. Starts a fresh control socket (which restarts the stream on the host) and a
    /// fresh retry budget. Refused once the stream was stopped.
    reconnect(reason: string = "manual"): Promise<boolean> {
        if (this.userStopped) {
            return Promise.resolve(false)
        }
        if (this.manualReconnectInFlight) {
            return this.manualReconnectInFlight
        }

        const cancellation = this.reconnectCancellation
        const run = async (): Promise<boolean> => {
            this.debugLog(`Reconnecting stream (${reason.replace(/[^a-z]/gi, "")})`)
            // A host-side termination stops automatic retries; an explicit reconnect may
            // start over. A user stop (stop()) is final and handled above.
            this.stopping = false
            this.clearControlReconnectTimer()
            this.controlReconnectAttempts = 0
            this.autoReconnectEnabled = true
            this.restartingControlSocket = true
            const generation = ++this.connectionGeneration
            this.controlSocketCancelled = false
            this.resetVideoReadyState()
            this.input.releaseAll()
            // Invalidate session-bound work before awaiting transport teardown. Quality
            // reconnects enter here directly, without the connection UI's manual action.
            this.emitLifecycle({ phase: "connecting" })

            try {
                if (this.transport) {
                    const transport = this.transport
                    this.transport = null
                    await transport.close()
                }

                if (!this.connectionCurrent(generation)) return false
                this.wsSendBuffer.length = 0
                this.iceServers = null
                if (this.settings.dataTransport == "auto") {
                    this.transportOverride = null
                }

                const oldWs = this.ws
                if (oldWs.readyState == WebSocket.OPEN || oldWs.readyState == WebSocket.CONNECTING) {
                    oldWs.close()
                }
                if (oldWs.readyState != WebSocket.CLOSED) {
                    await new Promise<void>((resolve) => {
                        const timeout = window.setTimeout(() => resolve(), 1000)
                        oldWs.addEventListener("close", () => {
                            window.clearTimeout(timeout)
                            resolve()
                        }, { once: true })
                    })
                }

                // Give the host a moment to release the previous session.
                await new Promise((resolve) => window.setTimeout(resolve, FALLBACK_RECONNECT_DELAY_MS))

                if (!this.connectionCurrent(generation) || cancellation !== this.reconnectCancellation || !this.autoReconnectEnabled) {
                    return false
                }
                this.ws = this.createControlWebSocket()
                this.sendInitMessage()
            } catch (error) {
                this.debugLog(`Stream reconnect failed: ${String(error)}`)
            } finally {
                if (generation === this.connectionGeneration) this.restartingControlSocket = false
            }

            if (!this.stopping && this.ws.readyState == WebSocket.CLOSED) {
                this.scheduleControlReconnect()
            }
            return !this.stopping
        }

        this.manualReconnectInFlight = run().finally(() => {
            this.manualReconnectInFlight = null
        })
        return this.manualReconnectInFlight
    }

    /// Cancel scheduled and in-progress recovery without stopping an established session.
    cancelReconnect() {
        this.reconnectCancellation += 1
        this.autoReconnectEnabled = false
        const hadTimer = this.controlReconnectTimer !== null
        this.clearControlReconnectTimer()
        if (!this.hasDispatchedVideoReady || this.restartingControlSocket || this.ws.readyState !== WebSocket.OPEN) {
            this.connectionGeneration += 1
            this.controlSocketCancelled = true
            this.restartingControlSocket = false
            this.wsSendBuffer.length = 0
            this.iceServers = null
            if (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING) this.ws.close()
        }
        if (hadTimer) {
            this.debugLog("Automatic reconnect cancelled")
        }
        this.emitLifecycle({ phase: "cancelled" })
    }

    /// Pause automatic retries (offline, auth denied) without counting them as attempts.
    setAutoReconnectEnabled(enabled: boolean) {
        this.autoReconnectEnabled = enabled
        if (!enabled) {
            this.clearControlReconnectTimer()
        }
    }

    /// Reconnect with a different bitrate for this session only. The value is clamped to
    /// the configured ceiling; saved settings are not touched. This is a stream restart,
    /// there is no live bitrate API.
    reconnectWithBitrate(kbps: number, reason: string = "quality"): Promise<boolean> {
        if (!Number.isFinite(kbps)) {
            return Promise.resolve(false)
        }
        const floor = Math.min(MIN_SESSION_BITRATE_KBPS, this.bitrateCeilingKbps)
        this.settings.bitrate = Math.round(Math.max(floor, Math.min(kbps, this.bitrateCeilingKbps)))
        return this.reconnect(reason)
    }

    getSessionBitrate(): number {
        return this.settings.bitrate
    }
    getBitrateCeiling(): number {
        return this.bitrateCeilingKbps
    }

    /// Change the audio receiver preset for this session. Applied to the live WebRTC
    /// receiver when the browser exposes jitterBufferTarget, otherwise on the next
    /// (re)connect of this session. Never persisted.
    setSessionAudioPreset(mode: AudioJitterPolicyMode): { live: boolean } {
        this.settings.audioJitterPolicy = mode
        if (this.transport instanceof WebRTCTransport) {
            const live = this.transport.setAudioJitterPolicyLive({ mode, targetMs: this.settings.audioJitterTargetMs })
            return { live }
        }
        return { live: false }
    }
    getSessionAudioPreset(): AudioJitterPolicyMode {
        return this.settings.audioJitterPolicy ?? "auto"
    }

    getTransportKind(): "webrtc" | "websocket" | "unknown" {
        if (this.transport instanceof WebRTCTransport) return "webrtc"
        if (this.transport instanceof WebSocketTransport) return "websocket"
        return "unknown"
    }

    getTransportDiagnostics() {
        return { requestedTransport: this.settings.dataTransport, webRtcFallbackCount: this.webRtcFallbackCount }
    }

    getMediaCounters(): StreamMediaCounters {
        return {
            hostSignals: this.hostSignals,
            dataVideoPackets: this.dataVideoPackets,
            dataAudioPackets: this.dataAudioPackets,
        }
    }

    /// Release every key and mouse button this client believes is pressed.
    releaseInput(_reason?: string) {
        this.input.releaseAll()
    }

    isStopped(): boolean {
        return this.userStopped
    }

    private onError(event: Event) {
        this.debugLog(`Web Socket or WebRtcPeer Error`)

        console.error(`Web Socket or WebRtcPeer Error`, event)
    }

    private sendWsMessage(message: StreamClientMessage) {
        if (this.controlSocketCancelled || this.stopping) return
        const raw = JSON.stringify(message)
        if (this.ws.readyState == WebSocket.OPEN) {
            this.ws.send(raw)
        } else {
            if (this.wsSendBuffer.length >= CONTROL_WS_SEND_BUFFER_MAX) {
                this.wsSendBuffer.shift()
                this.debugLog(`Control WebSocket send buffer full; dropping oldest queued message`)
            }
            this.wsSendBuffer.push(raw)
        }
    }
    private onRawWsMessage(event: MessageEvent) {
        const message = event.data
        if (typeof message != "string") {
            return
        }

        try {
            const json: StreamServerMessage = JSON.parse(message)
            void this.onMessage(json).catch((error) => {
                this.debugLog(`Failed to handle control WebSocket message: ${String(error)}`)
            })
        } catch (error) {
            this.debugLog(`Failed to parse control WebSocket message: ${String(error)}`)
        }
    }

    stop(): Promise<boolean> {
        this.stopping = true
        this.userStopped = true
        this.connectionGeneration += 1
        this.clearControlReconnectTimer()
        this.input.releaseAll()
        this.emitLifecycle({ phase: "stopped" })

        if (!this.sendGeneralMessage("Stop")) {
            if (this.ws.readyState == WebSocket.OPEN || this.ws.readyState == WebSocket.CONNECTING) {
                this.ws.close()
            }
            return Promise.resolve(false)
        }

        // Wait for the message to get sent
        return new Promise((resolve, _reject) => {
            setTimeout(() => resolve(true), 100)
        })
    }

    // -- Class Api
    addInfoListener(listener: InfoEventListener) {
        this.eventTarget.addEventListener("stream-info", listener as EventListenerOrEventListenerObject)
    }
    removeInfoListener(listener: InfoEventListener) {
        this.eventTarget.removeEventListener("stream-info", listener as EventListenerOrEventListenerObject)
    }

    getInput(): StreamInput {
        return this.input
    }
    getStats(): StreamStats {
        return this.stats
    }

    getStreamerSize(): [number, number] {
        return this.streamerSize
    }
}

function createPrettyList(list: Array<string>): string {
    return `[${list.join(", ")}]`
}
