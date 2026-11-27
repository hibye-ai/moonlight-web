import { applyTheme, getStoredTheme, watchSystemTheme } from "./theme.js";
import { AppShell } from "./component/shell.js";
import "./polyfill/index.js"
import { Api, apiDeleteAllSessions, apiDeleteSession, apiGetRole, apiGetServerBackup, apiGetSessions, apiGetUser, apiLogout, apiPostRole, apiPostUser, apiRestoreServerBackup, FetchError, getApi } from "./api.js";
import { Component, ComponentEvent } from "./component/index.js";
import { showNotification } from "./component/notification.js";
import { setTouchContextMenuEnabled } from "./polyfill/ios_right_click.js";
import { UserList } from "./component/user/list.js";
import { AddUserModal } from "./component/user/add_modal.js";
import { showMessage, showModal } from "./component/modal/index.js";
import { buildUrl } from "./config_.js";
import { DetailedUserPage } from "./component/user/detailed_page.js";
import { User, UserEventListener } from "./component/user/index.js";
import { DetailedRole, DetailedUser, SessionInfo } from "./api_bindings.js";
import { Role, RoleEventListener } from "./component/roles/index.js";
import { RoleList } from "./component/roles/list.js";
import { DetailedRolePage } from "./component/roles/detailed_page.js";
import { AddRoleModal } from "./component/roles/add_modal.js";
import { adoptRoleDefaultLanguage, getCurrentLanguage, getTranslations } from "./i18n.js";
import { getBrowserProfileBackup, restoreBrowserProfileBackup } from "./settings_profiles.js";

let I = getTranslations(getCurrentLanguage())

async function startApp() {
    applyTheme(getStoredTheme())
    watchSystemTheme()
    setTouchContextMenuEnabled(true)

    const api = await getApi()

    const bootstrapRole = await apiGetRole(api, { id: null })
    adoptRoleDefaultLanguage(bootstrapRole.role.default_settings)
    I = getTranslations(getCurrentLanguage())

    const currentUser = await checkPermissions(api)
    if (!currentUser) {
        return
    }

    const rootElement = document.getElementById("root")
    if (rootElement == null) {
        showNotification(I.admin.rootNotFound, "error")
        return;
    }

    const app = new AdminApp(api, currentUser)
    app.mount(rootElement)

    app.forceFetch()

    // -- App states
    let lastAppState: AppState | null = null
    if (sessionStorage) {
        const lastStateText = sessionStorage.getItem("mlAdminState")
        if (lastStateText) {
            lastAppState = JSON.parse(lastStateText)
        }
    }

    window.addEventListener("popstate", event => {
        if (event.state) {
            app.setAppState(event.state, false)
        }
    })

    if (lastAppState) {
        app.setAppState(lastAppState)
    } else {
        // set default state
        app.setAppState({ tab: "users", user_id: null })
    }
}

async function checkPermissions(api: Api): Promise<DetailedUser | null> {
    const user = await apiGetUser(api)

    if (user.role != "Admin") {
        await showMessage(I.admin.unauthorized)
        window.location.href = buildUrl("/")
        return null
    }

    return user
}

type AppState = { tab: "users", user_id: number | null } |
{ tab: "roles", role_id: number | null } |
{ tab: "sessions" } |
{ tab: "backup" }
function pushAppState(state: AppState, pushHistory: boolean) {
    if (pushHistory) {
        history.pushState(state, "")
    }

    if (sessionStorage) {
        sessionStorage.setItem("mlAdminState", JSON.stringify(state))
    }
}
function backAppState() {
    history.back()
}

startApp()

class AdminApp implements Component {

    private api: Api

    private shell: AppShell
    private content: HTMLElement

    private currentState: AppState | null = null

    // The actual content of the tabs
    private users: UserPanel | null = null
    private roles: RolePanel | null = null
    private sessions: SessionPanel | null = null
    private backup: BackupPanel | null = null

    constructor(api: Api, currentUser: DetailedUser) {
        this.api = api

        this.shell = new AppShell(I.ui.admin)
        this.content = this.shell.content
        this.content.classList.add("admin-panel-content")

        this.shell.addNavItem("users", "users", I.admin.users, () => this.setAppState({ tab: "users", user_id: null }))
        this.shell.addNavItem("roles", "key", I.admin.roles, () => this.setAppState({ tab: "roles", role_id: null }))
        this.shell.addNavItem("sessions", "clock", I.admin.sessions, () => this.setAppState({ tab: "sessions" }))
        this.shell.addNavItem("backup", "database", I.admin.backup, () => this.setAppState({ tab: "backup" }))

        this.shell.addFooterItem("library", I.ui.backToLibrary, () => {
            window.location.href = buildUrl("/")
        })
        // A default user is authenticated by server configuration rather than
        // a revocable browser session, so showing a logout action is misleading.
        if (!currentUser.is_default_user) {
            this.shell.addFooterItem("logout", I.ui.logout, async () => {
                await apiLogout(this.api)
                window.location.href = buildUrl("/")
            })
        }
    }

