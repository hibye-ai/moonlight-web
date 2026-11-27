import { en, type Translations } from "./locales/en.js"
import { zhCN } from "./locales/zh-CN.js"
import { ptBR } from "./locales/pt-BR.js"
import { frFr } from "./locales/fr-FR.js"
import { koKR } from "./locales/ko-KR.js"

export type Language = "en" | "zh-CN" | "pt-BR" | "fr-FR" | "ko-KR"

// Translations is defined in locales/en.ts (the canonical locale).
// Adding a new locale requires: (1) create web/locales/<code>.ts implementing
// Translations, (2) add it to the Language union and the locales map below.
export type { Translations }

const locales: Record<Language, Translations> = {
    "en": en,
    "zh-CN": zhCN,
    "pt-BR": ptBR,
    "fr-FR": frFr,
    "ko-KR": koKR,
}

export function getTranslations(language: Language): Translations {
    return locales[language]
}

const ALIASES: Record<string, Language> = {
    "zh": "zh-CN",
    "zh-CN": "zh-CN",
    "zh_CN": "zh-CN",
    "pt": "pt-BR",
    "pt-BR": "pt-BR",
    "pt_BR": "pt-BR",
    "ko": "ko-KR",
    "ko-KR": "ko-KR",
    "ko_KR": "ko-KR",
    "en": "en",
    "en-US": "en",
    "en_US": "en",
    "en-GB": "en",
    "en_GB": "en",
    "fr": "fr-FR",
    "fr-FR": "fr-FR",
    "fr_FR": "fr-FR",
}

// Korean is the local default. That is a fallback for missing/unsupported values only:
// a supported Language the user picked (en, fr-FR, ...) must be preserved as-is,
// otherwise every reload silently reverts the choice to Korean.
export function normalizeLanguage(language: unknown): Language {
    if (typeof language != "string") {
        return "ko-KR"
    }

    const alias = ALIASES[language]
    if (alias) {
        return alias
    }

    // Accept any regional variant of a supported base language, e.g. "fr-CA" or "zh-TW".
    const base = language.split(/[-_]/)[0].toLowerCase()
    const byBase: Record<string, Language> = {
        "zh": "zh-CN",
        "pt": "pt-BR",
        "ko": "ko-KR",
        "en": "en",
        "fr": "fr-FR",
    }

    return byBase[base] ?? "ko-KR"
}

function getStoredSettings(): Record<string, unknown> | null {
    try {
        const raw = localStorage.getItem("mlSettings")
        return raw ? JSON.parse(raw) : null
    } catch {
        return null
    }
}

export function getCurrentLanguage(): Language {
    return normalizeLanguage(getStoredSettings()?.language)
}

export function hasStoredLanguage(): boolean {
    return getStoredSettings()?.language != null
}

export function adoptRoleDefaultLanguage(_roleDefaultSettings: { language?: unknown } | null | undefined): boolean {
    if (hasStoredLanguage()) {
        return false
    }

    // This local fork is Korean-first. Persist the preference on first visit;
    // users can still select another language in Settings afterwards.
    const preferredLanguage: Language = "ko-KR"
    try {
        const settings = getStoredSettings() ?? {}
        settings.language = preferredLanguage
        localStorage.setItem("mlSettings", JSON.stringify(settings))
        return true
    } catch {
        localStorage.setItem("mlSettings", JSON.stringify({ language: preferredLanguage }))
        return true
    }
}

export function getLanguageOptions(): Array<{ value: Language, name: string }> {
    return [
        { value: "en", name: "English" },
        { value: "zh-CN", name: "中文" },
        { value: "pt-BR", name: "Português (Brasil)" },
        { value: "fr-FR", name: "Français" },
        { value: "ko-KR", name: "한국어" },
    ]
}
