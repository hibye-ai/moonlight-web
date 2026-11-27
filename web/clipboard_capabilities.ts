import { FetchError } from "./api.js"
import { PNG_LIMITS } from "./clipboard_png.js"

// Capability negotiation for the clipboard panel. Nothing here reads clipboard contents:
// the browser side only inspects API presence and permission state, and the server side
// only reports what the remote helper declared through its /capabilities endpoint.

export type BrowserClipboardSupport = {
    secure: boolean
    readText: boolean
    writeText: boolean
    readImage: boolean
    writeImage: boolean
}

export function browserClipboardSupport(): BrowserClipboardSupport {
    const secure = window.isSecureContext === true
    const clipboard = secure && typeof navigator !== "undefined" ? navigator.clipboard : undefined
    const hasItem = typeof (globalThis as { ClipboardItem?: unknown }).ClipboardItem === "function"
    return {
        secure,
        readText: typeof clipboard?.readText === "function",
        writeText: typeof clipboard?.writeText === "function",
        readImage: typeof clipboard?.read === "function",
        writeImage: typeof clipboard?.write === "function" && hasItem,
    }
}

export type PermissionStateOrUnknown = "granted" | "prompt" | "denied" | "unsupported"

/** Queries a clipboard permission without triggering a prompt. */
export async function queryClipboardPermission(name: "clipboard-read" | "clipboard-write"): Promise<PermissionStateOrUnknown> {
    try {
        const permissions = typeof navigator !== "undefined" ? navigator.permissions : undefined
        if (!permissions?.query) return "unsupported"
        // The clipboard names are not in the TS DOM PermissionName union.
        const status = await permissions.query({ name: name as unknown as PermissionName })
        const state = status?.state
        return state === "granted" || state === "prompt" || state === "denied" ? state : "unsupported"
    } catch {
        // Firefox and Safari reject the clipboard permission names.
        return "unsupported"
    }
}

export type RemoteClipboardStatus = {
    available: boolean
    maxTextBytes: number
    image: boolean
    maxImageBytes: number
}

/**
 * Parses GET /host/clipboard. `formats`/`max_image_bytes` are optional: a server or
 * helper that predates image support omits them and stays text-only.
 */
export function parseRemoteStatus(value: unknown): RemoteClipboardStatus | null {
    if (!value || typeof value !== "object") return null
    const status = value as { available?: unknown, max_bytes?: unknown, formats?: unknown, max_image_bytes?: unknown }
    if (typeof status.available !== "boolean") return null
    const maxTextBytes = typeof status.max_bytes === "number" && Number.isSafeInteger(status.max_bytes) && status.max_bytes > 0
        ? status.max_bytes : 64 * 1024
    const formats = Array.isArray(status.formats) ? status.formats.filter(format => typeof format === "string") : []
    const maxImage = typeof status.max_image_bytes === "number" && Number.isSafeInteger(status.max_image_bytes) && status.max_image_bytes > 0
        ? Math.min(status.max_image_bytes, PNG_LIMITS.maxBytes) : 0
    const image = status.available && formats.indexOf("image/png") !== -1 && maxImage > 0
    return { available: status.available, maxTextBytes, image, maxImageBytes: image ? maxImage : 0 }
}

export type ClipboardFailure =
    | "auth"
    | "permission"
    | "unsupported"
    | "too_large"
    | "invalid"
    | "not_configured"
    | "unavailable"
    | "empty"
    | "failed"

/** Maps a failed API call to a user-facing reason. */
export function classifyApiError(error: unknown): ClipboardFailure {
    if (!(error instanceof FetchError)) return "unavailable"
    const status = error.getResponse()?.status
    switch (status) {
        case undefined: return "unavailable"
        case 401: return "auth"
        case 403: return "permission"
        // 404: a server without the image routes; 415: the helper declined the format.
        case 404: return "unsupported"
        case 415: return "unsupported"
        case 409: return "not_configured"
        case 413: return "too_large"
        case 400: return "invalid"
        default: return status >= 500 ? "unavailable" : "failed"
    }
}

/** Host ownership denials revoke sync consent; browser permission errors do not. */
export function isClipboardAuthorizationError(error: unknown): boolean {
    if (!(error instanceof FetchError)) return false
    const status = error.getResponse()?.status
    return status === 401 || status === 403
}

/** Maps a browser Clipboard API failure. */
export function classifyBrowserError(error: unknown): ClipboardFailure {
    const name = (error as { name?: unknown } | null)?.name
    if (name === "NotAllowedError" || name === "SecurityError") return "permission"
    if (name === "DataError" || name === "NotFoundError") return "empty"
    if (error instanceof TypeError) return "unsupported"
    return "failed"
}
