import { Component } from "../index.js"
import { showNotification } from "../notification.js"
import { getCurrentLanguage, getTranslations } from "../../i18n.js"
import { FormModal } from "./form.js"

export interface Modal<Output> extends Component {
    onFinish(abort: AbortSignal): Promise<Output>
}

let modalAbort: AbortController | null = null
let previousModal: Modal<unknown> | null = null

// The page markup (web/index.html) provides these nodes, but this module is reachable from
// entry points that may be evaluated before that markup exists (dynamic imports, tests,
// module-level import order). Resolving them once at module evaluation time left the modal
// silently unusable for the whole page lifetime, so they are looked up on use and cached
// only once they are actually found.
let modalBackgroundCache: HTMLElement | null = null
let modalParentCache: HTMLElement | null = null

function getModalParent(): HTMLElement | null {
    if (modalParentCache?.isConnected) {
        return modalParentCache
    }
    modalParentCache = document.getElementById("modal-parent")
    return modalParentCache
}

function getModalBackgroundElement(): HTMLElement | null {
    if (modalBackgroundCache?.isConnected) {
        return modalBackgroundCache
    }
    modalBackgroundCache = document.getElementById("modal-overlay")
    return modalBackgroundCache
}

// Keyboard accessibility: remember what had focus when the modal opened, keep focus inside
// the modal while it is open, and give focus back afterwards. Without this a keyboard or
// screen-reader user could tab into the page behind the overlay and lost their place on
// close. This is an explicit trap because the markup does not use <dialog>.
let focusBeforeModal: HTMLElement | null = null
// True while a modal chain is open and the original focus target is still pending. A
// replacement modal must not re-capture the target from inside the modal being replaced.
let focusRestorePending = false

const FOCUSABLE = [
    "a[href]",
    "button:not([disabled])",
    "input:not([disabled])",
    "select:not([disabled])",
    "textarea:not([disabled])",
    "[tabindex]:not([tabindex='-1'])",
].join(",")

function focusableElements(root: HTMLElement | null): Array<HTMLElement> {
    if (!root) {
        return []
    }
    const found = Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE))
    // Fall back to the root itself so the trap always has somewhere to go.
    return found.length > 0 ? found : (root.tabIndex >= 0 ? [root] : [])
}

function onModalKeyDown(event: KeyboardEvent) {
    const modalParent = getModalParent()
    if (event.key != "Tab" || !modalParent) {
        return
    }

    const focusable = focusableElements(modalParent)
    if (focusable.length == 0) {
        return
    }

    const first = focusable[0]
    const last = focusable[focusable.length - 1]
    const active = document.activeElement

    if (event.shiftKey && (active == first || !modalParent.contains(active))) {
        event.preventDefault()
        last.focus()
    } else if (!event.shiftKey && (active == last || !modalParent.contains(active))) {
        event.preventDefault()
        first.focus()
    }
}

/** Put focus back where it was before the modal opened. */
function restoreFocus() {
    const target = focusBeforeModal
    focusBeforeModal = null
    focusRestorePending = false
    if (target && target.isConnected !== false) {
        target.focus()
    }
}

function rememberFocus() {
    // A replacement modal opened from inside the previous modal would otherwise record one
    // of the old modal's own controls as the "origin" to restore to.
    if (focusRestorePending) {
        return
    }

    const active = document.activeElement
    focusBeforeModal = active instanceof HTMLElement && active != document.body ? active : null
    focusRestorePending = true
}

// Don't allow context menu event through this background.
let modalBackgroundListenerAttached = false

function attachModalBackgroundGuards(background: HTMLElement | null) {
    if (!background || modalBackgroundListenerAttached) {
        return
    }
    modalBackgroundListenerAttached = true
    background.addEventListener("contextmenu", (event: Event) => {
        event.stopImmediatePropagation()
    })
}

export function getModalBackground(): HTMLElement | null {
    const background = getModalBackgroundElement()
    attachModalBackgroundGuards(background)
    return background
}

