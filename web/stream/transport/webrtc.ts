import { StreamSignalingMessage, TransportChannelId } from "../../api_bindings.js";
import { Logger } from "../log.js";
import { StatValue } from "../stats.js";
import { CAPABILITIES_CODECS, emptyVideoCodecs, maybeVideoCodecs, VideoCodecSupport } from "../video.js";
import { DataTransportChannel, Transport, TransportAudioSetup, TransportChannel, TransportChannelIdKey, TransportChannelIdValue, TransportVideoSetup, AudioTrackTransportChannel, VideoTrackTransportChannel, TrackTransportChannel, TransportShutdown } from "./index.js";
import { RtcStatsCollector, statReportIdentity } from "./rtc_stats.js";
import { AudioJitterPolicy, DEFAULT_AUDIO_JITTER_POLICY, ResolvedReceiverPolicy, resolveAudioReceiverPolicy, applyReceiverPolicy, applyReceiverPolicyLive } from "./audio_policy.js";

export class WebRTCTransport implements Transport {
    implementationName: string = "webrtc"

    private logger: Logger | null

    private peer: RTCPeerConnection | null = null

    constructor(logger?: Logger) {
        this.logger = logger ?? null
    }

    async initPeer(configuration?: RTCConfiguration) {
        this.logger?.debug(`Creating Client Peer`)

        if (this.peer) {
            this.logger?.debug(`Cannot create Peer because a Peer already exists`)
            return
        }

        // Configure web rtc
        // TODO: use this for signaling instead and extend the protocol so that the client also requests a control channel with name: "control", protocol:"moonlight-control-v1": https://www.ietf.org/archive/id/draft-ietf-wish-whep-02.html
        this.peer = new RTCPeerConnection(configuration)
        this.peer.addEventListener("error", this.onError.bind(this))

        this.peer.addEventListener("icecandidate", this.onIceCandidate.bind(this))

        this.peer.addEventListener("connectionstatechange", this.onConnectionStateChange.bind(this))
        this.peer.addEventListener("signalingstatechange", this.onSignalingStateChange.bind(this))
        this.peer.addEventListener("iceconnectionstatechange", this.onIceConnectionStateChange.bind(this))
        this.peer.addEventListener("icegatheringstatechange", this.onIceGatheringStateChange.bind(this))

        this.peer.addEventListener("track", this.onTrack.bind(this))
        this.peer.addEventListener("datachannel", this.onDataChannel.bind(this))

        this.initChannels()

        // Maybe we already received data
        if (this.remoteDescription) {
            await this.handleRemoteDescription(this.remoteDescription)
        }
        await this.tryDequeueIceCandidates()
    }

    private onError(event: Event) {
        this.logger?.debug(`Web Socket or WebRtcPeer Error`)

        console.error(`Web Socket or WebRtcPeer Error`, event)
    }

    onsendmessage: ((message: StreamSignalingMessage) => void) | null = null
    private sendMessage(message: StreamSignalingMessage) {
        if (this.onsendmessage) {
            this.onsendmessage(message)
        } else {
            this.logger?.debug("Failed to call onicecandidate because no handler is set")
        }
    }
    async onReceiveMessage(message: StreamSignalingMessage) {
        if ("Description" in message) {
            const description = message.Description;
            await this.handleRemoteDescription({
                type: description.ty as RTCSdpType,
                sdp: description.sdp
            })
        } else if ("AddIceCandidate" in message) {
            const candidate = message.AddIceCandidate
            await this.addIceCandidate({
                candidate: candidate.candidate,
                sdpMid: candidate.sdp_mid,
                sdpMLineIndex: candidate.sdp_mline_index,
                usernameFragment: candidate.username_fragment
            })
        }
    }

