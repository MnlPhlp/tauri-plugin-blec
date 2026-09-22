//! The UI of the example, free of BLE logic.
//!
//! Every component gets what it shows as props and reports what the user did
//! through event handlers; the calls into `dioxus-blec` all live in `main.rs`.

use dioxus::prelude::*;
use dioxus_blec::models::BleDevice;
use uuid::Uuid;

/// The last thing worth telling the user, shown under the title.
#[derive(Clone, PartialEq, Default)]
pub enum Status {
    #[default]
    None,
    Info(String),
    Error(String),
}

impl Status {
    pub fn info(text: impl Into<String>) -> Self {
        Self::Info(text.into())
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self::Error(text.into())
    }
}

#[component]
pub fn StatusBar(status: Status) -> Element {
    let (class, text) = match &status {
        Status::None => return rsx! {},
        Status::Info(text) => ("status", text),
        Status::Error(text) => ("status status-error", text),
    };
    rsx! {
        p { class, "{text}" }
    }
}

#[component]
pub fn Spinner() -> Element {
    rsx! {
        span { class: "spinner", aria_label: "busy" }
    }
}

/// Scan / stop and the permission check.
#[component]
pub fn ScanControls(
    ready: bool,
    scanning: bool,
    on_scan: EventHandler<()>,
    on_stop: EventHandler<()>,
    on_check_permissions: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "toolbar",
            if scanning {
                button { class: "btn btn-danger", onclick: move |_| on_stop.call(()),
                    Spinner {}
                    "Stop scan"
                }
            } else {
                button {
                    class: "btn btn-primary",
                    disabled: !ready,
                    onclick: move |_| on_scan.call(()),
                    "Scan"
                }
            }
            button {
                class: "btn",
                disabled: !ready,
                title: "Android only: asks for the bluetooth permissions",
                onclick: move |_| on_check_permissions.call(()),
                "Check permissions"
            }
        }
    }
}

/// The found devices, in the order `blec` reports them or, on request,
/// closest (strongest signal) first.
#[component]
pub fn DeviceList(
    devices: Vec<BleDevice>,
    scanning: bool,
    on_connect: EventHandler<String>,
) -> Element {
    let mut sort_by_rssi = use_signal(|| false);
    let mut devices = devices;
    if sort_by_rssi() {
        // Devices without an rssi go last.
        devices.sort_by_key(|device| std::cmp::Reverse(device.rssi));
    }

    rsx! {
        section { class: "device-list",
            div { class: "section-header",
                h2 { "Devices" }
                span { class: "muted",
                    if scanning {
                        Spinner {}
                        " scanning…"
                    } else {
                        "{devices.len()} found"
                    }
                }
                button {
                    class: if sort_by_rssi() { "btn btn-small btn-active" } else { "btn btn-small" },
                    title: "closest device first",
                    onclick: move |_| sort_by_rssi.toggle(),
                    "Sort by signal"
                }
            }
            if devices.is_empty() && !scanning {
                p { class: "muted empty", "No devices yet. Start a scan." }
            }
            for device in devices {
                DeviceCard { key: "{device.address}", device, on_connect }
            }
        }
    }
}

/// One device: name, address and signal strength; the rest unfolds on a click.
#[component]
pub fn DeviceCard(device: BleDevice, on_connect: EventHandler<String>) -> Element {
    let mut expanded = use_signal(|| false);
    let name = if device.name.is_empty() {
        "(unnamed)".to_string()
    } else {
        device.name.clone()
    };
    let address = device.address.clone();

    rsx! {
        article { class: "card",
            div { class: "card-header", onclick: move |_| expanded.toggle(),
                span { class: if expanded() { "chevron open" } else { "chevron" } }
                div { class: "card-title",
                    strong { "{name}" }
                    span { class: "mono muted", "{device.address}" }
                }
                Rssi { rssi: device.rssi }
                button {
                    class: "btn btn-primary btn-small",
                    onclick: move |e| {
                        // Do not fold or unfold the card as well.
                        e.stop_propagation();
                        on_connect.call(address.clone());
                    },
                    "Connect"
                }
            }
            if expanded() {
                DeviceDetails { device }
            }
        }
    }
}

