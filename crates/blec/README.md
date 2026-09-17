# blec

A cross platform BLE (Bluetooth Low Energy) client for rust.

On Linux, macOS, Windows and iOS this is a thin layer over
[btleplug](https://github.com/deviceplug/btleplug); on Android it uses its own backend.

For a Tauri app use [`tauri-plugin-blec`](../tauri-plugin-blec) instead, which wraps this crate
in a plugin with a JavaScript API and re-exports the whole rust API. For a Dioxus app use
[`dioxus-blec`](../dioxus-blec), which adds hooks and carries the android permissions.

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
Kotlin, in `android/lib`, an ordinary android library module: the bluetooth permissions in its
manifest plus the code that drives the platform bluetooth stack. Rust talks to it over a small JNI
bridge (`src/android/bridge.rs`).

The module is meant to be built into the app by the app's own gradle build, like any other
module. `tauri-plugin-blec` and `dioxus-blec` do that for you: their `android/` directories are
symlinks to `crates/blec/android/lib`, which tauri (`android_path`) and `dx` (`#[manganis::ffi]`)
pick up, so the classes end up in the apk and `blec` finds them in the app's class loader at
startup. **Nothing to set up on the app side.** A custom host adds
`crates/blec/android/lib` to its gradle build the same way (`include(":blec")` with the
directory as `projectDir`, `implementation(project(":blec"))`). The module's `consumer-rules.pro`
keeps the JNI entry points through the app's R8 pass.

### The `embedded-dex` feature

For a host without a gradle build, the cargo feature `embedded-dex` embeds the same Kotlin as a
prebuilt `classes.dex` (`src/android/classes.dex`, committed) and loads it with an
`InMemoryDexClassLoader` when `com.plugin.blec.Bridge` is not in the app. The app's class loader
is always tried first, so an app that does build the module never loads code at runtime even if
some dependency turned the feature on.

Two things to know about the dex path:

- **Dynamic code loading can be blocked.** Hardened builds (for example the GrapheneOS "dynamic
  code loading from memory" toggle) refuse `InMemoryDexClassLoader`; `blec::init()` then fails
  with `Error::Android` explaining it. This is why the gradle module is the default.
- The dex is loaded with the *boot* class loader as parent, not the app's: class loading is
  parent-first, and with the app's loader the app's own kotlin stdlib (a Tauri app has one) would
  shadow the shrunk copy R8 optimized the Kotlin against, which shows up as `IllegalAccessError`
  on stdlib internals. Exported `Java_*` symbols never resolve for a dex-loaded class either, so
  the natives are bound with `RegisterNatives` (also on the gradle path, where it is simply the
  more robust choice).

Rebuild the dex after changing anything under `android/lib/src` and commit the result; CI fails
when the committed dex does not match the sources:

```bash
crates/blec/android/build-dex.sh   # needs ANDROID_HOME and a JDK 17+
```

The build shrinks the kotlin stdlib into the same dex with R8 and fails if the result spilled
into a `classes2.dex`, which `InMemoryDexClassLoader(ByteBuffer, ClassLoader)` cannot load.

### Either way

- **`blec` needs a `JavaVM` and a `Context`.** `blec::init()` takes both from
  [`ndk-context`](https://crates.io/crates/ndk-context), which Dioxus initializes but Tauri
  (tao 0.35) does not. A host without `ndk-context` calls
  `blec::android::init_with(env, context)` first, from any thread with a `JNIEnv`;
  `tauri-plugin-blec` does that from `wry::prelude::dispatch` once the app is ready.
- **A main `Looper` must run.** `BluetoothLeScanner` and the gatt callbacks post to it. Any normal
  android app has one; a headless process has to run one itself.
- Runtime permissions need an `Activity`, which nobody here owns. `blec` tracks the current one
  through `Application.ActivityLifecycleCallbacks`; a host that has an activity earlier can hand it
  over with `blec::android::set_activity` (`tauri-plugin-blec` does this through
  `wry::prelude::dispatch`). Permission results are picked up by re-checking the permissions when
  the activity is resumed again, since there is no `onRequestPermissionsResult` to hook into.

For contributors: `crates/tauri-plugin-blec/android` and `crates/dioxus-blec/android` are git
symlinks. On Windows clone with `git config core.symlinks true` (needs developer mode or admin),
otherwise they check out as text files. Published crates are unaffected: `cargo package` stores
the linked files as regular files.

## License

MIT or Apache-2.0, at your option.
