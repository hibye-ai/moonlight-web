import { SelectComponent } from "../component/input.js"
import { Modal } from "../component/modal/index.js"
import { AudioOutputDevice, AudioOutputState, AudioPreset } from "./audio_output.js"
import { ConnectionView } from "./connection.js"
import { DiagnosticEvent, HealthSummary } from "./diagnostics.js"
import { CHORD_IDS, CHORDS, InputMode, isChordId, SHORTCUT_KEY_CODES, SHORTCUT_PREFIX_LABEL, ShortcutBinding, shortcutKeyLabel, validateBinding } from "./input_modes.js"
import { NetworkProfile, QualityRecommendation } from "./quality.js"
import { DesktopStrings } from "./strings.js"

// Sidebar/overlay UI for F1-F6. Plain DOM in the existing sidebar classes; all behaviour is
// delegated to DesktopSession through the action callbacks.

function el<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
    const element = document.createElement(tag)
    if (className) element.className = className
    if (text != null) element.textContent = text
    return element
}

function button(label: string, action: string, className = "btn btn-ghost desktop-button"): HTMLButtonElement {
    const element = el("button", className, label)
    element.type = "button"
    element.dataset.desktop = action
    element.setAttribute("data-desktop", action)
    return element
}

function sectionLabel(text: string): HTMLDivElement {
    return el("div", "sidebar-stream-section-label", text)
}

function setText(element: HTMLElement, text: string) {
    if (element.textContent !== text) {
        element.textContent = text
    }
}

export function connectionText(strings: DesktopStrings, view: ConnectionView): { main: string, detail: string } {
    let main: string = strings.connection.states[view.state]
    if (view.state === "reconnecting" && view.attempt > 0 && view.maxAttempts > 0) {
        main = strings.connection.attempt(view.attempt, view.maxAttempts)
    }
    if (view.state === "stalled" && view.reason === "decoderStalled") {
        main = strings.connection.states.decoderStalled
    }
    let detail = ""
    if (view.state === "failed" && view.reason) {
        detail = strings.connection.reasons[view.reason] ?? ""
    } else if (view.state === "connected" && view.verdict === "videoIdle") {
        detail = strings.connection.videoIdle
    }
    return { main, detail }
}

// -- Connection banner (overlay on the stream; visible only when something needs attention)

export type ConnectionActions = {
    reconnect(): void
    cancel(): void
    openLogin(): void
}

export class ConnectionBanner {
    readonly element = el("div", "stream-connection-banner")
    private text = el("p", "stream-connection-banner-text")
    private detail = el("p", "stream-connection-banner-detail")
    private reconnectButton: HTMLButtonElement
    private cancelButton: HTMLButtonElement
    private loginButton: HTMLButtonElement

    constructor(private strings: DesktopStrings, actions: ConnectionActions) {
        this.element.setAttribute("role", "status")
        this.element.setAttribute("aria-live", "polite")
        this.element.hidden = true
        this.reconnectButton = button(strings.connection.reconnect, "reconnect", "btn btn-primary desktop-button")
        this.cancelButton = button(strings.connection.cancel, "cancel")
        this.loginButton = button(strings.connection.login, "login")
        this.reconnectButton.addEventListener("click", () => actions.reconnect())
        this.cancelButton.addEventListener("click", () => actions.cancel())
        this.loginButton.addEventListener("click", () => actions.openLogin())
        const buttons = el("div", "stream-connection-banner-actions")
        buttons.append(this.cancelButton, this.loginButton, this.reconnectButton)
        this.element.append(this.text, this.detail, buttons)
        // The banner sits above the input layer; keep its events away from the stream.
        for (const type of ["keydown", "keyup", "mousedown", "mouseup", "click", "wheel", "touchstart", "touchend", "contextmenu"]) {
            this.element.addEventListener(type, event => event.stopPropagation())
        }
    }

