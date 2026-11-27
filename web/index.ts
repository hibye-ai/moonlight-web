import "./polyfill/index.js"
import { Api, getApi, apiPostHost, FetchError, apiLogout, apiGetUser, tryLogin, apiGetHost, apiGetRole, apiPatchRole } from "./api.js";
import { AddHostModal } from "./component/host/add_modal.js";
import { HostList } from "./component/host/list.js";
import { Component, ComponentEvent } from "./component/index.js";
import { showNotification } from "./component/notification.js";
import { showMessage, showModal } from "./component/modal/index.js";
import { setContextMenu } from "./component/context_menu.js";
import { GameList } from "./component/game/list.js";
import { Game } from "./component/game/index.js";
import { Host } from "./component/host/index.js";
import { App, DetailedRole, DetailedUser } from "./api_bindings.js";
import { getLocalStreamSettings, globalDefaultSettings, StreamSettingsComponent } from "./component/settings_menu.js";
import { getScopedStreamSettings, setScopedStreamSettings, SettingsProfileScope } from "./settings_profiles.js";
import { adoptRoleDefaultLanguage, getCurrentLanguage, getTranslations } from "./i18n.js";
import { setTouchContextMenuEnabled } from "./polyfill/ios_right_click.js";
import { buildUrl } from "./config_.js";
import { applyTheme, getStoredTheme, watchSystemTheme } from "./theme.js";
import { AppShell, makeActionButton } from "./component/shell.js";
import { icon, setButtonContent } from "./component/icons.js";

let I = getTranslations(getCurrentLanguage())

async function startApp() {
    setTouchContextMenuEnabled(true)
    applyTheme(getStoredTheme())
    watchSystemTheme()

    const api = await getApi()

    const bootstrapRole = await apiGetRole(api, { id: null })
    adoptRoleDefaultLanguage(bootstrapRole.role.default_settings)
    I = getTranslations(getCurrentLanguage())

    const rootElement = document.getElementById("root");
    if (rootElement == null) {
        showNotification(I.index.rootNotFound, "error")
        return;
    }

    let lastAppState: AppState | null = null
    if (sessionStorage) {
        const lastStateText = sessionStorage.getItem("mlState")
        if (lastStateText) {
            lastAppState = JSON.parse(lastStateText)
        }
    }

    const app = new MainApp(api, bootstrapRole.role)
    app.mount(rootElement)

    window.addEventListener("popstate", event => {
        if (event.state) {
            app.setAppState(event.state, false)
        }
    })

    app.forceFetch()

    if (lastAppState) {
        app.setAppState(lastAppState)
    }
}

startApp()

type DisplayStates = "hosts" | "games" | "settings"

type AppState = { display: DisplayStates, hostId?: number, appId?: number, appTitle?: string }
function setAppState(state: AppState, pushHistory: boolean) {
    if (pushHistory) {
        history.pushState(state, "")
    }

    if (sessionStorage) {
        sessionStorage.setItem("mlState", JSON.stringify(state))
    }
}
function backAppState() {
    history.back()
}

class MainApp implements Component {
    private api: Api
    private user: DetailedUser | null = null
    private role: DetailedRole | null = null

    private shell: AppShell

    private libraryNav: HTMLButtonElement
    private settingsNav: HTMLButtonElement
    private adminNav: HTMLButtonElement
    private accountButton: HTMLButtonElement

    // Page actions
    private backButton: HTMLButtonElement
    private hostAddButton: HTMLButtonElement
    private refreshButton: HTMLButtonElement
    private hostSettingsButton: HTMLButtonElement
    private saveRoleDefaultsButton: HTMLButtonElement

    // Different submenus
    private currentDisplay: DisplayStates | null = null

    private hostsView = document.createElement("div")
    private hostsEmpty = document.createElement("div")
    private hostList: HostList
    private gameList: GameList | null = null
    private settings: StreamSettingsComponent | null = null
    private settingsScope: SettingsProfileScope & { appTitle?: string } = {}

