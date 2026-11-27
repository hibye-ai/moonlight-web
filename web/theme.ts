export type ThemeMode = "system" | "dark" | "light"

export function normalizeTheme(value: unknown): ThemeMode {
    if (value === "light" || value === "system") return value
    return "dark"
}

export function resolveTheme(mode: ThemeMode): "dark" | "light" {
    if (mode === "system") {
        return window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark"
    }
    return mode
}

export function applyTheme(mode: ThemeMode): void {
    const normalized = normalizeTheme(mode)
    document.documentElement.dataset.themeMode = normalized
    const resolved = resolveTheme(normalized)
    document.documentElement.dataset.theme = resolved

    // Keep the mobile browser chrome in step with the page background
    const meta = document.querySelector('meta[name="theme-color"]')
    meta?.setAttribute("content", resolved === "light" ? "#f4f2ea" : "#0b0b0c")
}

export function getStoredTheme(): ThemeMode {
    try {
        const raw = localStorage.getItem("mlSettings")
        const parsed = raw ? JSON.parse(raw) : null
        return normalizeTheme(parsed?.theme)
    } catch {
        return "dark"
    }
}

export function toggleStoredTheme(): ThemeMode {
    const next: ThemeMode = resolveTheme(getStoredTheme()) === "dark" ? "light" : "dark"
    try {
        const raw = localStorage.getItem("mlSettings")
        const parsed = raw ? JSON.parse(raw) : {}
        parsed.theme = next
        localStorage.setItem("mlSettings", JSON.stringify(parsed))
    } catch {
        localStorage.setItem("mlSettings", JSON.stringify({ theme: next }))
    }
    applyTheme(next)
    return next
}

export function watchSystemTheme(): void {
    if (!window.matchMedia) return
    const media = window.matchMedia("(prefers-color-scheme: light)")
    media.addEventListener?.("change", () => {
        if (getStoredTheme() === "system") applyTheme("system")
    })
}