export async function showModal<Output>(modal: Modal<Output> | null): Promise<Output | null> {
    const i = getTranslations(getCurrentLanguage()).common
    const modalParent = getModalParent()
    const modalBackground = getModalBackground()
    if (modalParent == null) {
        showNotification(i.missingModalParent)
        return null
    }
    if (modalBackground == null) {
        showNotification(i.missingModalOverlay)
    }

    if (modalAbort != null) {
        modalBackground?.classList.add("modal-disabled")
        document.documentElement.classList.remove("modal-open")
        modalAbort.abort()
        modalAbort = null
    }

    if (!modal) {
        // Closing the last modal returns focus to whatever opened it.
        document.removeEventListener("keydown", onModalKeyDown, true)
        previousModal = null
        restoreFocus()
        return null
    }

    if (previousModal) {
        previousModal.unmount(modalParent)
    }
    previousModal = modal

    if (modalAbort == null) {
        // Only the first modal of a chain remembers the original focus target; replacing a
        // modal must not overwrite it with an element inside the old modal.
        rememberFocus()
    }

    const abortController = new AbortController()

    modalAbort = abortController
    modal.mount(modalParent)
    modalBackground?.classList.remove("modal-disabled")
    document.documentElement.classList.add("modal-open")
    document.addEventListener("keydown", onModalKeyDown, true)

    const output = await modal.onFinish(abortController.signal)

    modalBackground?.classList.add("modal-disabled")
    document.documentElement.classList.remove("modal-open")
    abortController.abort()
    if (modalAbort === abortController) {
        modalAbort = null
        document.removeEventListener("keydown", onModalKeyDown, true)
        restoreFocus()
    }

    return output
}

/// --- Helper Modals

export async function showPrompt(prompt: string, promptInit?: PromptInit): Promise<string | null> {
    const modal = new PromptModal(prompt, promptInit)

    return await showModal(modal)
}

type PromptInit = {
    defaultValue?: string,
    name?: string,
    type?: "text" | "password",
}

class PromptModal extends FormModal<string> {
    private message: HTMLElement = document.createElement("p")
    private textInput: HTMLInputElement = document.createElement("input")

    constructor(prompt: string, init?: PromptInit) {
        super()

        this.message.innerText = prompt

        if (init?.type) {
            this.textInput.type = init?.type
        }
        if (init?.defaultValue) {
            this.textInput.defaultValue = init?.defaultValue
        }
        if (init?.name) {
            this.textInput.name = init?.name
        }
    }

    reset(): void {
        this.textInput.value = ""
    }
    submit(): string | null {
        return this.textInput.value
    }

    mountForm(form: HTMLFormElement): void {
        this.message.classList.add("modal-message-text")
        form.appendChild(this.message)
        form.appendChild(this.textInput)
    }
}

type MessageInit = {
    signal?: AbortSignal
}

export async function showMessage(message: string, init?: MessageInit) {
    const modal = new MessageModal(message, init)

    await showModal(modal)
}

class MessageModal implements Component, Modal<void> {

    private signal?: AbortSignal
    private root: HTMLDivElement = document.createElement("div")
    private textElement: HTMLElement = document.createElement("p")
    private okButton: HTMLButtonElement = document.createElement("button")

    constructor(message: string, init?: MessageInit) {
        const i = getTranslations(getCurrentLanguage()).modal
        this.textElement.innerText = message

        this.okButton.innerText = i.ok
        this.okButton.classList.add("btn", "btn-primary")

        this.root.classList.add("modal-message")
        this.textElement.classList.add("modal-message-text")
        const actions = document.createElement("div")
        actions.classList.add("modal-actions")
        actions.appendChild(this.okButton)
        this.root.append(this.textElement, actions)

        this.signal = init?.signal
    }

    mount(parent: Element): void {
        parent.appendChild(this.root)
        this.okButton.focus()
    }
    unmount(parent: Element): void {
        parent.removeChild(this.root)
    }

    onFinish(abort: AbortSignal): Promise<void> {
        return new Promise((resolve, reject) => {
            let customController: AbortController | null = null

            if (this.signal) {
                customController = new AbortController()
                this.signal.addEventListener("abort", () => {
                    resolve()
                    customController?.abort()
                }, { once: true, signal: customController.signal })
            }

            this.okButton.addEventListener("click", () => {
                resolve()
                customController?.abort()
            }, { signal: customController?.signal })

            if (customController) {
                abort.addEventListener("abort", customController.abort.bind(customController))
            }
        })
    }
}
