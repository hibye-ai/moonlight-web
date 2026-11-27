import { Api, FetchError, fetchApi } from "../api.js"
import { buildUrl } from "../config_.js"
import { getCurrentLanguage, getTranslations } from "../i18n.js"
import {
    browserClipboardSupport, classifyApiError, classifyBrowserError, isClipboardAuthorizationError, parseRemoteStatus, queryClipboardPermission,
    type ClipboardFailure,
} from "../clipboard_capabilities.js"
import { decodeBase64Strict, encodeBase64, inspectPng, PNG_LIMITS } from "../clipboard_png.js"
import { getClipboardFeatureStrings } from "../clipboard_strings.js"
import {
    ClipboardSessionSync, clipboardFingerprint, type SyncDirection, type SyncState, type SyncTickResult, type SyncTimers,
} from "../clipboard_sync.js"

export type StreamClipboardOptions = {
    timers?: SyncTimers
    syncIntervalMs?: number
}

type OperationKind = "refresh" | "store" | "sendText" | "receiveText" | "sendImage" | "receiveImage" | "syncStart" | "sync"
type FeatureState = "working" | "success" | "fallback" | "error" | "info"
type BlockReason = "disposed" | "auth" | "inactive" | "hidden"
type SyncStopReason = "auth" | "inactive" | "hidden" | "permission" | "unsupported" | "not_configured"
type OperationGuard = () => boolean

const DEFAULT_SYNC_INTERVAL_MS = 2000

// Clipboard contents only ever live in this component's memory and DOM fields. They are
// never written to storage, history, logs or diagnostics, and no status text includes them.
export class StreamClipboard {
    readonly element = document.createElement("div")
    private text = document.createElement("textarea")
    private save = document.createElement("button")
    private clear = document.createElement("button")
    private status = document.createElement("p")
    private count = document.createElement("span")
    private remoteText = document.createElement("textarea")
    private remoteStatus = document.createElement("p")
    private reload = document.createElement("button")
    private copy = document.createElement("button")
    private login = document.createElement("button")
    private sendTextButton = document.createElement("button")
    private receiveTextButton = document.createElement("button")
    private sendImageButton = document.createElement("button")
    private receiveImageButton = document.createElement("button")
    private imagePreview = document.createElement("img")
    private featureStatus = document.createElement("p")
    private syncToggle = document.createElement("input")
    private syncDirection = document.createElement("select")
    private syncStatus = document.createElement("p")
    private reading = false
    private available = false
    private availabilityKnown = false
    private authRequired = false
    private busy = false
    private maxBytes = 64 * 1024
    private imageSupported = false
    private maxImageBytes = 0
    private labels = getTranslations(getCurrentLanguage()).clipboard
    private strings = getClipboardFeatureStrings()

    // Lifecycle: `null` until the stream owner reports a state. Manual operations are
    // allowed while unknown; sync requires an explicit `true`.
    private sessionActive: boolean | null = null
    private disposed = false
    private tail: Promise<void> = Promise.resolve()
    private pending = new Map<OperationKind, { generation: number, promise: Promise<void> }>()
    private activeOperations = 0
    private lastFingerprint: string | null = null
    private stagedImage: Uint8Array<ArrayBuffer> | null = null
    private previewUrl: string | null = null
    private syncStartToken = 0
    private syncStopReason: SyncStopReason | null = null
    private sync: ClipboardSessionSync
    private operationGeneration = 0
    private readonly onFocusChange = () => {
        if (document.visibilityState === "hidden" || !this.hasFocus()) {
            this.operationGeneration++
            // There is no baseline to resume until startup has completed.
            if (this.syncToggle.checked && this.sync.state === "off") this.stopSync("hidden")
        }
        this.sync.evaluate()
    }

