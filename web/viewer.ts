import "./polyfill/index.js"
import { Api, apiGetHost, FetchError } from "./api.js";
import { Component } from "./component/index.js";
import { showNotification } from "./component/notification.js";
import { InfoEvent, Stream } from "./stream/index.js"
import { Modal, showMessage, showModal } from "./component/modal/index.js";
import { setSidebar, setSidebarExtended, setSidebarStyle, Sidebar } from "./component/sidebar/index.js";
import { defaultStreamInputConfig, MouseMode, ScreenKeyboardSetVisibleEvent, StreamInputConfig } from "./stream/input.js";
import { Settings } from "./component/settings_menu.js";
import { applyAdaptiveBitrateRecommendation, getScopedStreamSettings, getNetworkProfile } from "./settings_profiles.js";
import { SelectComponent } from "./component/input.js";
import { DetailedRole, StreamCapabilities, StreamKeys, StreamPermissions } from "./api_bindings.js";
import { KeyboardModeEvent, KeyboardModeWillChangeEvent, ScreenKeyboard, TextEvent } from "./screen_keyboard.js";
import { FormModal } from "./component/modal/form.js";
import { streamStatsToText } from "./stream/stats.js";
import { getCurrentLanguage, getTranslations } from "./i18n.js";
import { requestKeyboardLock } from "./iframe.js";
import { icon, setButtonContent } from "./component/icons.js";
import { StreamClipboard } from "./component/clipboard.js";

import { DesktopSession } from "./desktop/session.js"
import { desktopStrings } from "./desktop/strings.js"
import { isEditableTarget } from "./desktop/input_modes.js"

let I = getTranslations(getCurrentLanguage())

export class ViewerApp implements Component {
    private api: Api

    private sidebar: ViewerSidebar

    private div = document.createElement("div")

    private statsDiv = document.createElement("div")
    private localTouchCursorDiv = document.createElement("div")
    private stream: Stream

    private inputConfig: StreamInputConfig = defaultStreamInputConfig()
    private previousMouseMode: MouseMode
    private autoEnterFullscreenOnStart: boolean = false
    private pendingAutoFullscreenPrompt: boolean = false
    private fullscreenPromptShown: boolean = false
    private fullscreenOnNextInteractionArmed: boolean = false
    private pendingAutoFullscreenTouchGesture: boolean = false
    private pendingAutoFullscreenMouseGesture: boolean = false
    private manualFullscreenExitRequested: boolean = false
    private toggleFullscreenWithKeybind: boolean = false
    private hasShownFullscreenEscapeWarning = false
    private keyboardViewportBaselineHeight: number | null = null
    private streamVideoTopOffsetPx: number = 0
    private desktop: DesktopSession
    private statsInterval: number
    private touchFrame: number | null = null
    private gamepadFrame: number | null = null
    private disposed = false
    private listenerCleanup: Array<() => void> = []
    private infoListener = this.onInfo.bind(this)
    private keyboardVisibleListener = this.onScreenKeyboardSetVisible.bind(this)
    private listen(target: EventTarget, type: string, fn: EventListener, options?: AddEventListenerOptions) {
        target.addEventListener(type, fn, options)
        this.listenerCleanup.push(() => target.removeEventListener(type, fn, options))
    }
    getDesktopSession() { return this.desktop }
    markManualStop() { this.desktop.markManualStop() }