    private remoteDescription: RTCSessionDescriptionInit | null = null
    private async handleRemoteDescription(sdp: RTCSessionDescriptionInit | null) {
        this.logger?.debug(`Received remote description: ${sdp?.type}`)

        const remoteDescription = sdp
        this.remoteDescription = remoteDescription
        if (!this.peer) {
            return
        }
        this.remoteDescription = null

        if (remoteDescription) {
            await this.peer.setRemoteDescription(remoteDescription)

            if (remoteDescription.type == "offer") {
                await this.peer.setLocalDescription()
                const localDescription = this.peer.localDescription
                if (!localDescription) {
                    this.logger?.debug("Peer didn't have a localDescription whilst receiving an offer and trying to answer")
                    return
                }

                this.logger?.debug(`Responding to offer description: ${localDescription.type}`)
                this.sendMessage({
                    Description: {
                        ty: localDescription.type,
                        sdp: localDescription.sdp ?? ""
                    }
                })
            }
        }
    }

    private onIceCandidate(event: RTCPeerConnectionIceEvent) {
        if (event.candidate) {
            const candidate = event.candidate.toJSON()
            this.logger?.debug(`Sending ice candidate: ${candidate.candidate}`)

            this.sendMessage({
                AddIceCandidate: {
                    candidate: candidate.candidate ?? "",
                    sdp_mid: candidate.sdpMid ?? null,
                    sdp_mline_index: candidate.sdpMLineIndex ?? null,
                    username_fragment: candidate.usernameFragment ?? null
                }
            })
        } else {
            this.logger?.debug("No new ice candidates")
        }
    }

    private iceCandidates: Array<RTCIceCandidateInit> = []
    private async addIceCandidate(candidate: RTCIceCandidateInit) {
        this.logger?.debug(`Received ice candidate: ${candidate.candidate}`)

        if (!this.peer) {
            this.logger?.debug("Buffering ice candidate")

            this.iceCandidates.push(candidate)
            return
        }
        await this.tryDequeueIceCandidates()

        await this.peer.addIceCandidate(candidate)
    }
    private async tryDequeueIceCandidates() {
        if (!this.peer) {
            this.logger?.debug("called tryDequeueIceCandidates without a peer")
            return
        }

        for (const candidate of this.iceCandidates) {
            await this.peer.addIceCandidate(candidate)
        }
        this.iceCandidates.length = 0
    }

    private wasConnected = false
    private onConnectionStateChange() {
        if (!this.peer) {
            this.logger?.debug("OnConnectionStateChange without a peer")
            return
        }

        let type: null | "fatal" | "recover" = null

        if (this.peer.connectionState == "connected") {
            type = "recover"

            if (this.onconnect) {
                this.onconnect()
            }
            this.wasConnected = true
        } else if ((this.peer.connectionState == "failed" || this.peer.connectionState == "closed") && this.peer.iceGatheringState == "complete") {
            type = "fatal"
        }

        if (this.peer.connectionState == "failed" || this.peer.connectionState == "closed") {
            if (this.onclose) {
                if (this.wasConnected) {
                    this.onclose("failed")
                } else {
                    this.onclose("failednoconnect")
                }
            }
        }

        this.logger?.debug(`Changing Peer State to ${this.peer.connectionState}`, {
            type: type ?? undefined
        })
    }
    private onSignalingStateChange() {
        if (!this.peer) {
            this.logger?.debug("OnSignalingStateChange without a peer")
            return
        }
        this.logger?.debug(`Changing Peer Signaling State to ${this.peer.signalingState}`)
    }
    private onIceConnectionStateChange() {
        if (!this.peer) {
            this.logger?.debug("OnIceConnectionStateChange without a peer")
            return
        }
        this.logger?.debug(`Changing Peer Ice State to ${this.peer.iceConnectionState}`)
    }
    private onIceGatheringStateChange() {
        if (!this.peer) {
            this.logger?.debug("OnIceGatheringStateChange without a peer")
            return
        }
        this.logger?.debug(`Changing Peer Ice Gathering State to ${this.peer.iceGatheringState}`)

        if (this.peer.iceConnectionState == "new" && this.peer.iceGatheringState == "complete") {
            // we failed without connection
            if (this.onclose) {
                this.onclose("failednoconnect")
            }
        }
    }

