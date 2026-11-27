import { StreamKeyModifiers, StreamKeys } from "../api_bindings.js"

// F5 input modes, chord buttons and local shortcuts.
//
// * "game" mode is the existing raw-key behaviour (default).
// * "text" mode lets printable keys and IME composition go through a hidden text field
//   (the existing ScreenKeyboard composition handling) and sends the composed text;
//   navigation/editing keys and chords stay raw keys.
// * Local shortcuts always use the fixed Ctrl+Alt+Shift prefix (no browser binds that), the
//   user only picks the final key from an allowlist and the action from an allowlist.
//   Nothing user-provided is ever evaluated.

export type InputMode = "game" | "text"
export const INPUT_MODES: ReadonlyArray<InputMode> = ["game", "text"]

export type ChordId = "ctrlAltDel" | "ctrlShiftEsc" | "altTab" | "altF4" | "win" | "winD"

export type ChordDefinition = { label: string, keys: ReadonlyArray<number> }

export const CHORDS: Readonly<Record<ChordId, ChordDefinition>> = Object.freeze({
    ctrlAltDel: { label: "Ctrl+Alt+Del", keys: [StreamKeys.VK_CONTROL, StreamKeys.VK_MENU, StreamKeys.VK_DELETE] },
    ctrlShiftEsc: { label: "Ctrl+Shift+Esc", keys: [StreamKeys.VK_CONTROL, StreamKeys.VK_SHIFT, StreamKeys.VK_ESCAPE] },
    altTab: { label: "Alt+Tab", keys: [StreamKeys.VK_MENU, StreamKeys.VK_TAB] },
    altF4: { label: "Alt+F4", keys: [StreamKeys.VK_MENU, StreamKeys.VK_F4] },
    win: { label: "Win", keys: [StreamKeys.VK_LWIN] },
    winD: { label: "Win+D", keys: [StreamKeys.VK_LWIN, StreamKeys.VK_KEY_D] },
})

export const CHORD_IDS = Object.keys(CHORDS) as ReadonlyArray<ChordId>

export function isChordId(value: unknown): value is ChordId {
    return typeof value === "string" && Object.prototype.hasOwnProperty.call(CHORDS, value)
}

const MODIFIER_MASKS: Record<number, number> = {
    [StreamKeys.VK_CONTROL]: StreamKeyModifiers.MASK_CTRL,
    [StreamKeys.VK_LCONTROL]: StreamKeyModifiers.MASK_CTRL,
    [StreamKeys.VK_MENU]: StreamKeyModifiers.MASK_ALT,
    [StreamKeys.VK_LMENU]: StreamKeyModifiers.MASK_ALT,
    [StreamKeys.VK_SHIFT]: StreamKeyModifiers.MASK_SHIFT,
    [StreamKeys.VK_LSHIFT]: StreamKeyModifiers.MASK_SHIFT,
    [StreamKeys.VK_LWIN]: StreamKeyModifiers.MASK_META,
}

export interface ChordInput {
    sendKey(isDown: boolean, key: number, modifiers: number): void
    releaseAll(): void
}

/// Press the chord's keys in order and release them in reverse. Anything already held is
/// released first so the remote never sees a stale modifier combined with the chord.
export function sendChord(input: ChordInput, id: string): boolean {
    if (!isChordId(id)) {
        return false
    }
    const keys = CHORDS[id].keys
    input.releaseAll()
    let modifiers = 0
    for (const key of keys) {
        input.sendKey(true, key, modifiers)
        modifiers |= MODIFIER_MASKS[key] ?? 0
    }
    for (let index = keys.length - 1; index >= 0; index--) {
        const key = keys[index]
        modifiers &= ~(MODIFIER_MASKS[key] ?? 0)
        input.sendKey(false, key, modifiers)
    }
    return true
}

// -- Shortcuts

export type ShortcutAction =
    "clipboardSendText" | "clipboardReceiveText" | "releaseAllInput" | "toggleInputMode" |
    "reconnect" | "audioRecover" | "toggleDiagnostics" | ChordId

export const SHORTCUT_ACTIONS: ReadonlyArray<ShortcutAction> = [
    "clipboardSendText",
    "clipboardReceiveText",
    "releaseAllInput",
    "toggleInputMode",
    "reconnect",
    "audioRecover",
    "toggleDiagnostics",
    ...CHORD_IDS,
]

export function isShortcutAction(value: unknown): value is ShortcutAction {
    return typeof value === "string" && (SHORTCUT_ACTIONS as ReadonlyArray<string>).indexOf(value) !== -1
}