    constructor(api: Api, hostId: number, appId: number, bootstrapRole: DetailedRole) {
        this.api = api
        I = getTranslations(getCurrentLanguage())

        const configuredSettings = getScopedStreamSettings(bootstrapRole.default_settings, { hostId, appId })
        const settings = applyAdaptiveBitrateRecommendation(configuredSettings, hostId, getNetworkProfile(hostId))
        Object.assign(this.inputConfig, {
            mouseMode: settings.mouseMode,
            mouseScrollMode: settings.mouseScrollMode,
            touchMode: settings.touchMode,
            localCursorSensitivity: settings.localCursorSensitivity,
            controllerConfig: settings.controllerConfig
        })

        // Configure sidebar
        this.sidebar = new ViewerSidebar(this, api, hostId)
        setSidebar(this.sidebar)

        // Configure stats element
        this.statsDiv.hidden = true
        this.statsDiv.classList.add("video-stats")
        this.localTouchCursorDiv.hidden = true
        this.localTouchCursorDiv.classList.add("local-touch-cursor")

        this.statsInterval = window.setInterval(() => {
            // Update stats display every 100ms
            const stats = this.getStream()?.getStats()
            if (stats && stats.isEnabled()) {
                this.statsDiv.hidden = false

                const text = streamStatsToText(stats.getCurrentStats())
                this.statsDiv.innerText = text
            } else {
                this.statsDiv.hidden = true
            }
        }, 100)
        this.div.appendChild(this.statsDiv)
        this.div.appendChild(this.localTouchCursorDiv)

        // Configure stream
        this.previousMouseMode = this.inputConfig.mouseMode

        const browserWidth = Math.max(document.documentElement.clientWidth || 0, window.innerWidth || 0)
        const browserHeight = Math.max(document.documentElement.clientHeight || 0, window.innerHeight || 0)

        this.autoEnterFullscreenOnStart = settings.enterFullscreenOnStreamStart
        this.toggleFullscreenWithKeybind = settings.toggleFullscreenWithKeybind

        this.stream = new Stream(this.api, hostId, appId, settings, [browserWidth, browserHeight], bootstrapRole.permissions, { bitrateCeilingKbps: configuredSettings.bitrate })
        this.desktop = new DesktopSession({
            stream: this.stream, clipboard: this.sidebar.getClipboard(), hostId,
            adaptiveBitrateEnabled: settings.adaptiveBitrate, audioJitterTargetMs: settings.audioJitterTargetMs,
            // This host-scoped server request checks session validity AND ownership. It
            // never converts an unreachable server into a successful authorization.
            probeAuth: async () => {
                try { await apiGetHost(this.api, { host_id: hostId }); return "ok" }
                catch (error) {
                    const status = error instanceof FetchError ? error.getResponse()?.status : null
                    return status === 401 || status === 403 ? "denied" : "unreachable"
                }
            },
            notify: showNotification,
            openClipboardPanel: () => this.sidebar.openDesktopTab("clipboard"),
            openStatusPanel: () => this.sidebar.openDesktopTab("status"),
            openLogin: () => window.location.reload(),
        })
        this.div.appendChild(this.desktop.getBannerElement())
        this.sidebar.attachDesktop(this.desktop)
        this.desktop.onInputModeChange(mode => this.sidebar.setDocumentMode(mode === "text"))
        this.sidebar.setDocumentMode(this.desktop.getInputMode() === "text")
        this.startStream(hostId, appId, bootstrapRole.permissions, settings, [browserWidth, browserHeight])

        // Configure input
        this.addListeners(document)
        this.addListeners(document.getElementById("input") as HTMLDivElement)
        const composition = this.sidebar.getScreenKeyboard().getHiddenElement()
        this.listen(composition, "keydown", this.onKeyDown.bind(this) as EventListener)
        this.listen(composition, "keyup", this.onKeyUp.bind(this) as EventListener)

        this.listen(document, "pointerlockchange", this.onPointerLockChange.bind(this))
        this.listen(document, "fullscreenchange", this.onFullscreenChange.bind(this))
        this.listen(window, "gamepadconnected", this.onGamepadConnect.bind(this) as EventListener)
        this.listen(window, "gamepaddisconnected", this.onGamepadDisconnect.bind(this) as EventListener)
        this.listen(window, "pagehide", () => this.dispose())
        // Connect all gamepads
        for (const gamepad of navigator.getGamepads()) {
            if (gamepad != null) {
                this.onGamepadAdd(gamepad)
            }
        }
    }
    private addListeners(element: HTMLElement | Document) {
        this.listen(element, "keydown", this.onKeyDown.bind(this) as EventListener, { passive: false })
        this.listen(element, "keyup", this.onKeyUp.bind(this) as EventListener, { passive: false })
        this.listen(element, "paste", this.onPaste.bind(this) as EventListener)

        this.listen(element, "mousedown", this.onMouseButtonDown.bind(this) as EventListener, { passive: false })
        this.listen(element, "mouseup", this.onMouseButtonUp.bind(this) as EventListener, { passive: false })
        this.listen(element, "mousemove", this.onMouseMove.bind(this) as EventListener, { passive: false })
        this.listen(element, "wheel", this.onMouseWheel.bind(this) as EventListener, { passive: false })
        this.listen(element, "contextmenu", this.onContextMenu.bind(this) as EventListener, { passive: false })

        this.listen(element, "touchstart", this.onTouchStart.bind(this) as EventListener, { passive: false })
        this.listen(element, "touchend", this.onTouchEnd.bind(this) as EventListener, { passive: false })
        this.listen(element, "touchcancel", this.onTouchCancel.bind(this) as EventListener, { passive: false })
        this.listen(element, "touchmove", this.onTouchMove.bind(this) as EventListener, { passive: false })
    }

    private async startStream(hostId: number, appId: number, permissions: StreamPermissions, settings: Settings, browserSize: [number, number]) {
        setSidebarStyle({
            edge: settings.sidebarEdge,
        })

        // Add app info listener
        this.stream.addInfoListener(this.infoListener)

        // Start animation frame loop
        this.onTouchUpdate()
        this.onGamepadUpdate()

        this.stream.getInput().addScreenKeyboardVisibleEvent(this.keyboardVisibleListener)

        this.stream.mount(this.div)

        if (this.autoEnterFullscreenOnStart) {
            this.pendingAutoFullscreenPrompt = true
            this.armFullscreenOnNextInteraction()
        }
    }

    private async onInfo(event: InfoEvent) {
        const data = event.detail

        if (data.type == "app") {
            const app = data.app

            document.title = `Stream: ${app.title}`
        } else if (data.type == "connectionComplete") {
            this.sidebar.onCapabilitiesChange(data.capabilities)
        }
    }

    private focusInput() {
        if (this.desktop?.getInputMode() === "text") {
            this.sidebar.setDocumentMode(true)
            return
        }
        if (this.stream.getInput().getCurrentPredictedTouchAction() != "screenKeyboard" && !this.sidebar.getScreenKeyboard().isVisible()) {
            const inputElement = document.getElementById("input") as HTMLDivElement
            inputElement.focus()
        }
    }

    onUserInteraction() {
        this.focusInput()

        this.stream.getVideoRenderer()?.onUserInteraction()
        this.stream.getAudioPlayer()?.onUserInteraction()
    }
    private armFullscreenOnNextInteraction() {
        if (this.autoEnterFullscreenOnStart) {
            this.fullscreenOnNextInteractionArmed = true
        }
    }
    private consumeAutoFullscreenInteraction(): boolean {
        if (!this.fullscreenOnNextInteractionArmed || this.isFullscreen()) {
            return false
        }

        this.fullscreenOnNextInteractionArmed = false
        void this.requestFullscreen().then(() => {
            if (!this.isFullscreen()) {
                this.armFullscreenOnNextInteraction()
            }
        })
        return true
    }
    private beginAutoFullscreenTouchGesture(): boolean {
        if (!this.fullscreenOnNextInteractionArmed || this.isFullscreen()) {
            return false
        }

        this.pendingAutoFullscreenTouchGesture = true
        return true
    }
    private consumeAutoFullscreenTouchGesture(): boolean {
        if (!this.pendingAutoFullscreenTouchGesture) {
            return false
        }

        this.pendingAutoFullscreenTouchGesture = false
        return this.consumeAutoFullscreenInteraction()
    }
    private onScreenKeyboardSetVisible(event: ScreenKeyboardSetVisibleEvent) {
        console.info(event.detail)
        const screenKeyboard = this.sidebar.getScreenKeyboard()

        const newShown = event.detail.visible
        if (newShown != screenKeyboard.isVisible()) {
            if (newShown) {
                screenKeyboard.show()
            } else {
                screenKeyboard.hide()
            }
        }
    }