    async forceFetch() {
        if (this.currentState?.tab == "users") {
            await this.users?.forceFetch()
        } else if (this.currentState?.tab == "roles") {
            await this.roles?.forceFetch()
        } else if (this.currentState?.tab == "sessions") {
            await this.sessions?.forceFetch()
        }
    }

    setAppState(state: AppState, pushIntoHistory?: boolean) {
        // check if tab changed and mount accordingly
        if (this.currentState?.tab != state.tab) {
            // Unmount old tab
            if (this.currentState?.tab == "users") {
                this.users?.unmount(this.content)
            } else if (this.currentState?.tab == "roles") {
                this.roles?.unmount(this.content)
            } else if (this.currentState?.tab == "sessions") {
                this.sessions?.unmount(this.content)
            } else if (this.currentState?.tab == "backup") {
                this.backup?.unmount(this.content)
            }

            // Mount and create (if necessary) new tab
            if (state.tab == "users") {
                if (!this.users) {
                    this.users = new UserPanel(this.api)
                    this.users.addUserChangedListener(event => {
                        pushAppState({ tab: "users", user_id: event.component.getUserId() }, true)
                    })
                }

                this.users.mount(this.content)
            } else if (state.tab == "roles") {
                if (!this.roles) {
                    this.roles = new RolePanel(this.api)
                    this.roles.addRoleChangedListener(event => {
                        pushAppState({ tab: "roles", role_id: event.component.getRoleId() }, true)
                    })
                }

                this.roles.mount(this.content)
            } else if (state.tab == "sessions") {
                if (!this.sessions) {
                    this.sessions = new SessionPanel(this.api)
                }
                this.sessions.mount(this.content)
                void this.sessions.forceFetch()
            } else if (state.tab == "backup") {
                if (!this.backup) {
                    this.backup = new BackupPanel(this.api)
                }
                this.backup.mount(this.content)
            }
        }

        // Save app state to browser history
        pushAppState(state, pushIntoHistory ?? true)

        const titles = { users: I.admin.users, roles: I.admin.roles, sessions: I.admin.sessions, backup: I.admin.backup }
        this.shell.setHeader({ eyebrow: I.ui.admin, title: titles[state.tab] })
        this.shell.setActiveNav(state.tab)
        this.shell.setView(`admin-${state.tab}`)

        // Change the content (e.g. user / role) of the tab
        this.currentState = state
        if (state.tab == "users" && state.user_id != null) {
            this.users?.setUserId(state.user_id)
        } else if (state.tab == "roles" && state.role_id != null) {
            this.roles?.setRoleId(state.role_id)
        }

        // Force fetch self to update data
        this.forceFetch()
    }

    mount(parent: HTMLElement): void {
        this.shell.mount(parent)
    }
    unmount(parent: HTMLElement): void {
        this.shell.unmount(parent)
    }
}

class UserPanel implements Component {

    private api: Api

    private rootDiv = document.createElement("div")

    private userPanel = document.createElement("div")
    private addUserButton = document.createElement("button")
    private userSearch = document.createElement("input")
    private userList: UserList

    private userInfoPage: DetailedUserPage | null = null

    constructor(api: Api) {
        this.api = api

        this.rootDiv.classList.add("admin-panel-users")

        // Select User Panel
        this.userPanel.classList.add("user-panel")
        this.rootDiv.appendChild(this.userPanel)

        const userPanelToolbar = document.createElement("div")
        userPanelToolbar.classList.add("user-panel-toolbar")
        const userPanelTitle = document.createElement("div")
        userPanelTitle.classList.add("user-panel-title")
        userPanelTitle.innerText = I.admin.users
        userPanelToolbar.appendChild(userPanelTitle)

        this.addUserButton.classList.add("user-add-button")
        this.addUserButton.innerText = I.admin.addUser
        this.addUserButton.addEventListener("click", async () => {
            const addUserModal = new AddUserModal(api)

            const userRequest = await showModal(addUserModal)

            if (userRequest) {
                try {
                    const newUser = await apiPostUser(this.api, userRequest)

                    this.userList.insertList(newUser.id, newUser)
                } catch (e) {
                    // 409 = Conflict
                    if (e instanceof FetchError && e.getResponse()?.status == 409) {
                        // Name already exists
                        await showMessage(I.admin.userExists(userRequest.name))
                    } else {
                        throw e
                    }
                }
            }
        })
        userPanelToolbar.appendChild(this.addUserButton)
        this.userPanel.appendChild(userPanelToolbar)

        const searchWrap = document.createElement("div")
        searchWrap.classList.add("user-search-wrap")
        this.userSearch.classList.add("user-search")
        this.userSearch.placeholder = I.admin.searchUser
        this.userSearch.type = "search"
        this.userSearch.addEventListener("input", this.onUserSearchChange.bind(this))
        searchWrap.appendChild(this.userSearch)
        this.userPanel.appendChild(searchWrap)

        this.userList = new UserList(api)
        this.userList.addUserClickedListener(this.onUserClicked.bind(this))
        this.userList.addUserDeletedListener(this.onUserDeleted.bind(this))
        this.userList.mount(this.userPanel)
    }

