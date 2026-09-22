//! `blec` in a Dioxus app, through the `dioxus-blec` hooks.
//!
//! Scans, connects, and reads/writes/subscribes on the characteristic
//! `examples/test-server` serves. On android there is nothing to set up for
//! bluetooth: `dioxus-blec` carries the permissions and `use_ble` initializes
//! `blec`.
//!
//! This file holds the BLE logic; the UI lives in [`components`].

mod components;

use components::{ConnectedPanel, DeviceList, ScanControls, Status, StatusBar, Value};
use dioxus::prelude::*;
use dioxus_blec::models::{ScanFilter, WriteType};
use dioxus_blec::{use_ble, use_ble_notifications, InitState};
use uuid::{uuid, Uuid};

const SERVICE_UUID: Uuid = uuid!("A07498CA-AD5B-474E-940D-16F1FBE7E8CD");
const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");

const SCAN_MS: u64 = 10_000;

static CSS: Asset = asset!("/assets/main.css");

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let ble = use_ble();
    let mut status = use_signal(Status::default);
    let mut read_value = use_signal(|| None::<String>);
    let mut subscribed = use_signal(|| false);

    // `Notifications` holds the subscription: mounting it subscribes,
    // unmounting it unsubscribes.
    let notifications = if subscribed() {
        rsx! { Notifications {} }
    } else {
        rsx! { Value { value: None, placeholder: "not subscribed" } }
    };

    rsx! {
        document::Stylesheet { href: CSS }
        main { class: "app",
            header {
                h1 { "blec" }
                span { class: "subtitle", "dioxus example" }
            }
            StatusBar { status: status() }

            if let InitState::Failed(e) = ble.init_state() {
                p { class: "error", "could not initialize: {e}" }
            } else if let Some(device) = ble.connected() {
                ConnectedPanel {
                    device,
                    read_value: read_value(),
                    subscribed: subscribed(),
                    notifications,
                    on_toggle_subscribe: move |()| subscribed.toggle(),
                    on_write: move |text: String| async move {
                        let written = ble
                            .write(
                                CHARACTERISTIC_UUID,
                                Some(SERVICE_UUID),
                                text.as_bytes(),
                                WriteType::WithResponse,
                            )
                            .await;
                        status.set(match written {
                            Ok(()) => Status::info(format!("sent {text:?}")),
                            Err(e) => Status::error(format!("send failed: {e}")),
                        });
                    },
                    on_read: move |()| async move {
                        match ble.read(CHARACTERISTIC_UUID, Some(SERVICE_UUID)).await {
                            Ok(data) => read_value.set(Some(String::from_utf8_lossy(&data).into_owned())),
                            Err(e) => status.set(Status::error(format!("read failed: {e}"))),
                        }
                    },
                    on_disconnect: move |()| async move {
                        status.set(match ble.disconnect().await {
                            Ok(()) => Status::info("disconnected"),
                            Err(e) => Status::error(format!("disconnect failed: {e}")),
                        });
                    },
                }
            } else {
                ScanControls {
                    ready: ble.is_ready(),
                    scanning: ble.scanning(),
                    on_scan: move |()| async move {
                        status.set(match ble.scan(SCAN_MS, ScanFilter::None).await {
                            Ok(()) => Status::None,
                            Err(e) => Status::error(format!("scan failed: {e}")),
                        });
                    },
                    on_stop: move |()| async move {
                        if let Err(e) = ble.stop_scan().await {
                            status.set(Status::error(format!("stop scan failed: {e}")));
                        }
                    },
                    // Android only. Sends the user to the app settings when
                    // they denied the permission before.
                    on_check_permissions: move |()| async move {
                        status.set(match ble.check_permissions(true).await {
                            Ok(true) => Status::info("permissions granted"),
                            Ok(false) => Status::error("permissions denied"),
                            Err(e) => Status::error(format!("permission check failed: {e}")),
                        });
                    },
                }
                DeviceList {
                    devices: ble.devices(),
                    scanning: ble.scanning(),
                    on_connect: move |address: String| async move {
                        status.set(Status::info(format!("connecting to {address}…")));
                        read_value.set(None);
                        subscribed.set(false);
                        status.set(match ble.connect(&address).await {
                            Ok(()) => Status::info("connected"),
                            Err(e) => Status::error(format!("connect failed: {e}")),
                        });
                    },
                }
            }
        }
    }
}

/// The last notification of the characteristic. Subscribed for as long as
/// this component is shown.
#[component]
fn Notifications() -> Element {
    let notified = use_ble_notifications(CHARACTERISTIC_UUID, Some(SERVICE_UUID));
    rsx! {
        Value {
            value: notified().map(|data| String::from_utf8_lossy(&data).into_owned()),
            placeholder: "no notification yet",
        }
    }
}