    constructor(api: Api, bootstrapRole: DetailedRole) {
        this.api = api
        this.role = bootstrapRole

        this.shell = new AppShell(I.ui.console)

        // Navigation
        this.libraryNav = this.shell.addNavItem("library", "library", I.ui.library, () => this.setCurrentDisplay("hosts"))
        this.settingsNav = this.shell.addNavItem("settings", "settings", I.index.settingsAction, () => this.setCurrentDisplay("settings", null))
        this.adminNav = this.shell.addNavItem("admin", "shield", I.ui.admin, () => {
            window.location.href = buildUrl("/admin.html")
        })
        this.adminNav.hidden = true

        this.accountButton = this.shell.addFooterItem("logout", I.ui.logout, this.onAccountClick.bind(this))

        // Page actions
        this.backButton = makeActionButton("back", I.index.back, "ghost")
        this.backButton.addEventListener("click", backAppState)

        this.hostAddButton = makeActionButton("plus", I.index.addHost, "primary")
        this.hostAddButton.addEventListener("click", this.addHost.bind(this))

        this.refreshButton = makeActionButton("refresh", I.index.reload, "ghost")
        this.refreshButton.addEventListener("click", async () => {
            this.refreshButton.classList.add("is-spinning")
            try {
                await this.forceFetch()
            } finally {
                this.refreshButton.classList.remove("is-spinning")
            }
        })

        this.hostSettingsButton = makeActionButton("settings", I.index.settingsAction, "ghost")
        this.hostSettingsButton.addEventListener("click", () => {
            const hostId = this.gameList?.getHostId() ?? null
            this.setCurrentDisplay("settings", hostId != null ? { hostId } : null)
        })

        this.saveRoleDefaultsButton = makeActionButton("save", I.settings.saveRoleDefaults)
        this.saveRoleDefaultsButton.addEventListener("click", this.onSaveRoleDefaults.bind(this))

        // Host list
        this.hostsView.classList.add("hosts-view")
        this.hostList = new HostList(api)
        this.hostList.addHostOpenListener(this.onHostOpen.bind(this))
        this.hostList.addHostsChangedListener(this.updateHostsEmpty.bind(this))
        this.hostList.mount(this.hostsView)

        this.hostsEmpty.classList.add("empty-state")
        this.hostsEmpty.hidden = true
        this.hostsEmpty.appendChild(icon("monitor", "empty-glyph"))
        const emptyTitle = document.createElement("h2")
        emptyTitle.innerText = I.ui.noHosts
        const emptyHint = document.createElement("p")
        emptyHint.innerText = I.ui.noHostsHint
        this.hostsEmpty.append(emptyTitle, emptyHint)
        this.hostsView.appendChild(this.hostsEmpty)

        // Settings
        this.settings = new StreamSettingsComponent(
            bootstrapRole.permissions,
            getScopedStreamSettings(bootstrapRole.default_settings)
        )
        this.settings.addChangeListener(this.onSettingsChange.bind(this))

        this.setCurrentDisplay("hosts")

        // Context Menu
        document.body.addEventListener("contextmenu", this.onContextMenu.bind(this), { passive: false })
    }

    private async onAccountClick() {
        if (this.user?.is_default_user) {
            const success = await tryLogin()
            if (success) {
                window.location.reload()
            }
        } else {
            await apiLogout(this.api)
            window.location.reload()
        }
    }

    private hostsLoaded = false
    private updateHostsEmpty() {
        this.hostsEmpty.hidden = !this.hostsLoaded || this.hostList.getHosts().length > 0

        // The games view may have been restored before the host list arrived
        if (this.currentDisplay == "games" && this.gameList) {
            const hostName = this.hostList.getHost(this.gameList.getHostId())?.getCache()?.name ?? null
            if (hostName) {
                this.gameList.setHostName(hostName)
                this.shell.setHeader({ eyebrow: hostName, title: I.index.gamesTitle, subtitle: I.index.gamesSubtitle })
            }
        }
    }

    setAppState(state: AppState, pushIntoHistory?: boolean) {
        if (state.display == "hosts") {
            this.setCurrentDisplay("hosts", null, pushIntoHistory)
        } else if (state.display == "games" && state.hostId != null) {
            this.setCurrentDisplay("games", { hostId: state.hostId }, pushIntoHistory)
        } else if (state.display == "settings") {
            this.setCurrentDisplay("settings", { hostId: state.hostId, appId: state.appId, appTitle: state.appTitle }, pushIntoHistory)
        }
    }

    private async addHost() {
        const modal = new AddHostModal()

        let host = await showModal(modal);

        if (host) {
            let newHost
            try {
                newHost = await apiPostHost(this.api, host)
            } catch (e) {
                if (e instanceof FetchError) {
                    const response = e.getResponse()
                    if (response && response.status == 404) {
                        showNotification(I.index.addHostUnreachable(host.address))
                        return
                    }
                }
                throw e
            }

            this.hostList.insertList(newHost.host_id, newHost)
        }
    }

    private onContextMenu(event: MouseEvent) {
        if (this.currentDisplay == "hosts" || this.currentDisplay == "games") {
            const elements = [
                {
                    name: I.index.reload,
                    callback: this.forceFetch.bind(this)
                }
            ]

            setContextMenu(event, {
                elements
            })
        }
    }

    private async onHostOpen(event: ComponentEvent<Host>) {
        const hostId = event.component.getHostId()

        this.setCurrentDisplay("games", { hostId })
    }