    addUserChangedListener(listener: UserEventListener) {
        this.userList.addUserClickedListener(listener)
    }

    getCurrentUserId(): number | null {
        return this.userInfoPage?.getUserId() ?? null
    }

    async forceFetch() {
        await this.userList.forceFetch()
    }

    private onUserSearchChange() {
        this.userList.setFilter(this.userSearch.value)
    }

    private async onUserClicked(event: ComponentEvent<User>) {
        await this.setUserId(event.component.getUserId())
    }
    async setUserId(userId: number) {
        const user = await apiGetUser(this.api, {
            user_id: userId,
            name: null
        })

        this.setUserInfo(user)
    }
    private setUserInfo(user: DetailedUser | null) {
        if (this.userInfoPage) {
            this.userInfoPage.unmount(this.rootDiv)
            this.userInfoPage.removeDeletedListener(this.onUserDeleted.bind(this))
        }

        this.userInfoPage = null
        if (user) {
            this.userInfoPage = new DetailedUserPage(this.api, user)
            this.userInfoPage.addDeletedListener(this.onUserDeleted.bind(this))
            this.userInfoPage.mount(this.rootDiv)
        }
    }

    private onUserDeleted(event: ComponentEvent<User>) {
        if (this.userInfoPage?.getUserId() == event.component.getUserId()) {
            this.setUserInfo(null)
        }
        this.userList.removeUser(event.component.getUserId())
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.rootDiv)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.rootDiv)
    }
}

class RolePanel implements Component {

    private api: Api

    private rootDiv = document.createElement("div")

    private rolePanel = document.createElement("div")
    private addRoleButton = document.createElement("button")
    private roleSearch = document.createElement("input")
    private roleList: RoleList

    private roleInfoPage: DetailedRolePage | null = null

    constructor(api: Api) {
        this.api = api

        this.rootDiv.classList.add("admin-panel-roles")

        // Select Role Panel
        this.rolePanel.classList.add("role-panel")
        this.rootDiv.appendChild(this.rolePanel)

        const rolePanelToolbar = document.createElement("div")
        rolePanelToolbar.classList.add("role-panel-toolbar")
        const rolePanelTitle = document.createElement("div")
        rolePanelTitle.classList.add("role-panel-title")
        rolePanelTitle.innerText = I.admin.roles
        rolePanelToolbar.appendChild(rolePanelTitle)

        this.addRoleButton.classList.add("role-add-button")
        this.addRoleButton.innerText = I.admin.addRole
        this.addRoleButton.addEventListener("click", async () => {
            const addRoleModal = new AddRoleModal()

            const roleRequest = await showModal(addRoleModal)

            if (roleRequest) {
                try {
                    const newRole = await apiPostRole(this.api, roleRequest)

                    this.roleList.insertList(newRole.role.id, newRole.role)
                } catch (e) {
                    // 409 = Conflict
                    if (e instanceof FetchError && e.getResponse()?.status == 409) {
                        // Name already exists
                        await showMessage(I.admin.roleExists(roleRequest.name))
                    } else {
                        throw e
                    }
                }
            }
        })
        rolePanelToolbar.appendChild(this.addRoleButton)
        this.rolePanel.appendChild(rolePanelToolbar)

        const roleSearchWrap = document.createElement("div")
        roleSearchWrap.classList.add("role-search-wrap")
        this.roleSearch.classList.add("role-search")
        this.roleSearch.placeholder = I.admin.searchRole
        this.roleSearch.type = "search"
        this.roleSearch.addEventListener("input", this.onRoleSearchChange.bind(this))
        roleSearchWrap.appendChild(this.roleSearch)
        this.rolePanel.appendChild(roleSearchWrap)

        this.roleList = new RoleList(api)
        this.roleList.addRoleClickedListener(this.onRoleClicked.bind(this))
        this.roleList.addRoleDeletedListener(this.onRoleDeleted.bind(this))
        this.roleList.mount(this.rolePanel)
    }