    // Input
    getInputConfig(): StreamInputConfig {
        return this.inputConfig
    }
    setInputConfig(config: StreamInputConfig) {
        Object.assign(this.inputConfig, config)

        this.stream.getInput().setConfig(this.inputConfig)
        this.renderLocalTouchCursor()
    }

    // Keyboard
    onKeyDown(event: KeyboardEvent) {
        const field = this.sidebar.getScreenKeyboard().getHiddenElement()
        const route = this.desktop.handleKeyDown(event, field)
        if (route === "ignore" || route === "browser") return
        if (route === "handled") { event.stopPropagation(); return }
        if (route === "text") {
            // Native input/composition events in the existing ScreenKeyboard own text.
            // setDocumentMode keeps this field focused before a printable key arrives.
            if (this.desktop.getInputMode() === "text") this.sidebar.setDocumentMode(true)
            event.stopPropagation()
            return
        }
        // Editing keys in the composition field are generated by its input handler.
        if (event.target === field && ["Backspace", "Delete", "Enter"].indexOf(event.code) !== -1) return
        this.onUserInteraction()
        event.preventDefault()
        this.stream.getInput().onKeyDown(event)
        event.stopPropagation()
    }

    private isTogglingFullscreenWithKeybind: "waitForCtrl" | "makingFullscreen" | "none" = "none"
    onKeyUp(event: KeyboardEvent) {
        const field = this.sidebar.getScreenKeyboard().getHiddenElement()
        if (this.desktop.handleKeyUp(event, field) !== "remote") return
        if (event.target === field && ["Backspace", "Delete", "Enter"].indexOf(event.code) !== -1) return
        this.onUserInteraction()

        event.preventDefault()
        this.stream.getInput().onKeyUp(event)
        event.stopPropagation()

        if (this.toggleFullscreenWithKeybind && this.isTogglingFullscreenWithKeybind == "none" && event.ctrlKey && event.shiftKey && event.code == "KeyI") {
            this.isTogglingFullscreenWithKeybind = "waitForCtrl"
        }
        if (this.isTogglingFullscreenWithKeybind == "waitForCtrl" && (event.code == "ControlRight" || event.code == "ControlLeft")) {
            this.isTogglingFullscreenWithKeybind = "makingFullscreen";

            (async () => {
                if (this.isFullscreen()) {
                    await this.exitPointerLock()
                    await this.exitFullscreen()
                } else {
                    await this.requestFullscreen()
                    await this.requestPointerLock()
                }

                this.isTogglingFullscreenWithKeybind = "none"
            })()
        }
    }

    onPaste(event: ClipboardEvent) {
        if (document.documentElement.classList.contains("modal-open")) return
        if (isEditableTarget(event.target)) return
        this.onUserInteraction()

        this.stream.getInput().onPaste(event)

        event.stopPropagation()
    }

    // Mouse
    onMouseButtonDown(event: MouseEvent) {
        if (this.consumeAutoFullscreenInteraction()) {
            this.pendingAutoFullscreenMouseGesture = true
            event.preventDefault()
            event.stopPropagation()
            return
        }

        this.onUserInteraction()

        event.preventDefault()
        this.stream.getInput().onMouseDown(event, this.getStreamRect());

        event.stopPropagation()
    }
    onMouseButtonUp(event: MouseEvent) {
        if (this.pendingAutoFullscreenMouseGesture) {
            this.pendingAutoFullscreenMouseGesture = false
            event.preventDefault()
            event.stopPropagation()
            return
        }

        this.onUserInteraction()

        event.preventDefault()
        this.stream.getInput().onMouseUp(event)

        event.stopPropagation()
    }
    onMouseMove(event: MouseEvent) {
        if (this.pendingAutoFullscreenMouseGesture) {
            event.preventDefault()
            event.stopPropagation()
            return
        }

        event.preventDefault()
        this.stream.getInput().onMouseMove(event, this.getStreamRect())

        event.stopPropagation()
    }
    onMouseWheel(event: WheelEvent) {
        event.preventDefault()
        this.stream.getInput().onMouseWheel(event)

        event.stopPropagation()
    }
    onContextMenu(event: MouseEvent) {
        event.preventDefault()

        event.stopPropagation()
    }