    constructor(private api: Api, private hostId: number, options: StreamClipboardOptions = {}) {
        this.element.className = "stream-clipboard"
        this.sync = new ClipboardSessionSync({
            timers: options.timers ?? {
                setTimeout: (callback, delay) => window.setTimeout(callback, delay),
                clearTimeout: handle => window.clearTimeout(handle as number),
            },
            intervalMs: options.syncIntervalMs ?? DEFAULT_SYNC_INTERVAL_MS,
            canRun: () => this.syncGateOpen(),
            tick: direction => this.syncTick(direction),
            onStateChange: state => this.renderSyncState(state),
        })
        this.element.append(this.buildQuickSection(), this.buildSyncSection())

        const remoteLabel = document.createElement("label")
        remoteLabel.htmlFor = "stream-clipboard-remote"
        remoteLabel.textContent = this.labels.remoteLabel
        this.remoteText.id = "stream-clipboard-remote"
        this.remoteText.className = "stream-clipboard-remote-text"
        this.remoteText.rows = 4
        this.remoteText.readOnly = true
        this.remoteText.spellcheck = false
        this.remoteText.placeholder = this.labels.emptyRemote
        this.remoteText.setAttribute("aria-describedby", "stream-clipboard-remote-status")
        this.remoteStatus.id = "stream-clipboard-remote-status"
        this.remoteStatus.className = "stream-clipboard-status"
        this.remoteStatus.setAttribute("role", "status")
        this.remoteStatus.setAttribute("aria-live", "polite")
        this.reload.type = "button"
        this.reload.textContent = this.labels.reload
        this.reload.addEventListener("click", () => { void this.refresh() })
        this.copy.type = "button"
        this.copy.className = "btn-primary"
        this.copy.textContent = this.labels.copyLocal
        this.copy.addEventListener("click", () => { void this.copyToLocal() })
        this.login.id = "stream-clipboard-login"
        this.login.type = "button"
        this.login.textContent = getTranslations(getCurrentLanguage()).modal.login
        this.login.hidden = true
        this.login.addEventListener("click", () => { window.open(buildUrl("/"), "_blank", "noopener,noreferrer") })
        const remoteActions = document.createElement("div")
        remoteActions.className = "stream-clipboard-actions"
        remoteActions.append(this.reload, this.copy, this.login)
        const remoteSection = document.createElement("section")
        remoteSection.className = "stream-clipboard-remote-section"
        remoteSection.append(remoteLabel, this.remoteText, remoteActions, this.remoteStatus)
        this.element.appendChild(remoteSection)
        const hint = document.createElement("p")
        hint.className = "stream-clipboard-hint"
        hint.id = "stream-clipboard-hint"
        hint.textContent = this.labels.hint
        const label = document.createElement("label")
        label.htmlFor = "stream-clipboard-text"
        label.textContent = this.labels.label
        this.text.id = "stream-clipboard-text"
        this.text.rows = 5
        this.text.placeholder = this.labels.placeholder
        this.text.setAttribute("aria-describedby", "stream-clipboard-hint stream-clipboard-count")
        this.text.spellcheck = false
        this.text.autocomplete = "off"
        this.text.addEventListener("input", () => {
            this.status.textContent = this.authRequired ? this.labels.authRequired
                : this.availabilityKnown && !this.available ? this.labels.unavailable : ""
            this.updateButtons()
        })
        this.text.addEventListener("paste", event => this.onPaste(event as ClipboardEvent))
        this.count.id = "stream-clipboard-count"
        this.count.className = "stream-clipboard-count"
        this.save.type = "button"
        this.save.className = "btn-primary"
        this.save.textContent = this.labels.save
        this.save.addEventListener("click", () => { void this.store() })
        this.clear.type = "button"
        this.clear.textContent = this.labels.clear
        this.clear.addEventListener("click", () => {
            this.text.value = ""
            this.status.textContent = ""
            this.stagedImage = null
            this.clearPreview()
            this.updateButtons()
            this.text.focus()
        })
        const actions = document.createElement("div")
        actions.className = "stream-clipboard-actions"
        actions.append(this.save, this.clear)
        this.status.className = "stream-clipboard-status"
        this.status.setAttribute("role", "status")
        this.status.setAttribute("aria-live", "polite")
        this.element.append(hint, label, this.text, this.count, actions, this.status)

        // Pausing must be immediate, so focus/visibility are observed here rather than
        // waiting for the next sync tick. Removed again in dispose().
        window.addEventListener("blur", this.onFocusChange)
        window.addEventListener("focus", this.onFocusChange)
        document.addEventListener("visibilitychange", this.onFocusChange)
        this.renderSyncState("off")
        this.updateButtons()
    }

    // -- Public interface (wired by the stream page / shortcut owner) -------------------

    /** Reads this device's clipboard text and writes it to the remote PC. */
    sendLocalText(): Promise<void> {
        return this.exclusive("sendText", valid => this.doSendLocalText(valid))
    }

    /** Reads the remote PC's clipboard text and copies it to this device. */
    receiveRemoteText(): Promise<void> {
        return this.exclusive("receiveText", valid => this.doReceiveRemoteText(valid))
    }

    /** Sends a PNG from this device's clipboard (or one pasted into the panel). */
    sendLocalImage(): Promise<void> {
        return this.exclusive("sendImage", valid => this.doSendLocalImage(valid))
    }

    /** Copies the remote PC's clipboard PNG to this device (preview fallback). */
    receiveRemoteImage(): Promise<void> {
        return this.exclusive("receiveImage", valid => this.doReceiveRemoteImage(valid))
    }

    /** Stream lifecycle. Going inactive stops sync; reconnecting needs a new opt-in. */
    setSessionActive(active: boolean) {
        if (this.disposed) return
        if (this.sessionActive !== active) this.operationGeneration++
        this.sessionActive = active
        if (!active) this.stopSync("inactive")
        this.updateButtons()
    }

    dispose() {
        if (this.disposed) return
        this.disposed = true
        this.operationGeneration++
        this.syncStartToken++
        this.sync.stop()
        window.removeEventListener("blur", this.onFocusChange)
        window.removeEventListener("focus", this.onFocusChange)
        document.removeEventListener("visibilitychange", this.onFocusChange)
        this.stagedImage = null
        this.clearPreview()
        this.lastFingerprint = null
    }

    async refresh() {
        if (this.reading || this.busy || this.disposed) return
        return this.exclusive("refresh", valid => this.doRefresh(valid))
    }

    // -- Operation plumbing ------------------------------------------------------------