    addRoleChangedListener(listener: RoleEventListener) {
        this.roleList.addRoleClickedListener(listener)
    }

    getCurrentRoleId(): number | null {
        return this.roleInfoPage?.getRoleId() ?? null
    }

    async forceFetch() {
        await this.roleList.forceFetch()
    }

    private onRoleSearchChange() {
        this.roleList.setFilter(this.roleSearch.value)
    }

    private async onRoleClicked(event: ComponentEvent<Role>) {
        await this.setRoleId(event.component.getRoleId())
    }
    async setRoleId(roleId: number) {
        const role = await apiGetRole(this.api, {
            id: roleId,
        })

        this.setRoleInfo(role.role)
    }
    private setRoleInfo(role: DetailedRole | null) {
        if (this.roleInfoPage) {
            this.roleInfoPage.unmount(this.rootDiv)
            this.roleInfoPage.removeDeletedListener(this.onRoleDeleted.bind(this))
        }

        this.roleInfoPage = null
        if (role) {
            this.roleInfoPage = new DetailedRolePage(this.api, role)
            this.roleInfoPage.addDeletedListener(this.onRoleDeleted.bind(this))
            this.roleInfoPage.mount(this.rootDiv)
        }
    }

    private onRoleDeleted(event: ComponentEvent<Role>) {
        if (this.roleInfoPage?.getRoleId() == event.component.getRoleId()) {
            this.setRoleInfo(null)
        }
        this.roleList.removeRole(event.component.getRoleId())
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.rootDiv)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.rootDiv)
    }
}


function formatSessionDuration(seconds: number | bigint): string {
    const safe = Math.max(0, Math.floor(Number(seconds)))
    if (safe < 60) return `${safe}s`
    if (safe < 3600) return `${Math.floor(safe / 60)}m`
    if (safe < 86400) return `${Math.floor(safe / 3600)}h ${Math.floor((safe % 3600) / 60)}m`
    return `${Math.floor(safe / 86400)}d ${Math.floor((safe % 86400) / 3600)}h`
}

class SessionPanel implements Component {
    private api: Api
    private root = document.createElement("section")
    private list = document.createElement("div")
    private refreshButton = document.createElement("button")
    private logoutAllButton = document.createElement("button")

    constructor(api: Api) {
        this.api = api
        this.root.classList.add("session-panel")

        const header = document.createElement("div")
        header.classList.add("admin-tool-header")
        const title = document.createElement("h2")
        title.innerText = I.admin.sessions
        header.appendChild(title)
        const actions = document.createElement("div")
        actions.classList.add("admin-tool-actions")

        this.refreshButton.type = "button"
        this.refreshButton.innerText = I.admin.refreshSessions
        this.refreshButton.addEventListener("click", () => void this.forceFetch())
        actions.appendChild(this.refreshButton)

        this.logoutAllButton.type = "button"
        this.logoutAllButton.classList.add("danger-button")
        this.logoutAllButton.innerText = I.admin.logoutAllSessions
        this.logoutAllButton.addEventListener("click", () => void this.logoutAll())
        actions.appendChild(this.logoutAllButton)
        header.appendChild(actions)
        this.root.appendChild(header)

        this.list.classList.add("session-list")
        this.root.appendChild(this.list)
    }

    async forceFetch() {
        this.refreshButton.disabled = true
        try {
            const response = await apiGetSessions(this.api)
            this.render(response.sessions)
        } finally {
            this.refreshButton.disabled = false
        }
    }

