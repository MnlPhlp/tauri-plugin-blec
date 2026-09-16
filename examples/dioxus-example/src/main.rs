//! `blec` in a Dioxus app, through the `dioxus-blec` hooks.
//!
//! Scans, connects, and reads/writes/subscribes on the characteristic
//! `examples/test-server` serves. On android there is nothing to set up for
//! bluetooth: `dioxus-blec` carries the permissions and `use_ble` initializes
//! `blec`.

use dioxus::prelude::*;
use dioxus_blec::models::{ScanFilter, WriteType};
use dioxus_blec::{use_ble, use_ble_notifications, InitState};
use uuid::{uuid, Uuid};

const SERVICE_UUID: Uuid = uuid!("A07498CA-AD5B-474E-940D-16F1FBE7E8CD");
const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");

const SCAN_MS: u64 = 5000;

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let ble = use_ble();
    let mut status = use_signal(String::new);
    let mut to_send = use_signal(|| "hello".to_string());
    let notified = use_ble_notifications(CHARACTERISTIC_UUID, Some(SERVICE_UUID));

    if let InitState::Failed(e) = ble.init_state() {
        return rsx! {
            main { h1 { "blec" } p { "could not initialize: {e}" } }
        };
    }

    rsx! {
        main {
            h1 { "blec" }
            p { "{status}" }

            div {
                button {
                    disabled: !ble.is_ready(),
                    onclick: move |_| async move {
                        // Android only. Sends the user to the app settings when
                        // they denied the permission before.
                        match ble.check_permissions(true).await {
                            Ok(true) => status.set("permissions granted".into()),
                            Ok(false) => status.set("permissions denied".into()),
                            Err(e) => status.set(format!("permission check failed: {e}")),
                        }
                    },
                    "Check permissions"
                }
                button {
                    disabled: !ble.is_ready() || ble.scanning(),
                    onclick: move |_| async move {
                        status.set("scanning…".into());
                        match ble.scan(SCAN_MS, ScanFilter::None).await {
                            Ok(()) => status.set("scan started".into()),
                            Err(e) => status.set(format!("scan failed: {e}")),
                        }
                    },
                    if ble.scanning() { "Scanning…" } else { "Scan" }
                }
            }

            if let Some(device) = ble.connected() {
                Connected {
                    address: device.address,
                    notified: notified()
                        .map(|data| String::from_utf8_lossy(&data).into_owned())
                        .unwrap_or_default(),
                    to_send: to_send(),
                    on_send: move |data: String| async move {
                        let sent = ble
                            .write(
                                CHARACTERISTIC_UUID,
                                Some(SERVICE_UUID),
                                data.as_bytes(),
                                WriteType::WithResponse,
                            )
                            .await;
                        if let Err(e) = sent {
                            status.set(format!("send failed: {e}"));
                        }
                    },
                    on_input: move |data| to_send.set(data),
                    on_disconnect: move |()| async move {
                        if let Err(e) = ble.disconnect().await {
                            status.set(format!("disconnect failed: {e}"));
                        }
                    },
                }
            } else {
                ul {
                    for device in ble.devices() {
                        li {
                            key: "{device.address}",
                            button {
                                onclick: move |_| {
                                    let address = device.address.clone();
                                    async move {
                                        status.set(format!("connecting to {address}…"));
                                        match ble.connect(&address).await {
                                            Ok(()) => status.set("connected".into()),
                                            Err(e) => status.set(format!("connect failed: {e}")),
                                        }
                                    }
                                },
                                "{device.name} ({device.address}) {device.rssi:?}"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Connected(
    address: String,
    notified: String,
    to_send: String,
    on_send: EventHandler<String>,
    on_input: EventHandler<String>,
    on_disconnect: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            h2 { "{address}" }
            p { "notified: {notified}" }
            input {
                value: "{to_send}",
                oninput: move |e| on_input.call(e.value()),
            }
            button { onclick: move |_| on_send.call(to_send.clone()), "Send" }
            button { onclick: move |_| on_disconnect.call(()), "Disconnect" }
        }
    }
}
