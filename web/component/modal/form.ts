import { Component } from "../index.js"
import { getCurrentLanguage, getTranslations } from "../../i18n.js"
import { Modal, showModal } from "./index.js"

export abstract class FormModal<Output> implements Component, Modal<Output | null> {

    private formElement: HTMLFormElement = document.createElement("form")
    private mounted: boolean = false
    private submitButton: HTMLButtonElement = document.createElement("button")
    private cancelButton: HTMLButtonElement = document.createElement("button")
    private actionsElement: HTMLDivElement = document.createElement("div")

    constructor() {
        const i = getTranslations(getCurrentLanguage()).modal
        this.submitButton.type = "submit"
        this.submitButton.innerText = i.ok

        this.cancelButton.innerText = i.cancel
        this.cancelButton.type = "button"
        this.cancelButton.classList.add("btn", "btn-ghost")
        this.submitButton.classList.add("btn", "btn-primary")
        this.actionsElement.classList.add("modal-actions")
        this.actionsElement.append(this.cancelButton, this.submitButton)
        this.formElement.classList.add("modal-form")

        this.formElement.addEventListener("submit", (event) => event.preventDefault())
    }

    protected setSubmitLabel(label: string) {
        this.submitButton.innerText = label
    }

    protected setCancelLabel(label: string) {
        this.cancelButton.innerText = label
    }

    abstract reset(): void
    abstract submit(): Output | null

    abstract mountForm(form: HTMLFormElement): void

    mount(parent: Element): void {
        if (!this.mounted) {
            // Build the fields once per instance. `mounted` previously never became true,
            // so every reuse of the same FormModal appended a second copy of every field.
            this.mountForm(this.formElement)
            this.formElement.appendChild(this.actionsElement)
            this.mounted = true
        }

        this.reset()

        parent.appendChild(this.formElement)
    }
    unmount(parent: Element): void {
        // The element can already be detached when the modal was replaced while a
        // successor had mounted; removing it again would throw and abort the cleanup.
        if (this.formElement.parentElement == parent) {
            parent.removeChild(this.formElement)
        }
    }

    onFinish(signal: AbortSignal): Promise<Output | null> {
        const abortController = new AbortController()
        const abort = () => {
            abortController.abort()
            resolveOnce(null)
        }

        let settled = false
        let resolveOnce: (output: Output | null) => void = () => { }

        const promise = new Promise<Output | null>((resolve) => {
            resolveOnce = (output) => {
                if (settled) {
                    return
                }
                settled = true
                resolve(output)
            }

            this.formElement.addEventListener("submit", event => {
                const output = this.submit()

                if (output == null) {
                    return
                }

                abortController.abort()
                resolveOnce(output)
            }, { signal: abortController.signal })

            this.cancelButton.addEventListener("click", event => {
                event.preventDefault()

                abortController.abort()
                resolveOnce(null)
            }, { signal: abortController.signal })

            document.addEventListener("keydown", event => {
                if (event.key == "Escape") {
                    abortController.abort()
                    resolveOnce(null)
                }
            }, { signal: abortController.signal })
        })

        if (signal.aborted) {
            // Replaced/closed before this modal even started listening.
            abortController.abort()
            resolveOnce(null)
        } else {
            // The modal was replaced (showModal aborts the previous controller). Without
            // this the `await showModal(...)` of the previous call never settled.
            signal.addEventListener("abort", abort, { once: true })
        }

        return promise
    }
}