    // Touch
    onTouchStart(event: TouchEvent) {
        if (this.beginAutoFullscreenTouchGesture()) {
            event.preventDefault()
            event.stopPropagation()
            return
        }

        this.onUserInteraction()

        event.preventDefault()
        this.stream.getInput().onTouchStart(event, this.getStreamRect())

        event.stopPropagation()
    }
    onTouchEnd(event: TouchEvent) {
        if (this.consumeAutoFullscreenTouchGesture()) {
            event.preventDefault()
            event.stopPropagation()
            return
        }

        this.onUserInteraction()

        event.preventDefault()
        this.stream.getInput().onTouchEnd(event, this.getStreamRect())

        event.stopPropagation()
    }
    onTouchCancel(event: TouchEvent) {
        if (this.pendingAutoFullscreenTouchGesture) {
            this.pendingAutoFullscreenTouchGesture = false
            event.preventDefault()
            event.stopPropagation()
            return
        }

        this.pendingAutoFullscreenTouchGesture = false

        this.onUserInteraction()

        event?.preventDefault()
        this.stream.getInput().onTouchCancel(event, this.getStreamRect())

        event.stopPropagation()
    }
    onTouchUpdate() {
        if (this.disposed) return
        this.stream.getInput().onTouchUpdate(this.getStreamRect())
        this.updateKeyboardViewportVideoOffset()
        this.renderLocalTouchCursor()

        this.touchFrame = window.requestAnimationFrame(this.onTouchUpdate.bind(this))
    }
    onTouchMove(event: TouchEvent) {
        if (this.pendingAutoFullscreenTouchGesture) {
            event.preventDefault()
            event.stopPropagation()
            return
        }

        event.preventDefault()
        this.stream.getInput().onTouchMove(event, this.getStreamRect())

        event.stopPropagation()
    }

    // Gamepad
    onGamepadConnect(event: GamepadEvent) {
        this.onGamepadAdd(event.gamepad)
    }
    onGamepadAdd(gamepad: Gamepad) {
        this.stream.getInput().onGamepadConnect(gamepad)
    }
    onGamepadDisconnect(event: GamepadEvent) {
        this.stream.getInput().onGamepadDisconnect(event)
    }
    onGamepadUpdate() {
        if (this.disposed) return
        this.stream.getInput().onGamepadUpdate()

        this.gamepadFrame = window.requestAnimationFrame(this.onGamepadUpdate.bind(this))
    }

    // Fullscreen
    private async promptAutoFullscreen() {
        await showModal(new AutoFullscreenModal(this.requestFullscreen.bind(this)))
    }
    async requestFullscreen(showEscapeWarning: boolean = true) {
        const body = document.body
        if (body) {
            if (!("requestFullscreen" in body && typeof body.requestFullscreen == "function")) {
                await showMessage(I.stream.fullscreenUnsupported)

                return
            }

            this.focusInput()

            if (!this.isFullscreen()) {
                try {
                    await body.requestFullscreen({
                        navigationUI: "hide"
                    })
                } catch (e) {
                    console.warn("failed to request fullscreen", e)
                }
            }

            try {
                await requestKeyboardLock();
                if (showEscapeWarning && !this.hasShownFullscreenEscapeWarning) {
                    showNotification(I.stream.fullscreenEscapeHint, "info")
                    this.hasShownFullscreenEscapeWarning = true
                }
            } catch (e) {
                console.warn("Keyboard lock failed, skipping notification.", e);
            }

            if (this.getStream()?.getInput().getConfig().mouseMode == "relative") {
                await this.requestPointerLock()
            }

            try {
                if (screen && "orientation" in screen) {
                    const orientation = screen.orientation

                    if ("lock" in orientation && typeof orientation.lock == "function") {
                        await orientation.lock("landscape")
                    }
                }
            } catch (e) {
                console.warn("failed to set orientation to landscape", e)
            }
        } else {
            console.warn("root element not found")
        }
    }
    async exitFullscreen() {
        if ("keyboard" in navigator && navigator.keyboard && "unlock" in navigator.keyboard) {
            await navigator.keyboard.unlock()
        }

        if ("exitFullscreen" in document && typeof document.exitFullscreen == "function") {
            await document.exitFullscreen()
        }
    }
    isFullscreen(): boolean {
        return "fullscreenElement" in document && !!document.fullscreenElement
    }
    private async onFullscreenChange() {
        if (this.isFullscreen()) {
            this.fullscreenOnNextInteractionArmed = false
            this.pendingAutoFullscreenTouchGesture = false
            this.pendingAutoFullscreenMouseGesture = false
            this.manualFullscreenExitRequested = false
        } else {
            const manualExit = this.manualFullscreenExitRequested
            this.manualFullscreenExitRequested = false

            if (this.autoEnterFullscreenOnStart && !manualExit) {
                this.armFullscreenOnNextInteraction()
            }
        }

        this.checkFullyImmersed()
    }
    markManualFullscreenExitRequested() {
        this.manualFullscreenExitRequested = true
    }

    // Pointer Lock
    async requestPointerLock(errorIfNotFound: boolean = false) {
        this.previousMouseMode = this.inputConfig.mouseMode

        const inputElement = document.getElementById("input") as HTMLDivElement

        if (inputElement && "requestPointerLock" in inputElement && typeof inputElement.requestPointerLock == "function") {
            this.focusInput()

            this.inputConfig.mouseMode = "relative"
            this.setInputConfig(this.inputConfig)

            setSidebarExtended(false)

            const onLockError = () => {
                document.removeEventListener("pointerlockerror", onLockError)

                // Fallback: try to request pointer lock without options
                inputElement.requestPointerLock()
            }

            document.addEventListener("pointerlockerror", onLockError, { once: true })

            try {
                let promise = inputElement.requestPointerLock({
                    unadjustedMovement: true
                })

                if (promise) {
                    await promise
                } else {
                    inputElement.requestPointerLock()
                }
            } catch (error) {
                // Some platforms do not support unadjusted movement. If you
                // would like PointerLock anyway, request again.
                if (error instanceof Error && error.name == "NotSupportedError") {
                    inputElement.requestPointerLock()
                } else {
                    throw error
                }
            } finally {
                document.removeEventListener("pointerlockerror", onLockError)
            }

        } else if (errorIfNotFound) {
            await showMessage(I.stream.pointerLockUnsupported)
        }
    }
    async exitPointerLock() {
        if ("exitPointerLock" in document && typeof document.exitPointerLock == "function") {
            document.exitPointerLock()
        }
    }
    private onPointerLockChange() {
        this.checkFullyImmersed()

        if (!document.pointerLockElement) {
            this.inputConfig.mouseMode = this.previousMouseMode
            this.setInputConfig(this.inputConfig)
        }
    }