    private rebuildSettingsForScope(scope: SettingsProfileScope & { appTitle?: string }) {
        if (!this.role) return
        const wasMounted = this.currentDisplay === "settings" && this.settings != null
        if (wasMounted) this.settings?.unmount(this.shell.content)
        this.settingsScope = scope
        this.settings = new StreamSettingsComponent(
            this.role.permissions,
            getScopedStreamSettings(this.role.default_settings, scope)
        )
        this.settings.addChangeListener(this.onSettingsChange.bind(this))
        if (wasMounted) this.settings.mount(this.shell.content)
    }

    private onGameSettings(event: ComponentEvent<Game>) {
        const game = event.component
        this.setCurrentDisplay("settings", {
            hostId: game.getHostId(),
            appId: game.getAppId(),
            appTitle: game.getTitle(),
        })
    }

    private onSettingsChange() {
        if (!this.settings) {
            showNotification(I.index.saveSettingsFailed)
            return
        }

        const previousLanguage = getLocalStreamSettings(globalDefaultSettings()).language
        const newSettings = this.settings.getStreamSettings()

        // Store settings in the active global / host / app profile.
        setScopedStreamSettings(newSettings, this.settingsScope)

        if (previousLanguage !== newSettings.language) {
            window.location.reload()
        }
    }

    private async onSaveRoleDefaults() {
        if (!this.settings || !this.role || this.user?.role !== "Admin") {
            showNotification(I.settings.saveRoleDefaultsFailed)
            return
        }

        this.saveRoleDefaultsButton.disabled = true

        try {
            const newSettings = this.settings.getStreamSettings()
            await apiPatchRole(this.api, {
                id: this.role.id,
                name: null,
                ty: this.role.ty,
                default_settings: newSettings,
                permissions: null,
            })

            this.role = {
                ...this.role,
                default_settings: newSettings,
            }

            await showMessage(I.settings.saveRoleDefaultsSuccess)
        } catch {
            showNotification(I.settings.saveRoleDefaultsFailed)
        } finally {
            this.saveRoleDefaultsButton.disabled = false
        }
    }

    private setCurrentDisplay(display: "hosts",
        extraInfo?: null,
        pushIntoHistory?: boolean
    ): void
    private setCurrentDisplay(
        display: "games",
        extraInfo?: {
            hostId?: number | null,
            hostCache?: Array<App>
        },
        pushIntoHistory?: boolean
    ): void
    private setCurrentDisplay(display: "settings", extraInfo?: { hostId?: number | null, appId?: number | null, appTitle?: string } | null, pushIntoHistory?: boolean): void

