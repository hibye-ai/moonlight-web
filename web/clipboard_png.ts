// PNG-only clipboard payload checks shared by both transfer directions.
//
// These are cheap structural checks (signature, IHDR, size) run before any upload and
// before a received image is handed to the browser clipboard. The server and the native
// helper repeat them independently, and the helper also performs a real decode before
// writing to the remote clipboard, so the browser is never the only line of defence.

export const PNG_LIMITS = {
    maxBytes: 4 * 1024 * 1024,
    maxDimension: 4096,
    maxPixels: 16 * 1024 * 1024,
} as const

export type PngInfo = { width: number, height: number, bytes: number }
export type PngRejection = "invalid" | "too_large" | "dimensions"
export type PngCheck = { ok: true, info: PngInfo } | { ok: false, reason: PngRejection }

const SIGNATURE = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
// Allowed bit depths per PNG colour type (PNG spec, table 11.1).
const BIT_DEPTHS: Record<number, number[]> = {
    0: [1, 2, 4, 8, 16],
    2: [8, 16],
    3: [1, 2, 4, 8],
    4: [8, 16],
    6: [8, 16],
}

export function inspectPng(bytes: Uint8Array, maxBytes: number = PNG_LIMITS.maxBytes): PngCheck {
    if (bytes.length > Math.min(maxBytes, PNG_LIMITS.maxBytes)) return { ok: false, reason: "too_large" }
    // Signature (8) + IHDR length/type (8) + IHDR data (13) + CRC (4).
    if (bytes.length < 33) return { ok: false, reason: "invalid" }
    for (let i = 0; i < SIGNATURE.length; i++) {
        if (bytes[i] !== SIGNATURE[i]) return { ok: false, reason: "invalid" }
    }
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    if (view.getUint32(8) !== 13) return { ok: false, reason: "invalid" }
    if (bytes[12] !== 0x49 || bytes[13] !== 0x48 || bytes[14] !== 0x44 || bytes[15] !== 0x52) {
        return { ok: false, reason: "invalid" }
    }
    const width = view.getUint32(16)
    const height = view.getUint32(20)
    const depth = bytes[24]
    const color = bytes[25]
    if (width === 0 || height === 0) return { ok: false, reason: "invalid" }
    const depths = BIT_DEPTHS[color]
    if (!depths || depths.indexOf(depth) === -1 || bytes[26] !== 0 || bytes[27] !== 0 || bytes[28] > 1) {
        return { ok: false, reason: "invalid" }
    }
    if (width > PNG_LIMITS.maxDimension || height > PNG_LIMITS.maxDimension || width * height > PNG_LIMITS.maxPixels) {
        return { ok: false, reason: "dimensions" }
    }
    return { ok: true, info: { width, height, bytes: bytes.length } }
}

const ENCODE_CHUNK = 0x6000 // multiple of 3, small enough for String.fromCharCode spread

export function encodeBase64(bytes: Uint8Array): string {
    const parts: string[] = []
    for (let offset = 0; offset < bytes.length; offset += ENCODE_CHUNK) {
        const slice = bytes.subarray(offset, offset + ENCODE_CHUNK)
        parts.push(btoa(String.fromCharCode.apply(null, slice as unknown as number[])))
    }
    return parts.join("")
}

const STRICT_BASE64 = /^[A-Za-z0-9+/]*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/

export type Base64Result = { ok: true, bytes: Uint8Array<ArrayBuffer> } | { ok: false, reason: "invalid" | "too_large" }

/** Canonical padded base64 only (no whitespace, no URL alphabet); size checked before decoding. */
export function decodeBase64Strict(text: string, maxBytes: number): Base64Result {
    if (typeof text !== "string" || text.length % 4 !== 0) return { ok: false, reason: "invalid" }
    const padding = text.endsWith("==") ? 2 : text.endsWith("=") ? 1 : 0
    const decodedLength = (text.length / 4) * 3 - padding
    if (decodedLength > maxBytes) return { ok: false, reason: "too_large" }
    if (!STRICT_BASE64.test(text)) return { ok: false, reason: "invalid" }
    // Unused bits before '=' must be zero. Native PngCodec rejects aliases such as
    // AB== for AA==; apply the same canonical wire contract before browser writes.
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    const tail = alphabet.indexOf(text.charAt(text.length - padding - 1))
    if ((padding === 2 && (tail & 15) !== 0) || (padding === 1 && (tail & 3) !== 0)) {
        return { ok: false, reason: "invalid" }
    }
    let binary: string
    try {
        binary = atob(text)
    } catch {
        return { ok: false, reason: "invalid" }
    }
    const bytes = new Uint8Array(binary.length)
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
    return { ok: true, bytes }
}