    private channels: Array<TransportChannel | null> = []
    private initChannels() {
        if (!this.peer) {
            this.logger?.debug("Failed to initialize channel without peer")
            return
        }
        if (this.channels.length > 0) {
            this.logger?.debug("Already initialized channels")
            return
        }

        for (const channelRaw in TransportChannelId) {
            const channel = channelRaw as TransportChannelIdKey

            if (channel == "HOST_VIDEO") {
                const channel: VideoTrackTransportChannel = new WebRTCInboundTrackTransportChannel<"videotrack">(this.logger, "videotrack", "video", this.videoTrackHolder)
                this.channels[TransportChannelId.HOST_VIDEO] = channel
                continue
            }
            if (channel == "HOST_AUDIO") {
                const channel: AudioTrackTransportChannel = new WebRTCInboundTrackTransportChannel<"audiotrack">(this.logger, "audiotrack", "audio", this.audioTrackHolder)
                this.channels[TransportChannelId.HOST_AUDIO] = channel
                continue
            }

            // All Data Channels are created by the server
            const id = TransportChannelId[channel]
            this.channels[id] = new WebRTCDataTransportChannel(channel, null, undefined, () => this.failInputChannel())
        }
    }

    private videoTrackHolder: TrackHolder = { ontrack: null, track: null }
    private videoReceiver: RTCRtpReceiver | null = null

    private audioTrackHolder: TrackHolder = { ontrack: null, track: null }
    private audioReceiver: RTCRtpReceiver | null = null

    // New/missing settings let the browser absorb jitter; explicit policies remain.
    audioJitterPolicy: AudioJitterPolicy = DEFAULT_AUDIO_JITTER_POLICY
    private resolvedAudioPolicy: ResolvedReceiverPolicy | null = null

    /// Name of the audio receiver policy actually applied, for the stats display.
    getAudioJitterPolicyName(): string {
        return (this.resolvedAudioPolicy ?? resolveAudioReceiverPolicy(this.audioJitterPolicy)).name
    }

    /// Session preset switch on the live audio receiver. Only the audio receiver is touched;
    /// the transport, video and the stream keep running. Returns whether it applied live.
    setAudioJitterPolicyLive(policy: AudioJitterPolicy): boolean {
        this.audioJitterPolicy = policy
        if (!this.audioReceiver) {
            return false
        }
        const resolved = resolveAudioReceiverPolicy(policy)
        const applied = applyReceiverPolicyLive(this.audioReceiver, resolved)
        if (applied) {
            this.resolvedAudioPolicy = resolved
            this.logger?.debug(`Audio receiver jitter policy changed live: ${resolved.name}`)
        }
        return applied
    }

    private onTrack(event: RTCTrackEvent) {
        const track = event.track

        const receiver = event.receiver
        if (track.kind == "video") {
            this.videoReceiver = receiver

            // Video stays on the low-latency settings; the audit recommends holding this
            // fixed while the audio policy is compared.
            receiver.jitterBufferTarget = 0
            if ("playoutDelayHint" in receiver) {
                receiver.playoutDelayHint = 0
            }
        } else if (track.kind == "audio") {
            this.audioReceiver = receiver

            // Audio may use a different policy (opt-in). The default is still 0/0.
            this.resolvedAudioPolicy = resolveAudioReceiverPolicy(this.audioJitterPolicy)
            applyReceiverPolicy(receiver, this.resolvedAudioPolicy)
            this.logger?.debug(`Audio receiver jitter policy: ${this.resolvedAudioPolicy.name}`)
        }

        this.logger?.debug(`Adding receiver: ${track.kind}, ${track.id}, ${track.label}`)

        if (track.kind == "video") {
            if ("contentHint" in track) {
                track.contentHint = "motion"
            }

            this.videoTrackHolder.track = track
            if (!this.videoTrackHolder.ontrack) {
                throw "No video track listener registered!"
            }
            this.videoTrackHolder.ontrack()
        } else if (track.kind == "audio") {
            this.audioTrackHolder.track = track
            if (!this.audioTrackHolder.ontrack) {
                throw "No audio track listener registered!"
            }
            this.audioTrackHolder.ontrack()
        }
    }