    // -- Fully immersed Fullscreen -> Fullscreen API + Pointer Lock
    private checkFullyImmersed() {
        if ("pointerLockElement" in document && document.pointerLockElement &&
            "fullscreenElement" in document && document.fullscreenElement) {
            // We're fully immersed -> remove sidebar
            setSidebar(null)
        } else {
            setSidebar(this.sidebar)
        }
    }
    private renderLocalTouchCursor() {
        const localCursorState = this.stream.getInput().getLocalCursorState()
        if (!localCursorState?.visible) {
            this.localTouchCursorDiv.hidden = true
            return
        }

        const rect = this.getStreamRect()
        if (rect.width <= 0 || rect.height <= 0) {
            this.localTouchCursorDiv.hidden = true
            return
        }

        this.localTouchCursorDiv.hidden = false
        this.localTouchCursorDiv.style.left = `${rect.left + localCursorState.x * rect.width}px`
        this.localTouchCursorDiv.style.top = `${rect.top + localCursorState.y * rect.height}px`
    }

    onScreenKeyboardModeWillChange(event: KeyboardModeWillChangeEvent) {
        if (event.detail.enabled) {
            this.captureKeyboardViewportBaseline()
        }
    }

    private captureKeyboardViewportBaseline() {
        this.keyboardViewportBaselineHeight = window.visualViewport?.height ?? null
        this.streamVideoTopOffsetPx = 0
        this.applyStreamVideoTopOffset()
        this.updateKeyboardFloatingButtonPosition()
    }
    resetKeyboardViewportVideoOffset() {
        this.keyboardViewportBaselineHeight = null
        this.streamVideoTopOffsetPx = 0
        this.applyStreamVideoTopOffset()
        this.resetKeyboardFloatingButtonPosition()
    }
    private updateKeyboardViewportVideoOffset() {
        this.updateKeyboardFloatingButtonPosition()

        const screenKeyboard = this.sidebar.getScreenKeyboard()
        const visualViewport = window.visualViewport
        const baselineHeight = this.keyboardViewportBaselineHeight
        const localCursorState = this.stream.getInput().getLocalCursorState()

        if (!screenKeyboard.isVisible() || !visualViewport || baselineHeight == null) {
            if (this.streamVideoTopOffsetPx != 0 && !screenKeyboard.isVisible()) {
                this.resetKeyboardViewportVideoOffset()
            }
            return
        }

        const viewportShrink = baselineHeight - visualViewport.height
        if (viewportShrink < 80) {
            if (this.streamVideoTopOffsetPx != 0) {
                this.streamVideoTopOffsetPx = 0
                this.applyStreamVideoTopOffset()
            }
            return
        }

        const streamRect = this.getStreamRect()
        if (streamRect.width <= 0 || streamRect.height <= 0) {
            return
        }

        const visibleTop = visualViewport.offsetTop
        const visibleBottom = visualViewport.offsetTop + visualViewport.height

        let newTopOffsetPx = this.streamVideoTopOffsetPx
        if (localCursorState.visible) {
            let delta = 0

            const safeMargin = Math.min(100, visualViewport.height * 0.25)
            const cursorY = streamRect.top + localCursorState.y * streamRect.height

            if (cursorY < visibleTop + safeMargin) {
                delta = visibleTop + safeMargin - cursorY
            } else if (cursorY > visibleBottom - safeMargin) {
                delta = visibleBottom - safeMargin - cursorY
            }

            newTopOffsetPx += delta
        } else {
            const screenTopToVideoTop = visualViewport.height - streamRect.height
            if (screenTopToVideoTop > 0) {
                newTopOffsetPx = visibleTop - screenTopToVideoTop
            }
        }

        if (Math.abs(newTopOffsetPx - this.streamVideoTopOffsetPx) >= 1) {
            this.streamVideoTopOffsetPx = newTopOffsetPx
            this.applyStreamVideoTopOffset()
        }
    }
    private applyStreamVideoTopOffset() {
        if (Math.abs(this.streamVideoTopOffsetPx) < 0.5) {
            document.documentElement.style.removeProperty("--stream-video-top")
            return
        }

        document.documentElement.style.setProperty("--stream-video-top", `calc(50% + ${this.streamVideoTopOffsetPx}px)`)
    }
    private updateKeyboardFloatingButtonPosition() {
        const screenKeyboard = this.sidebar.getScreenKeyboard()
        const visualViewport = window.visualViewport
        if (!screenKeyboard.isVisible() || !visualViewport) {
            this.resetKeyboardFloatingButtonPosition()
            return
        }

        const bottomInset = Math.min(16, visualViewport.height * 0.08)
        const buttonTop = visualViewport.offsetTop + visualViewport.height - bottomInset
        document.documentElement.style.setProperty("--stream-keyboard-button-top", `${buttonTop}px`)
    }
    private resetKeyboardFloatingButtonPosition() {
        document.documentElement.style.removeProperty("--stream-keyboard-button-top")
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.div)
    }
    unmount(parent: HTMLElement): void {
        this.dispose()
        parent.removeChild(this.div)
    }
    dispose() {
        if (this.disposed) return
        this.disposed = true
        this.desktop.dispose()
        window.clearInterval(this.statsInterval)
        if (this.touchFrame != null) window.cancelAnimationFrame(this.touchFrame)
        if (this.gamepadFrame != null) window.cancelAnimationFrame(this.gamepadFrame)
        for (const cleanup of this.listenerCleanup.splice(0)) cleanup()
        this.stream.removeInfoListener(this.infoListener)
        this.stream.getInput().removeScreenKeyboardVisibleEvent(this.keyboardVisibleListener)
        this.stream.getStats().setEnabled(false)
        this.stream.getStats().setCollectionEnabled(false)
        this.stream.unmount(this.div)
        void this.stream.stop()
        this.sidebar.dispose()
        setSidebar(null)
    }

    getStreamRect(): DOMRect {
        // The bounding rect of the videoElement or canvasElement can be bigger than the actual video
        // -> We need to correct for this when sending positions, else positions are wrong
        return this.stream.getVideoRenderer()?.getStreamRect() ?? new DOMRect()
    }
    getStream(): Stream | null {
        return this.stream
    }
}

