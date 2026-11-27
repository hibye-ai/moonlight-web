import { Api, apiGetAppImage, apiGetApps } from "../../api.js";
import { App } from "../../api_bindings.js";
import { FetchListComponent } from "../fetch_list.js";
import { ComponentEvent } from "../index.js";
import { Game, GameCache, GameEventListener, hashHue, startStream, stopStream } from "./index.js";
import { getCurrentLanguage, getTranslations } from "../../i18n.js";
import { icon } from "../icons.js";
import { makeActionButton } from "../shell.js";
import { isPlaceholderArt } from "./art.js";

export class GameList extends FetchListComponent<App, Game> {
    private api: Api

    private eventTarget = new EventTarget()

    private hostId: number
    private hostName: string | null = null
    private activeApp: number | null = null

    private root = document.createElement("div")

    // Hero
    private hero = document.createElement("section")
    private heroBackdrop = document.createElement("div")
    private heroPoster = document.createElement("div")
    private heroEyebrow = document.createElement("p")
    private heroTitle = document.createElement("h2")
    private heroMeta = document.createElement("p")
    private heroActions = document.createElement("div")
    private resumeButton: HTMLButtonElement
    private stopButton: HTMLButtonElement
    private heroArtAppId: number | null = null
    private heroArtUrl: string | null = null

    // Toolbar
    private toolbar = document.createElement("div")
    private search = document.createElement("input")
    private emptyElement = document.createElement("p")

    constructor(api: Api, hostId: number, cache: App[] | null) {
        super({
            listClasses: ["game-grid"],
            elementLiClasses: ["animated-list-element", "game-element"]
        })

        this.api = api

        this.hostId = hostId

        const i = getTranslations(getCurrentLanguage())

        this.root.classList.add("games-view")

        // Hero
        this.hero.classList.add("hero")
        this.heroBackdrop.classList.add("hero-backdrop")
        this.heroPoster.classList.add("hero-poster")
        const heroCopy = document.createElement("div")
        heroCopy.classList.add("hero-copy")
        this.heroEyebrow.classList.add("hero-eyebrow")
        this.heroTitle.classList.add("hero-title")
        this.heroMeta.classList.add("hero-meta")
        this.heroActions.classList.add("hero-actions")

        this.resumeButton = makeActionButton("play", i.game.resumeSession, "primary")
        this.resumeButton.addEventListener("click", () => {
            if (this.activeApp != null) {
                startStream(this.hostId, this.activeApp)
            }
        })
        this.stopButton = makeActionButton("stop", i.game.stopCurrentSession, "ghost")
        this.stopButton.addEventListener("click", async () => {
            this.stopButton.disabled = true
            try {
                await stopStream(this.api, this.hostId)
            } finally {
                this.stopButton.disabled = false
            }
            this.eventTarget.dispatchEvent(new Event("ml-gamereload"))
        })
        this.heroActions.append(this.resumeButton, this.stopButton)

        heroCopy.append(this.heroEyebrow, this.heroTitle, this.heroMeta, this.heroActions)
        this.hero.append(this.heroBackdrop, heroCopy, this.heroPoster)
        this.root.appendChild(this.hero)

        // Toolbar
        this.toolbar.classList.add("games-toolbar")
        const searchWrap = document.createElement("label")
        searchWrap.classList.add("search-field")
        searchWrap.appendChild(icon("search"))
        this.search.type = "search"
        this.search.placeholder = i.ui.searchGames
        this.search.setAttribute("aria-label", i.ui.searchGames)
        this.search.addEventListener("input", this.applyFilter.bind(this))
        searchWrap.appendChild(this.search)
        this.toolbar.appendChild(searchWrap)
        this.root.appendChild(this.toolbar)

        this.list.mount(this.root)

        this.emptyElement.classList.add("empty-inline")
        this.emptyElement.innerText = i.ui.noResults
        this.emptyElement.hidden = true
        this.root.appendChild(this.emptyElement)

        this.renderHero()

        // Update cache
        if (cache != null) {
            this.updateCache(cache)
        } else {
            this.forceFetch()
        }
    }

    setHostName(name: string | null) {
        this.hostName = name
        this.renderHero()
    }

    setActiveGame(appId: number | null) {
        this.activeApp = appId

        this.forceFetch()
    }