    update(view: ConnectionView) {
        const visible = view.state !== "connected" && view.state !== "stopped"
        this.element.hidden = !visible
        this.element.dataset.state = view.state
        const { main, detail } = connectionText(this.strings, view)
        setText(this.text, main)
        setText(this.detail, detail)
        this.detail.hidden = detail.length === 0
        this.cancelButton.hidden = !view.canCancel
        this.loginButton.hidden = view.state !== "authRequired"
        this.reconnectButton.hidden = !view.canReconnect
    }
}

// -- Status tab

export type StatusPanelActions = ConnectionActions & {
    recoverAudio(): void
    setAudioPreset(preset: AudioPreset): void
    setOutputDevice(id: string): void
    setNetworkProfile(profile: NetworkProfile | null): void
    applyRecommendation(): void
    applyManualBitrate(kbps: number): void
    downloadDiagnostics(): void
    resetDiagnostics(): void
}

export type StatusPanelInit = {
    preset: AudioPreset
    customTargetMs: number
    profile: NetworkProfile | null
    adaptiveEnabled: boolean
}

export type StatusViewModel = {
    connection: ConnectionView
    audio: {
        state: AudioOutputState
        recovering: boolean
        message: string
        presetNote: "live" | "nextConnect" | null
        outputSupported: boolean
        outputDevices: ReadonlyArray<AudioOutputDevice>
    }
    quality: {
        recommendation: QualityRecommendation
        applying: boolean
    }
    health: HealthSummary | null
    events: ReadonlyArray<DiagnosticEvent>
}

const TIMELINE_VISIBLE = 8

export class DesktopStatusPanel {
    readonly element = el("div", "sidebar-stream-control-panel desktop-status-panel")

    private connectionState = el("p", "desktop-status-main")
    private connectionDetail = el("p", "desktop-muted")
    private reconnectButton: HTMLButtonElement
    private cancelButton: HTMLButtonElement
    private loginButton: HTMLButtonElement

    private audioState = el("p", "desktop-status-main")
    private audioMessage = el("p", "desktop-muted")
    private audioRecover: HTMLButtonElement
    private presetNote = el("p", "desktop-muted")
    private outputContainer = el("div", "desktop-output")
    private outputSelect: SelectComponent | null = null
    private outputKey = ""

    private qualityCurrent = el("p", "desktop-status-main")
    private qualityRecommendation = el("p", "desktop-muted")
    private qualityApply: HTMLButtonElement
    private manualInput = el("input")

    private healthRows = new Map<string, { level: HTMLSpanElement, reasons: HTMLSpanElement }>()
    private timeline = el("ul", "desktop-timeline")
    private timelineKey = ""