    /**
     * Serializes every clipboard operation. A second call of the same kind while one is
     * queued or running in this generation joins it instead of repeating the transfer.
     */
    private exclusive(kind: OperationKind, body: (valid: OperationGuard) => Promise<void>): Promise<void> {
        if (this.disposed) return Promise.resolve()
        const generation = this.operationGeneration
        const existing = this.pending.get(kind)
        // A new opt-in replaces an invalidated baseline; the old one remains serialized
        // but may not absorb the new generation's request.
        if (existing && existing.generation === generation && kind !== "syncStart") return existing.promise
        // Capture at invocation, not when a queued operation eventually starts.
        const valid = this.operationGuard()
        this.activeOperations++
        const run: Promise<void> = this.tail
            .then(() => (valid() ? body(valid) : undefined))
            .catch(error => {
                // Preparation/status errors must also leave working state; never log contents.
                if (valid()) this.handleApiFailure(error)
            })
            .then(() => {
                this.activeOperations--
                if (this.pending.get(kind)?.promise === run) this.pending.delete(kind)
                if (!this.disposed) this.updateButtons()
            })
        this.pending.set(kind, { generation, promise: run })
        this.tail = run
        return run
    }

    private blockedReason(): BlockReason | null {
        if (this.disposed) return "disposed"
        if (this.authRequired) return "auth"
        if (this.sessionActive === false) return "inactive"
        if (document.visibilityState === "hidden") return "hidden"
        return null
    }

    private operationGuard(): OperationGuard {
        const generation = this.operationGeneration
        return () => !this.disposed && generation === this.operationGeneration
    }

    private transferAllowed(valid: OperationGuard): boolean {
        return valid() && this.blockedReason() === null
    }

    private hasFocus(): boolean {
        return typeof document.hasFocus === "function" ? document.hasFocus() : true
    }

    private reportBlocked(reason: BlockReason) {
        if (reason === "disposed") return
        const message = reason === "auth" ? this.strings.authRequired
            : reason === "inactive" ? this.strings.inactive : this.strings.hidden
        this.setFeatureStatus("error", message, reason)
    }

    /** Runs the status probe once; it never reads clipboard contents. */
    private async ensureStatus(valid: OperationGuard) {
        if (!this.availabilityKnown) await this.loadStatus(valid)
    }

    private async loadStatus(valid: OperationGuard) {
        const result = parseRemoteStatus(await fetchApi(this.api, "/host/clipboard", "GET", { query: { host_id: this.hostId } }))
        if (!result) throw new Error("Invalid clipboard status")
        if (!this.transferAllowed(valid)) return
        this.availabilityKnown = true
        this.available = result.available
        this.maxBytes = result.maxTextBytes
        this.imageSupported = result.image
        this.maxImageBytes = result.maxImageBytes
        this.updateButtons()
    }

    /** Common prologue: gate, status probe, configured helper. Returns false to abort. */
    private async prepare(valid: OperationGuard): Promise<boolean> {
        const blocked = this.blockedReason()
        if (blocked) {
            this.reportBlocked(blocked)
            return false
        }
        this.setFeatureStatus("working", this.strings.working)
        await this.ensureStatus(valid)
        if (!this.transferAllowed(valid)) return false
        if (!this.available) {
            this.setFeatureStatus("error", this.strings.notConfigured, "not_configured")
            return false
        }
        return true
    }

    private handleApiFailure(error: unknown) {
        if (this.disposed) return
        const reason = classifyApiError(error)
        this.invalidateAuthorization(error)
        this.setFeatureStatus("error", this.failureMessage(reason, "text"), reason)
    }

    private invalidateAuthorization(error: unknown): boolean {
        if (!isClipboardAuthorizationError(error)) return false
        if (classifyApiError(error) === "auth") this.requireLogin()
        else {
            // HTTP403 is a host ownership denial. Keep the forbidden UI reason and
            // permit a fresh manual action to recheck access, but revoke sync consent.
            this.operationGeneration++
            this.availabilityKnown = false
            this.stopSync("auth")
        }
        return true
    }

    private failureMessage(reason: ClipboardFailure, kind: "text" | "image"): string {
        const s = this.strings
        switch (reason) {
            case "auth": return s.authRequired
            case "permission": return s.forbidden
            case "unsupported": return kind === "image" ? s.imageUnsupported : s.failed
            case "too_large": return kind === "image" ? s.tooLargeImage : s.tooLargeText
            case "invalid": return kind === "image" ? s.invalidImage : s.failed
            case "not_configured": return s.notConfigured
            case "unavailable": return s.unavailable
            case "empty": return kind === "image" ? s.remoteImageEmpty : s.remoteTextEmpty
            default: return s.failed
        }
    }

    private setFeatureStatus(state: FeatureState, message: string, reason?: string) {
        if (this.disposed) return
        this.featureStatus.textContent = message
        this.featureStatus.dataset.state = state
        if (reason) this.featureStatus.dataset.reason = reason
        else delete this.featureStatus.dataset.reason
    }

    private textAllowed(text: string): boolean {
        return new TextEncoder().encode(text).length <= this.maxBytes && text.indexOf("\0") === -1
    }

    // -- Text --------------------------------------------------------------------------

    private async doSendLocalText(valid: OperationGuard) {
        if (!await this.prepare(valid) || !this.transferAllowed(valid)) return
        const support = browserClipboardSupport()
        if (!support.readText) {
            this.manualPaste(support.secure ? this.strings.manualPasteText : `${this.strings.insecureContext} ${this.strings.manualPasteText}`, "unsupported")
            return
        }
        let text: string
        try {
            text = await navigator.clipboard.readText()
        } catch (error) {
            if (!this.transferAllowed(valid)) return
            const reason = classifyBrowserError(error)
            this.manualPaste(reason === "permission" ? `${this.strings.permissionDenied} ${this.strings.manualPasteText}` : this.strings.manualPasteText,
                reason === "permission" ? "permission" : "unsupported")
            return
        }
        if (!this.transferAllowed(valid)) return
        if (typeof text !== "string" || text.length === 0) {
            this.setFeatureStatus("info", this.strings.localTextEmpty, "empty")
            return
        }
        if (!this.textAllowed(text)) {
            this.setFeatureStatus("error", this.strings.tooLargeText, "too_large")
            return
        }
        try {
            await this.writeRemoteText(text, valid)
        } catch (error) {
            if (!this.transferAllowed(valid)) return
            this.handleApiFailure(error)
            return
        }
        if (!this.transferAllowed(valid)) return
        this.setFeatureStatus("success", this.strings.textSent)
    }

