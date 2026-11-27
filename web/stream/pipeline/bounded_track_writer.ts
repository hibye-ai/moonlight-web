import { StatValue } from "../stats.js";

type Sample = { close(): void, duration?: number | null }

// Only one write enters the browser sink at a time. Video keeps the newest
// decoded frame; audio keeps a short contiguous tail. Never drop encoded video.
export class BoundedTrackWriter<T extends Sample> {
    private pending: Array<{ sample: T, at: number, durationMs: number }> = []
    private active = false
    private closed = false
    private activeAt = 0
    private dropped = 0
    private written = 0
    private errors = 0
    private maxWriteMs = 0

    constructor(private writer: WritableStreamDefaultWriter<T>, private kind: "video" | "audio", private now = () => performance.now()) {}

    submit(sample: T) {
        if (this.closed) { this.drop(sample); return }
        const durationMs = Math.max(0, (sample.duration ?? 10000) / 1000)
        this.pending.push({ sample, at: this.now(), durationMs })
        const countLimit = this.kind === "video" ? 1 : 12
        while (this.pending.length > countLimit || (this.pending.length > 1 && this.pending.reduce((n, item) => n + item.durationMs, 0) > 100)) {
            this.drop(this.pending.shift()!.sample)
        }
        this.drain()
    }

    private drop(sample: T) {
        this.dropped++
        sample.close()
    }

    private drain() {
        if (this.active || this.closed) return
        const maxAgeMs = this.kind === "video" ? 100 : 150
        while (this.pending.length && this.now() - this.pending[0].at > maxAgeMs) this.drop(this.pending.shift()!.sample)
        const item = this.pending.shift()
        if (!item) return
        this.active = true
        this.activeAt = this.now()
        // Catch synchronous failures and rejected writes, including shutdown.
        Promise.resolve().then(() => this.writer.write(item.sample)).then(() => {
            this.written++
        }, () => {
            this.errors++
            this.close()
        }).finally(() => {
            item.sample.close()
            this.maxWriteMs = Math.max(this.maxWriteMs, this.now() - this.activeAt)
            this.active = false
            this.drain()
        })
    }

    close() {
        if (this.closed) return
        this.closed = true
        while (this.pending.length) this.drop(this.pending.shift()!.sample)
        void this.writer.abort().catch(() => {})
    }

    reportStats(stats: Record<string, StatValue>, prefix: string) {
        stats[`${prefix}Pending`] = this.pending.length
        stats[`${prefix}PendingMs`] = this.pending.reduce((n, item) => n + item.durationMs, 0)
        stats[`${prefix}WriteAgeMs`] = this.active ? this.now() - this.activeAt : 0
        stats[`${prefix}MaxWriteMs`] = this.maxWriteMs
        stats[`${prefix}Dropped`] = this.dropped
        stats[`${prefix}Written`] = this.written
        stats[`${prefix}Errors`] = this.errors
    }
}
