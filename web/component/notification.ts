import { Component } from "./index.js"
import { icon, IconName } from "./icons.js"
import { ListComponent } from "./list.js"

type NotificationLevel = "error" | "warn" | "info"

const ERROR_REMOVAL_TIME_MS = 10000

const notificationListElement = document.getElementById("notification-list")
const notificationListComponent = new ListComponent<NotificationComponent>([], { listClasses: ["notification-list"], elementLiClasses: ["notification-element"] })
if (notificationListElement) {
    notificationListComponent.mount(notificationListElement)
}

let alertedNotificationListNotFound = false

export function showNotification(message: string, level: NotificationLevel = "error", errorObject?: any) {
    console.error(message, errorObject)

    if (!notificationListElement) {
        if (!alertedNotificationListNotFound) {
            alert("couldn't find the notification element")
            alertedNotificationListNotFound = true
        }
        alert(message)
        return;
    }

    let error: NotificationComponent
    if (level == "error") {
        error = new NotificationComponent(message, "error")
    } else if (level == "warn") {
        error = new NotificationComponent(message, "warn")
    } else if (level == "info") {
        error = new NotificationComponent(message, "info")
    } else {
        error = new NotificationComponent(`Unknown notification level (\"${level}\") for message: ${message}`, "error")
    }

    notificationListComponent.append(error)

    setTimeout(() => {
        notificationListComponent.removeValue(error)
    }, ERROR_REMOVAL_TIME_MS)
}

function handleError(event: ErrorEvent) {
    showNotification(`${event.error}`, "error", event)
}
function handleRejection(event: PromiseRejectionEvent) {
    showNotification(`${event.reason}`, "error", event)
}

window.addEventListener("error", handleError)
window.addEventListener("unhandledrejection", handleRejection)

class NotificationComponent implements Component {
    private root: HTMLDivElement = document.createElement("div")
    private messageElement: HTMLElement = document.createElement("p")

    constructor(message: string, level: IconName & ("error" | "warn" | "info")) {
        this.root.classList.add("toast", `toast-${level}`)
        this.messageElement.innerText = message
        this.messageElement.classList.add("notification-message")

        this.root.append(icon(level, "toast-icon"), this.messageElement)
    }

    mount(parent: Element): void {
        parent.appendChild(this.root)
    }
    unmount(parent: Element): void {
        parent.removeChild(this.root)
    }
}