    private async writeRemoteText(text: string, valid: OperationGuard) {
        if (!this.transferAllowed(valid)) return
        await fetchApi(this.api, "/host/clipboard", "POST", {
            json: { host_id: this.hostId, text }, response: "ignore",
        })
        if (!this.transferAllowed(valid)) return
        this.lastFingerprint = clipboardFingerprint(text)
        this.remoteText.value = text
        this.remoteStatus.textContent = ""
    }

    /** Fetches remote text; `null` means the response was malformed. */
    private async readRemoteText(): Promise<string | null> {
        const contents = await fetchApi(this.api, "/host/clipboard/read", "POST", { json: { host_id: this.hostId } })
        if (!contents || typeof contents.has_text !== "boolean" || typeof contents.text !== "string") return null
        return contents.has_text ? contents.text : ""
    }

    private async doReceiveRemoteText(valid: OperationGuard) {
        if (!await this.prepare(valid) || !this.transferAllowed(valid)) return
        let text: string | null
        try {
            text = await this.readRemoteText()
        } catch (error) {
            if (!this.transferAllowed(valid)) return
            this.handleApiFailure(error)
            return
        }
        if (!this.transferAllowed(valid)) return
        if (text === null) {
            this.setFeatureStatus("error", this.strings.failed, "invalid")
            return
        }
        this.remoteText.value = text
        this.remoteStatus.textContent = text ? "" : this.labels.emptyRemote
        if (!text) {
            this.setFeatureStatus("info", this.strings.remoteTextEmpty, "empty")
            return
        }
        this.lastFingerprint = clipboardFingerprint(text)
        if (browserClipboardSupport().writeText) {
            try {
                await navigator.clipboard.writeText(text)
                if (this.transferAllowed(valid)) this.setFeatureStatus("success", this.strings.textReceived)
                return
            } catch (error) {
                if (!this.transferAllowed(valid)) return
                this.manualCopy(classifyBrowserError(error) === "permission" ? "permission" : "unsupported")
                return
            }
        }
        this.manualCopy("unsupported")
    }

    /** Selects the remote text for Ctrl+C and tries the legacy copy command once. */
    private manualCopy(reason: "permission" | "unsupported") {
        this.remoteText.focus()
        this.remoteText.select()
        let copied = false
        try {
            copied = document.execCommand("copy")
        } catch {
            copied = false
        }
        if (copied) this.setFeatureStatus("success", this.strings.textReceived)
        else this.setFeatureStatus("fallback", this.strings.manualCopyText, reason)
    }

    private manualPaste(message: string, reason: "permission" | "unsupported") {
        this.text.focus()
        this.setFeatureStatus("fallback", message, reason)
    }

    // -- Images (PNG only) -------------------------------------------------------------

    private async doSendLocalImage(valid: OperationGuard) {
        if (!await this.prepare(valid) || !this.transferAllowed(valid)) return
        if (!this.imageSupported) {
            this.setFeatureStatus("error", this.strings.imageUnsupported, "unsupported")
            return
        }
        let bytes = this.stagedImage
        if (!bytes) {
            const support = browserClipboardSupport()
            if (!support.readImage) {
                this.manualPaste(support.secure ? this.strings.manualPasteImage : `${this.strings.insecureContext} ${this.strings.manualPasteImage}`, "unsupported")
                return
            }
            let items: ClipboardItems
            try {
                items = await navigator.clipboard.read()
            } catch (error) {
                if (!this.transferAllowed(valid)) return
                const reason = classifyBrowserError(error)
                if (reason === "empty") this.setFeatureStatus("info", this.strings.localImageEmpty, "empty")
                else this.manualPaste(reason === "permission" ? `${this.strings.permissionDenied} ${this.strings.manualPasteImage}` : this.strings.manualPasteImage,
                    reason === "permission" ? "permission" : "unsupported")
                return
            }
            if (!this.transferAllowed(valid)) return
            // Only image/png is ever taken; HTML, file lists and other types are ignored.
            const item = items.find(entry => entry.types.indexOf("image/png") !== -1)
            if (!item) {
                this.setFeatureStatus("info", this.strings.localImageEmpty, "empty")
                return
            }
            let blob: Blob
            try {
                blob = await item.getType("image/png")
            } catch {
                if (!this.transferAllowed(valid)) return
                this.setFeatureStatus("info", this.strings.localImageEmpty, "empty")
                return
            }
            if (!this.transferAllowed(valid)) return
            if (blob.size > this.maxImageBytes) {
                this.setFeatureStatus("error", this.strings.tooLargeImage, "too_large")
                return
            }
            bytes = new Uint8Array(await blob.arrayBuffer())
            if (!this.transferAllowed(valid)) return
        }
        const check = inspectPng(bytes, this.maxImageBytes)
        if (!check.ok) {
            const reason = check.reason === "invalid" ? "invalid" : "too_large"
            this.setFeatureStatus("error", reason === "invalid" ? this.strings.invalidImage : this.strings.tooLargeImage, reason)
            return
        }
        try {
            await fetchApi(this.api, "/host/clipboard/image", "POST", {
                json: { host_id: this.hostId, png_base64: encodeBase64(bytes) }, response: "ignore",
            })
        } catch (error) {
            if (!this.transferAllowed(valid)) return
            const reason = classifyApiError(error)
            this.invalidateAuthorization(error)
            this.setFeatureStatus("error", this.failureMessage(reason, "image"), reason)
            return
        }
        if (!this.transferAllowed(valid)) return
        if (this.stagedImage === bytes) {
            this.stagedImage = null
            this.clearPreview()
        }
        this.setFeatureStatus("success", this.strings.imageSent)
    }