class AutoFullscreenModal implements Component, Modal<void> {
    private message = document.createElement("p")
    private root = document.createElement("div")
    private okButton = document.createElement("button")
    private cancelButton = document.createElement("button")
    private onConfirm: () => Promise<void>

    constructor(onConfirm: () => Promise<void>) {
        this.onConfirm = onConfirm
        this.message.innerText = I.stream.autoFullscreenPrompt
        this.okButton.innerText = I.modal.ok
        this.cancelButton.innerText = I.modal.cancel
        this.okButton.classList.add("btn", "btn-primary")
        this.cancelButton.classList.add("btn", "btn-ghost")
        this.message.classList.add("modal-message-text")
        this.root.classList.add("modal-message")
    }

    mount(parent: HTMLElement): void {
        const actions = document.createElement("div")
        actions.classList.add("modal-actions")
        actions.append(this.cancelButton, this.okButton)
        this.root.appendChild(this.message)
        this.root.appendChild(actions)
        parent.appendChild(this.root)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.root)
    }

    onFinish(abort: AbortSignal): Promise<void> {
        return new Promise((resolve) => {
            this.okButton.addEventListener("click", async () => {
                await this.onConfirm()
                resolve()
            }, { once: true, signal: abort })

            this.cancelButton.addEventListener("click", () => {
                resolve()
            }, { once: true, signal: abort })
        })
    }
}

class ViewerSidebar implements Component, Sidebar {
    private clipboard: StreamClipboard
    private controls: HTMLElement
    private statusSlot = document.createElement("div")
    private selectDesktopTab: (tab: "controls" | "clipboard" | "status") => void = () => {}
    getClipboard() { return this.clipboard }
    openDesktopTab(tab: "clipboard" | "status") { this.selectDesktopTab(tab); setSidebarExtended(true) }
    attachDesktop(session: DesktopSession) {
        this.statusSlot.appendChild(session.getStatusPanelElement())
        this.controls.appendChild(session.getInputControlsElement())
    }
    setDocumentMode(enabled: boolean) { this.screenKeyboard.setDocumentMode(enabled) }
    dispose() { this.screenKeyboard.dispose() }

    private app: ViewerApp

    private div = document.createElement("div")

    private buttonDiv = document.createElement("div")

    private sendKeycodeButton = document.createElement("button")

    private keyboardButton = document.createElement("button")
    private floatingKeyboardButton = document.createElement("button")
    private screenKeyboard = new ScreenKeyboard()

    private lockMouseButton = document.createElement("button")
    private fullscreenButton = document.createElement("button")

    private statsButton = document.createElement("button")
    private exitStreamButton = document.createElement("button")

    private mouseMode: SelectComponent
    private touchMode: SelectComponent