    constructor(private strings: DesktopStrings, private actions: StatusPanelActions, init: StatusPanelInit) {
        this.element.id = "stream-status-panel"
        this.element.setAttribute("role", "tabpanel")
        this.element.setAttribute("aria-labelledby", "stream-status-tab")

        // Connection
        const connection = el("section", "desktop-section")
        this.connectionState.setAttribute("data-desktop", "connection-state")
        this.connectionState.setAttribute("role", "status")
        this.connectionState.setAttribute("aria-live", "polite")
        this.connectionDetail.setAttribute("data-desktop", "connection-detail")
        this.reconnectButton = button(strings.connection.reconnect, "reconnect", "btn btn-primary desktop-button")
        this.cancelButton = button(strings.connection.cancel, "cancel")
        this.loginButton = button(strings.connection.login, "login")
        this.reconnectButton.addEventListener("click", () => actions.reconnect())
        this.cancelButton.addEventListener("click", () => actions.cancel())
        this.loginButton.addEventListener("click", () => actions.openLogin())
        const connectionActions = el("div", "desktop-actions")
        connectionActions.append(this.cancelButton, this.loginButton, this.reconnectButton)
        connection.append(sectionLabel(strings.connection.title), this.connectionState, this.connectionDetail, connectionActions)

        // Audio
        const audio = el("section", "desktop-section")
        this.audioState.setAttribute("data-desktop", "audio-state")
        this.audioState.setAttribute("aria-live", "polite")
        this.audioMessage.setAttribute("data-desktop", "audio-message")
        this.audioRecover = button(strings.audio.recover, "audio-recover", "btn btn-primary desktop-button")
        this.audioRecover.addEventListener("click", () => actions.recoverAudio())
        const preset = new SelectComponent("desktopAudioPreset", [
            { value: "game", name: strings.audio.presets.game },
            { value: "desktop", name: strings.audio.presets.desktop },
            { value: "custom", name: strings.audio.presets.custom(init.customTargetMs) },
        ], { displayName: strings.audio.preset, preSelectedOption: init.preset })
        preset.addChangeListener(() => {
            const value = preset.getValue()
            if (value === "game" || value === "desktop" || value === "custom") {
                actions.setAudioPreset(value)
            }
        })
        this.presetNote.setAttribute("data-desktop", "audio-preset-note")
        const presetHelp = el("p", "desktop-muted", strings.audio.presetNote)
        const audioModes = el("div", "sidebar-stream-modes")
        preset.mount(audioModes)
        audioModes.appendChild(this.outputContainer)
        audio.append(sectionLabel(strings.audio.title), this.audioState, this.audioMessage, this.audioRecover, audioModes, this.presetNote, presetHelp)

        // Quality
        const quality = el("section", "desktop-section")
        const qualityModes = el("div", "sidebar-stream-modes")
        const profile = new SelectComponent("desktopNetworkProfile", [
            { value: "default", name: strings.quality.profiles.default },
            { value: "lan", name: strings.quality.profiles.lan },
            { value: "remote", name: strings.quality.profiles.remote },
        ], { displayName: strings.quality.profile, preSelectedOption: init.profile ?? "default" })
        profile.addChangeListener(() => {
            const value = profile.getValue()
            actions.setNetworkProfile(value === "lan" || value === "remote" ? value : null)
        })
        profile.mount(qualityModes)
        const profileHint = el("p", "desktop-muted", strings.quality.profileHint)
        const adaptive = el("p", "desktop-muted", init.adaptiveEnabled ? strings.quality.adaptiveOn : strings.quality.adaptiveOff)
        this.qualityCurrent.setAttribute("data-desktop", "quality-current")
        this.qualityRecommendation.setAttribute("data-desktop", "quality-recommendation")
        this.qualityRecommendation.setAttribute("aria-live", "polite")
        this.qualityApply = button(strings.quality.apply, "quality-apply", "btn btn-primary desktop-button")
        this.qualityApply.hidden = true
        this.qualityApply.addEventListener("click", () => actions.applyRecommendation())

        const manualRow = el("div", "input-div input-number desktop-manual")
        const manualLabel = el("label", undefined, strings.quality.manual)
        manualLabel.htmlFor = "desktopManualBitrate"
        this.manualInput.id = "desktopManualBitrate"
        this.manualInput.type = "number"
        this.manualInput.min = "500"
        this.manualInput.step = "500"
        this.manualInput.inputMode = "numeric"
        const manualApply = button(strings.quality.manualApply, "quality-manual-apply")
        manualApply.addEventListener("click", () => {
            const kbps = Number.parseInt(this.manualInput.value, 10)
            if (Number.isFinite(kbps) && kbps > 0) {
                actions.applyManualBitrate(kbps)
            }
        })
        manualRow.append(manualLabel, this.manualInput)
        quality.append(sectionLabel(strings.quality.title), qualityModes, profileHint, adaptive, this.qualityCurrent, this.qualityRecommendation, this.qualityApply, manualRow, manualApply)

        // Diagnostics
        const diagnostics = el("section", "desktop-section")
        const health = el("div", "desktop-health")
        for (const domain of ["network", "decoder", "audio", "host"]) {
            const row = el("div", "desktop-health-row")
            row.setAttribute("data-desktop", `health-${domain}`)
            const name = el("span", "desktop-health-name", strings.diagnostics.domains[domain])
            const level = el("span", "desktop-health-level", strings.diagnostics.levels.unknown)
            level.dataset.level = "unknown"
            const reasons = el("span", "desktop-health-reasons")
            row.append(name, level, reasons)
            health.appendChild(row)
            this.healthRows.set(domain, { level, reasons })
        }
        const hint = el("p", "desktop-muted", strings.diagnostics.hint)
        const timelineLabel = el("div", "desktop-subtitle", strings.diagnostics.timeline)
        const download = button(strings.diagnostics.download, "diagnostics-download")
        download.addEventListener("click", () => actions.downloadDiagnostics())
        const reset = button(strings.diagnostics.reset, "diagnostics-reset")
        reset.addEventListener("click", () => actions.resetDiagnostics())
        const diagnosticActions = el("div", "desktop-actions")
        diagnosticActions.append(reset, download)
        const privacy = el("p", "desktop-muted", strings.diagnostics.privacy)
        diagnostics.append(sectionLabel(strings.diagnostics.title), health, hint, timelineLabel, this.timeline, diagnosticActions, privacy)

        this.element.append(connection, audio, quality, diagnostics)
    }