    private async doReceiveRemoteImage(valid: OperationGuard) {
        if (!await this.prepare(valid) || !this.transferAllowed(valid)) return
        if (!this.imageSupported) {
            this.setFeatureStatus("error", this.strings.imageUnsupported, "unsupported")
            return
        }
        let result: { has_image?: unknown, png_base64?: unknown }
        try {
            result = await fetchApi(this.api, "/host/clipboard/image/read", "POST", { json: { host_id: this.hostId } })
        } catch (error) {
            if (!this.transferAllowed(valid)) return
            const reason = classifyApiError(error)
            this.invalidateAuthorization(error)
            this.setFeatureStatus("error", this.failureMessage(reason, "image"), reason)
            return
        }
        if (!this.transferAllowed(valid)) return
        if (!result || typeof result.has_image !== "boolean" || typeof result.png_base64 !== "string") {
            this.setFeatureStatus("error", this.strings.invalidImage, "invalid")
            return
        }
        if (!result.has_image) {
            this.setFeatureStatus("info", this.strings.remoteImageEmpty, "empty")
            return
        }
        const decoded = decodeBase64Strict(result.png_base64, this.maxImageBytes)
        if (!decoded.ok) {
            this.setFeatureStatus("error", decoded.reason === "too_large" ? this.strings.tooLargeImage : this.strings.invalidImage, decoded.reason)
            return
        }
        const check = inspectPng(decoded.bytes, this.maxImageBytes)
        if (!check.ok) {
            const reason = check.reason === "invalid" ? "invalid" : "too_large"
            this.setFeatureStatus("error", reason === "invalid" ? this.strings.invalidImage : this.strings.tooLargeImage, reason)
            return
        }
        const blob = new Blob([decoded.bytes], { type: "image/png" })
        this.showPreview(blob)
        if (!browserClipboardSupport().writeImage) {
            this.setFeatureStatus("fallback", this.strings.manualCopyImage, "unsupported")
            return
        }
        try {
            await navigator.clipboard.write([new ClipboardItem({ "image/png": blob })])
        } catch (error) {
            if (!this.transferAllowed(valid)) return
            this.setFeatureStatus("fallback", this.strings.manualCopyImage,
                classifyBrowserError(error) === "permission" ? "permission" : "unsupported")
            return
        }
        if (this.transferAllowed(valid)) this.setFeatureStatus("success", this.strings.imageReceived)
    }

    /** Manual fallback: an image pasted into the draft field is staged for sending. */
    private onPaste(event: ClipboardEvent) {
        const valid = this.operationGuard()
        const data = event.clipboardData
        if (!data || this.disposed) return
        const plain = typeof data.getData === "function" ? data.getData("text/plain") : ""
        if (plain) return
        const items = Array.prototype.slice.call(data.items ?? []) as DataTransferItem[]
        const png = items.filter(item => item.kind === "file" && item.type === "image/png")[0]
        if (!png) return
        event.preventDefault()
        if (this.availabilityKnown && !this.imageSupported) {
            this.setFeatureStatus("error", this.strings.imageUnsupported, "unsupported")
            return
        }
        const file = png.getAsFile()
        if (!file) return
        const limit = this.maxImageBytes > 0 ? this.maxImageBytes : PNG_LIMITS.maxBytes
        if (file.size > limit) {
            this.setFeatureStatus("error", this.strings.tooLargeImage, "too_large")
            return
        }
        void file.arrayBuffer().then(buffer => {
            if (!this.transferAllowed(valid)) return
            const bytes = new Uint8Array(buffer)
            const check = inspectPng(bytes, limit)
            if (!check.ok) {
                const reason = check.reason === "invalid" ? "invalid" : "too_large"
                this.setFeatureStatus("error", reason === "invalid" ? this.strings.invalidImage : this.strings.tooLargeImage, reason)
                return
            }
            this.stagedImage = bytes
            this.showPreview(new Blob([bytes], { type: "image/png" }))
            this.setFeatureStatus("info", this.strings.imageStaged)
        }, () => {
            if (!this.transferAllowed(valid)) return
            this.setFeatureStatus("error", this.strings.invalidImage, "invalid")
        })
    }

    private showPreview(blob: Blob) {
        this.clearPreview()
        this.previewUrl = URL.createObjectURL(blob)
        this.imagePreview.src = this.previewUrl
        this.imagePreview.hidden = false
    }

    private clearPreview() {
        if (this.previewUrl) URL.revokeObjectURL(this.previewUrl)
        this.previewUrl = null
        this.imagePreview.removeAttribute("src")
        this.imagePreview.hidden = true
    }