    async forceFetch() {
        const apps = await apiGetApps(this.api, {
            host_id: this.hostId,
        })

        this.updateCache(apps)
    }

    updateCache(cache: App[]): void {
        super.updateCache(cache)
        this.applyFilter()
        this.renderHero()
    }

    private applyFilter() {
        const query = this.search.value.trim().toLocaleLowerCase()
        if (query.length == 0) {
            this.list.setFilter()
        } else {
            this.list.setFilter(game => game.getTitle().toLocaleLowerCase().includes(query))
        }
        const visible = this.list.get().filter(game => query.length == 0 || game.getTitle().toLocaleLowerCase().includes(query))
        this.emptyElement.hidden = visible.length > 0 || this.list.get().length == 0
    }

    private renderHero() {
        const i = getTranslations(getCurrentLanguage())
        const games = this.list.get()
        const active = this.activeApp != null ? games.find(game => game.getAppId() == this.activeApp) : undefined

        this.hero.classList.toggle("is-live", active != null)
        this.resumeButton.hidden = active == null
        this.stopButton.hidden = active == null
        this.heroActions.hidden = active == null

        if (active) {
            this.heroEyebrow.innerText = i.ui.nowPlaying
            this.heroTitle.innerText = active.getTitle()
            this.heroMeta.innerText = this.hostName ?? ""
        } else {
            this.heroEyebrow.innerText = i.ui.readyToPlay
            this.heroTitle.innerText = this.hostName ?? ""
            this.heroMeta.innerText = games.length > 0 ? i.ui.appCount(games.length) : ""
        }
        this.heroMeta.hidden = this.heroMeta.innerText.length == 0

        this.hero.style.setProperty("--hue", `${hashHue(active?.getTitle() ?? this.hostName ?? "")}`)
        this.setHeroArt(active?.getAppId() ?? null)
    }

    private async setHeroArt(appId: number | null) {
        if (this.heroArtAppId == appId) {
            return
        }
        this.heroArtAppId = appId

        if (this.heroArtUrl) {
            URL.revokeObjectURL(this.heroArtUrl)
            this.heroArtUrl = null
        }
        this.hero.style.removeProperty("--art")
        this.hero.classList.remove("has-art")

        if (appId == null) {
            return
        }

        const blob = await apiGetAppImage(this.api, { host_id: this.hostId, app_id: appId, force_refresh: false })
        if (this.heroArtAppId != appId || await isPlaceholderArt(blob)) {
            return
        }
        this.heroArtUrl = URL.createObjectURL(blob)
        this.hero.style.setProperty("--art", `url("${this.heroArtUrl}")`)
        this.hero.classList.add("has-art")
    }

    private createCache(data: App): GameCache {
        const cache = data as GameCache
        cache.activeApp = this.activeApp
        return cache
    }

    protected updateComponentData(component: Game, data: App): void {
        const cache = this.createCache(data)

        component.updateCache(cache)
    }
    protected getComponentDataId(component: Game): number {
        return component.getAppId()
    }
    protected getDataId(data: App): number {
        return data.app_id
    }
    protected insertList(dataId: number, data: App): void {
        const cache = this.createCache(data)

        const game = new Game(this.api, this.hostId, dataId, cache)
        game.addForceReloadListener(this.onForceReload.bind(this))
        game.addSettingsListener(this.onSettings.bind(this))

        this.list.append(game)
    }

    private onForceReload(event: ComponentEvent<Game>) {
        this.eventTarget.dispatchEvent(new ComponentEvent("ml-gamereload", event.component))
    }
    private onSettings(event: ComponentEvent<Game>) {
        this.eventTarget.dispatchEvent(new ComponentEvent("ml-gamesettings", event.component))
    }

    addForceReloadListener(listener: GameEventListener) {
        this.eventTarget.addEventListener("ml-gamereload", listener as any)
    }
    removeForceReloadListener(listener: GameEventListener) {
        this.eventTarget.removeEventListener("ml-gamereload", listener as any)
    }
    addSettingsListener(listener: GameEventListener) {
        this.eventTarget.addEventListener("ml-gamesettings", listener as any)
    }
    removeSettingsListener(listener: GameEventListener) {
        this.eventTarget.removeEventListener("ml-gamesettings", listener as any)
    }

    getHostId(): number {
        return this.hostId
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.root)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.root)
    }
}