    update(vm: StatusViewModel) {
        const strings = this.strings

        const { main, detail } = connectionText(strings, vm.connection)
        setText(this.connectionState, main)
        setText(this.connectionDetail, detail)
        this.connectionDetail.hidden = detail.length === 0
        this.cancelButton.hidden = !vm.connection.canCancel
        this.loginButton.hidden = vm.connection.state !== "authRequired"
        this.reconnectButton.disabled = !vm.connection.canReconnect

        setText(this.audioState, strings.audio.states[vm.audio.state] ?? strings.audio.states.unknown)
        setText(this.audioMessage, vm.audio.message)
        this.audioMessage.hidden = vm.audio.message.length === 0
        this.audioRecover.disabled = vm.audio.recovering
        setText(this.audioRecover, vm.audio.recovering ? strings.audio.recovering : strings.audio.recover)
        setText(this.presetNote, vm.audio.presetNote === "live" ? strings.audio.presetLive : vm.audio.presetNote === "nextConnect" ? strings.audio.presetNextConnect : "")
        this.presetNote.hidden = vm.audio.presetNote == null
        this.updateOutputDevices(vm.audio.outputSupported, vm.audio.outputDevices)

        const rec = vm.quality.recommendation
        setText(this.qualityCurrent, strings.quality.current(rec.currentKbps, rec.ceilingKbps) + (rec.manual ? ` · ${strings.quality.manualActive}` : ""))
        let recText: string = strings.quality.recommendation.none
        if (vm.quality.applying) recText = strings.quality.reconnecting
        else if (rec.kind === "lower" && rec.targetKbps != null) recText = strings.quality.recommendation.lower(rec.targetKbps)
        else if (rec.kind === "raise" && rec.targetKbps != null) recText = strings.quality.recommendation.raise(rec.targetKbps)
        else if (rec.kind === "insufficient") recText = strings.quality.recommendation.insufficient
        setText(this.qualityRecommendation, recText)
        this.qualityApply.hidden = vm.quality.applying || !((rec.kind === "lower" || rec.kind === "raise") && rec.targetKbps != null)
        this.manualInput.max = String(rec.ceilingKbps)
        if (!this.manualInput.value) {
            this.manualInput.placeholder = String(rec.currentKbps)
        }

        for (const [domain, row] of this.healthRows) {
            const value = vm.health ? (vm.health as Record<string, { level: string, reasons: string[] }>)[domain] : null
            const level = value?.level ?? "unknown"
            row.level.dataset.level = level
            setText(row.level, strings.diagnostics.levels[level] ?? strings.diagnostics.levels.unknown)
            setText(row.reasons, (value?.reasons ?? []).map(reason => strings.diagnostics.reasons[reason] ?? "").filter(Boolean).join(", "))
        }

        this.updateTimeline(vm.events)
    }

