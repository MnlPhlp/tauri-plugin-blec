//! `blec` in a plain rust app, without tauri.
//!
//! Scans, connects, and reads/writes/subscribes on the characteristic
//! `examples/test-server` serves. On android this is all the app does about
//! bluetooth: `blec::init()` loads the embedded dex, and the permissions come
//! from `Dioxus.toml`.

use blec::models::{BleDevice, ScanFilter, WriteType};
use blec::OnDisconnectHandler;
use dioxus::prelude::*;
use uuid::{uuid, Uuid};

const SERVICE_UUID: Uuid = uuid!("A07498CA-AD5B-474E-940D-16F1FBE7E8CD");
const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");

const SCAN_MS: u64 = 5000;

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let mut status = use_signal(String::new);
    let mut devices = use_signal(Vec::<BleDevice>::new);
    let mut to_send = use_signal(|| "hello".to_string());
    // The disconnect and notification callbacks run on blec's tasks, so these
    // two need the thread safe signal storage.
    let mut connected = use_signal_sync(|| None::<String>);
    let notified = use_signal_sync(String::new);

    // One handler for the whole process; every later `get_handler()` returns it.
    let handler = use_resource(|| async move { blec::init().await.map(|_| ()) });

    let ready = matches!(&*handler.read_unchecked(), Some(Ok(())));
    if let Some(Err(e)) = &*handler.read_unchecked() {
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
                    disabled: !ready,
                    onclick: move |_| async move {
                        // Android only. Sends the user to the app settings when
                        // they denied the permission before.
                        match blec::check_permissions(true).await {
                            Ok(true) => status.set("permissions granted".into()),
                            Ok(false) => status.set("permissions denied".into()),
                            Err(e) => status.set(format!("permission check failed: {e}")),
                        }
                    },
                    "Check permissions"
                }
                button {
                    disabled: !ready,
                    onclick: move |_| async move {
                        devices.set(vec![]);
                        status.set("scanning…".into());
                        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                        spawn(async move {
                            while let Some(found) = rx.recv().await {
                                devices.set(found);
                            }
                        });
                        let handler = match blec::get_handler() {
                            Ok(handler) => handler,
                            Err(e) => return status.set(format!("{e}")),
                        };
                        match handler.discover(Some(tx), SCAN_MS, ScanFilter::None, false).await {
                            Ok(()) => status.set("scan finished".into()),
                            Err(e) => status.set(format!("scan failed: {e}")),
                        }
                    },
                    "Scan"
                }
            }

            if let Some(address) = connected() {
                Connected {
                    address,
                    notified: notified(),
                    to_send: to_send(),
                    on_send: move |data: String| async move {
                        let Ok(handler) = blec::get_handler() else { return };
                        if let Err(e) = handler
                            .send_data(
                                CHARACTERISTIC_UUID,
                                Some(SERVICE_UUID),
                                data.as_bytes(),
                                WriteType::WithResponse,
                            )
                            .await
                        {
                            status.set(format!("send failed: {e}"));
                        }
                    },
                    on_input: move |data| to_send.set(data),
                    on_disconnect: move |()| async move {
                        let Ok(handler) = blec::get_handler() else { return };
                        if let Err(e) = handler.disconnect().await {
                            status.set(format!("disconnect failed: {e}"));
                        }
                        connected.set(None);
                    },
                }
            } else {
                ul {
                    for device in devices() {
                        li {
                            key: "{device.address}",
                            button {
                                onclick: move |_| {
                                    let address = device.address.clone();
                                    async move {
                                        status.set(format!("connecting to {address}…"));
                                        let handler = match blec::get_handler() {
                                            Ok(handler) => handler,
                                            Err(e) => return status.set(format!("{e}")),
                                        };
                                        let on_disconnect = OnDisconnectHandler::from_sync(move || {
                                            connected.set(None);
                                        });
                                        if let Err(e) =
                                            handler.connect(&address, on_disconnect, false).await
                                        {
                                            return status.set(format!("connect failed: {e}"));
                                        }
                                        let subscribed = handler
                                            .subscribe(
                                                CHARACTERISTIC_UUID,
                                                Some(SERVICE_UUID),
                                                move |data: Vec<u8>| {
                                                    // Notifications arrive more
                                                    // than once, so the handler
                                                    // is `Fn`: take a fresh copy
                                                    // of the signal each time
                                                    // instead of mutating the
                                                    // captured one.
                                                    let mut notified = notified;
                                                    notified.set(
                                                        String::from_utf8_lossy(&data).into_owned(),
                                                    );
                                                },
                                            )
                                            .await;
                                        if let Err(e) = subscribed {
                                            status.set(format!("subscribe failed: {e}"));
                                        } else {
                                            status.set("connected".into());
                                        }
                                        connected.set(Some(address));
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
