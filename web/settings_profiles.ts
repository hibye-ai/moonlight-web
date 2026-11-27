import { Settings, getLocalStreamSettings, setLocalStreamSettings } from "./component/settings_menu.js"
import { StreamStatsData } from "./stream/stats.js"
import { classifyQualitySample, isNetworkProfile, NetworkProfile, thresholdsFor } from "./desktop/quality.js"

const PROFILE_STORAGE_KEY = "mlSettingsProfiles"
// v2 adds `networkProfiles` and per-profile adaptive records ("<host>@lan"/"<host>@remote").
// v1 data is read as-is; the legacy "<host>" adaptive record is kept and still used when no
// network profile is chosen.
const PROFILE_VERSION = 2
const ADAPTIVE_RECORD_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000

export type SettingsProfileScope = {
    hostId?: number | null
    appId?: number | null
}

type AdaptiveHostRecord = {
    scale: number
    goodSamples: number
    updatedAt: number
}

type SettingsProfileStore = {
    version: number
    hosts: Record<string, Settings>
    apps: Record<string, Settings>
    adaptiveHosts: Record<string, AdaptiveHostRecord>
    networkProfiles: Record<string, NetworkProfile>
}

function emptyStore(): SettingsProfileStore {
    return { version: PROFILE_VERSION, hosts: {}, apps: {}, adaptiveHosts: {}, networkProfiles: {} }
}

function loadStore(): SettingsProfileStore {
    try {
        const raw = localStorage.getItem(PROFILE_STORAGE_KEY)
        if (!raw) return emptyStore()
        const parsed = JSON.parse(raw)
        const networkProfiles: Record<string, NetworkProfile> = {}
        if (parsed?.networkProfiles && typeof parsed.networkProfiles === "object") {
            for (const [hostId, profile] of Object.entries(parsed.networkProfiles)) {
                if (isNetworkProfile(profile)) networkProfiles[hostId] = profile
            }
        }
        return {
            version: PROFILE_VERSION,
            hosts: parsed?.hosts && typeof parsed.hosts === "object" ? parsed.hosts : {},
            apps: parsed?.apps && typeof parsed.apps === "object" ? parsed.apps : {},
            adaptiveHosts: parsed?.adaptiveHosts && typeof parsed.adaptiveHosts === "object" ? parsed.adaptiveHosts : {},
            networkProfiles,
        }
    } catch {
        localStorage.removeItem(PROFILE_STORAGE_KEY)
        return emptyStore()
    }
}

function saveStore(store: SettingsProfileStore) {
    localStorage.setItem(PROFILE_STORAGE_KEY, JSON.stringify(store))
}

function deepMerge(target: any, source: any): any {
    if (!source || typeof source !== "object") return target
    for (const key in source) {
        const sourceValue = source[key]
        if (sourceValue && typeof sourceValue === "object" && !Array.isArray(sourceValue)) {
            const targetValue = target[key]
            target[key] = deepMerge(targetValue && typeof targetValue === "object" ? targetValue : {}, sourceValue)
        } else if (sourceValue !== undefined) {
            target[key] = sourceValue
        }
    }
    return target
}

function cloneSettings(settings: Settings): Settings {
    if (typeof structuredClone === "function") return structuredClone(settings)
    return JSON.parse(JSON.stringify(settings))
}

function appKey(hostId: number, appId: number): string {
    return `${hostId}:${appId}`
}

export function getScopedStreamSettings(defaultSettings: Settings, scope: SettingsProfileScope = {}): Settings {
    const settings = getLocalStreamSettings(defaultSettings)
    const store = loadStore()
    if (scope.hostId != null && store.hosts[String(scope.hostId)]) {
        deepMerge(settings, store.hosts[String(scope.hostId)])
    }
    if (scope.hostId != null && scope.appId != null) {
        const app = store.apps[appKey(scope.hostId, scope.appId)]
        if (app) deepMerge(settings, app)
    }
    return settings
}

export function setScopedStreamSettings(settings: Settings, scope: SettingsProfileScope = {}) {
    if (scope.hostId == null) {
        setLocalStreamSettings(settings)
        return
    }

    const store = loadStore()
    if (scope.appId != null) {
        store.apps[appKey(scope.hostId, scope.appId)] = cloneSettings(settings)
    } else {
        store.hosts[String(scope.hostId)] = cloneSettings(settings)
    }
    saveStore(store)
}

export function getBrowserProfileBackup(): unknown {
    return {
        globalSettings: localStorage.getItem("mlSettings"),
        profiles: localStorage.getItem(PROFILE_STORAGE_KEY),
    }
}

export function restoreBrowserProfileBackup(value: any) {
    if (!value || typeof value !== "object") return
    if (typeof value.globalSettings === "string") localStorage.setItem("mlSettings", value.globalSettings)
    if (typeof value.profiles === "string") localStorage.setItem(PROFILE_STORAGE_KEY, value.profiles)
}

/// The user's network profile choice for a host, or null when none was chosen. Never
/// inferred from addresses.
export function getNetworkProfile(hostId: number): NetworkProfile | null {
    return loadStore().networkProfiles[String(hostId)] ?? null
}

