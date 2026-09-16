# blec

A cross platform BLE (Bluetooth Low Energy) client for rust.

On Linux, macOS, Windows and iOS this is a thin layer over
[btleplug](https://github.com/deviceplug/btleplug); on Android it uses its own backend.

For a Tauri app use [`tauri-plugin-blec`](../tauri-plugin-blec) instead, which wraps this crate
in a plugin with a JavaScript API and re-exports the whole rust API.

## Usage

```rust
use uuid::{uuid, Uuid};
use blec::models::{ScanFilter, WriteType};
use blec::OnDisconnectHandler;

const CHARACTERISTIC_UUID: Uuid = uuid!("51FF12BB-3ED8-46E5-B4F9-D64E2FEC021B");

async fn example() -> Result<(), blec::Error> {
    let handler = blec::init().await?;
    handler.discover(None, 1000, ScanFilter::None, false).await?;
    handler
        .connect("00:00:00:00:00:00", OnDisconnectHandler::None, false)
        .await?;
    handler
        .send_data(CHARACTERISTIC_UUID, None, &[1, 2, 3], WriteType::WithResponse)
        .await?;
    Ok(())
}
```

`blec::init()` creates the handler once; `blec::get_handler()` returns it from anywhere afterwards.

## Mock backend

The `mock` cargo feature (desktop only) replaces the platform backend with an in-process
simulation (`blec::mock`) that an app or a test can script: devices appear and disappear, links
drop, operations fail, hang or answer slowly, notifications arrive. See the handler tests in
`src/handler/tests`.

## Android internals

Android has no pure-rust path to GATT: `BluetoothGattCallback` and `ScanCallback` are abstract
classes, and `java.lang.reflect.Proxy` only implements interfaces. So the android backend is
Kotlin, in `android/dex/src/main/java/com/plugin/blec/`.

To keep that an implementation detail rather than something every app has to wire up, the Kotlin
is compiled to a single `classes.dex` that is committed as `src/android/classes.dex` and embedded
with `include_bytes!`. At startup `blec` loads it with an `InMemoryDexClassLoader` and binds the
callbacks with `RegisterNatives` (exported `Java_*` symbols never resolve for a dex-loaded class,
because ART looks them up through the class' own loader). The loader's parent is the boot class
loader, not the app's: class loading is parent-first, and with the app's loader the app's own
kotlin stdlib (a Tauri app has one) would shadow the shrunk copy R8 optimized the Kotlin against,
which shows up as `IllegalAccessError` on stdlib internals. The Kotlin side only needs the
framework, so it shares nothing with the app. **An app using `blec` therefore needs no
gradle module and no kotlin — only the permissions in its manifest.**

Rebuild the dex after changing anything under `android/dex/src` and commit the result:

```bash
crates/blec/android/build-dex.sh   # needs ANDROID_HOME and a JDK 17+
```

The build shrinks the kotlin stdlib into the same dex with R8 and fails if the result spilled
into a `classes2.dex`, which `InMemoryDexClassLoader(ByteBuffer, ClassLoader)` cannot load.

Three things to know when using this:

- **`blec` needs a `JavaVM` and a `Context`.** `blec::init()` takes both from
  [`ndk-context`](https://crates.io/crates/ndk-context), which Dioxus initializes but Tauri
  (tao 0.35) does not. A host without `ndk-context` calls
  `blec::android::init_with(env, context)` first, from any thread with a `JNIEnv`;
  `tauri-plugin-blec` does that from `wry::prelude::dispatch` once the app is ready.
- **Dynamic code loading can be blocked.** Hardened builds (for example the GrapheneOS "dynamic
  code loading" toggle) refuse `InMemoryDexClassLoader`. `blec::init()` then fails with
  `Error::Android` explaining it.
- **A main `Looper` must run.** `BluetoothLeScanner` and the gatt callbacks post to it. Any normal
  android app has one; a headless process has to run one itself.

Runtime permissions need an `Activity`, which nobody here owns. `blec` tracks the current one
through `Application.ActivityLifecycleCallbacks`; a host that has an activity earlier can hand it
over with `blec::android::set_activity` (`tauri-plugin-blec` does this through
`wry::prelude::dispatch`). Permission results are picked up by re-checking the permissions when
the activity is resumed again, since there is no `onRequestPermissionsResult` to hook into.

## License

MIT or Apache-2.0, at your option.
