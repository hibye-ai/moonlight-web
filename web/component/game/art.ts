import { IconName } from "../icons.js"

// Sunshine ships grayscale stock covers (desktop.png, box.png) for apps without custom art.
// Real cover art is virtually never fully desaturated, so a colorless image is treated as a placeholder
// and replaced with a poster drawn in the current theme.

const SAMPLE_WIDTH = 18
const SAMPLE_HEIGHT = 24
// Mean per-pixel channel spread (0-255) below which an image counts as grayscale
const GRAYSCALE_THRESHOLD = 6

const cache = new WeakMap<Blob, Promise<boolean>>()

export function isPlaceholderArt(blob: Blob): Promise<boolean> {
    let result = cache.get(blob)
    if (!result) {
        result = detectGrayscale(blob).catch(() => false)
        cache.set(blob, result)
    }
    return result
}

async function detectGrayscale(blob: Blob): Promise<boolean> {
    if (typeof createImageBitmap != "function") {
        return false
    }

    const bitmap = await createImageBitmap(blob)
    const canvas = document.createElement("canvas")
    canvas.width = SAMPLE_WIDTH
    canvas.height = SAMPLE_HEIGHT
    const context = canvas.getContext("2d", { willReadFrequently: true })
    if (!context) {
        bitmap.close()
        return false
    }
    context.drawImage(bitmap, 0, 0, SAMPLE_WIDTH, SAMPLE_HEIGHT)
    bitmap.close()

    const pixels = context.getImageData(0, 0, SAMPLE_WIDTH, SAMPLE_HEIGHT).data
    let spread = 0
    let counted = 0
    for (let index = 0; index < pixels.length; index += 4) {
        if (pixels[index + 3] < 16) {
            continue
        }
        const r = pixels[index], g = pixels[index + 1], b = pixels[index + 2]
        spread += Math.max(r, g, b) - Math.min(r, g, b)
        counted++
    }

    return counted > 0 && spread / counted < GRAYSCALE_THRESHOLD
}

/// Icon used on the themed placeholder poster, or null to show the title's initials.
export function placeholderIcon(title: string): IconName | null {
    const normalized = title.toLocaleLowerCase()
    if (/desktop|데스크톱|데스크탑|바탕\s*화면|桌面|bureau|área de trabalho/.test(normalized)) {
        return "monitor"
    }
    if (/steam|big\s*picture|playnite|launcher/.test(normalized)) {
        return "gamepad"
    }
    return null
}