    constructor(app: ViewerApp, api: Api, hostId: number) {
        this.app = app

        // Configure divs
        this.div.classList.add("sidebar-stream")

        const header = document.createElement("div")
        header.classList.add("sidebar-stream-header")
        const headerCopy = document.createElement("div")
        headerCopy.classList.add("sidebar-stream-header-copy")
        const headerGlyph = document.createElement("span")
        headerGlyph.classList.add("brand-glyph")
        const headerTitle = document.createElement("strong")
        headerTitle.innerText = I.ui.streamControls
        const headerSubtitle = document.createElement("span")
        headerSubtitle.innerText = I.ui.streamControlsHint
        headerCopy.appendChild(headerTitle)
        headerCopy.appendChild(headerSubtitle)
        header.appendChild(headerGlyph)
        header.appendChild(headerCopy)
        this.div.appendChild(header)

        const controls = this.controls = document.createElement("div")
        controls.className = "sidebar-stream-control-panel"
        controls.id = "stream-controls-panel"
        controls.setAttribute("role", "tabpanel")
        controls.setAttribute("aria-labelledby", "stream-controls-tab")
        const clipboard = this.clipboard = new StreamClipboard(api, hostId)
        clipboard.element.id = "stream-clipboard-panel"
        clipboard.element.hidden = true
        clipboard.element.setAttribute("role", "tabpanel")
        clipboard.element.setAttribute("aria-labelledby", "stream-clipboard-tab")
        const tabs = document.createElement("div")
        tabs.className = "sidebar-stream-tabs"
        tabs.setAttribute("role", "tablist")
        tabs.setAttribute("aria-label", I.ui.streamControls)
        const controlsTab = document.createElement("button")
        controlsTab.id = "stream-controls-tab"
        controlsTab.textContent = I.clipboard.controls
        const clipboardTab = document.createElement("button")
        clipboardTab.id = "stream-clipboard-tab"
        clipboardTab.textContent = I.clipboard.title
        const statusTab = document.createElement("button")
        statusTab.id = "stream-status-tab"
        statusTab.textContent = desktopStrings(getCurrentLanguage()).statusTab
        this.statusSlot.id = "stream-status-panel"
        const tabPanels: Array<[HTMLButtonElement, HTMLElement]> = [[controlsTab, controls], [clipboardTab, clipboard.element], [statusTab, this.statusSlot]]
        const selectTab = (selected: number) => {
            tabPanels.forEach(([tab, panel], index) => {
                panel.hidden = index !== selected
                tab.setAttribute("aria-selected", String(index === selected))
                tab.tabIndex = index === selected ? 0 : -1
            })
            this.screenKeyboard.hide()
            if (selected === 1) void clipboard.refresh()
        }
        this.selectDesktopTab = tab => selectTab(tab === "controls" ? 0 : tab === "clipboard" ? 1 : 2)
        tabPanels.forEach(([tab, panel], index) => {
            tab.setAttribute("role", "tab")
            tab.setAttribute("aria-controls", panel.id)
            tab.type = "button"
            tab.addEventListener("click", () => selectTab(index))
            tab.addEventListener("keydown", event => {
                if (["ArrowLeft", "ArrowRight", "Home", "End"].indexOf(event.key) === -1) return
                event.preventDefault()
                const target = event.key === "Home" ? 0 : event.key === "End" ? 2 : (index + (event.key === "ArrowLeft" ? 2 : 1)) % 3
                selectTab(target)
                tabPanels[target][0].focus()
            })
            tabs.appendChild(tab)
        })
        selectTab(0)
        this.div.append(tabs, controls, clipboard.element, this.statusSlot)
        stopPropagationOn(this.div)

        const quickLabel = document.createElement("div")
        quickLabel.classList.add("sidebar-stream-section-label")
        quickLabel.innerText = I.ui.quickControls
        controls.appendChild(quickLabel)

        this.buttonDiv.classList.add("sidebar-stream-buttons")
        controls.appendChild(this.buttonDiv)

        const modeSection = document.createElement("div")
        modeSection.classList.add("sidebar-stream-modes")
        const modeLabel = document.createElement("div")
        modeLabel.classList.add("sidebar-stream-section-label")
        modeLabel.innerText = I.ui.inputModes
        modeSection.appendChild(modeLabel)
        controls.appendChild(modeSection)

        // Send keycode
        setButtonContent(this.sendKeycodeButton, "hash", I.stream.sendKeycode)
        this.sendKeycodeButton.classList.add("sidebar-stream-action")
        this.sendKeycodeButton.dataset.action = "keycode"
        this.sendKeycodeButton.addEventListener("click", async () => {
            const key = await showModal(new SendKeycodeModal())

            if (key == null) {
                return
            }

            this.app.getStream()?.getInput().sendKey(true, key, 0)
            this.app.getStream()?.getInput().sendKey(false, key, 0)
        })
        this.buttonDiv.appendChild(this.sendKeycodeButton)

        // Pointer Lock
        setButtonContent(this.lockMouseButton, "mouse", I.stream.lockMouse)
        this.lockMouseButton.classList.add("sidebar-stream-action")
        this.lockMouseButton.dataset.action = "mouse"
        this.lockMouseButton.addEventListener("click", async () => {
            await this.app.requestPointerLock(true)
        })
        this.buttonDiv.appendChild(this.lockMouseButton)

        // Pop up keyboard
        setButtonContent(this.keyboardButton, "keyboard", I.stream.keyboard)
        this.keyboardButton.classList.add("sidebar-stream-action")
        this.keyboardButton.dataset.action = "keyboard"
        this.keyboardButton.addEventListener("click", async () => {
            setSidebarExtended(false)
            this.screenKeyboard.show()
        })
        this.buttonDiv.appendChild(this.keyboardButton)

        this.floatingKeyboardButton.replaceChildren(icon("close"))
        this.floatingKeyboardButton.title = I.stream.hideKeyboard
        this.floatingKeyboardButton.ariaLabel = I.stream.hideKeyboard
        this.floatingKeyboardButton.classList.add("stream-keyboard-floating-button")
        this.floatingKeyboardButton.addEventListener("click", event => {
            event.preventDefault()
            event.stopPropagation()
            this.screenKeyboard.hide()
        })
        stopPropagationOn(this.floatingKeyboardButton)
        this.screenKeyboard.addKeyDownListener(this.onKeyDown.bind(this))
        this.screenKeyboard.addKeyUpListener(this.onKeyUp.bind(this))
        this.screenKeyboard.addTextListener(this.onText.bind(this))
        this.screenKeyboard.addKeyboardModeWillChangeListener(this.app.onScreenKeyboardModeWillChange.bind(this.app))
        this.screenKeyboard.addKeyboardModeListener(this.onKeyboardModeChange.bind(this))
        this.div.appendChild(this.screenKeyboard.getHiddenElement())


        // Fullscreen
        setButtonContent(this.fullscreenButton, "fullscreen", I.stream.fullscreen)
        this.fullscreenButton.classList.add("sidebar-stream-action")
        this.fullscreenButton.dataset.action = "fullscreen"
        this.fullscreenButton.addEventListener("click", async () => {
            if (this.app.isFullscreen()) {
                this.app.markManualFullscreenExitRequested()
                await this.app.exitFullscreen()
            } else {
                await this.app.requestFullscreen()
            }
        })
        this.buttonDiv.appendChild(this.fullscreenButton)

        // Stats
        setButtonContent(this.statsButton, "stats", I.stream.stats)
        this.statsButton.classList.add("sidebar-stream-action")
        this.statsButton.dataset.action = "stats"
        this.statsButton.addEventListener("click", () => {
            const stats = this.app.getStream()?.getStats()
            if (stats) {
                stats.toggle()
            }
        })
        this.buttonDiv.appendChild(this.statsButton)

        // Close stream
        setButtonContent(this.exitStreamButton, "power", I.stream.exit)
        this.exitStreamButton.classList.add("sidebar-stream-action", "sidebar-stream-action-danger")
        this.exitStreamButton.dataset.action = "exit"
        this.exitStreamButton.addEventListener("click", async () => {
            this.app.markManualStop()
            const stream = this.app.getStream()
            if (stream) {
                const success = await stream.stop()
                if (!success) {
                    console.debug("Failed to close stream correctly")
                }
            }

            if (window.matchMedia('(display-mode: standalone)').matches) {
                history.back()
            } else {
                window.close()
            }

        })
        this.buttonDiv.appendChild(this.exitStreamButton)

        // Select Mouse Mode
        this.mouseMode = new SelectComponent("mouseMode", [
            { value: "relative", name: I.stream.relative },
            { value: "follow", name: I.stream.follow },
            { value: "localCursor", name: I.stream.localCursor },
            { value: "pointAndDrag", name: I.stream.pointAndDrag }
        ], {
            displayName: I.stream.mouseMode,
            preSelectedOption: this.app.getInputConfig().mouseMode
        })
        this.mouseMode.addChangeListener(this.onMouseModeChange.bind(this))
        this.mouseMode.mount(modeSection)

        // Select Touch Mode
        this.touchMode = new SelectComponent("touchMode", [
            { value: "touch", name: I.stream.touch },
            { value: "mouseRelative", name: I.stream.relative },
            { value: "localCursor", name: I.stream.localCursor },
            { value: "pointAndDrag", name: I.stream.pointAndDrag }
        ], {
            displayName: I.stream.touchMode,
            preSelectedOption: this.app.getInputConfig().touchMode
        })
        this.touchMode.addChangeListener(this.onTouchModeChange.bind(this))
        this.touchMode.mount(modeSection)
    }