    // -- Session sync (text only, explicit opt-in, never persisted) --------------------

    private syncGateOpen(): boolean {
        return !this.disposed && !this.authRequired && this.sessionActive === true && this.available
            && document.visibilityState !== "hidden" && this.hasFocus()
    }

    private onSyncToggle() {
        if (this.syncToggle.checked) void this.startSync(this.selectedDirection())
        else this.stopSync(null)
    }

    private onSyncDirection() {
        if (this.sync.state === "off" && !this.syncToggle.checked) return
        this.sync.stop()
        void this.startSync(this.selectedDirection())
    }

    private selectedDirection(): SyncDirection {
        return this.syncDirection.value === "local-to-remote" ? "local-to-remote" : "remote-to-local"
    }

    /**
     * Starting runs inside the user's change gesture: it checks the gate and permissions,
     * then records a baseline so content already on either clipboard is not pushed.
     */
    private startSync(direction: SyncDirection): Promise<void> {
        const token = ++this.syncStartToken
        this.syncStopReason = null
        return this.exclusive("syncStart", async operationValid => {
            const valid = () => operationValid() && token === this.syncStartToken && this.syncToggle.checked
            const refuse = (reason: SyncStopReason) => {
                if (valid()) this.stopSync(reason)
            }
            if (!valid()) return
            if (this.authRequired) return refuse("auth")
            if (this.sessionActive !== true) return refuse("inactive")
            if (document.visibilityState === "hidden" || !this.hasFocus()) return refuse("hidden")
            try {
                await this.ensureStatus(valid)
            } catch (error) {
                if (!valid()) return
                this.invalidateAuthorization(error)
                return refuse(this.authRequired ? "auth" : "not_configured")
            }
            if (!valid() || !this.transferAllowed(valid)) return
            if (!this.available) return refuse("not_configured")
            const support = browserClipboardSupport()
            let baseline: string
            if (direction === "local-to-remote") {
                if (!support.readText) return refuse("unsupported")
                const before = await queryClipboardPermission("clipboard-read")
                if (!valid() || !this.syncGateOpen()) return
                if (before === "unsupported") return refuse("unsupported")
                if (before === "denied") return refuse("permission")
                try {
                    baseline = await navigator.clipboard.readText()
                } catch {
                    return refuse("permission")
                }
                if (!valid() || !this.syncGateOpen()) return
                // Without a persistent grant every tick would prompt again; refuse instead.
                if (before !== "granted" && await queryClipboardPermission("clipboard-read") !== "granted") return refuse("permission")
            } else {
                if (!support.writeText) return refuse("unsupported")
                if (await queryClipboardPermission("clipboard-write") === "denied") return refuse("permission")
                if (!valid() || !this.syncGateOpen()) return
                let remote: string | null
                try {
                    remote = await this.readRemoteText()
                } catch (error) {
                    if (!valid()) return
                    if (this.invalidateAuthorization(error)) {
                        return refuse("auth")
                    }
                    remote = ""
                }
                baseline = remote ?? ""
            }
            if (!valid() || !this.syncGateOpen()) return
            this.lastFingerprint = clipboardFingerprint(baseline)
            this.sync.start(direction)
        })
    }

    private stopSync(reason: SyncStopReason | null) {
        this.syncStartToken++
        this.syncStopReason = reason
        this.syncToggle.checked = false
        if (this.sync.state === "off") this.renderSyncState("off")
        else this.sync.stop()
    }

    private async syncTick(direction: SyncDirection): Promise<SyncTickResult> {
        // Never queue behind a manual transfer; just try again next interval.
        if (this.activeOperations > 0) return "continue"
        let result: SyncTickResult = "continue"
        const token = this.syncStartToken
        await this.exclusive("sync", async operationValid => {
            const valid = () => operationValid() && token === this.syncStartToken && this.syncToggle.checked && this.syncGateOpen()
            if (!valid()) return
            result = direction === "remote-to-local" ? await this.syncRemoteToLocal(valid) : await this.syncLocalToRemote(valid)
        })
        return result
    }

    private async syncRemoteToLocal(valid: OperationGuard): Promise<SyncTickResult> {
        let text: string | null
        try {
            text = await this.readRemoteText()
        } catch (error) {
            if (!valid()) return "continue"
            if (this.invalidateAuthorization(error)) {
                return "stop"
            }
            // Transient helper/network problems keep sync on; the next tick retries.
            return "continue"
        }
        if (!valid() || !text || !this.syncGateOpen()) return "continue"
        const fingerprint = clipboardFingerprint(text)
        if (fingerprint === this.lastFingerprint) return "continue"
        try {
            await navigator.clipboard.writeText(text)
        } catch (error) {
            if (!valid()) return "continue"
            this.stopSync(classifyBrowserError(error) === "permission" ? "permission" : "unsupported")
            return "stop"
        }
        if (!valid()) return "continue"
        this.lastFingerprint = fingerprint
        if (document.activeElement !== this.remoteText) this.remoteText.value = text
        return "continue"
    }

