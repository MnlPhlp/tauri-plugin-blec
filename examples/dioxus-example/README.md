# blec in a Dioxus app

The same scan / connect / send / subscribe flow as
[`../plugin-blec-example`](../plugin-blec-example), but without tauri: this app uses the
[`dioxus-blec`](../../crates/dioxus-blec) hooks on top of the [`blec`](../../crates/blec) crate.

Run [`../test-server`](../test-server) on another machine to have something to talk to.

## Desktop

```bash
dx serve
```

## Android

```bash
dx serve --platform android --device
```

`--device` makes `dx` build for the phone attached over `adb`. Without it `dx` builds for the
emulator (`x86_64`) even when no emulator is running, and the install fails on an arm64 phone with
`INSTALL_FAILED_NO_MATCHING_ABIS`.

There is nothing to set up for bluetooth. `Dioxus.toml` only declares `min_sdk = 26`; the
permissions come with `dioxus-blec`, which `dx` merges into the manifest, and the android
implementation is a dex embedded in `blec`. See the android section of the
[`dioxus-blec` README](../../crates/dioxus-blec/README.md#android).

"Check permissions" asks the user for the bluetooth permissions; on a fresh install the system
dialog also appears by itself on the first scan. If the user denied them before, android will not
show the dialog again and the button sends them to the app settings instead.

## Without a bluetooth adapter

```bash
dx serve --features mock
```

replaces the platform backend with `blec::mock`, which has no devices until the app adds some —
see the mock section of the [`blec` README](../../crates/blec/README.md).