    private updateOutputDevices(supported: boolean, devices: ReadonlyArray<AudioOutputDevice>) {
        const key = supported ? devices.map(device => `${device.id}\u0000${device.label}`).join("\u0001") : ""
        if (key === this.outputKey) {
            return
        }
        this.outputKey = key
        this.outputContainer.replaceChildren()
        this.outputSelect = null
        if (!supported || devices.length === 0) {
            return
        }
        const select = new SelectComponent("desktopAudioOutput", devices.map(device => ({ value: device.id, name: device.label })), {
            displayName: this.strings.audio.output,
            preSelectedOption: devices[0].id,
        })
        select.addChangeListener(() => {
            const value = select.getValue()
            if (value) this.actions.setOutputDevice(value)
        })
        select.mount(this.outputContainer)
        this.outputSelect = select
    }

    private updateTimeline(events: ReadonlyArray<DiagnosticEvent>) {
        const visible = events.slice(-TIMELINE_VISIBLE).reverse()
        const key = visible.map(event => `${event.t}:${event.code}`).join("|")
        if (key === this.timelineKey) {
            return
        }
        this.timelineKey = key
        this.timeline.replaceChildren()
        if (visible.length === 0) {
            this.timeline.appendChild(el("li", "desktop-muted", this.strings.diagnostics.empty))
            return
        }
        for (const event of visible) {
            const item = el("li", "desktop-timeline-item")
            item.setAttribute("data-desktop", "timeline-item")
            const time = el("span", "desktop-timeline-time", formatTime(event.t))
            const details = Object.entries(event.detail).map(([key, value]) => `${key}=${value}`).join(" ")
            const code = el("span", "desktop-timeline-code", details ? `${event.code} ${details}` : event.code)
            item.append(time, code)
            this.timeline.appendChild(item)
        }
    }
}

function formatTime(t: number): string {
    const date = new Date(t)
    const pad = (n: number) => String(n).padStart(2, "0")
    return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}

// -- Input controls (inside the existing controls tab)

export type InputControlActions = {
    setInputMode(mode: InputMode): void
    sendChord(id: string): void
    releaseAll(): void
    editShortcuts(): void
}

export class DesktopInputControls {
    readonly element = el("div", "sidebar-stream-modes desktop-input-controls")
    private mode: SelectComponent

    constructor(strings: DesktopStrings, actions: InputControlActions, initialMode: InputMode) {
        this.mode = new SelectComponent("desktopKeyboardMode", [
            { value: "game", name: strings.input.modes.game },
            { value: "text", name: strings.input.modes.text },
        ], { displayName: strings.input.keyboardMode, preSelectedOption: initialMode })
        this.mode.addChangeListener(() => {
            const value = this.mode.getValue()
            if (value === "game" || value === "text") actions.setInputMode(value)
        })
        this.mode.mount(this.element)

        const chordLabel = el("div", "sidebar-stream-section-label", strings.input.chords)
        const chords = el("div", "desktop-chords")
        for (const id of CHORD_IDS) {
            const chord = el("button", "btn btn-ghost desktop-chord", CHORDS[id].label)
            chord.type = "button"
            chord.setAttribute("data-chord", id)
            chord.addEventListener("click", () => actions.sendChord(id))
            chords.appendChild(chord)
        }
        const release = button(strings.input.releaseAll, "release-all")
        release.addEventListener("click", () => actions.releaseAll())
        const shortcuts = button(strings.input.shortcuts, "shortcuts")
        shortcuts.addEventListener("click", () => actions.editShortcuts())
        const row = el("div", "desktop-actions")
        row.append(release, shortcuts)
        this.element.append(chordLabel, chords, row)
    }
}

// -- Shortcut editor (modal)