const LETTERS = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".split("").map(letter => `Key${letter}`)
const DIGITS = "0123456789".split("").map(digit => `Digit${digit}`)
const FUNCTION_KEYS = Array.from({ length: 12 }, (_, index) => `F${index + 1}`)
    .filter(code => code !== "F5" && code !== "F11" && code !== "F12")

/// Final keys a shortcut may use (always combined with Ctrl+Alt+Shift).
export const SHORTCUT_KEY_CODES: ReadonlyArray<string> = [...LETTERS, ...DIGITS, ...FUNCTION_KEYS]

export function isShortcutKeyCode(value: unknown): value is string {
    return typeof value === "string" && SHORTCUT_KEY_CODES.indexOf(value) !== -1
}

export function shortcutKeyLabel(code: string): string {
    if (code.startsWith("Key")) return code.slice(3)
    if (code.startsWith("Digit")) return code.slice(5)
    return code
}

export const SHORTCUT_PREFIX_LABEL = "Ctrl+Alt+Shift"

export type ShortcutBinding = { action: ShortcutAction, code: string | null }

export function defaultShortcutBindings(): ShortcutBinding[] {
    return SHORTCUT_ACTIONS.map(action => ({
        action,
        code: action === "clipboardSendText" ? "KeyV" : action === "clipboardReceiveText" ? "KeyC" : null,
    }))
}

export type BindingError = "unknownAction" | "unsupportedKey" | "conflict"

export function validateBinding(binding: { action: unknown, code: unknown }, bindings: ReadonlyArray<ShortcutBinding>): BindingError | null {
    if (!isShortcutAction(binding.action)) return "unknownAction"
    if (binding.code == null) return null
    if (!isShortcutKeyCode(binding.code)) return "unsupportedKey"
    if (bindings.some(other => other.action !== binding.action && other.code === binding.code)) return "conflict"
    return null
}

export function hasShortcutPrefix(event: Pick<KeyboardEvent, "ctrlKey" | "altKey" | "shiftKey" | "metaKey">): boolean {
    return event.ctrlKey && event.altKey && event.shiftKey && !event.metaKey
}

export function matchShortcut(event: Pick<KeyboardEvent, "code" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">, bindings: ReadonlyArray<ShortcutBinding>): ShortcutAction | null {
    if (!hasShortcutPrefix(event) || !isShortcutKeyCode(event.code)) {
        return null
    }
    return bindings.find(binding => binding.code === event.code)?.action ?? null
}

// -- Preferences (local to this browser)

export const DESKTOP_INPUT_PREFS_KEY = "mlDesktopInputPrefs"
const DESKTOP_INPUT_PREFS_VERSION = 1

export type DesktopInputPrefs = {
    inputMode: InputMode
    bindings: ShortcutBinding[]
}

function normalizeBindings(entries: unknown): ShortcutBinding[] {
    const result = defaultShortcutBindings()
    if (!Array.isArray(entries)) {
        return result
    }
    const storedCodes = new Set<string>()
    const overridden = new Set<ShortcutAction>()
    for (const entry of entries.slice(0, SHORTCUT_ACTIONS.length * 2)) {
        if (!entry || typeof entry !== "object") continue
        const { action, code } = entry as { action: unknown, code: unknown }
        if (!isShortcutAction(action) || overridden.has(action)) continue
        if (code != null && !isShortcutKeyCode(code)) continue
        if (code != null && storedCodes.has(code)) continue
        overridden.add(action)
        if (code != null) storedCodes.add(code)
        result.find(binding => binding.action === action)!.code = code == null ? null : code
    }
    // A default that collides with an explicitly stored key yields to the user's choice.
    for (const binding of result) {
        if (!overridden.has(binding.action) && binding.code != null && storedCodes.has(binding.code)) {
            binding.code = null
        }
    }
    return result
}

export function loadDesktopInputPrefs(storage: Pick<Storage, "getItem"> = localStorage): DesktopInputPrefs {
    let parsed: any = null
    try {
        const raw = storage.getItem(DESKTOP_INPUT_PREFS_KEY)
        parsed = raw ? JSON.parse(raw) : null
    } catch {
        parsed = null
    }
    return {
        inputMode: parsed?.inputMode === "text" ? "text" : "game",
        bindings: normalizeBindings(parsed?.bindings),
    }
}

export function saveDesktopInputPrefs(prefs: { inputMode: unknown, bindings: unknown }, storage: Pick<Storage, "setItem"> = localStorage) {
    const normalized: DesktopInputPrefs = {
        inputMode: prefs.inputMode === "text" ? "text" : "game",
        bindings: normalizeBindings(prefs.bindings),
    }
    storage.setItem(DESKTOP_INPUT_PREFS_KEY, JSON.stringify({ version: DESKTOP_INPUT_PREFS_VERSION, ...normalized }))
}

