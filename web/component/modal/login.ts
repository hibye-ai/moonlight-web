import { ComponentEvent } from "../index.js"
import { getCurrentLanguage, getTranslations } from "../../i18n.js"
import { InputComponent } from "../input.js"
import { FormModal } from "./form.js"
import { toggleStoredTheme } from "../../theme.js"
import { icon } from "../icons.js"

export type UserAuth = {
    name: string,
    password: string
}

export class ApiUserPasswordPrompt extends FormModal<UserAuth> {

    private brand: HTMLDivElement = document.createElement("div")
    private text: HTMLElement = document.createElement("h2")
    private subtitle: HTMLElement = document.createElement("p")
    private themeButton: HTMLButtonElement = document.createElement("button")

    private name: InputComponent
    private password: InputComponent
    private passwordFile: InputComponent

    constructor() {
        super()
        const i = getTranslations(getCurrentLanguage()).modal
        const ui = getTranslations(getCurrentLanguage()).ui

        this.setSubmitLabel(i.login)
        this.brand.classList.add("login-brand")
        this.brand.innerHTML = '<span class="brand-glyph" aria-hidden="true"></span><span class="brand-text"><strong>Moonlight</strong><small>Web Console</small></span>'
        this.themeButton.type = "button"
        this.themeButton.classList.add("icon-btn", "login-theme-toggle")
        const syncThemeIcon = () => {
            const light = document.documentElement.dataset.theme === "light"
            this.themeButton.replaceChildren(icon(light ? "moon" : "sun"))
            this.themeButton.title = ui.toggleTheme
            this.themeButton.setAttribute("aria-label", ui.toggleTheme)
        }
        syncThemeIcon()
        this.themeButton.addEventListener("click", () => { toggleStoredTheme(); syncThemeIcon() })
        this.brand.appendChild(this.themeButton)
        this.text.innerText = i.login
        this.text.classList.add("login-title")
        this.subtitle.innerText = i.loginSubtitle
        this.subtitle.classList.add("login-subtitle")

        this.name = new InputComponent("ml-api-name", "text", i.username, {
            formRequired: true,
            autocomplete: "username"
        })

        this.password = new InputComponent("ml-api-password", "password", i.password, {
            formRequired: true,
            autocomplete: "current-password"
        })

        this.passwordFile = new InputComponent("ml-api-password-file", "file", i.passwordAsFile, { accept: ".txt" })
        this.passwordFile.addChangeListener(this.setFilePassword.bind(this))
    }

    private async setFilePassword(event: ComponentEvent<InputComponent>) {
        const files = event.component.getFiles()
        if (!files) {
            return
        }

        const file = files[0]
        if (!file) {
            return
        }
        const text = await file.text()

        // Remove carriage return and new line
        const password = text
            .replace(/\r/g, "")
            .replace(/\n/g, "")

        this.password.setValue(password)
    }

    reset(): void {
        this.name.reset()
        this.password.reset()
        this.passwordFile.reset()
    }
    submit(): UserAuth | null {
        const name = this.name.getValue()
        const password = this.password.getValue()

        if (name && password) {
            return { name, password }
        } else {
            return null
        }
    }

    onFinish(abort: AbortSignal): Promise<UserAuth | null> {
        const abortController = new AbortController()
        abort.addEventListener("abort", abortController.abort.bind(abortController))

        return new Promise((resolve, reject) => {
            super.onFinish(abortController.signal).then((data) => {
                abortController.abort()
                resolve(data)
            }, (data) => {
                abortController.abort()
                reject(data)
            })
        })
    }

    mount(parent: Element): void {
        super.mount(parent)
        document.getElementById("modal-overlay")?.classList.add("modal-login")
    }
    unmount(parent: Element): void {
        super.unmount(parent)
        document.getElementById("modal-overlay")?.classList.remove("modal-login")
    }

    mountForm(form: HTMLFormElement): void {
        form.classList.add("login-form")
        form.appendChild(this.brand)
        form.appendChild(this.text)
        form.appendChild(this.subtitle)
        this.name.mount(form)

        this.password.mount(form)
        this.passwordFile.mount(form)
    }
}
