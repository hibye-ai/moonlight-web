import { Stream, InfoEvent } from "../stream/index.js"
import { getCurrentLanguage } from "../i18n.js"
import { showModal } from "../component/modal/index.js"
import { AdaptiveBitrateAdvisor, getNetworkProfile, setNetworkProfile } from "../settings_profiles.js"
import { AudioRecovery, AudioPreset, AudioOutputDevice, collectAudioOutputs, observeAudioOutput, policyFromPreset, presetFromPolicy, supportsOutputSelection, listOutputDevices, setOutputDevice } from "./audio_output.js"
import { AuthProbeResult, ConnectionController, MediaProgressMonitor } from "./connection.js"
import { DiagnosticsBus, DiagnosticsHistory, HealthEvaluator, HealthSummary, buildDiagnosticsReport, downloadDiagnosticsReport } from "./diagnostics.js"
import { defaultShortcutBindings, loadDesktopInputPrefs, saveDesktopInputPrefs, routeKeyDown, sendChord, isChordId, InputMode, ShortcutAction } from "./input_modes.js"
import { ConnectionBanner, DesktopStatusPanel, DesktopInputControls, ShortcutEditorModal } from "./panel.js"
import { LiveQualityMonitor, NetworkProfile } from "./quality.js"
import { ClipboardSessionBridge } from "./session_clipboard.js"
import { desktopStrings } from "./strings.js"

export type DesktopSessionOptions = {
    stream: Stream
    clipboard: object
    hostId: number
    adaptiveBitrateEnabled: boolean
    audioJitterTargetMs: number
    probeAuth: () => Promise<AuthProbeResult>
    win?: Window
    doc?: Document
    nav?: Pick<Navigator, "onLine"> & Partial<Pick<Navigator, "mediaDevices">>
    now?: () => number
    notify?: (message: string, type?: "info" | "error") => void
    openClipboardPanel: () => void
    openStatusPanel: () => void
    openLogin?: () => void
}

// The single owner of desktop status sampling, environment listeners and session UI.
// No content is read here: clipboard is a public action/lifecycle port only.
export class DesktopSession {
    private win: Window
    private doc: Document
    private nav: DesktopSessionOptions["nav"]
    private now: () => number
    private strings = desktopStrings(getCurrentLanguage())
    private bus: DiagnosticsBus
    private connection: ConnectionController
    private progress = new MediaProgressMonitor()
    private evaluator = new HealthEvaluator()
    private health: HealthSummary | null = null
    private history = new DiagnosticsHistory()
    private quality: LiveQualityMonitor
    private advisor: AdaptiveBitrateAdvisor | null
    private profile: NetworkProfile | null
    private lastAdvisorAt = 0
    private audioRecovery: AudioRecovery
    private preset: AudioPreset
    private audioMessage = ""
    private recovering = false
    private presetNote: "live" | "nextConnect" | null = null
    private outputDevices: AudioOutputDevice[] = []
    private outputsLoadedFor: unknown = null
    private outputSupported = false
    private applying = false
    private prefs = loadDesktopInputPrefs()
    private modeListeners: Array<(mode: InputMode) => void> = []
    private consumed = new Set<string>()
    private panel: DesktopStatusPanel
    private banner: ConnectionBanner
    private inputControls: DesktopInputControls
    private clipboard: ClipboardSessionBridge
    private interval: number
    private disposed = false
    private unsubscribe: () => void
    private infoListener = (event: InfoEvent) => {
        if (event.detail.type !== "lifecycle" || this.disposed) return
        const lifecycle = event.detail.lifecycle
        if (lifecycle.phase === "connecting" || lifecycle.phase === "connected") {
            this.progress.reset()
            this.evaluator = new HealthEvaluator()
        }
        this.connection.onLifecycle(lifecycle)
    }
    private visibilityListener = () => {
        const visible = this.doc.visibilityState === "visible"
        if (!visible) this.releaseAll("hidden")
        this.connection.onVisibilityChange(visible)
        this.render()
    }
    private onlineListener = () => this.connection.onOnline()
    private offlineListener = () => this.connection.onOffline()
    private blurListener = () => this.releaseAll("blur")