#[component]
fn Rssi(rssi: Option<i16>) -> Element {
    match rssi {
        Some(rssi) => rsx! {
            span { class: "rssi", title: "signal strength", "{rssi} dBm" }
        },
        None => rsx! {},
    }
}

/// Everything the advertisement told us about a device.
#[component]
fn DeviceDetails(device: BleDevice) -> Element {
    let mut manufacturer_data: Vec<_> = device.manufacturer_data.iter().collect();
    manufacturer_data.sort_by_key(|(id, _)| **id);
    let mut service_data: Vec<_> = device.service_data.iter().collect();
    service_data.sort_by_key(|(uuid, _)| **uuid);

    rsx! {
        dl { class: "details",
            dt { "State" }
            dd {
                if device.is_connected { "connected" } else { "not connected" }
                if device.is_bonded { ", bonded" }
            }
            dt { "TX power" }
            dd {
                match device.tx_power_level {
                    Some(level) => rsx! { "{level} dBm" },
                    None => rsx! { span { class: "muted", "—" } },
                }
            }
            dt { "Services" }
            dd {
                UuidList { uuids: device.services.clone() }
            }
            dt { "Manufacturer data" }
            dd {
                if manufacturer_data.is_empty() {
                    span { class: "muted", "—" }
                }
                for (id, data) in manufacturer_data {
                    div { class: "mono",
                        span { class: "muted", "0x{id:04x}: " }
                        "{hex(data)}"
                    }
                }
            }
            dt { "Service data" }
            dd {
                if service_data.is_empty() {
                    span { class: "muted", "—" }
                }
                for (uuid, data) in service_data {
                    div { class: "mono",
                        span { class: "muted", "{uuid}: " }
                        "{hex(data)}"
                    }
                }
            }
        }
    }
}

#[component]
fn UuidList(uuids: Vec<Uuid>) -> Element {
    if uuids.is_empty() {
        return rsx! { span { class: "muted", "—" } };
    }
    rsx! {
        for uuid in uuids {
            div { class: "mono", key: "{uuid}", "{uuid}" }
        }
    }
}

/// Write, read and notifications on the connected device.
///
/// `notifications` is what to show in the notify row: the parent owns the
/// subscription and renders the last notification (or a placeholder) into it.
#[component]
pub fn ConnectedPanel(
    device: BleDevice,
    read_value: Option<String>,
    subscribed: bool,
    notifications: Element,
    on_write: EventHandler<String>,
    on_read: EventHandler<()>,
    on_toggle_subscribe: EventHandler<()>,
    on_disconnect: EventHandler<()>,
) -> Element {
    let mut to_send = use_signal(|| "hello".to_string());
    let name = if device.name.is_empty() {
        device.address.clone()
    } else {
        device.name.clone()
    };

    rsx! {
        section { class: "card connected",
            div { class: "section-header",
                div { class: "card-title",
                    h2 { "{name}" }
                    span { class: "mono muted", "{device.address}" }
                }
                button { class: "btn btn-danger", onclick: move |_| on_disconnect.call(()), "Disconnect" }
            }

            div { class: "row",
                label { "Write" }
                input {
                    value: "{to_send}",
                    placeholder: "text to send",
                    oninput: move |e| to_send.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            on_write.call(to_send());
                        }
                    },
                }
                button { class: "btn btn-primary", onclick: move |_| on_write.call(to_send()), "Send" }
            }

            div { class: "row",
                label { "Read" }
                Value { value: read_value, placeholder: "not read yet" }
                button { class: "btn btn-primary", onclick: move |_| on_read.call(()), "Read" }
            }

            div { class: "row",
                label { "Notify" }
                {notifications}
                button {
                    class: if subscribed { "btn" } else { "btn btn-primary" },
                    onclick: move |_| on_toggle_subscribe.call(()),
                    if subscribed { "Unsubscribe" } else { "Subscribe" }
                }
            }
        }
    }
}

/// A read-only value with a placeholder while there is none.
#[component]
pub fn Value(value: Option<String>, placeholder: String) -> Element {
    match value {
        Some(value) => rsx! { output { class: "value mono", "{value}" } },
        None => rsx! { output { class: "value muted", "{placeholder}" } },
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
