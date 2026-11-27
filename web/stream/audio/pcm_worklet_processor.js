class MoonlightPcmPlayer extends AudioWorkletProcessor {
    constructor() {
        super()
        this.queue = []
        this.offset = 0
        this.port.onmessage = event => {
            const channels = event.data && event.data.channels
            if (!Array.isArray(channels) || channels.length === 0) return
            this.queue.push(channels)
            if (this.queue.length > 24) {
                this.queue.splice(0, this.queue.length - 12)
                this.offset = 0
            }
        }
    }

    process(_inputs, outputs) {
        const output = outputs[0]
        if (!output || output.length === 0) return true
        for (const channel of output) channel.fill(0)

        let outOffset = 0
        const quantum = output[0].length
        while (outOffset < quantum && this.queue.length > 0) {
            const frame = this.queue[0]
            const source = frame[0]
            if (!source || this.offset >= source.length) {
                this.queue.shift()
                this.offset = 0
                continue
            }
            const count = Math.min(quantum - outOffset, source.length - this.offset)
            for (let channel = 0; channel < output.length; channel++) {
                const input = frame[channel]
                if (input) output[channel].set(input.subarray(this.offset, this.offset + count), outOffset)
            }
            this.offset += count
            outOffset += count
            if (this.offset >= source.length) {
                this.queue.shift()
                this.offset = 0
            }
        }
        return true
    }
}

registerProcessor("moonlight-pcm-player", MoonlightPcmPlayer)
