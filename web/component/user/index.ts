import { Api, apiDeleteUser, apiGetUser } from "../../api.js";
import { DetailedUser } from "../../api_bindings.js";
import { getCurrentLanguage, getTranslations } from "../../i18n.js";
import { setContextMenu } from "../context_menu.js";
import { Component, ComponentEvent } from "../index.js";

export type UserEventListener = (event: ComponentEvent<User>) => void

export async function tryDeleteUser(api: Api, id: number) {
    await apiDeleteUser(api, { id })
}

export class User implements Component {

    private api: Api

    private user: DetailedUser | { id: number }

    private div = document.createElement("div")
    private avatarElement = document.createElement("span")
    private copyElement = document.createElement("div")
    private nameElement = document.createElement("p")
    private roleElement = document.createElement("p")
    private chevronElement = document.createElement("span")

    constructor(api: Api, user: DetailedUser | { id: number }) {
        this.api = api

        this.div.classList.add("user-card")
        this.avatarElement.classList.add("user-avatar")
        this.copyElement.classList.add("user-card-copy")
        this.nameElement.classList.add("user-card-name")
        this.roleElement.classList.add("user-card-role")
        this.chevronElement.classList.add("user-card-chevron")
        this.chevronElement.innerText = "›"

        this.copyElement.appendChild(this.nameElement)
        this.copyElement.appendChild(this.roleElement)
        this.div.appendChild(this.avatarElement)
        this.div.appendChild(this.copyElement)
        this.div.appendChild(this.chevronElement)
        this.div.addEventListener("click", this.onClick.bind(this))
        this.div.addEventListener("contextmenu", this.onContextMenu.bind(this))

        this.user = user
        if ("name" in user) {
            this.updateCache(user)
        } else {
            this.forceFetch()
        }
    }

    async forceFetch() {
        const user = await apiGetUser(this.api, {
            name: null,
            user_id: this.user.id,
        })

        this.updateCache(user)
    }
    updateCache(user: DetailedUser) {
        this.user = user

        this.nameElement.innerText = user.name
        this.avatarElement.innerText = user.name.trim().slice(0, 1).toUpperCase() || "?"
        this.roleElement.innerText = user.role
        this.div.title = user.name
    }

    private onClick() {
        this.div.dispatchEvent(new ComponentEvent("ml-userclicked", this))
    }

    private onContextMenu(event: MouseEvent) {
        const i = getTranslations(getCurrentLanguage()).admin
        setContextMenu(event, {
            elements: [
                {
                    name: i.delete,
                    callback: this.onDelete.bind(this)
                }
            ]
        })
    }

    addClickedListener(listener: UserEventListener, options?: EventListenerOptions) {
        this.div.addEventListener("ml-userclicked", listener as any, options)
    }
    removeClickedListener(listener: UserEventListener) {
        this.div.removeEventListener("ml-userclicked", listener as any)
    }

    private onDelete() {
        tryDeleteUser(this.api, this.user.id)

        this.div.dispatchEvent(new ComponentEvent("ml-userdeleted", this))
    }

    addDeletedListener(listener: UserEventListener, options?: EventListenerOptions) {
        this.div.addEventListener("ml-userdeleted", listener as any, options)
    }
    removeDeletedListener(listener: UserEventListener) {
        this.div.removeEventListener("ml-userdeleted", listener as any)
    }

    getCache(): DetailedUser | null {
        if ("name" in this.user) {
            return this.user
        } else {
            return null
        }
    }

    getUserId(): number {
        return this.user.id
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.div)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.div)
    }
}
