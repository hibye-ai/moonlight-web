import { Component, ComponentEvent } from "../index.js";
import { Api, apiGetAppImage, apiHostCancel } from "../../api.js";
import { App } from "../../api_bindings.js";
import { getCurrentLanguage, getTranslations } from "../../i18n.js";
import { setContextMenu } from "../context_menu.js";
import { showMessage } from "../modal/index.js";
import { buildUrl } from "../../config_.js";
import { icon } from "../icons.js";
import { makeIconButton } from "../shell.js";
import { isPlaceholderArt, placeholderIcon } from "./art.js";

export type GameCache = App & { activeApp: number | null }

export type GameEventListener = (event: ComponentEvent<Game>) => void

export function startStream(hostId: number, appId: number) {
    const query = new URLSearchParams({
        hostId: `${hostId}`,
        appId: `${appId}`,
    })

    if (window.matchMedia('(display-mode: standalone)').matches) {
        // If we're in a pwa: open in the current tab
        // If we don't do this we might get a url bar at the top
        window.location.href = buildUrl(`/stream.html?${query}`)
    } else {
        window.open(buildUrl(`/stream.html?${query}`), "_blank")
    }
}

export async function stopStream(api: Api, hostId: number): Promise<boolean> {
    const i = getTranslations(getCurrentLanguage()).game
    const response = await apiHostCancel(api, { host_id: hostId })
    if (!response.success) {
        await showMessage(i.failedToCloseApp)
    }
    return response.success
}

export class Game implements Component {
    private api: Api

    private hostId: number
    private appId: number

    private mounted: number = 0
    private divElement: HTMLDivElement = document.createElement("div")

    private imageBlob: Blob | null = null
    private imageBlobUrl: string | null = null
    private imageElement: HTMLImageElement = document.createElement("img")
    private posterElement: HTMLDivElement = document.createElement("div")
    private fallbackElement: HTMLDivElement = document.createElement("div")
    private titleElement: HTMLParagraphElement = document.createElement("p")
    private tagsElement: HTMLDivElement = document.createElement("div")

    private cache: GameCache

    constructor(api: Api, hostId: number, appId: number, cache: GameCache) {
        this.api = api

        this.hostId = hostId
        this.appId = appId

        this.cache = cache

        const ui = getTranslations(getCurrentLanguage()).ui

        this.divElement.classList.add("game-tile")
        this.divElement.tabIndex = 0
        this.divElement.setAttribute("role", "button")

        this.posterElement.classList.add("game-poster")
        this.fallbackElement.classList.add("game-poster-fallback")
        this.imageElement.classList.add("game-art")
        this.imageElement.alt = ""
        this.imageElement.decoding = "async"
        this.imageElement.hidden = true
        this.tagsElement.classList.add("game-tags")

        const playOverlay = document.createElement("div")
        playOverlay.classList.add("game-play")
        playOverlay.appendChild(icon("play"))

        const moreButton = makeIconButton("more", ui.more)
        moreButton.classList.add("tile-more")
        moreButton.addEventListener("click", event => {
            event.stopPropagation()
            this.onContextMenu(event)
        })

        this.posterElement.append(this.fallbackElement, this.imageElement, this.tagsElement, playOverlay, moreButton)

        this.titleElement.classList.add("game-title")

        this.divElement.append(this.posterElement, this.titleElement)

        this.divElement.addEventListener("click", this.onClick.bind(this))
        this.divElement.addEventListener("contextmenu", this.onContextMenu.bind(this))
        this.divElement.addEventListener("keydown", event => {
            if (event.key == "Enter" || event.key == " ") {
                event.preventDefault()
                this.divElement.click()
            }
        })

        this.forceLoadImage(false)

        this.updateCache(cache)
    }

    async forceLoadImage(forceServerRefresh: boolean) {
        const blob = await apiGetAppImage(this.api, {
            host_id: this.hostId,
            app_id: this.appId,
            force_refresh: forceServerRefresh
        })

        // Sunshine's gray stock covers get a themed poster instead
        if (await isPlaceholderArt(blob)) {
            this.posterElement.classList.add("is-placeholder")
            return
        }
        this.posterElement.classList.remove("is-placeholder")

        this.imageBlob = blob
        this.updateImage()
    }
    private updateImage() {
        // generate and set url
        if (this.imageBlob && !this.imageBlobUrl && this.mounted > 0) {
            this.imageBlobUrl = URL.createObjectURL(this.imageBlob)

            this.imageElement.onload = () => {
                this.imageElement.hidden = false
                this.posterElement.classList.add("has-art")
            }
            this.imageElement.src = this.imageBlobUrl
        }

        // revoke url
        if (this.imageBlobUrl && this.mounted <= 0) {
            URL.revokeObjectURL(this.imageBlobUrl)
            this.imageBlobUrl = null

            this.posterElement.classList.remove("has-art")
            this.imageElement.hidden = true
            this.imageElement.removeAttribute("src")
        }
    }