    // Handle data channels created by the remote peer (server)
    private onDataChannel(event: RTCDataChannelEvent) {
        const remoteChannel = event.channel
        const label = remoteChannel.label

        this.logger?.debug(`Received remote data channel: ${label}`)

        // Map the channel label to the corresponding TransportChannelId
        const channelKey = label.toUpperCase() as TransportChannelIdKey
        if (channelKey in TransportChannelId) {
            const id = TransportChannelId[channelKey]
            const existingChannel = this.channels[id]

            // If we already have a channel for this ID, replace its underlying RTCDataChannel
            // with the remote one so we can receive messages from the server
            if (existingChannel && existingChannel.type === "data") {
                this.logger?.debug(`Replacing underlying channel for ${label} with remote channel`);
                (existingChannel as WebRTCDataTransportChannel).replaceChannel(remoteChannel)
            } else {
                this.logger?.debug(`Creating new channel for ${label}`)
                this.channels[id] = new WebRTCDataTransportChannel(label, remoteChannel, undefined, () => this.failInputChannel())
            }
        } else {
            this.logger?.debug(`Unknown remote data channel: ${label}`)
        }
    }

    async setupHostVideo(_setup: TransportVideoSetup): Promise<VideoCodecSupport> {
        // TODO: check transport type

        let capabilities
        if ("getCapabilities" in RTCRtpReceiver && (capabilities = RTCRtpReceiver.getCapabilities("video"))) {
            const codecs = emptyVideoCodecs()

            for (const codec in codecs) {
                const supportRequirements = CAPABILITIES_CODECS[codec]

                if (!supportRequirements) {
                    continue
                }

                let supported = false
                capabilityCodecLoop: for (const codecCapability of capabilities.codecs) {
                    if (codecCapability.mimeType != supportRequirements.mimeType) {
                        continue
                    }

                    for (const fmtpLine of supportRequirements.fmtpLine) {
                        if (!codecCapability.sdpFmtpLine?.includes(fmtpLine)) {
                            continue capabilityCodecLoop
                        }
                    }

                    supported = true
                    break
                }

                codecs[codec] = supported
            }

            return codecs
        } else {
            return maybeVideoCodecs()
        }
    }

    async setupHostAudio(_setup: TransportAudioSetup): Promise<void> {
        // TODO: check transport type
    }

    getChannel(id: TransportChannelIdValue): TransportChannel {
        const channel = this.channels[id]
        if (!channel) {
            this.logger?.debug("Failed to setup video without peer")
            throw `Failed to get channel because it is not yet initialized, Id: ${id}`
        }

        return channel
    }

    onconnect: (() => void) | null = null

    onclose: ((shutdown: TransportShutdown) => void) | null = null
    private failInputChannel() {
        this.logger?.debug("Input channel stalled; closing the session to release input state")
        void this.close()
        this.onclose?.("failed")
    }
    async close(): Promise<void> {
        this.logger?.debug("Closing WebRTC Peer")
        for (const channel of this.channels) {
            if (channel instanceof WebRTCDataTransportChannel) channel.dispose()
        }
        this.peer?.close()
    }

    // Per-stream inbound-rtp history: audio/video/SSRC are reported separately and
    // cumulative counters are turned into interval means (see rtc_stats.ts).
    private inbound = new RtcStatsCollector()

