import { DetailedHost, DetailedUser, UndetailedHost } from "../../api_bindings.js"
import { Api, apiDeleteHost, apiGetHost, isDetailedHost, apiPostPair, apiWakeUp, apiGetUser, apiPatchHost } from "../../api.js"
import { Component, ComponentEvent } from "../index.js"
import { getCurrentLanguage, getTranslations } from "../../i18n.js"
import { setContextMenu } from "../context_menu.js"
import { showNotification } from "../notification.js"
import { showMessage } from "../modal/index.js"
import { icon } from "../icons.js"
import { makeIconButton } from "../shell.js"

export type HostEventListener = (event: ComponentEvent<Host>) => void

export class Host implements Component {
    private api: Api

    private hostId: number
    private userCache: DetailedUser | null = null
    private cache: UndetailedHost | DetailedHost | null = null

    private divElement: HTMLDivElement = document.createElement("div")

    private cardElement: HTMLDivElement = document.createElement("div")
    private visualElement: HTMLDivElement = document.createElement("div")
    private badgeElement: HTMLSpanElement = document.createElement("span")
    private nameElement: HTMLElement = document.createElement("h3")
    private statusElement: HTMLSpanElement = document.createElement("span")
    private ownerElement: HTMLSpanElement = document.createElement("span")
    private moreButton: HTMLButtonElement

    constructor(api: Api, hostId: number, host: UndetailedHost | DetailedHost | null) {
        this.api = api

        this.hostId = hostId
        this.cache = host

        const ui = getTranslations(getCurrentLanguage()).ui

        this.cardElement.classList.add("host-tile", "is-loading")
        this.cardElement.tabIndex = 0
        this.cardElement.setAttribute("role", "button")

        this.visualElement.classList.add("host-visual")
        this.visualElement.appendChild(icon("monitor", "host-glyph"))
        this.badgeElement.classList.add("host-badge")
        this.visualElement.appendChild(this.badgeElement)

        const top = document.createElement("div")
        top.classList.add("host-tile-top")
        this.statusElement.classList.add("status-chip")
        this.moreButton = makeIconButton("more", ui.more)
        this.moreButton.classList.add("tile-more")
        this.moreButton.addEventListener("click", event => {
            event.stopPropagation()
            this.onContextMenu(event)
        })
        top.append(this.statusElement, this.moreButton)

        const copy = document.createElement("div")
        copy.classList.add("host-tile-copy")
        this.nameElement.classList.add("host-name")
        this.ownerElement.classList.add("host-owner")
        copy.append(this.nameElement, this.ownerElement)

        this.cardElement.append(top, this.visualElement, copy)
        this.divElement.appendChild(this.cardElement)

        this.cardElement.addEventListener("keydown", event => {
            if (event.key == "Enter" || event.key == " ") {
                event.preventDefault()
                this.cardElement.click()
            }
        })

        this.divElement.addEventListener("click", this.onClick.bind(this))
        this.divElement.addEventListener("contextmenu", this.onContextMenu.bind(this))

        // Update cache
        if (host != null) {
            this.updateCache(host, null)

            apiGetUser(api).then((user) => this.userCache = user)
        } else {
            this.forceFetch()
        }
    }

    async forceFetch() {
        const [newCache, user] = await Promise.all([
            apiGetHost(this.api, {
                host_id: this.hostId,
            }),
            apiGetUser(this.api)
        ])

        this.updateCache(newCache, user)
    }
    async getCurrentGame(): Promise<number | null> {
        await this.forceFetch()

        if (this.cache && isDetailedHost(this.cache) && this.cache.current_game != 0) {
            return this.cache.current_game
        } else {
            return null
        }
    }

    private async onClick(event: MouseEvent) {
        if (this.cache?.server_state == null) {
            this.onContextMenu(event)
        } else if (this.cache?.paired == "Paired") {
            this.divElement.dispatchEvent(new ComponentEvent("ml-hostopen", this))
        } else {
            await this.pair()
        }
    }

    private onContextMenu(event: MouseEvent) {
        const i = getTranslations(getCurrentLanguage()).host
        const elements = []

        if (this.cache?.server_state != null) {
            elements.push({
                name: i.showDetails,
                callback: this.showDetails.bind(this),
            })

            elements.push({
                name: i.open,
                callback: this.onClick.bind(this)
            })
        } else if (this.cache?.paired == "Paired") {
            elements.push({
                name: i.sendWakeUpPacket,
                callback: this.wakeUp.bind(this)
            })
        }

        elements.push({
            name: i.reload,
            callback: async () => this.forceFetch()
        })

        if (this.cache?.server_state != null && this.cache?.paired == "NotPaired") {
            elements.push({
                name: i.pair,
                callback: this.pair.bind(this)
            })
        }

        // Make private / global
        if (this.userCache?.role == "Admin") {
            if (this.cache?.owner == "Global") {
                elements.push({
                    name: i.makePrivate,
                    callback: this.makePrivate.bind(this),
                    classes: ["context-menu-element-red"]
                })
            } else if (this.cache?.owner == "ThisUser") {
                elements.push({
                    name: i.makeGlobal,
                    callback: this.makeGlobal.bind(this),
                    classes: ["context-menu-element-red"]
                })
            }
        }

        if (this.cache?.owner == "ThisUser" || this.userCache?.role == "Admin") {
            elements.push({
                name: i.removeHost,
                callback: this.remove.bind(this)
            })
        }

        setContextMenu(event, {
            elements
        })
    }