    constructor(private options: DesktopSessionOptions) {
        this.win = options.win ?? window
        this.doc = options.doc ?? document
        this.nav = options.nav ?? navigator
        this.now = options.now ?? (() => Date.now())
        this.bus = new DiagnosticsBus({ now: this.now })
        this.profile = getNetworkProfile(options.hostId)
        this.advisor = options.adaptiveBitrateEnabled ? new AdaptiveBitrateAdvisor(options.hostId, this.profile) : null
        this.quality = new LiveQualityMonitor({ ceilingKbps: options.stream.getBitrateCeiling(), currentKbps: options.stream.getSessionBitrate(), profile: this.profile, now: this.now })
        this.preset = presetFromPolicy(options.stream.getSessionAudioPreset())
        this.audioRecovery = new AudioRecovery(() => options.stream.getAudioPlayer(), this.bus)
        this.clipboard = new ClipboardSessionBridge(options.clipboard, { diagnostics: this.bus, onUnsupported: () => {
            options.openClipboardPanel()
            options.notify?.(this.strings.clipboard.unsupported, "info")
        } })
        this.connection = new ConnectionController({
            target: options.stream, probeAuth: options.probeAuth,
            isOnline: () => this.nav?.onLine !== false,
            isVisible: () => this.doc.visibilityState === "visible",
            diagnostics: this.bus, now: this.now,
            setTimeout: (fn, ms) => this.win.setTimeout(fn, ms), clearTimeout: id => this.win.clearTimeout(id),
        })
        const connectionActions = {
            reconnect: () => { void this.connection.manualReconnect() },
            cancel: () => this.connection.cancel(),
            openLogin: () => options.openLogin?.(),
        }
        this.banner = new ConnectionBanner(this.strings, connectionActions)
        this.panel = new DesktopStatusPanel(this.strings, {
            ...connectionActions,
            recoverAudio: () => { void this.recoverAudio() },
            setAudioPreset: preset => this.setPreset(preset),
            setOutputDevice: id => { void this.changeOutput(id) },
            setNetworkProfile: profile => this.setProfile(profile),
            applyRecommendation: () => {
                const recommendation = this.quality.getRecommendation()
                if (recommendation.targetKbps != null) void this.applyBitrate(recommendation.targetKbps, false)
            },
            applyManualBitrate: kbps => { void this.applyBitrate(kbps, true) },
            downloadDiagnostics: () => downloadDiagnosticsReport(this.getDiagnosticsReport(), this.doc),
            resetDiagnostics: () => { this.bus.reset(); this.render() },
        }, { preset: this.preset, customTargetMs: options.audioJitterTargetMs, profile: this.profile, adaptiveEnabled: options.adaptiveBitrateEnabled })
        this.inputControls = new DesktopInputControls(this.strings, {
            setInputMode: mode => this.setInputMode(mode),
            sendChord: id => { sendChord(options.stream.getInput(), id); this.bus.record("input.chord", { action: id }); this.render() },
            releaseAll: () => this.releaseAll("gesture"),
            editShortcuts: () => { void this.editShortcuts() },
        }, this.prefs.inputMode)
        this.unsubscribe = this.connection.subscribe(() => this.render())
        options.stream.addInfoListener(this.infoListener)
        options.stream.getStats().setCollectionEnabled(true)
        this.doc.addEventListener("visibilitychange", this.visibilityListener)
        this.win.addEventListener("online", this.onlineListener)
        this.win.addEventListener("offline", this.offlineListener)
        this.win.addEventListener("blur", this.blurListener)
        this.interval = this.win.setInterval(() => this.tick(), 1000)
        this.bus.record("connection.connecting")
        this.tick()
    }

    getStatusPanelElement() { return this.panel.element }
    getBannerElement() { return this.banner.element }
    getInputControlsElement() { return this.inputControls.element }
    getInputMode() { return this.prefs.inputMode }
    onInputModeChange(listener: (mode: InputMode) => void) { this.modeListeners.push(listener) }
    setInputMode(mode: InputMode) {
        if (this.disposed) return
        this.releaseAll("gesture")
        this.prefs.inputMode = mode
        saveDesktopInputPrefs(this.prefs)
        this.bus.record("input.mode", { mode })
        for (const listener of this.modeListeners) listener(mode)
        this.render()
    }