    async getStats(): Promise<Record<string, StatValue>> {
        const statsData: Record<string, StatValue> = {}
        if (this.peer) {
            if (typeof this.peer.iceConnectionState === "string") statsData.webrtcIceState = this.peer.iceConnectionState
            if (typeof this.peer.connectionState === "string") statsData.webrtcConnectionState = this.peer.connectionState
        }

        // Always label the audio receiver policy so an A/B capture is self-describing even
        // when the connection never produced an inbound-rtp report.
        statsData.webrtcAudioJitterPolicy = this.getAudioJitterPolicyName()
        const dataChannels = this.channels.filter((c): c is WebRTCDataTransportChannel => c instanceof WebRTCDataTransportChannel)
        statsData.webrtcInputQueuedMessages = dataChannels.reduce((n, c) => n + c.queuedMessages, 0)
        statsData.webrtcInputBufferedBytes = dataChannels.reduce((n, c) => n + (c.estimatedBufferedBytes() ?? 0), 0)
        statsData.webrtcInputCoalesced = dataChannels.reduce((n, c) => n + c.coalesced, 0)
        statsData.webrtcInputFailures = dataChannels.reduce((n, c) => n + c.failures, 0)

        // AR-12: audio-only sessions have no video receiver. Bailing out here would hide
        // every audio SSRC from the stats display, so either receiver is enough to sample.
        if (!this.videoReceiver && !this.audioReceiver) {
            return statsData
        }

        // AR-12: `RTCPeerConnection.getStats()` and `RTCRtpReceiver.getStats()` are NOT
        // interchangeable. The peer connection may return an empty or partial map (early in
        // the session, before the receiver has been attached), and some browsers put the
        // inbound-rtp counters only on the receiver. So all available sources are queried
        // and merged instead of letting a present-but-empty peer map suppress the receivers
        // (`peerReports ?? fallback` did exactly that). Order matters: the peer connection is
        // the authoritative source, so its copy of a report wins over a receiver's copy.
        const merged = await this.collectStatReports([
            { name: "peer", source: this.peer },
            { name: "audioReceiver", source: this.audioReceiver },
            { name: "videoReceiver", source: this.videoReceiver },
        ])

        // A single collector pass over the merged snapshot: per-kind aggregation and the
        // per-stream history must see one coherent view, not one pass per source.
        if (merged.queriedAny) {
            Object.assign(statsData, this.inbound.collect(merged.entries))
        }

        const selectedPairId = merged.entries.find(([, value]) => value.type === "transport" && value.selectedCandidatePairId)?.[1].selectedCandidatePairId
        const selectedPair = merged.entries.find(([, value]) => value.type === "candidate-pair" && (selectedPairId ? value.id === selectedPairId : value.state === "succeeded" && (value.nominated || value.selected)))?.[1]
        for (const [_key, value] of merged.entries) {
            // inbound-rtp is handled per stream above; a second pass would re-introduce the
            // audio/video overwrite this collector exists to prevent.
            if ((value as any).type == "inbound-rtp") {
                continue
            }

            if (value === selectedPair) {
                for (const [id, prefix] of [[value.localCandidateId, "webrtcLocal"], [value.remoteCandidateId, "webrtcRemote"]]) {
                    const candidate = merged.entries.find(([, item]) => item.id === id)?.[1]
                    if (!candidate) continue
                    if (["host", "srflx", "prflx", "relay"].indexOf(candidate.candidateType) !== -1) statsData[`${prefix}CandidateType`] = candidate.candidateType
                    if (["udp", "tcp"].indexOf(candidate.protocol) !== -1) statsData[`${prefix}Protocol`] = candidate.protocol
                    if (["udp", "tcp", "tls"].indexOf(candidate.relayProtocol) !== -1) statsData[`${prefix}RelayProtocol`] = candidate.relayProtocol
                }
                if (value.currentRoundTripTime != null) {
                    statsData.webrtcRttMs = value.currentRoundTripTime * 1000
                }
                if (value.availableIncomingBitrate != null) {
                    statsData.webrtcAvailableIncomingKbps = value.availableIncomingBitrate / 1000
                }
            }
        }

        return statsData
    }

