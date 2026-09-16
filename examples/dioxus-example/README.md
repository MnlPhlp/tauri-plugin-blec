# blec in a Dioxus app

The same scan / connect / send / subscribe flow as
[`../plugin-blec-example`](../plugin-blec-example), but without tauri: this app uses the
[`blec`](../../crates/blec) crate directly.

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

Everything android needs is in `Dioxus.toml`: `min_sdk = 26`, the `bluetooth_le` feature and the
permissions. `[android.permissions]` cannot express permission attributes, so the
`neverForLocation` flag on `BLUETOOTH_SCAN` and `maxSdkVersion` on the legacy `BLUETOOTH`
permission are raw XML in `[android.raw] manifest`, which `dx` pastes into the generated
manifest.

Do not use `[android] manifest = "<file>"` for this: `dx` 0.7 parses the key but never reads the
file. A permission that only lives there is missing from the app, and android then reports it as
denied although the settings show "Nearby devices" as granted.

There is no gradle module and no kotlin: `blec::init()` loads the android implementation from a
dex embedded in the crate. See [Android internals](../../crates/blec/README.md#android-internals).

"Check permissions" asks the user for the bluetooth permissions; on a fresh install the system
dialog also appears by itself on the first scan. If the user denied them before, android will not
show the dialog again and the button sends them to the app settings instead.

## Without a bluetooth adapter

```bash
dx serve --features mock
```

replaces the platform backend with `blec::mock`, which has no devices until the app adds some —
see the mock section of the [`blec` README](../../crates/blec/README.md).