    onCapabilitiesChange(capabilities: StreamCapabilities) {
        this.touchMode.setOptionEnabled("touch", capabilities.touch)
    }

    getScreenKeyboard(): ScreenKeyboard {
        return this.screenKeyboard
    }

    // -- Keyboard
    private onText(event: TextEvent) {
        this.app.getStream()?.getInput().sendText(event.detail.text)
    }
    private onKeyDown(event: KeyboardEvent) {
        this.app.getStream()?.getInput().onKeyDown(event)
    }
    private onKeyUp(event: KeyboardEvent) {
        this.app.getStream()?.getInput().onKeyUp(event)
    }
    private onKeyboardModeChange(event: KeyboardModeEvent) {
        if (event.detail.enabled) {
            this.floatingKeyboardButton.classList.add("visible")
        } else {
            this.floatingKeyboardButton.classList.remove("visible")
            this.app.resetKeyboardViewportVideoOffset()
        }
    }

    // -- Mouse Mode
    private onMouseModeChange() {
        const config = this.app.getInputConfig()
        config.mouseMode = this.mouseMode.getValue() as any
        this.app.setInputConfig(config)
    }

    // -- Touch Mode
    private onTouchModeChange() {
        const config = this.app.getInputConfig()
        config.touchMode = this.touchMode.getValue() as any
        this.app.setInputConfig(config)
    }

    extended(): void {

    }
    unextend(): void {

    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.div)
        const appRoot = document.getElementById("root")
            ; (appRoot ?? document.body).appendChild(this.floatingKeyboardButton)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.div)
        if (this.floatingKeyboardButton.parentElement) {
            this.floatingKeyboardButton.parentElement.removeChild(this.floatingKeyboardButton)
        }
    }
}

class SendKeycodeModal extends FormModal<number> {

    private dropdownSearch: SelectComponent

    constructor() {
        super()

        const keyList = []
        for (const keyNameRaw in StreamKeys) {
            const keyName = keyNameRaw as keyof typeof StreamKeys
            const keyValue = StreamKeys[keyName]

            const PREFIX = "VK_"

            let name: string = keyName
            if (name.startsWith(PREFIX)) {
                name = name.slice(PREFIX.length)
            }

            keyList.push({
                value: keyValue.toString(),
                name
            })
        }

        this.dropdownSearch = new SelectComponent("winKeycode", keyList, {
            hasSearch: true,
            displayName: I.stream.selectKeycode
        })
    }

    mountForm(form: HTMLFormElement): void {
        this.dropdownSearch.mount(form)
    }


    reset(): void {
        this.dropdownSearch.reset()
    }

    submit(): number | null {
        const keyString = this.dropdownSearch.getValue()
        if (keyString == null) {
            return null
        }

        return parseInt(keyString)
    }
}

// Stop propagation so the stream doesn't get it
export function stopPropagationOn(element: HTMLElement) {
    element.addEventListener("paste", onStopPropagation)
    element.addEventListener("keydown", onStopPropagation)
    element.addEventListener("keyup", onStopPropagation)
    element.addEventListener("keypress", onStopPropagation)
    element.addEventListener("click", onStopPropagation)
    element.addEventListener("mousedown", onStopPropagation)
    element.addEventListener("mouseup", onStopPropagation)
    element.addEventListener("mousemove", onStopPropagation)
    element.addEventListener("wheel", onStopPropagation)
    element.addEventListener("contextmenu", onStopPropagation)
    element.addEventListener("touchstart", onStopPropagation)
    element.addEventListener("touchmove", onStopPropagation)
    element.addEventListener("touchend", onStopPropagation)
    element.addEventListener("touchcancel", onStopPropagation)
}
function onStopPropagation(event: Event) {
    event.stopPropagation()
}