    /// Merge the report maps of `peer`, `audioReceiver` and `videoReceiver` into one snapshot.
    ///
    /// * A source that is absent, or that has no `getStats()` (a partially initialised
    ///   transport), is skipped — not treated as an empty report.
    /// * A source whose `getStats()` throws is logged and skipped; the remaining sources are
    ///   still used. The error is never silently discarded, and one broken source cannot hide
    ///   the others. If EVERY source fails the caller keeps the previous history instead of
    ///   recording a fake "no streams" sample.
    /// * Reports are de-duplicated by report identity (`type|id`, else `type|kind|ssrc`) so a
    ///   report present in both the peer connection and a receiver is counted once, with the
    ///   authoritative peer value kept.
    private async collectStatReports(sources: Array<{ name: string, source: { getStats(): Promise<RTCStatsReport> } | null }>): Promise<{ entries: Array<[string, any]>, queriedAny: boolean }> {
        const entries: Array<[string, any]> = []
        const seen = new Set<string>()
        let queriedAny = false

        for (const [index, { name, source }] of sources.entries()) {
            if (source == null || typeof source.getStats != "function") {
                continue
            }

            let reports: RTCStatsReport | null = null
            try {
                reports = await source.getStats()
            } catch (error) {
                this.logger?.debug(`Failed to read ${name} stats: ${String(error)}`)
                continue
            }
            if (reports == null || typeof reports.entries != "function") {
                this.logger?.debug(`Ignoring ${name} stats: getStats() returned no report map`)
                continue
            }

            queriedAny = true
            for (const [key, report] of reports.entries()) {
                const identity = statReportIdentity(key, report, index)
                if (seen.has(identity)) {
                    // Peer wins: it was merged first, so the authoritative copy is kept.
                    continue
                }
                seen.add(identity)
                entries.push([identity, report])
            }
        }

        return { entries, queriedAny }
    }

    /// Forget inbound-rtp history so the next sample starts a new interval (new peer).
    resetStatsHistory() {
        this.inbound.reset()
    }
}

type TrackHolder = {
    ontrack: (() => void) | null
    track: MediaStreamTrack | null
}

// This receives track data
class WebRTCInboundTrackTransportChannel<T extends string> implements TrackTransportChannel {
    type: T

    canReceive: boolean = true
    canSend: boolean = false

    private logger: Logger | null

    private label: string
    private trackHolder: TrackHolder

    constructor(logger: Logger | null, type: T, label: string, trackHolder: TrackHolder) {
        this.logger = logger

        this.type = type
        this.label = label
        this.trackHolder = trackHolder

        this.trackHolder.ontrack = this.onTrack.bind(this)
    }
    setTrack(_track: MediaStreamTrack | null): void {
        throw "WebRTCInboundTrackTransportChannel cannot addTrack"
    }

    private onTrack() {
        const track = this.trackHolder.track
        if (!track) {
            this.logger?.debug("WebRTC TrackHolder.track is null!")
            return
        }

        for (const listener of this.trackListeners) {
            listener(track)
        }
    }


    private trackListeners: Array<(track: MediaStreamTrack) => void> = []
    addTrackListener(listener: (track: MediaStreamTrack) => void): void {
        if (this.trackHolder.track) {
            listener(this.trackHolder.track)
        }
        this.trackListeners.push(listener)
    }
    removeTrackListener(listener: (track: MediaStreamTrack) => void): void {
        const index = this.trackListeners.indexOf(listener)
        if (index != -1) {
            this.trackListeners.splice(index, 1)
        }
    }
}

export class WebRTCDataTransportChannel implements DataTransportChannel {
    type: "data" = "data"

    canReceive: boolean = true
    canSend: boolean = true

    private logger: Logger | null = null

    private label: string
    private channel: RTCDataChannel | null
    private boundTryDequeueSendQueue: () => void
    private boundOnMessage: (event: MessageEvent) => void

    private disposed = false
    private queueBytes = 0
    private queueSince = 0
    private queueTimer: ReturnType<typeof setTimeout> | null = null
    private onFailure: (() => void) | undefined
    coalesced = 0
    failures = 0
    get queuedMessages() { return this.sendQueue.length }

    constructor(label: string, channel: RTCDataChannel | null, logger?: Logger, onFailure?: () => void) {
        this.label = label
        this.channel = channel
        this.boundOnMessage = this.onMessage.bind(this)
        this.boundTryDequeueSendQueue = this.tryDequeueSendQueue.bind(this)

        this.logger = logger ?? null
        this.onFailure = onFailure
        this.channel = null
        if (channel) this.replaceChannel(channel)
    }

