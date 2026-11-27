import { TransportChannelId } from "../../api_bindings.js";
import { ByteBuffer } from "../buffer.js";
import { Logger } from "../log.js";
import { StatValue } from "../stats.js";
import { allVideoCodecs, VideoCodecSupport } from "../video.js";
import { DataTransportChannel, Transport, TransportAudioSetup, TransportChannel, TransportChannelIdKey, TransportChannelIdValue, TransportShutdown, TransportVideoSetup } from "./index.js";

export class WebSocketTransport implements Transport {
    readonly implementationName: string = "web_socket"

    private logger: Logger | null = null
    private ws: WebSocket
    private buffer: ByteBuffer

    private channels: Array<WebSocketDataTransportChannel> = []
    private receivedMessages = 0
    private receivedBytes = 0
    private lastMessageAt: number | null = null
    private maxReceiveGapMs = 0
    private readonly messageListener = (event: MessageEvent) => {
        const data = event.data
        if (!(data instanceof ArrayBuffer) || data.byteLength === 0) return
        const now = performance.now()
        if (this.lastMessageAt != null) this.maxReceiveGapMs = Math.max(this.maxReceiveGapMs, now - this.lastMessageAt)
        this.lastMessageAt = now
        this.receivedMessages++
        this.receivedBytes += data.byteLength
        const channel = this.channels[new Uint8Array(data, 0, 1)[0]]
        // Dispatch once. Each consumer owns its payload, while the shared send
        // buffer is never touched by receive callbacks.
        if (channel) channel.receive(data.slice(1))
    }
    private readonly closeListener = (event: CloseEvent) => this.onWsClose(event)

    constructor(ws: WebSocket, buffer: ByteBuffer, logger: Logger | null) {
        if (logger) {
            this.logger = logger
        }

        this.ws = ws
        this.buffer = buffer

        // Very important, set the binary type to arraybuffer
        this.ws.binaryType = "arraybuffer"

        this.ws.addEventListener("close", this.closeListener)
        this.ws.addEventListener("message", this.messageListener)

        for (const keyRaw in TransportChannelId) {
            const key = keyRaw as TransportChannelIdKey
            const id = TransportChannelId[key]

            this.channels[id] = new WebSocketDataTransportChannel(this.ws, id, this.buffer)
        }
    }

    getChannel(id: TransportChannelIdValue): TransportChannel {
        return this.channels[id]
    }

    async setupHostVideo(setup: TransportVideoSetup): Promise<VideoCodecSupport> {
        if (setup.type.indexOf("data") == -1) {
            this.logger?.debug("Cannot use Web Socket Transport: Found no supported video pipeline")
            throw "Cannot use Web Socket Transport: Found no supported video pipeline"
        }

        return allVideoCodecs()
    }
    async setupHostAudio(setup: TransportAudioSetup): Promise<void> {
        if (setup.type.indexOf("data") == -1) {
            this.logger?.debug("Cannot use Web Socket Transport: Found no supported audio pipeline")
            throw "Cannot use Web Socket Transport: Found no supported audio pipeline"
        }
    }

    onclose: ((shutdown: TransportShutdown) => void) | null = null

    private onWsClose(event: CloseEvent) {
        if (this.onclose) {
            this.onclose(event.wasClean ? "disconnect" : "failed")
        }
    }
    async close(): Promise<void> {
        this.ws.removeEventListener("message", this.messageListener)
        this.ws.removeEventListener("close", this.closeListener)
        for (const channel of this.channels) channel?.close()
        // do nothing, we don't own this ws, the stream owns the ws
        // -> maybe we changed protocol
        this.logger?.debug("Web Socket transport close called, not closing Web Socket because it might still be needed")
    }
    async getStats(): Promise<Record<string, StatValue>> {
        return {
            websocketReceivedMessages: this.receivedMessages,
            websocketReceivedBytes: this.receivedBytes,
            websocketSendBufferedBytes: this.ws.bufferedAmount,
            websocketLastReceiveAgeMs: this.lastMessageAt == null ? 0 : performance.now() - this.lastMessageAt,
            websocketMaxReceiveGapMs: this.maxReceiveGapMs,
        }
    }

}

class WebSocketDataTransportChannel implements DataTransportChannel {
    readonly type: "data" = "data"

    private ws: WebSocket
    private id: TransportChannelIdValue
    private buffer: ByteBuffer

    constructor(ws: WebSocket, id: TransportChannelIdValue, buffer: ByteBuffer) {
        this.ws = ws
        this.id = id
        this.buffer = buffer

    }

    canReceive: boolean = true
    canSend: boolean = true

    private receiveListeners: Array<(data: ArrayBuffer) => void> = []
    addReceiveListener(listener: (data: ArrayBuffer) => void): void {
        this.receiveListeners.push(listener)
    }
    removeReceiveListener(listener: (data: ArrayBuffer) => void): void {
        const index = this.receiveListeners.indexOf(listener)
        if (index != -1) {
            this.receiveListeners.splice(index, 1)
        }
    }

    receive(data: ArrayBuffer) {
        if (!this.canReceive) return
        for (const listener of this.receiveListeners) {
            listener(data)
        }
    }

    send(message: ArrayBuffer): void {
        if (!this.canSend) return
        this.buffer.reset()

        this.buffer.putU8(this.id)
        this.buffer.putU8Array(new Uint8Array(message))

        this.buffer.flip()

        this.ws.send(this.buffer.getRemainingBuffer())
    }

    estimatedBufferedBytes(): number | null {
        return this.ws.bufferedAmount
    }

    close() {
        this.canReceive = false
        this.canSend = false
        this.receiveListeners = []
    }
}
