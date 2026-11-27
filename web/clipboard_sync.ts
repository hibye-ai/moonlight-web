// Session-only clipboard auto-sync scheduler.
//
// One direction at a time, one timer at a time, and never two ticks in flight. Every
// enable/disable bumps a generation so a tick that completes after a stop, pause or
// dispose cannot reschedule itself or act on stale state. The opt-in lives only in this
// object: it is never persisted and starts off for every new page/session.

export type SyncDirection = "remote-to-local" | "local-to-remote"
export type SyncState = "off" | "on" | "paused"
export type SyncTickResult = "continue" | "stop"

export type SyncTimers = {
    setTimeout(callback: () => void, delay: number): unknown
    clearTimeout(handle: unknown): void
}

export type SyncOptions = {
    timers: SyncTimers
    intervalMs: number
    /** Whether ticks may run right now (session active, page visible and focused, ...). */
    canRun: () => boolean
    /** Performs one sync step. Must not throw; "stop" turns sync off. */
    tick: (direction: SyncDirection) => Promise<SyncTickResult>
    onStateChange: (state: SyncState) => void
}

/**
 * In-memory echo guard key (length + 64-bit non-cryptographic hash). It is compared with
 * the last transferred value only and is never stored or logged.
 */
export function clipboardFingerprint(text: string): string {
    let h1 = 0xdeadbeef ^ text.length
    let h2 = 0x41c6ce57 ^ text.length
    for (let i = 0; i < text.length; i++) {
        const code = text.charCodeAt(i)
        h1 = Math.imul(h1 ^ code, 2654435761)
        h2 = Math.imul(h2 ^ code, 1597334677)
    }
    h1 = Math.imul(h1 ^ (h1 >>> 16), 2246822507) ^ Math.imul(h2 ^ (h2 >>> 13), 3266489909)
    h2 = Math.imul(h2 ^ (h2 >>> 16), 2246822507) ^ Math.imul(h1 ^ (h1 >>> 13), 3266489909)
    return `${text.length}:${(h2 >>> 0).toString(16)}:${(h1 >>> 0).toString(16)}`
}

export class ClipboardSessionSync {
    private enabled = false
    private paused = false
    private current: SyncDirection = "remote-to-local"
    private timer: unknown = null
    private running = false
    private generation = 0

    constructor(private options: SyncOptions) { }

    get direction(): SyncDirection {
        return this.current
    }

    get state(): SyncState {
        return !this.enabled ? "off" : this.paused ? "paused" : "on"
    }

    start(direction: SyncDirection) {
        this.generation++
        this.cancelTimer()
        this.current = direction
        this.enabled = true
        this.paused = false
        this.evaluate(false)
        this.options.onStateChange(this.state)
    }

    stop() {
        const wasEnabled = this.enabled
        this.generation++
        this.enabled = false
        this.paused = false
        this.cancelTimer()
        if (wasEnabled) this.options.onStateChange(this.state)
    }

    /**
     * Re-checks the run gate. Pausing cancels the timer synchronously; resuming
     * schedules an immediate tick unless one is already running or pending.
     */
    evaluate(immediate = true) {
        if (!this.enabled) return
        const before = this.state
        if (this.options.canRun()) {
            this.paused = false
            if (this.timer === null && !this.running) this.schedule(immediate ? 0 : this.options.intervalMs)
        } else {
            this.paused = true
            this.cancelTimer()
        }
        if (this.state !== before) this.options.onStateChange(this.state)
    }

    private schedule(delay: number) {
        const generation = this.generation
        this.timer = this.options.timers.setTimeout(() => {
            this.timer = null
            void this.fire(generation)
        }, delay)
    }

    private cancelTimer() {
        if (this.timer !== null) {
            this.options.timers.clearTimeout(this.timer)
            this.timer = null
        }
    }

    private async fire(generation: number) {
        if (generation !== this.generation || !this.enabled || this.running) return
        if (!this.options.canRun()) {
            this.evaluate()
            return
        }
        this.running = true
        let result: SyncTickResult
        try {
            result = await this.options.tick(this.current)
        } catch {
            result = "continue"
        } finally {
            this.running = false
        }
        if (generation !== this.generation || !this.enabled) return
        if (result === "stop") {
            this.stop()
            return
        }
        if (this.options.canRun()) {
            if (this.timer === null) this.schedule(this.options.intervalMs)
        } else {
            this.evaluate()
        }
    }
}