    // Replace the underlying channel with a new one (e.g., from remote peer)
    // This is used when we receive a data channel from the server that should
    // replace our locally created one for receiving messages
    replaceChannel(newChannel: RTCDataChannel): void {
        // Remove listener from old channel
        this.channel?.removeEventListener("open", this.boundTryDequeueSendQueue)
        this.channel?.removeEventListener("message", this.boundOnMessage)
        this.channel?.removeEventListener("bufferedamountlow", this.boundTryDequeueSendQueue)
        if (this.disposed) { newChannel.close(); return }
        // Add listener to new channel
        this.channel = newChannel
        this.channel.addEventListener("open", this.boundTryDequeueSendQueue)
        this.channel.addEventListener("message", this.boundOnMessage)
        this.channel.bufferedAmountLowThreshold = 64 * 1024
        this.channel.addEventListener("bufferedamountlow", this.boundTryDequeueSendQueue)
        this.tryDequeueSendQueue()
    }

    private sendQueue: Array<ArrayBuffer> = []
    send(message: ArrayBuffer): void {
        if (this.disposed) return
        if (this.channel?.readyState === "closed" || this.channel?.readyState === "closing") { this.fail(); return }
        if (this.channel?.readyState === "open" && !this.sendQueue.length && this.channel.bufferedAmount < 128 * 1024) {
            try { this.channel.send(message); return } catch { this.fail(); return }
        }
        // Only absolute coordinates supersede earlier messages. Relative movement,
        // key/button releases and all other reliable messages retain their order.
        if (this.label.toUpperCase() === "MOUSE_ABSOLUTE" && this.sendQueue.length) {
            this.coalesced += this.sendQueue.length
            this.sendQueue = []
            this.queueBytes = 0
        }
        if (this.sendQueue.length >= 256 || this.queueBytes + message.byteLength > 512 * 1024) { this.fail(); return }
        if (!this.sendQueue.length) this.queueSince = performance.now()
        this.sendQueue.push(message.slice(0))
        this.queueBytes += message.byteLength
        this.armQueueTimer()
    }
    private armQueueTimer() {
        if (this.queueTimer != null || this.disposed || !this.sendQueue.length) return
        this.queueTimer = setTimeout(() => {
            this.queueTimer = null
            const limit = this.channel?.readyState === "open" ? 500 : 10000
            if (performance.now() - this.queueSince >= limit) {
                if (this.label.toUpperCase() === "MOUSE_ABSOLUTE") this.clearQueue()
                else this.fail()
            } else this.tryDequeueSendQueue()
        }, 100)
    }
    private clearQueue() {
        this.sendQueue = []
        this.queueBytes = 0
        if (this.queueTimer != null) clearTimeout(this.queueTimer)
        this.queueTimer = null
    }
    private fail() {
        if (this.disposed) return
        this.failures++
        this.dispose()
        this.onFailure?.()
    }
    dispose() {
        this.disposed = true
        this.clearQueue()
        this.channel?.removeEventListener("open", this.boundTryDequeueSendQueue)
        this.channel?.removeEventListener("message", this.boundOnMessage)
        this.channel?.removeEventListener("bufferedamountlow", this.boundTryDequeueSendQueue)
        this.receiveListeners = []
    }
    private tryDequeueSendQueue() {
        if (this.disposed) return
        if (!this.channel || this.channel.readyState != "open") {
            this.armQueueTimer()
            return
        }
        while (this.sendQueue.length && this.channel.bufferedAmount < 128 * 1024) {
            const message = this.sendQueue[0]
            try { this.channel.send(message) } catch { this.fail(); return }
            this.sendQueue.shift()
            this.queueBytes -= message.byteLength
        }
        if (!this.sendQueue.length) this.clearQueue()
        else this.armQueueTimer()
    }

    private onMessage(event: MessageEvent) {
        const data = event.data
        if (!(data instanceof ArrayBuffer)) {
            console.warn(`received text data on webrtc channel ${this.label}`)
            return
        }

        for (const listener of this.receiveListeners) {
            listener(event.data)
        }
    }
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
    estimatedBufferedBytes(): number | null {
        return this.queueBytes + (this.channel?.bufferedAmount ?? 0)
    }
}