    private async syncLocalToRemote(valid: OperationGuard): Promise<SyncTickResult> {
        const permission = await queryClipboardPermission("clipboard-read")
        if (!valid()) return "continue"
        if (permission !== "granted") {
            this.stopSync("permission")
            return "stop"
        }
        let text: string
        try {
            text = await navigator.clipboard.readText()
        } catch (error) {
            if (!valid()) return "continue"
            this.stopSync(classifyBrowserError(error) === "permission" ? "permission" : "unsupported")
            return "stop"
        }
        if (!valid() || !this.syncGateOpen() || !text || !this.textAllowed(text)) return "continue"
        if (clipboardFingerprint(text) === this.lastFingerprint) return "continue"
        try {
            await this.writeRemoteText(text, valid)
        } catch (error) {
            if (!valid()) return "continue"
            if (this.invalidateAuthorization(error)) {
                return "stop"
            }
        }
        return "continue"
    }

    private renderSyncState(state: SyncState) {
        if (this.disposed) return
        const s = this.strings
        this.syncStatus.dataset.state = state
        if (state === "off" && this.syncStopReason) this.syncStatus.dataset.reason = this.syncStopReason
        else delete this.syncStatus.dataset.reason
        if (state === "on") {
            const direction = this.sync.direction === "remote-to-local" ? s.syncRemoteToLocal : s.syncLocalToRemote
            this.syncStatus.textContent = s.syncOn(direction)
        } else if (state === "paused") {
            this.syncStatus.textContent = s.syncPaused
        } else {
            switch (this.syncStopReason) {
                case "auth": this.syncStatus.textContent = s.syncStoppedAuth; break
                case "inactive": this.syncStatus.textContent = s.syncStoppedInactive; break
                case "hidden": this.syncStatus.textContent = s.hidden; break
                case "permission": this.syncStatus.textContent = s.syncStoppedPermission; break
                case "unsupported": this.syncStatus.textContent = s.syncUnsupported; break
                case "not_configured": this.syncStatus.textContent = s.syncStoppedUnavailable; break
                default: this.syncStatus.textContent = s.syncOff
            }
        }
    }

    // -- Layout ------------------------------------------------------------------------

    private buildQuickSection(): HTMLElement {
        const s = this.strings
        const section = document.createElement("section")
        section.className = "stream-clipboard-remote-section stream-clipboard-quick"
        const title = document.createElement("p")
        title.className = "stream-clipboard-hint"
        title.textContent = s.quickTitle
        const button = (element: HTMLButtonElement, id: string, label: string, action: () => Promise<void>, primary = false) => {
            element.id = id
            element.type = "button"
            element.textContent = label
            if (primary) element.className = "btn-primary"
            element.addEventListener("click", () => { void action() })
        }
        button(this.receiveTextButton, "stream-clipboard-receive-text", s.receiveRemoteText, () => this.receiveRemoteText(), true)
        button(this.sendTextButton, "stream-clipboard-send-text", s.sendLocalText, () => this.sendLocalText(), true)
        button(this.receiveImageButton, "stream-clipboard-receive-image", s.receiveRemoteImage, () => this.receiveRemoteImage())
        button(this.sendImageButton, "stream-clipboard-send-image", s.sendLocalImage, () => this.sendLocalImage())
        const textActions = document.createElement("div")
        textActions.className = "stream-clipboard-actions"
        textActions.append(this.receiveTextButton, this.sendTextButton)
        const imageTitle = document.createElement("p")
        imageTitle.className = "stream-clipboard-hint"
        imageTitle.textContent = s.imageTitle
        const imageActions = document.createElement("div")
        imageActions.className = "stream-clipboard-actions"
        imageActions.append(this.receiveImageButton, this.sendImageButton)
        this.imagePreview.id = "stream-clipboard-image-preview"
        this.imagePreview.alt = s.imagePreviewAlt
        this.imagePreview.hidden = true
        this.imagePreview.style.maxWidth = "100%"
        this.imagePreview.style.maxHeight = "25dvh"
        this.imagePreview.style.objectFit = "contain"
        this.featureStatus.id = "stream-clipboard-feature-status"
        this.featureStatus.className = "stream-clipboard-status"
        this.featureStatus.setAttribute("role", "status")
        this.featureStatus.setAttribute("aria-live", "polite")
        section.append(title, textActions, imageTitle, imageActions, this.imagePreview, this.featureStatus)
        return section
    }

    private buildSyncSection(): HTMLElement {
        const s = this.strings
        const section = document.createElement("section")
        section.className = "stream-clipboard-remote-section stream-clipboard-sync"
        const toggleLabel = document.createElement("label")
        toggleLabel.htmlFor = "stream-clipboard-sync"
        this.syncToggle.id = "stream-clipboard-sync"
        this.syncToggle.type = "checkbox"
        this.syncToggle.checked = false
        this.syncToggle.setAttribute("aria-describedby", "stream-clipboard-sync-hint stream-clipboard-sync-status")
        this.syncToggle.addEventListener("change", () => this.onSyncToggle())
        const toggleText = document.createElement("span")
        toggleText.textContent = ` ${s.syncLabel}`
        toggleLabel.append(this.syncToggle, toggleText)
        const directionLabel = document.createElement("label")
        directionLabel.htmlFor = "stream-clipboard-sync-direction"
        directionLabel.textContent = s.syncDirectionLabel
        this.syncDirection.id = "stream-clipboard-sync-direction"
        const options: Array<[SyncDirection, string]> = [["remote-to-local", s.syncRemoteToLocal], ["local-to-remote", s.syncLocalToRemote]]
        for (const [value, label] of options) {
            const option = document.createElement("option")
            option.value = value
            option.textContent = label
            this.syncDirection.appendChild(option)
        }
        this.syncDirection.value = "remote-to-local"
        this.syncDirection.addEventListener("change", () => this.onSyncDirection())
        const hint = document.createElement("p")
        hint.id = "stream-clipboard-sync-hint"
        hint.className = "stream-clipboard-hint"
        hint.textContent = s.syncHint
        this.syncStatus.id = "stream-clipboard-sync-status"
        this.syncStatus.className = "stream-clipboard-status"
        this.syncStatus.setAttribute("role", "status")
        this.syncStatus.setAttribute("aria-live", "polite")
        section.append(toggleLabel, directionLabel, this.syncDirection, hint, this.syncStatus)
        return section
    }