    // Route before ViewerApp's legacy focus/preventDefault handlers. Composition field
    // remains native; all other forms and modal dialogs are left entirely alone.
    handleKeyDown(event: KeyboardEvent, compositionTarget: unknown): "remote" | "text" | "browser" | "ignore" | "handled" {
        if (this.disposed) return "ignore"
        const route = routeKeyDown(event, { mode: this.prefs.inputMode, bindings: this.prefs.bindings, modalOpen: this.doc.documentElement.classList.contains("modal-open"), compositionTarget })
        if (route.kind !== "shortcut") return route.kind
        event.preventDefault()
        this.consumed.add(event.code)
        // Release modifiers already forwarded before the last shortcut key arrived.
        this.options.stream.getInput().raiseAllKeys()
        this.options.stream.releaseInput("shortcut")
        this.bus.record("input.shortcut", { action: route.action })
        this.runAction(route.action)
        this.render()
        return "handled"
    }
    handleKeyUp(event: KeyboardEvent, compositionTarget: unknown): "remote" | "ignore" {
        if (this.consumed.delete(event.code)) { event.preventDefault(); return "ignore" }
        const route = routeKeyDown(event, { mode: this.prefs.inputMode, bindings: this.prefs.bindings, modalOpen: this.doc.documentElement.classList.contains("modal-open"), compositionTarget })
        return route.kind === "remote" ? "remote" : "ignore"
    }
    private runAction(action: ShortcutAction) {
        if (action === "clipboardSendText" || action === "clipboardReceiveText") {
            void this.clipboard.run(action === "clipboardSendText" ? "sendText" : "receiveText")
        } else if (action === "releaseAllInput") this.releaseAll("shortcut")
        else if (action === "toggleInputMode") this.setInputMode(this.prefs.inputMode === "game" ? "text" : "game")
        else if (action === "reconnect") void this.connection.manualReconnect("shortcut")
        else if (action === "audioRecover") void this.recoverAudio()
        else if (action === "toggleDiagnostics") this.options.openStatusPanel()
        else if (isChordId(action)) sendChord(this.options.stream.getInput(), action)
    }
    private async editShortcuts() {
        this.releaseAll("gesture")
        const result = await showModal(new ShortcutEditorModal(this.strings, this.prefs.bindings, defaultShortcutBindings))
        if (!result || this.disposed) return
        this.prefs.bindings = result
        saveDesktopInputPrefs(this.prefs)
    }
    private releaseAll(trigger: string) {
        this.options.stream.releaseInput(trigger)
        this.consumed.clear()
        this.bus.record("input.releaseAll", { trigger })
    }
    private async recoverAudio() {
        if (this.recovering || this.disposed) return
        this.recovering = true
        this.render()
        const result = await this.audioRecovery.recover()
        if (this.disposed) return
        this.recovering = false
        this.audioMessage = result.result === "done" ? this.strings.audio.recoverDone : this.strings.audio.recoverFailed
        this.tick()
    }
    private setPreset(preset: AudioPreset) {
        const result = this.options.stream.setSessionAudioPreset(policyFromPreset(preset))
        this.preset = preset
        this.presetNote = result.live ? "live" : "nextConnect"
        this.bus.record("audio.preset", { preset, live: result.live })
        this.render()
    }
    private async changeOutput(id: string) {
        const result = await setOutputDevice(collectAudioOutputs(this.options.stream.getAudioPlayer()), id, this.outputDevices)
        if (this.disposed) return
        this.bus.record("audio.output", { result })
        if (result !== "done") this.audioMessage = this.strings.audio.outputFailed
        this.render()
    }
    private setProfile(profile: NetworkProfile | null) {
        this.profile = profile
        setNetworkProfile(this.options.hostId, profile)
        this.quality.setProfile(profile)
        this.advisor = this.options.adaptiveBitrateEnabled ? new AdaptiveBitrateAdvisor(this.options.hostId, profile) : null
        this.bus.record("quality.profile", { profile: profile ?? "default" })
        this.render()
    }
    private async applyBitrate(kbps: number, manual: boolean) {
        const state = this.connection.getView().state
        if (this.applying || this.disposed || (state !== "connected" && state !== "stalled")) return
        if (!Number.isFinite(kbps)) return
        const target = Math.min(this.options.stream.getBitrateCeiling(), Math.max(500, Math.round(kbps)))
        this.applying = true
        this.bus.record("quality.apply", { kbps: target, manual })
        this.render()
        try {
            if (await this.options.stream.reconnectWithBitrate(target, manual ? "manual" : "quality")) {
                this.quality.setCurrent(this.options.stream.getSessionBitrate(), { manual })
            }
        } finally { this.applying = false; if (!this.disposed) this.tick() }
    }
    tick() {
        if (this.disposed) return
        const stream = this.options.stream
        const stats = stream.getStats().getCurrentStats()
        const state = this.connection.getView().state
        const live = state === "connected" || state === "stalled"
        if (live) this.history.sample(this.now(), stream.getTransportKind(), this.doc.visibilityState === "visible", stats)
        if (live && this.doc.visibilityState === "visible") {
            const counters = stream.getMediaCounters()
            const n = (value: unknown): number | null => typeof value === "number" && Number.isFinite(value) ? value : null
            this.connection.onMediaVerdict(this.progress.sample({ at: this.now(),
                videoPackets: counters.dataVideoPackets ?? n(stats.transport.webrtcPacketsReceived),
                audioPackets: counters.dataAudioPackets ?? n(stats.transport.webrtcAudioPacketsReceived),
                framesDecoded: n(stats.transport.webrtcVideoFramesDecoded), hostSignals: counters.hostSignals,
            }))
            const before = this.quality.getRecommendation()
            const rec = this.quality.sample(stats)
            if (before.kind !== rec.kind || before.targetKbps !== rec.targetKbps) this.bus.record("quality.recommendation", { kind: rec.kind, targetKbps: rec.targetKbps })
            if (this.advisor && this.now() - this.lastAdvisorAt >= 5000) { this.lastAdvisorAt = this.now(); this.advisor.sample(stats) }
        }
        this.health = live ? this.evaluator.sample(stats, this.now(), { audioState: observeAudioOutput(stream.getAudioPlayer()) }) : null
        const player = stream.getAudioPlayer()
        const outputs = collectAudioOutputs(player)
        this.outputSupported = supportsOutputSelection(outputs)
        if (player !== this.outputsLoadedFor) {
            this.outputsLoadedFor = player
            this.outputDevices = []
            if (this.outputSupported) void listOutputDevices(this.nav?.mediaDevices, (i, id) => id === "default" ? this.strings.audio.outputDefault : this.strings.audio.outputN(i + 1)).then(devices => {
                if (this.disposed || this.outputsLoadedFor !== player) return
                this.outputDevices = devices; this.render()
            })
        }
        this.render()
    }
    private render() {
        if (this.disposed) return
        const view = this.connection.getView()
        this.clipboard.updateLifecycle({ state: view.state, visible: this.doc.visibilityState === "visible" })
        this.banner.update(view)
        this.panel.update({ connection: view, audio: { state: observeAudioOutput(this.options.stream.getAudioPlayer()), recovering: this.recovering,
            message: this.audioMessage, presetNote: this.presetNote, outputSupported: this.outputSupported, outputDevices: this.outputDevices },
            quality: { recommendation: this.quality.getRecommendation(), applying: this.applying }, health: this.health, events: this.bus.events() })
    }
    getDiagnosticsReport() {
        const view = this.connection.getView()
        const rec = this.quality.getRecommendation()
        return buildDiagnosticsReport({ generatedAt: this.now(), connection: { state: view.state, transport: this.options.stream.getTransportKind(), reconnectAttempts: view.attempt, ...this.options.stream.getTransportDiagnostics?.() },
            samples: this.history.snapshot(),
            stats: this.options.stream.getStats().getCurrentStats(), health: this.health,
            audio: { state: observeAudioOutput(this.options.stream.getAudioPlayer()), preset: this.preset, outputSelectable: this.outputSupported },
            quality: { profile: this.profile ?? "default", configuredKbps: rec.ceilingKbps, sessionKbps: rec.currentKbps, recommendation: rec.kind, targetKbps: rec.targetKbps ?? undefined },
            input: { mode: this.prefs.inputMode }, events: this.bus.events() })
    }
    markManualStop() { this.connection.markManualStop(); this.options.stream.setAutoReconnectEnabled(false); this.options.stream.cancelReconnect(); this.releaseAll("gesture") }
    dispose() {
        if (this.disposed) return
        this.markManualStop()
        this.disposed = true
        this.win.clearInterval(this.interval)
        this.doc.removeEventListener("visibilitychange", this.visibilityListener)
        this.win.removeEventListener("online", this.onlineListener)
        this.win.removeEventListener("offline", this.offlineListener)
        this.win.removeEventListener("blur", this.blurListener)
        this.options.stream.removeInfoListener(this.infoListener)
        this.unsubscribe()
        this.connection.dispose()
        this.clipboard.dispose()
        this.modeListeners = []
    }
}