    private render(sessions: SessionInfo[]) {
        this.list.replaceChildren()
        if (sessions.length === 0) {
            const empty = document.createElement("p")
            empty.classList.add("admin-tool-empty")
            empty.innerText = I.admin.sessionEmpty
            this.list.appendChild(empty)
            this.logoutAllButton.disabled = true
            return
        }
        this.logoutAllButton.disabled = false

        for (const session of sessions) {
            const row = document.createElement("div")
            row.classList.add("session-card")
            const copy = document.createElement("div")
            copy.classList.add("session-card-copy")
            const name = document.createElement("strong")
            name.innerText = session.user_name
            copy.appendChild(name)
            const meta = document.createElement("span")
            meta.innerText = `ID ${session.user_id} · ${I.admin.sessionAge} ${formatSessionDuration(session.age_seconds)} · ${I.admin.sessionExpires} ${formatSessionDuration(session.expires_in_seconds)}`
            copy.appendChild(meta)
            row.appendChild(copy)

            if (session.current) {
                const badge = document.createElement("span")
                badge.classList.add("current-session-badge")
                badge.innerText = I.admin.currentSession
                row.appendChild(badge)
            }

            const revoke = document.createElement("button")
            revoke.type = "button"
            revoke.classList.add("session-revoke-button")
            revoke.innerText = I.admin.revokeSession
            revoke.addEventListener("click", async () => {
                revoke.disabled = true
                try {
                    await apiDeleteSession(this.api, { id: session.id })
                    if (session.current) {
                        window.location.href = buildUrl("/")
                    } else {
                        await this.forceFetch()
                    }
                } finally {
                    revoke.disabled = false
                }
            })
            row.appendChild(revoke)
            this.list.appendChild(row)
        }
    }

    private async logoutAll() {
        if (!window.confirm(I.admin.sessionLogoutAllConfirm)) return
        this.logoutAllButton.disabled = true
        try {
            await apiDeleteAllSessions(this.api)
            window.location.href = buildUrl("/")
        } finally {
            this.logoutAllButton.disabled = false
        }
    }

    mount(parent: HTMLElement): void { parent.appendChild(this.root) }
    unmount(parent: HTMLElement): void { parent.removeChild(this.root) }
}

class BackupPanel implements Component {
    private api: Api
    private root = document.createElement("section")
    private fileInput = document.createElement("input")

    constructor(api: Api) {
        this.api = api
        this.root.classList.add("backup-panel")

        const header = document.createElement("div")
        header.classList.add("admin-tool-header")
        const title = document.createElement("h2")
        title.innerText = I.admin.backup
        header.appendChild(title)
        this.root.appendChild(header)

        const warning = document.createElement("p")
        warning.classList.add("backup-warning")
        warning.innerText = I.admin.backupWarning
        this.root.appendChild(warning)

        const actions = document.createElement("div")
        actions.classList.add("backup-actions")
        const exportButton = document.createElement("button")
        exportButton.type = "button"
        exportButton.innerText = I.admin.backupExport
        exportButton.addEventListener("click", () => void this.exportBackup(exportButton))
        actions.appendChild(exportButton)

        const importButton = document.createElement("button")
        importButton.type = "button"
        importButton.classList.add("danger-button")
        importButton.innerText = I.admin.backupImport
        importButton.addEventListener("click", () => this.fileInput.click())
        actions.appendChild(importButton)
        this.root.appendChild(actions)

        this.fileInput.type = "file"
        this.fileInput.accept = "application/json,.json"
        this.fileInput.hidden = true
        this.fileInput.addEventListener("change", () => void this.importBackup())
        this.root.appendChild(this.fileInput)
    }

    private async exportBackup(button: HTMLButtonElement) {
        button.disabled = true
        try {
            const server = await apiGetServerBackup(this.api)
            const bundle = {
                format: "moonlight-web-backup",
                version: 1,
                createdAt: new Date().toISOString(),
                server,
                browser: getBrowserProfileBackup(),
            }
            const blob = new Blob([JSON.stringify(bundle, null, 2)], { type: "application/json" })
            const url = URL.createObjectURL(blob)
            const link = document.createElement("a")
            link.href = url
            link.download = `moonlight-web-backup-${new Date().toISOString().replace(/[:.]/g, "-")}.json`
            document.body.appendChild(link)
            link.click()
            link.remove()
            window.setTimeout(() => URL.revokeObjectURL(url), 0)
        } finally {
            button.disabled = false
        }
    }

    private async importBackup() {
        const file = this.fileInput.files?.[0]
        this.fileInput.value = ""
        if (!file) return

        let bundle: any
        try {
            bundle = JSON.parse(await file.text())
        } catch {
            await showMessage(I.admin.backupInvalid)
            return
        }
        if (bundle?.format !== "moonlight-web-backup" || bundle?.version !== 1 || !bundle?.server) {
            await showMessage(I.admin.backupInvalid)
            return
        }
        if (!window.confirm(I.admin.backupRestoreConfirm)) return

        try {
            await apiRestoreServerBackup(this.api, bundle.server)
            restoreBrowserProfileBackup(bundle.browser)
            window.location.href = buildUrl("/")
        } catch {
            await showMessage(I.admin.backupRestoreFailed)
        }
    }

    mount(parent: HTMLElement): void { parent.appendChild(this.root) }
    unmount(parent: HTMLElement): void { parent.removeChild(this.root) }
}