export function setNetworkProfile(hostId: number, profile: NetworkProfile | null) {
    const store = loadStore()
    if (profile == null) {
        delete store.networkProfiles[String(hostId)]
    } else if (isNetworkProfile(profile)) {
        store.networkProfiles[String(hostId)] = profile
    }
    saveStore(store)
}

function adaptiveKey(hostId: number, profile: NetworkProfile | null | undefined): string {
    return profile ? `${hostId}@${profile}` : String(hostId)
}

function isFresh(record: AdaptiveHostRecord | undefined): record is AdaptiveHostRecord {
    return !!record && typeof record === "object" && Date.now() - record.updatedAt <= ADAPTIVE_RECORD_MAX_AGE_MS
}

function activeAdaptiveRecord(store: SettingsProfileStore, hostId: number, profile?: NetworkProfile | null): AdaptiveHostRecord {
    const key = adaptiveKey(hostId, profile)
    const record = store.adaptiveHosts[key]
    if (!isFresh(record)) {
        // A new per-profile record starts from what the host learned before profiles
        // existed (copied, the legacy record itself stays untouched).
        const legacy = profile ? store.adaptiveHosts[String(hostId)] : undefined
        const fresh = isFresh(legacy)
            ? { scale: Number(legacy.scale) || 1, goodSamples: 0, updatedAt: Date.now() }
            : { scale: 1, goodSamples: 0, updatedAt: Date.now() }
        store.adaptiveHosts[key] = fresh
        return fresh
    }
    record.scale = Math.min(1, Math.max(0.35, Number(record.scale) || 1))
    record.goodSamples = Math.max(0, Number(record.goodSamples) || 0)
    return record
}

export function applyAdaptiveBitrateRecommendation(settings: Settings, hostId: number, profile?: NetworkProfile | null): Settings {
    const effective = cloneSettings(settings)
    if (!effective.adaptiveBitrate) return effective

    const store = loadStore()
    const record = activeAdaptiveRecord(store, hostId, profile)
    if (profile) {
        // Persist the seeded per-profile record (and the v2 layout) right away.
        saveStore(store)
    }
    const configured = Math.max(1, effective.bitrate)
    const recommended = Math.max(Math.min(configured, 1500), Math.floor((configured * record.scale) / 500) * 500)
    effective.bitrate = Math.min(configured, Math.max(500, recommended))
    return effective
}

function numberStat(value: unknown): number | null {
    return typeof value === "number" && Number.isFinite(value) ? value : null
}

/// Next-start adaptive bitrate: learns a per-host (and, if chosen, per-network-profile)
/// scale that is applied when the NEXT stream starts. It never changes a running stream.
export class AdaptiveBitrateAdvisor {
    private hostId: number
    private profile: NetworkProfile | null
    private lastPacketsReceived: number | null = null
    private lastPacketsLost: number | null = null
    private lastFramesDropped: number | null = null

    constructor(hostId: number, profile: NetworkProfile | null = null) {
        this.hostId = hostId
        this.profile = profile
    }

    sample(stats: StreamStatsData) {
        const received = numberStat(stats.transport.webrtcPacketsReceived)
        const lost = numberStat(stats.transport.webrtcPacketsLost)
        const dropped = numberStat(stats.transport.webrtcFramesDropped)
        const rtt = numberStat(stats.transport.webrtcRttMs) ?? stats.browserRtt ?? stats.streamerRttMs

        let lossRate: number | null = null
        let droppedDelta = 0
        if (received != null && lost != null && this.lastPacketsReceived != null && this.lastPacketsLost != null) {
            const receivedDelta = Math.max(0, received - this.lastPacketsReceived)
            const lostDelta = Math.max(0, lost - this.lastPacketsLost)
            if (receivedDelta + lostDelta >= 30) lossRate = lostDelta / (receivedDelta + lostDelta)
        }
        if (dropped != null && this.lastFramesDropped != null) {
            droppedDelta = Math.max(0, dropped - this.lastFramesDropped)
        }
        this.lastPacketsReceived = received
        this.lastPacketsLost = lost
        this.lastFramesDropped = dropped

        if (rtt == null && lossRate == null && dropped == null) return

        // Without a chosen profile these are the historical thresholds (250/140/80 ms,
        // 10/4/1 % loss, 10/3 dropped frames); see QUALITY_THRESHOLDS.default.
        const quality = classifyQualitySample({ rtt, lossRate, droppedDelta }, thresholdsFor(this.profile))
        const severe = quality === "severe"
        const bad = severe || quality === "bad"
        const good = quality === "good"

        const store = loadStore()
        const record = activeAdaptiveRecord(store, this.hostId, this.profile)
        if (bad) {
            record.scale = Math.max(0.35, record.scale * (severe ? 0.65 : 0.82))
            record.goodSamples = 0
        } else if (good) {
            record.goodSamples += 1
            if (record.goodSamples >= 3) {
                record.scale = Math.min(1, record.scale + 0.10)
                record.goodSamples = 0
            }
        } else {
            record.goodSamples = 0
        }
        record.updatedAt = Date.now()
        saveStore(store)
    }
}