export class ShortcutEditor {
    readonly element = el("div", "desktop-shortcut-editor")
    private selects = new Map<string, HTMLSelectElement>()
    private error = el("p", "desktop-error")

    constructor(private strings: DesktopStrings, bindings: ReadonlyArray<ShortcutBinding>) {
        const hint = el("p", "desktop-muted", strings.shortcuts.hint)
        this.error.setAttribute("role", "alert")
        this.element.append(hint)
        for (const binding of bindings) {
            const row = el("div", "input-div input-select desktop-shortcut-row")
            const label = el("label", undefined, this.actionLabel(binding.action))
            const id = `desktopShortcut-${binding.action}`
            label.htmlFor = id
            const select = el("select")
            select.id = id
            const none = el("option", undefined, strings.shortcuts.none)
            none.value = ""
            select.appendChild(none)
            for (const code of SHORTCUT_KEY_CODES) {
                const option = el("option", undefined, `${SHORTCUT_PREFIX_LABEL}+${shortcutKeyLabel(code)}`)
                option.value = code
                if (code === binding.code) option.selected = true
                select.appendChild(option)
            }
            select.value = binding.code ?? ""
            row.append(label, select)
            this.element.appendChild(row)
            this.selects.set(binding.action, select)
        }
        this.element.appendChild(this.error)
    }

    private actionLabel(action: string): string {
        if (isChordId(action)) return this.strings.shortcuts.chordAction(CHORDS[action].label)
        return this.strings.shortcuts.actions[action] ?? action
    }

    setBindings(bindings: ReadonlyArray<ShortcutBinding>) {
        for (const binding of bindings) {
            const select = this.selects.get(binding.action)
            if (select) select.value = binding.code ?? ""
        }
        this.error.textContent = ""
    }

    /// Current selection, or null (with a visible error) when two actions share a key.
    read(): ShortcutBinding[] | null {
        const bindings: ShortcutBinding[] = []
        for (const [action, select] of this.selects) {
            bindings.push({ action: action as ShortcutBinding["action"], code: select.value ? select.value : null })
        }
        for (const binding of bindings) {
            if (validateBinding(binding, bindings) != null) {
                this.error.textContent = this.strings.shortcuts.conflict
                return null
            }
        }
        this.error.textContent = ""
        return bindings
    }
}

export class ShortcutEditorModal implements Modal<ShortcutBinding[] | null> {
    private root = el("div", "modal-message desktop-shortcut-modal")
    private editor: ShortcutEditor
    private saveButton: HTMLButtonElement
    private cancelButton: HTMLButtonElement
    private resetButton: HTMLButtonElement

    constructor(strings: DesktopStrings, bindings: ReadonlyArray<ShortcutBinding>, defaults: () => ShortcutBinding[]) {
        const title = el("h3", undefined, strings.shortcuts.title)
        this.editor = new ShortcutEditor(strings, bindings)
        this.saveButton = el("button", "btn btn-primary", strings.shortcuts.save)
        this.cancelButton = el("button", "btn btn-ghost", strings.shortcuts.cancel)
        this.resetButton = el("button", "btn btn-ghost", strings.shortcuts.reset)
        for (const b of [this.saveButton, this.cancelButton, this.resetButton]) b.type = "button"
        this.resetButton.addEventListener("click", () => this.editor.setBindings(defaults()))
        const actions = el("div", "modal-actions")
        actions.append(this.resetButton, this.cancelButton, this.saveButton)
        this.root.append(title, this.editor.element, actions)
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.root)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.root)
    }

    onFinish(abort: AbortSignal): Promise<ShortcutBinding[] | null> {
        return new Promise(resolve => {
            this.saveButton.addEventListener("click", () => {
                const result = this.editor.read()
                if (result) resolve(result)
            }, { signal: abort })
            this.cancelButton.addEventListener("click", () => resolve(null), { signal: abort })
            abort.addEventListener("abort", () => resolve(null), { once: true })
        })
    }
}