    private async showDetails() {
        const i = getTranslations(getCurrentLanguage()).host
        let host = this.cache;
        if (!host || !isDetailedHost(host)) {
            host = await apiGetHost(this.api, {
                host_id: this.hostId,
            })
        }
        if (!host || !isDetailedHost(host)) {
            showNotification(i.failedToGetDetails(this.hostId))
            return;
        }
        this.updateCache(host, this.userCache)

        await showMessage(i.details(host))
    }

    addHostRemoveListener(listener: HostEventListener, options?: EventListenerOptions) {
        this.divElement.addEventListener("ml-hostremove", listener as any, options)
    }
    removeHostRemoveListener(listener: HostEventListener, options?: EventListenerOptions) {
        this.divElement.removeEventListener("ml-hostremove", listener as any, options)
    }

    addHostOpenListener(listener: HostEventListener, options?: EventListenerOptions) {
        this.divElement.addEventListener("ml-hostopen", listener as any, options)
    }
    removeHostOpenListener(listener: HostEventListener, options?: EventListenerOptions) {
        this.divElement.removeEventListener("ml-hostopen", listener as any, options)
    }

    private async makeGlobal() {
        await apiPatchHost(this.api, {
            host_id: this.hostId,
            change_owner: true,
            owner: null,
        })

        if (this.cache) {
            this.cache.owner = "Global"
        }
    }
    private async makePrivate() {
        const user = this.userCache ?? await apiGetUser(this.api)

        await apiPatchHost(this.api, {
            host_id: this.hostId,
            change_owner: true,
            owner: user.id,
        })

        if (this.cache) {
            this.cache.owner = "ThisUser"
        }
    }

    private async remove() {
        await apiDeleteHost(this.api, {
            host_id: this.getHostId()
        })

        this.divElement.dispatchEvent(new ComponentEvent("ml-hostremove", this))
    }
    private async wakeUp() {
        const i = getTranslations(getCurrentLanguage()).host
        await apiWakeUp(this.api, {
            host_id: this.getHostId()
        })

        await showMessage(i.wakeUpSent)
    }
    private async pair() {
        const i = getTranslations(getCurrentLanguage()).host
        if (this.cache?.paired == "Paired") {
            await this.forceFetch()

            if (this.cache?.paired == "Paired") {
                showMessage(i.alreadyPaired)
                return;
            }
        }

        const responseStream = await apiPostPair(this.api, {
            host_id: this.getHostId()
        })

        if (typeof responseStream.response == "string") {
            throw `failed to pair (stage 1): ${responseStream.response}`
        }

        const messageAbort = new AbortController()
        showMessage(i.pairPrompt(this.getCache()?.name ?? "", responseStream.response.Pin), { signal: messageAbort.signal })

        const resultResponse = await responseStream.next()

        if (!resultResponse) {
            // Stream ended without a stage-2 result (host not ready / gave up).
            // Keep the PIN modal open so the user can still enter the PIN on the host.
            return
        }

        if (typeof resultResponse == "string") {
            if (resultResponse == "PairError") {
                // Host rejected the attempt (most common: a stale pairing session is
                // still open on the host). Keep the PIN modal up with a hint.
                showMessage(i.pairRetry(responseStream.response?.Pin ?? ""))
                return
            }
            messageAbort.abort()
            throw `failed to pair (stage 2): ${resultResponse}`
        }

        messageAbort.abort()
        this.updateCache(resultResponse.Paired, null)
    }

    getHostId(): number {
        return this.hostId
    }

    /// Shown while the host list is still waiting for this host's live status.
    setLoading(loading: boolean) {
        const ui = getTranslations(getCurrentLanguage()).ui
        if (loading && this.cache?.server_state == null) {
            this.cardElement.classList.remove("is-offline")
            this.cardElement.classList.add("is-loading")
            this.badgeElement.replaceChildren()
            this.statusElement.innerText = ui.checking
        } else if (!loading && this.cardElement.classList.contains("is-loading") && this.cache) {
            this.updateCache(this.cache, null)
        }
    }

    getCache(): DetailedHost | UndetailedHost | null {
        return this.cache
    }

    updateCache(host: UndetailedHost | DetailedHost, user: DetailedUser | null) {
        const i = getTranslations(getCurrentLanguage()).host
        if (this.getHostId() != host.host_id) {
            showNotification(i.overwriteMismatch(this.getHostId(), host.host_id))
            return
        }

        if (this.cache == null) {
            this.cache = host
        } else {
            // if server_state == null it means this host is offline
            // -> updating cache means setting it to offline
            if (this.cache.server_state != null) {
                Object.assign(this.cache, host)
            } else {
                this.cache = host
            }
        }

        if (user) {
            this.userCache = user
        }

        // Update Elements
        this.nameElement.innerText = this.cache.name

        this.cardElement.classList.remove("is-online", "is-offline", "is-locked", "is-busy", "is-loading")
        this.badgeElement.replaceChildren()
        if (this.cache.server_state == null) {
            this.badgeElement.appendChild(icon("offline"))
            this.statusElement.innerText = i.offline
            this.cardElement.classList.add("is-offline")
        } else if (this.cache.paired != "Paired") {
            this.badgeElement.appendChild(icon("lock"))
            this.statusElement.innerText = i.pairRequired
            this.cardElement.classList.add("is-locked")
        } else {
            this.statusElement.innerText = i.online
            this.cardElement.classList.add("is-online")
            if (this.cache.server_state == "Busy") {
                this.statusElement.innerText = getTranslations(getCurrentLanguage()).ui.busy
                this.cardElement.classList.add("is-busy")
            }
        }
        this.ownerElement.innerText = this.cache.owner == "Global" ? i.sharedHost : i.privateHost
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.divElement)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.divElement)
    }
}
