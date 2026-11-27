import { ConnectionState } from "./connection.js"
import { DiagnosticsBus } from "./diagnostics.js"

// F2 integration seam (worker A side). Worker B owns StreamClipboard and adds the public
// methods below; this bridge only routes explicit user actions (shortcuts/buttons) to them
// and tells the clipboard when a session may be active. It never touches the clipboard API
// itself and never sees clipboard contents. Every method is optional here so the stream
// page keeps working against a StreamClipboard that does not have them (yet).

export interface StreamClipboardContract {
    sendLocalText?(): Promise<void>
    receiveRemoteText?(): Promise<void>
    sendLocalImage?(): Promise<void>
    receiveRemoteImage?(): Promise<void>
    setSessionActive?(active: boolean): void
    dispose?(): void
}

export type ClipboardAction = "sendText" | "receiveText" | "sendImage" | "receiveImage"
export type ClipboardActionResult = "done" | "busy" | "unsupported" | "error"

const METHOD_FOR_ACTION: Record<ClipboardAction, keyof StreamClipboardContract> = {
    sendText: "sendLocalText",
    receiveText: "receiveRemoteText",
    sendImage: "sendLocalImage",
    receiveImage: "receiveRemoteImage",
}

/// Session sync may only run while the stream is up, the tab is visible and the user is
/// authorized. Anything else (hidden tab, disconnect, auth failure, stop) turns it off.
export function isClipboardSessionAllowed(input: { state: ConnectionState, visible: boolean }): boolean {
    return input.visible && (input.state === "connected" || input.state === "stalled")
}

export type ClipboardBridgeOptions = {
    diagnostics?: DiagnosticsBus | null
    /// Called when the clipboard does not implement an action (e.g. open the clipboard tab).
    onUnsupported?: (action: ClipboardAction) => void
}

export class ClipboardSessionBridge {
    private clipboard: StreamClipboardContract
    private options: ClipboardBridgeOptions
    private inFlight = false
    private lastActive: boolean | null = null
    private disposed = false

    constructor(clipboard: object, options: ClipboardBridgeOptions = {}) {
        this.clipboard = clipboard as StreamClipboardContract
        this.options = options
    }

    private has(method: keyof StreamClipboardContract): boolean {
        return typeof this.clipboard[method] === "function"
    }

    capabilities() {
        return {
            sendText: this.has("sendLocalText"),
            receiveText: this.has("receiveRemoteText"),
            sendImage: this.has("sendLocalImage"),
            receiveImage: this.has("receiveRemoteImage"),
            session: this.has("setSessionActive"),
        }
    }

    async run(action: ClipboardAction): Promise<ClipboardActionResult> {
        if (this.disposed) {
            return "unsupported"
        }
        const method = METHOD_FOR_ACTION[action]
        if (!method || !this.has(method)) {
            this.record(action, "unsupported")
            this.options.onUnsupported?.(action)
            return "unsupported"
        }
        if (this.inFlight) {
            this.record(action, "busy")
            return "busy"
        }
        this.inFlight = true
        try {
            await (this.clipboard[method] as () => Promise<void>).call(this.clipboard)
            this.record(action, "done")
            return "done"
        } catch {
            // The clipboard shows its own status; the error text is never logged here.
            this.record(action, "error")
            return "error"
        } finally {
            this.inFlight = false
        }
    }

    updateLifecycle(input: { state: ConnectionState, visible: boolean }) {
        this.setSessionActive(isClipboardSessionAllowed(input))
    }

    setSessionActive(active: boolean) {
        if (this.disposed || this.lastActive === active) {
            return
        }
        this.lastActive = active
        if (this.has("setSessionActive")) {
            try {
                this.clipboard.setSessionActive!(active)
            } catch {
                // Never let the clipboard break stream lifecycle handling.
            }
        }
        this.options.diagnostics?.record("clipboard.session", { active })
    }

    dispose() {
        if (this.disposed) {
            return
        }
        if (this.lastActive === true) {
            this.setSessionActive(false)
        }
        this.disposed = true
        if (this.has("dispose")) {
            try {
                this.clipboard.dispose!()
            } catch {
                // ignore
            }
        }
    }

    private record(action: ClipboardAction, result: ClipboardActionResult) {
        this.options.diagnostics?.record("clipboard.action", { action, result })
    }
}