    getImageBlob(): Blob | null {
        return this.imageBlob
    }

    updateCache(cache: GameCache) {
        this.cache = cache
        this.titleElement.innerText = this.cache.title
        this.divElement.title = this.cache.title
        const placeholder = placeholderIcon(this.cache.title)
        if (placeholder) {
            this.fallbackElement.replaceChildren(icon(placeholder, "game-poster-glyph"))
        } else {
            this.fallbackElement.innerText = monogram(this.cache.title)
        }

        this.tagsElement.replaceChildren()
        if (this.isActive()) {
            const live = document.createElement("span")
            live.classList.add("tag", "tag-live")
            live.innerText = "LIVE"
            this.tagsElement.appendChild(live)
        }
        if (this.cache.is_hdr_supported) {
            const hdr = document.createElement("span")
            hdr.classList.add("tag")
            hdr.innerText = "HDR"
            this.tagsElement.appendChild(hdr)
        }

        this.divElement.classList.toggle("is-active", this.isActive())
        this.divElement.classList.toggle("is-dimmed", !this.isActive() && this.cache.activeApp != null)
    }

    private async onClick(event: MouseEvent) {
        const i = getTranslations(getCurrentLanguage()).game
        if (this.cache.activeApp != null) {
            const elements = []

            if (this.isActive()) {
                elements.push({
                    name: i.resumeSession,
                    classes: ["context-menu-element-resume"],
                    callback: async () => {
                        startStream(this.hostId, this.appId)

                        const event = new ComponentEvent("ml-gamereload", this)
                        this.divElement.dispatchEvent(event)
                    }
                })
            }

            elements.push({
                name: i.stopCurrentSession,
                classes: ["context-menu-element-stop"],
                callback: async () => {
                    await stopStream(this.api, this.hostId)

                    const event = new ComponentEvent("ml-gamereload", this)
                    this.divElement.dispatchEvent(event)
                }
            })

            setContextMenu(event, {
                elements
            })
        } else {
            startStream(this.hostId, this.appId)

            await new Promise(r => window.setTimeout(r, 6000))

            const event = new ComponentEvent("ml-gamereload", this)
            this.divElement.dispatchEvent(event)
        }
    }

    private onContextMenu(event: MouseEvent) {
        const i = getTranslations(getCurrentLanguage()).game
        const elements = []

        elements.push({
            name: i.open,
            callback: async () => {
                startStream(this.hostId, this.appId)

                const event = new ComponentEvent("ml-gamereload", this)
                this.divElement.dispatchEvent(event)
            }
        })

        elements.push({
            name: i.settingsForApp,
            callback: () => {
                this.divElement.dispatchEvent(new ComponentEvent("ml-gamesettings", this))
            }
        })

        elements.push({
            name: i.showDetails,
            callback: this.showDetails.bind(this),
        })

        setContextMenu(event, {
            elements
        })
    }

    private async showDetails() {
        const app = this.cache
        const i = getTranslations(getCurrentLanguage()).game
        await showMessage(i.details(app))
    }

    private isActive(): boolean {
        return this.cache.activeApp == this.appId
    }

    addForceReloadListener(listener: GameEventListener) {
        this.divElement.addEventListener("ml-gamereload", listener as any)
    }
    removeForceReloadListener(listener: GameEventListener) {
        this.divElement.removeEventListener("ml-gamereload", listener as any)
    }
    addSettingsListener(listener: GameEventListener) {
        this.divElement.addEventListener("ml-gamesettings", listener as any)
    }
    removeSettingsListener(listener: GameEventListener) {
        this.divElement.removeEventListener("ml-gamesettings", listener as any)
    }

    getTitle(): string {
        return this.cache.title
    }
    getCache(): GameCache {
        return this.cache
    }

    getHostId(): number {
        return this.hostId
    }
    getAppId(): number {
        return this.appId
    }

    mount(parent: HTMLElement): void {
        this.mounted++
        this.updateImage()

        parent.appendChild(this.divElement)
    }
    unmount(parent: HTMLElement): void {

        parent.removeChild(this.divElement)

        this.mounted--
        this.updateImage()
    }
}

function monogram(title: string): string {
    const words = title.trim().split(/\s+/).filter(word => word.length > 0)
    if (words.length == 0) {
        return "?"
    }
    if (words.length == 1) {
        return Array.from(words[0]).slice(0, 2).join("").toUpperCase()
    }
    return (Array.from(words[0])[0] + Array.from(words[1])[0]).toUpperCase()
}

export function hashHue(text: string): number {
    let hash = 0
    for (const char of text) {
        hash = (hash * 31 + char.codePointAt(0)!) >>> 0
    }
    return hash % 360
}
