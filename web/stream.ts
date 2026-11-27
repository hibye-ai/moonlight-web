import { applyTheme, getStoredTheme, watchSystemTheme } from "./theme.js"
import "./polyfill/index.js"
import { apiGetRole, getApi } from "./api.js"
import { showNotification } from "./component/notification.js"
import { getModalBackground, showMessage } from "./component/modal/index.js"
import { getSidebarRoot } from "./component/sidebar/index.js"
import { adoptRoleDefaultLanguage, getCurrentLanguage, getTranslations } from "./i18n.js"
import { ViewerApp, stopPropagationOn } from "./viewer.js"

let I = getTranslations(getCurrentLanguage())

async function startApp() {
    applyTheme(getStoredTheme())
    watchSystemTheme()
    const api = await getApi()

    const bootstrapRole = await apiGetRole(api, { id: null })
    adoptRoleDefaultLanguage(bootstrapRole.role.default_settings)
    I = getTranslations(getCurrentLanguage())

    const rootElement = document.getElementById("root");
    if (rootElement == null) {
        showNotification(I.stream.rootNotFound, "error")
        return;
    }

    // Get Host and App via Query
    const queryParams = new URLSearchParams(location.search)

    const hostIdStr = queryParams.get("hostId")
    const appIdStr = queryParams.get("appId")
    if (hostIdStr == null || appIdStr == null) {
        await showMessage(I.stream.missingHostOrApp)

        window.close()
        return
    }
    const hostId = Number.parseInt(hostIdStr)
    const appId = Number.parseInt(appIdStr)

    // event propagation on overlays
    const sidebarRoot = getSidebarRoot()
    if (sidebarRoot) {
        stopPropagationOn(sidebarRoot)
    }

    const modalBackground = getModalBackground()
    if (modalBackground) {
        stopPropagationOn(modalBackground)
    }

    // Start and Mount App
    const app = new ViewerApp(api, hostId, appId, bootstrapRole.role)
    app.mount(rootElement);

    (window as any)["app"] = app
}

// Prevent starting transition
window.requestAnimationFrame(() => {
    // Note: elements is a live array
    const elements = document.getElementsByClassName("prevent-start-transition")
    while (elements.length > 0) {
        elements.item(0)?.classList.remove("prevent-start-transition")
    }
})

startApp()