    // -- Original text panel -----------------------------------------------------------

    private async doRefresh(valid: OperationGuard) {
        this.reading = true
        this.available = false
        this.availabilityKnown = false
        this.authRequired = false
        this.login.hidden = true
        this.status.textContent = ""
        this.remoteText.value = ""
        this.remoteStatus.textContent = this.labels.loading
        this.updateButtons()
        try {
            const blocked = this.blockedReason()
            if (blocked) {
                this.remoteStatus.textContent = blocked === "inactive" ? this.strings.inactive : blocked === "hidden" ? this.strings.hidden : ""
                return
            }
            await this.loadStatus(valid)
            if (!this.transferAllowed(valid)) return
            if (!this.busy) this.status.textContent = this.available ? "" : this.labels.unavailable
            if (this.available) {
                const text = await this.readRemoteText()
                if (!this.transferAllowed(valid)) return
                if (text === null) throw new Error("Invalid clipboard contents")
                this.remoteText.value = text
                this.remoteStatus.textContent = this.remoteText.value ? "" : this.labels.emptyRemote
            } else {
                this.remoteStatus.textContent = this.labels.unavailable
            }
        } catch (error) {
            if (!this.transferAllowed(valid)) return
            const code = error instanceof FetchError ? error.getResponse()?.status : undefined
            if (!this.invalidateAuthorization(error)) {
                this.remoteStatus.textContent = code === 413 ? this.labels.remoteTooLong : this.labels.readFailed
            }
        } finally {
            this.reading = false
            if (!this.disposed) this.updateButtons()
        }
    }

    private requireLogin() {
        this.operationGeneration++
        this.available = false
        this.availabilityKnown = false
        this.authRequired = true
        this.login.hidden = false
        this.remoteText.value = ""
        this.remoteStatus.textContent = this.labels.authRequired
        this.status.textContent = this.labels.authRequired
        // The draft and any staged image are kept so nothing typed is lost.
        this.stopSync("auth")
    }

    private async copyToLocal() {
        const valid = this.operationGuard()
        if (this.copy.disabled || !this.transferAllowed(valid)) return
        try {
            if (!navigator.clipboard?.writeText) throw new Error("Clipboard API unavailable")
            await navigator.clipboard.writeText(this.remoteText.value)
            if (!this.transferAllowed(valid)) return
            this.lastFingerprint = clipboardFingerprint(this.remoteText.value)
            this.remoteStatus.textContent = this.labels.copied
        } catch {
            if (!this.transferAllowed(valid)) return
            // Keep copying available on HTTP and browsers without Clipboard API
            // permission. Selecting the readonly text also supports manual copy.
            this.remoteText.focus()
            this.remoteText.select()
            try {
                this.remoteStatus.textContent = document.execCommand("copy") ? this.labels.copied : this.labels.copyFailed
            } catch {
                this.remoteStatus.textContent = this.labels.copyFailed
            }
        }
    }

    private updateButtons() {
        const bytes = new TextEncoder().encode(this.text.value).length
        const invalid = bytes > this.maxBytes || this.text.value.includes("\0")
        this.count.textContent = `${bytes.toLocaleString()} / ${this.maxBytes.toLocaleString()} bytes`
        this.save.disabled = this.busy || this.reading || !this.available || bytes === 0 || invalid
        this.reload.disabled = this.busy || this.reading
        this.copy.disabled = this.busy || this.reading || this.remoteText.value.length === 0
        this.clear.disabled = this.busy || (bytes === 0 && !this.stagedImage)
        this.text.readOnly = this.busy
        if (invalid) this.status.textContent = this.labels.tooLong
        const unconfigured = this.availabilityKnown && !this.available
        const inactive = this.sessionActive === false
        this.sendTextButton.disabled = unconfigured || inactive
        this.receiveTextButton.disabled = unconfigured || inactive
        const imageOff = unconfigured || inactive || (this.availabilityKnown && !this.imageSupported)
        this.sendImageButton.disabled = imageOff
        this.receiveImageButton.disabled = imageOff
        this.syncToggle.disabled = unconfigured
    }

    private async store() {
        if (this.save.disabled) return
        return this.exclusive("store", valid => this.doStore(valid))
    }

    private async doStore(valid: OperationGuard) {
        if (!this.transferAllowed(valid)) return
        this.busy = true
        this.status.textContent = ""
        this.save.textContent = this.labels.saving
        this.updateButtons()
        try {
            await this.writeRemoteText(this.text.value, valid)
            if (!this.transferAllowed(valid)) return
            this.status.textContent = this.labels.saved
        } catch (error) {
            if (!this.transferAllowed(valid)) return
            if (!this.invalidateAuthorization(error)) {
                this.status.textContent = this.labels.failed
            }
        } finally {
            this.busy = false
            if (!this.disposed) {
                this.save.textContent = this.labels.save
                this.updateButtons()
            }
        }
    }
}