// -- Key routing

const NON_TEXT_INPUT_TYPES = ["checkbox", "radio", "button", "submit", "reset", "range", "color", "file", "image"]

export function isEditableTarget(target: unknown): boolean {
    if (!target || typeof target !== "object") return false
    const element = target as { tagName?: string, type?: string, isContentEditable?: boolean }
    if (element.isContentEditable === true) return true
    const tag = typeof element.tagName === "string" ? element.tagName.toUpperCase() : ""
    if (tag === "TEXTAREA" || tag === "SELECT") return true
    if (tag === "INPUT") {
        return NON_TEXT_INPUT_TYPES.indexOf(String(element.type ?? "").toLowerCase()) === -1
    }
    return false
}

type KeyLike = Pick<KeyboardEvent, "key" | "code" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey" | "isComposing" | "keyCode"> & {
    getModifierState?: (key: string) => boolean
    target?: EventTarget | null
}

/// Keys that produce text (and therefore belong to the composition field in text mode).
export function isTextProducingKey(event: KeyLike): boolean {
    if (event.isComposing || event.keyCode === 229) return true
    const key = event.key
    if (key === "Process" || key === "Dead") return true
    if (event.metaKey) return false
    const altGraph = typeof event.getModifierState === "function" && event.getModifierState("AltGraph")
    if ((event.ctrlKey || event.altKey) && !altGraph) return false
    return typeof key === "string" && Array.from(key).length === 1
}

export type KeyRoute =
    { kind: "shortcut", action: ShortcutAction } |
    { kind: "browser" } |
    { kind: "text" } |
    { kind: "remote" } |
    { kind: "ignore" }

export type KeyRouteContext = {
    mode: InputMode
    bindings: ReadonlyArray<ShortcutBinding>
    modalOpen: boolean
    compositionTarget: unknown
}

export function routeKeyDown(event: KeyLike, context: KeyRouteContext): KeyRoute {
    if (context.modalOpen) {
        return { kind: "ignore" }
    }
    const target = event.target ?? null
    // The legacy on-screen textarea always owns native text, including in game mode.
    const nativeText = context.mode === "text" || (context.compositionTarget != null && target === context.compositionTarget)
    if (target !== context.compositionTarget) {
        let node = target as { tagName?: string, parentElement?: unknown } | null
        const seen = new Set<unknown>()
        while (node && !seen.has(node)) {
            seen.add(node)
            if (isEditableTarget(node) || ["BUTTON", "INPUT", "SELECT"].indexOf(String(node.tagName ?? "").toUpperCase()) !== -1) return { kind: "ignore" }
            node = node.parentElement as typeof node
        }
    }

    // An active composition (including AltGr/dead keys) must never invoke a local
    // action merely because the physical key happens to match a configured shortcut.
    if (event.isComposing || event.keyCode === 229 || event.key === "Process" || event.key === "Dead" || event.key === "AltGraph" ||
        (typeof event.getModifierState === "function" && event.getModifierState("AltGraph"))) {
        return { kind: nativeText ? "text" : "remote" }
    }

    // Browser-owned navigation/tab/window/devtools combinations are never stolen.
    if (event.code === "F5" || event.code === "F11" || event.code === "F12" ||
        (event.metaKey && ["KeyL", "KeyR", "KeyW", "KeyT", "KeyN", "KeyQ"].indexOf(event.code) !== -1) ||
        (event.ctrlKey && !event.altKey && ["KeyL", "KeyR", "KeyW", "KeyT", "KeyN", "KeyJ", "KeyU", "Tab", "PageUp", "PageDown", "F4"].indexOf(event.code) !== -1) ||
        (event.ctrlKey && event.shiftKey && !event.altKey && ["KeyI", "KeyJ", "KeyC"].indexOf(event.code) !== -1) ||
        (event.altKey && !event.ctrlKey && ["ArrowLeft", "ArrowRight", "Home", "F4"].indexOf(event.code) !== -1)) {
        return { kind: "browser" }
    }

    const action = matchShortcut(event, context.bindings)
    if (action) {
        return { kind: "shortcut", action }
    }

    // Existing behaviour: Ctrl+Shift+V pastes locally (paste event), F11 toggles fullscreen.
    if (event.shiftKey && event.ctrlKey && event.code == "KeyV") {
        return { kind: "browser" }
    }
    if (event.code == "F11") {
        return { kind: "browser" }
    }

    if (nativeText && isTextProducingKey(event)) {
        return { kind: "text" }
    }
    return { kind: "remote" }
}
