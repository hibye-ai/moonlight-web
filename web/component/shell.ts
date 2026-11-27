import { Component } from "./index.js"
import { icon, IconName, setButtonContent } from "./icons.js"
import { getCurrentLanguage, getTranslations } from "../i18n.js"
import { toggleStoredTheme } from "../theme.js"

export type ShellHeader = {
    eyebrow?: string
    title: string
    subtitle?: string
}

/// The page frame shared by the library and the admin console:
/// a navigation rail (bottom bar on phones) and a main column with a page header.
export class AppShell implements Component {
    private root = document.createElement("div")

    private rail = document.createElement("nav")
    private brand = document.createElement("div")
    private navList = document.createElement("div")
    private railFooter = document.createElement("div")
    private themeButton = document.createElement("button")

    private main = document.createElement("main")
    private header = document.createElement("header")
    private headerCopy = document.createElement("div")
    private eyebrow = document.createElement("p")
    private title = document.createElement("h1")
    private subtitle = document.createElement("p")
    readonly actions = document.createElement("div")
    readonly content = document.createElement("div")

    private navItems = new Map<string, HTMLButtonElement>()

    constructor(brandSubtitle: string) {
        const i = getTranslations(getCurrentLanguage()).ui

        this.root.classList.add("shell")

        // Rail
        this.rail.classList.add("rail")
        this.brand.classList.add("rail-brand")
        this.brand.innerHTML = `<span class="brand-glyph" aria-hidden="true"></span><span class="brand-text"><strong>Moonlight</strong><small></small></span>`
        const small = this.brand.querySelector("small")
        if (small) small.innerText = brandSubtitle
        this.rail.appendChild(this.brand)

        this.navList.classList.add("rail-nav")
        this.rail.appendChild(this.navList)

        this.railFooter.classList.add("rail-footer")
        this.rail.appendChild(this.railFooter)

        this.themeButton.type = "button"
        this.themeButton.classList.add("rail-item")
        const syncTheme = () => {
            const light = document.documentElement.dataset.theme === "light"
            setButtonContent(this.themeButton, light ? "moon" : "sun", i.toggleTheme)
        }
        syncTheme()
        this.themeButton.addEventListener("click", () => {
            toggleStoredTheme()
            syncTheme()
        })
        this.railFooter.appendChild(this.themeButton)

        // Main column
        this.main.classList.add("shell-main")
        this.header.classList.add("page-header")
        this.headerCopy.classList.add("page-header-copy")
        this.eyebrow.classList.add("page-eyebrow")
        this.title.classList.add("page-title")
        this.subtitle.classList.add("page-subtitle")
        this.headerCopy.append(this.eyebrow, this.title, this.subtitle)
        this.actions.classList.add("page-actions")
        this.header.append(this.headerCopy, this.actions)
        this.content.classList.add("page-content")
        this.main.append(this.header, this.content)

        this.root.append(this.rail, this.main)
    }

    addNavItem(id: string, iconName: IconName, label: string, onClick: () => void): HTMLButtonElement {
        const button = document.createElement("button")
        button.type = "button"
        button.classList.add("rail-item")
        button.dataset.nav = id
        setButtonContent(button, iconName, label)
        button.addEventListener("click", onClick)
        this.navList.appendChild(button)
        this.navItems.set(id, button)
        return button
    }
    setNavVisible(id: string, visible: boolean) {
        const item = this.navItems.get(id)
        if (item) item.hidden = !visible
    }
    setActiveNav(id: string | null) {
        for (const [key, button] of this.navItems) {
            button.classList.toggle("is-active", key === id)
            if (key === id) {
                button.setAttribute("aria-current", "page")
            } else {
                button.removeAttribute("aria-current")
            }
        }
    }

    /// A button pinned to the bottom of the rail (theme, account...). Inserted before the theme toggle.
    addFooterItem(iconName: IconName, label: string, onClick: () => void): HTMLButtonElement {
        const button = document.createElement("button")
        button.type = "button"
        button.classList.add("rail-item")
        setButtonContent(button, iconName, label)
        button.addEventListener("click", onClick)
        this.railFooter.appendChild(button)
        return button
    }

    setHeader(header: ShellHeader) {
        this.eyebrow.innerText = header.eyebrow ?? ""
        this.eyebrow.hidden = !header.eyebrow
        this.title.innerText = header.title
        this.subtitle.innerText = header.subtitle ?? ""
        this.subtitle.hidden = !header.subtitle
        document.title = `${header.title} · Moonlight`
    }
    setView(view: string) {
        this.root.dataset.view = view
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.root)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.root)
    }
}

/// Makes a toolbar button with an icon and a label (label collapses on small screens via CSS).
export function makeActionButton(iconName: IconName, label: string, variant: "primary" | "ghost" | "danger" | "default" = "default"): HTMLButtonElement {
    const button = document.createElement("button")
    button.type = "button"
    button.classList.add("btn")
    if (variant != "default") {
        button.classList.add(`btn-${variant}`)
    }
    setButtonContent(button, iconName, label)
    return button
}

export function makeIconButton(iconName: IconName, label: string): HTMLButtonElement {
    const button = document.createElement("button")
    button.type = "button"
    button.classList.add("icon-btn")
    button.appendChild(icon(iconName))
    button.title = label
    button.setAttribute("aria-label", label)
    return button
}