    private setCurrentDisplay(
        display: "hosts" | "games" | "settings",
        extraInfo?: {
            hostId?: number | null,
            hostCache?: Array<App>
            appId?: number | null
            appTitle?: string
        } | null,
        pushIntoHistory_?: boolean
    ) {
        const pushIntoHistory = pushIntoHistory_ === undefined ? true : pushIntoHistory_

        if (display == "games" && extraInfo?.hostId == null) {
            // invalid input state
            throw "invalid display state was requested"
        }

        // Check if we need to change
        if (this.currentDisplay == display) {
            const requestedHostId = extraInfo?.hostId ?? null
            const requestedAppId = extraInfo?.appId ?? null
            const settingsScopeChanged = this.currentDisplay == "settings"
                && (this.settingsScope.hostId ?? null) !== requestedHostId
                || this.currentDisplay == "settings"
                && (this.settingsScope.appId ?? null) !== requestedAppId
            if (this.currentDisplay == "games" && this.gameList?.getHostId() != extraInfo?.hostId) {
                // fall through
            } else if (settingsScopeChanged) {
                // fall through
            } else {
                return
            }
        }

        // Unmount the current display
        const content = this.shell.content
        if (this.currentDisplay == "hosts") {
            this.hostsView.remove()
        } else if (this.currentDisplay == "games") {
            this.gameList?.unmount(content)
        } else if (this.currentDisplay == "settings") {
            this.settings?.unmount(content)
        }
        this.shell.actions.replaceChildren()

        // Mount the new display
        if (display == "hosts") {
            this.shell.setHeader({ eyebrow: I.ui.library, title: I.index.hostsTitle, subtitle: I.index.hostsSubtitle })
            this.shell.actions.append(this.refreshButton, this.hostAddButton)

            content.appendChild(this.hostsView)

            setAppState({ display: "hosts" }, pushIntoHistory)
        } else if (display == "games" && extraInfo?.hostId != null) {
            const hostName = this.hostList.getHost(extraInfo.hostId)?.getCache()?.name ?? null
            this.shell.setHeader({ eyebrow: hostName ?? I.ui.library, title: I.index.gamesTitle, subtitle: I.index.gamesSubtitle })
            this.shell.actions.append(this.backButton, this.refreshButton, this.hostSettingsButton)

            if (this.gameList?.getHostId() != extraInfo?.hostId) {
                this.gameList = new GameList(this.api, extraInfo?.hostId, extraInfo?.hostCache ?? null)
                this.gameList.addForceReloadListener(this.forceFetch.bind(this))
                this.gameList.addSettingsListener(this.onGameSettings.bind(this))
            }
            this.gameList.setHostName(hostName)

            this.gameList.mount(content)

            this.refreshGameListActiveGame()

            setAppState({ display: "games", hostId: this.gameList?.getHostId() }, pushIntoHistory)
        } else if (display == "settings") {
            const scope = { hostId: extraInfo?.hostId ?? null, appId: extraInfo?.appId ?? null, appTitle: extraInfo?.appTitle }
            this.rebuildSettingsForScope(scope)
            let subtitle = I.index.settingsSubtitle
            let eyebrow = I.ui.globalScope
            if (scope.hostId != null && scope.appId != null) {
                subtitle = I.index.appSettingsSubtitle(scope.appTitle ?? `App ${scope.appId}`)
                eyebrow = I.ui.appScope
            } else if (scope.hostId != null) {
                const hostName = this.hostList.getHost(scope.hostId)?.getCache()?.name ?? `PC ${scope.hostId}`
                subtitle = I.index.hostSettingsSubtitle(hostName)
                eyebrow = I.ui.hostScope
            }
            this.shell.setHeader({ eyebrow, title: I.index.settingsTitle, subtitle })
            if (scope.hostId != null) {
                this.shell.actions.appendChild(this.backButton)
            }
            if (this.user?.role == "Admin" && scope.hostId == null) {
                this.shell.actions.appendChild(this.saveRoleDefaultsButton)
            }

            this.settings?.mount(content)

            setAppState({ display: "settings", hostId: scope.hostId ?? undefined, appId: scope.appId ?? undefined, appTitle: scope.appTitle }, pushIntoHistory)
        }

        this.shell.setView(display)
        this.shell.setActiveNav(display == "settings" ? "settings" : "library")
        window.scrollTo({ top: 0 })

        this.currentDisplay = display
    }

    async forceFetch() {
        const promiseUser = this.refreshUserRole()
        const promiseRoles = this.refreshUserPermissions()

        await Promise.all([
            this.hostList.forceFetch(),
            this.gameList?.forceFetch()
        ])
        this.hostsLoaded = true
        this.updateHostsEmpty()

        if (this.currentDisplay == "games"
            && this.gameList
            && !this.hostList.getHost(this.gameList.getHostId())) {
            // The newly fetched list doesn't contain the hosts game view we're in -> go to hosts
            this.setCurrentDisplay("hosts")
        }

        await Promise.all([
            promiseUser,
            promiseRoles,
            this.refreshGameListActiveGame()
        ])
    }
    private async refreshUserRole() {
        this.user = await apiGetUser(this.api)

        if (this.user.is_default_user) {
            setButtonContent(this.accountButton, "login", I.ui.login)
        } else {
            setButtonContent(this.accountButton, "logout", I.ui.logout)
        }

        this.adminNav.hidden = this.user.role != "Admin"

        if (this.user.role == "Admin") {
            if (this.currentDisplay == "settings" && this.settingsScope.hostId == null && !this.shell.actions.contains(this.saveRoleDefaultsButton)) {
                this.shell.actions.appendChild(this.saveRoleDefaultsButton)
            }
        } else {
            this.saveRoleDefaultsButton.remove()
        }
    }
    private async refreshUserPermissions() {
        const response = await apiGetRole(this.api, { id: null })
        this.role = response.role

        if (this.role.permissions.allow_add_hosts) {
            this.hostAddButton.disabled = false
        } else {
            this.hostAddButton.disabled = true
        }
    }
    private async refreshGameListActiveGame() {
        const gameList = this.gameList
        const hostId = gameList?.getHostId()
        if (hostId == null) {
            return
        }

        const host = this.hostList.getHost(hostId)

        let currentGame = null
        if (host != null) {
            currentGame = await host.getCurrentGame()
        } else {
            const host = await apiGetHost(this.api, { host_id: hostId })
            if (host.current_game != 0) {
                currentGame = host.current_game
            }
        }

        if (currentGame != null) {
            gameList?.setActiveGame(currentGame)
        } else {
            gameList?.setActiveGame(null)
        }
    }

    mount(parent: HTMLElement): void {
        this.shell.mount(parent)
    }
    unmount(parent: HTMLElement): void {
        this.shell.unmount(parent)
    }
}
