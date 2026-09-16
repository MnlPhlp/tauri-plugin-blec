# Split tauri-plugin-blec into a host-agnostic `blec` core + thin Tauri plugin

## Context

The BLE handler (`src/handler.rs`, `models.rs`, `error.rs`, `mock/`, `handler/tests/`) is already
Tauri-free. Only three things bind the crate to Tauri:

1. `src/lib.rs` / `src/commands.rs` – plugin registration and `#[tauri::command]`s (expected, stays in the plugin).
2. `src/android.rs` – the Android backend talks to Kotlin exclusively through
   `tauri::plugin::PluginHandle::run_mobile_plugin` (blocking) + `tauri::ipc::Channel` callbacks.
3. `android/` – Kotlin extends `app.tauri.plugin.Plugin`, uses `Invoke`/`Channel`/`JSObject`/Jackson
   `parseArgs`, `requestPermissionForAliases`/`@PermissionCallback`.

Goal: a core crate `blec` usable from plain Rust (primary target: a Dioxus 0.7 mobile app) on desktop
and Android, where Android needs **no Gradle/Kotlin step** from the consumer (only manifest
permissions), and this repo's Tauri plugin becomes a thin command layer over the core.

Decisions taken with the user: core crate name `blec` (user owns it on crates.io, currently 0.3.4 →
new major), Cargo workspace in this repo, Dioxus 0.7.x example, Tauri plugin keeps a manifest-only
Android Gradle module.

## Options considered for Android

| Option | Consumer effort | Verdict |
|---|---|---|
| A. Pure Rust via JNI, no bytecode | none | **Impossible** for GATT: `BluetoothGattCallback`/`ScanCallback` are abstract classes; `java.lang.reflect.Proxy` only does interfaces. |
| B. Keep Kotlin as Gradle module (Tauri `android_path`, Dioxus `#[manganis::ffi]`) | Gradle module per host framework | Two host-specific glue layers, nothing for plain Rust. Rejected. |
| C. Replace Kotlin with the `android-ble` crate (embedded dex + Rust GATT logic, bluest-shaped API) | none | Throws away ~1800 lines of tuned Kotlin (write queue, timeouts, transient-status handling); permissions unimplemented; 0.2.x crate; API mismatch with the btleplug traits `Handler` uses. Possible future direction. |
| **D. Keep the Kotlin, strip Tauri from it, compile to a `classes.dex` committed in the crate, `include_bytes!` it, load at runtime via `InMemoryDexClassLoader` + JNI `RegisterNatives`** | manifest permissions only | **Chosen.** Proven pattern (android-ble/jni-min-helper, Slint Android backend, seamless-android, Dioxus' own `manganis::android::CallbackSystem`). Host R8 can't touch the dex. API 26+ = current minSdk. |

Facts D relies on (researched):
- `ndk_context::android_context()` gives `vm()` + `context()` under both Tauri (tao → **Application**) and Dioxus 0.7.4+ (**Activity**). Both hosts expose `wry::prelude::dispatch(|env, activity, webview| ..)` (tauri re-exports `wry` at `tauri::wry`) so a host layer can hand the Activity to the core for runtime-permission prompts.
- `Java_*` exported symbols do **not** resolve for dex-loaded classes (ART scopes lookup by class loader) → `RegisterNatives` is mandatory.
- `BluetoothLeScanner` posts to the main Looper; Tauri/Dioxus pump it. Headless hosts must run a Looper (documented limitation).
- Nobody owns the Activity subclass → no `onRequestPermissionsResult`. Request with `Activity.requestPermissions`, re-check `checkSelfPermission` in `onActivityResumed` (`Application.registerActivityLifecycleCallbacks`), which also tracks the current Activity as fallback.
- Dioxus 0.7.4+ `Dioxus.toml`: `[android.permissions]`, `[android] manifest = ...`, `[android] features`.
- jni: tauri/wry/tao use `jni 0.21`, btleplug 0.13 pulls `jni 0.22` on Android (already in `Cargo.lock`). Core uses **jni 0.22** (`register_native_methods`, `native_method!` with catch_unwind, `Global<T>`); the VM comes from ndk-context raw pointers so the host's jni version is irrelevant.

## Desktop/iOS backend: btleplug stays

btleplug 0.13 remains the backend for Linux, macOS, iOS and Windows, and its `api` traits remain the
internal backend abstraction that the Android and mock backends implement. Switching libraries would
rewrite the handler, mock and Android adapter for no functional gain, and no other maintained crate
covers all four platforms. On Android btleplug is compiled only for its traits/types; its `droidplug`
backend is never initialised.

Deferred follow-up (separate refactor, not part of this split): replace the btleplug traits with a
small internal `Backend` trait covering only the methods the handler uses. That would remove the sync
`services()` problem at the root instead of caching around it, drop the `todo!()` stubs on Android,
simplify the mock, and stop compiling btleplug for the Android target altogether.

## Target layout

```
Cargo.toml                      # [workspace] members = ["crates/*"], exclude examples (they have own lockfiles)
crates/blec/                    # core crate, published as `blec`
  Cargo.toml                    # no tauri dep. [target.'cfg(target_os="android")'.dependencies] jni=0.22, ndk-context=0.1, serde_json, base64, async-trait, tokio-stream
  src/lib.rs                    # init/try_init/get_handler/check_permissions/ALLOW_IBEACONS, re-exports
  src/handler.rs (+ handler/tests/), models.rs, error.rs, mock/   # moved verbatim (git mv), doc examples re-pointed
  src/android/mod.rs            # Adapter/Manager/Peripheral (today's android.rs, re-targeted to Bridge)
  src/android/bridge.rs         # JNI bridge: dex loading, natives, pending-call + channel maps
  src/android/classes.dex       # committed prebuilt dex (built from android/)
  android/                      # Kotlin sources + Gradle project that only produces classes.dex
    build.gradle.kts, settings.gradle, build-dex.sh, proguard-rules.pro
    src/main/java/com/plugin/blec/{Bridge.kt, Invoke.kt, BleClient.kt, Peripheral.kt}
crates/tauri-plugin-blec/       # the Tauri plugin (moved: build.rs, permissions/, guest-js/, package.json, rollup, dist-js, src/{lib,commands}.rs)
  android/                      # manifest-only module: build.gradle.kts + src/main/AndroidManifest.xml
examples/plugin-blec-example    # path dep updated to ../../crates/tauri-plugin-blec
examples/dioxus-example         # new: dx 0.7 app using `blec` directly
examples/{test-server,stress-server,stress-client}  # path deps updated
```

Root `Cargo.toml` becomes a virtual manifest. `tauri_plugin::Builder::android_path("android")` and
`permissions/` are resolved relative to `crates/tauri-plugin-blec/`, so moving the plugin into a
subdirectory is fine (`links = "tauri-plugin-blec"` stays). npm `package.json`/`rollup.config.js`
move with it; `.github/workflows/docs.yml` path updated.

## Part 1 – core crate `crates/blec`

Public API (kept identical to today's Rust API so the Tauri plugin can re-export it):

```rust
pub use error::Error; pub use handler::{Handler, OnDisconnectHandler, SubscriptionHandler};
pub use models::{Timeouts, TimeoutsMs}; pub mod models; #[cfg(feature="mock")] pub mod mock;
pub static ALLOW_IBEACONS: AtomicBool;
pub async fn init() -> Result<&'static Handler>;   // Handler::new() + HANDLER.set(); idempotent
pub fn get_handler() -> Result<&'static Handler>;
pub async fn check_permissions(ask_if_denied: bool) -> Result<bool>;  // true off-android
#[cfg(target_os="android")] pub mod android { pub fn set_activity(env: *mut JNIEnv, activity: jobject); pub fn init() -> Result<()>; }
```

- Keep the `static HANDLER: OnceCell<Handler>` + `&'static self` pattern (minimal change; the handler spawns tasks that borrow itself). `init()` is async and returns the handler; the Tauri layer wraps it in `async_runtime::block_on` as today (`lib.rs:29`).
- `Handler::new()` on Android calls `android::init()` first (idempotent, `OnceCell`).
- `error.rs:46-48` `PluginInvoke(tauri::…PluginInvokeError)` → `#[cfg(android)] Android(String)` plus `Timeout` reuse; `impl From<jni::errors::Error>`.
- `handler.rs` doc examples: `tauri_plugin_blec::get_handler()` → `blec::get_handler()`.
- `check_permissions` becomes async (it awaits a Kotlin resolve that may take a user interaction). The Tauri command `check_permissions` in `commands.rs:233` becomes `async fn` (Tauri supports async commands; JS API unchanged).
- Feature `mock` unchanged (gated `cfg(any(test, feature = "mock"))`).

### Android bridge (`crates/blec/src/android/bridge.rs`)

Protocol mirrors Tauri's Invoke/Channel so `android/mod.rs` and the Kotlin change mechanically:

| Direction | JNI call | Notes |
|---|---|---|
| Rust → Kotlin | `static void Bridge.invoke(long id, String cmd, String argsJson)` | replaces `run_mobile_plugin`; non-blocking on Kotlin side |
| Rust → Kotlin | `static void Bridge.init(Context ctx)` / `static void Bridge.setActivity(Activity a)` | |
| Kotlin → Rust (native) | `static void Bridge.nativeResolve(long id, String json)` | completes pending oneshot |
| Kotlin → Rust (native) | `static void Bridge.nativeReject(long id, String message)` | |
| Kotlin → Rust (native) | `static void Bridge.nativeChannelSend(long channelId, String json)` | replaces `Channel.send` |

Rust side:
- `struct Bridge { vm: JavaVM, loader: Global<JObject>, bridge_class: Global<JClass>, pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value,String>>>>, channels: Mutex<HashMap<u64, mpsc::UnboundedSender<Value>>>, next_id: AtomicU64 }` in a `static BRIDGE: OnceCell<Bridge>`.
- `async fn call<P: Serialize, R: DeserializeOwned>(cmd: &'static str, params: P, timeout: Duration) -> Result<R>`: allocate id, insert oneshot, attach thread (`vm.attach_current_thread()`), call `Bridge.invoke`, await oneshot with `timeout + IPC_TIMEOUT_MARGIN` (keeps semantics of `call_plugin_with_timeout`, `android.rs:678`). Removes the `spawn_blocking` hop.
- `fn channel<T: DeserializeOwned>() -> (Channel, mpsc::UnboundedReceiver<T>)` where `#[derive(Serialize)] struct Channel(u64)` serializes as a plain integer id (Kotlin `Channel(id)`), so `ScanParams { on_device: Channel }`, `NotifyParams { channel }`, `"events"` keep their JSON shape. A small forwarding task deserializes `Value → T`.
- Natives implemented with `jni::native_method!` (catch_unwind built in); they only take the lock and send; never block.
- `init()`: `ndk_context::android_context()` → `JavaVM::from_raw(vm)`, `attach_current_thread`; `context.getClassLoader()` as parent; `new_direct_byte_buffer(include_bytes!("classes.dex"))`; `new_object("dalvik/system/InMemoryDexClassLoader", "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V")`; `loader.loadClass("com.plugin.blec.Bridge")`; `register_native_methods` (3 natives); `Bridge.init(context)`; store `Global`s. Map `SecurityException` (GrapheneOS DCL toggle) to `Error::Android("dynamic code loading blocked…")`.
- `set_activity(env, activity)`: `Bridge.setActivity(activity)`; hosts call it from the main thread via `wry::prelude::dispatch`.

`android/mod.rs` (today's `android.rs`) call-site mapping:
- every `get_handle().run_mobile_plugin::<R>(cmd, params)` → `bridge::call::<_, R>(cmd, params, IPC_DEFAULT_TIMEOUT).await` (quick queries) or the existing per-op timeout for `connect/request_mtu/disconnect/discover_services/write/read/subscribe/unsubscribe`.
- `events()` / `notifications()` / `start_scan()` → `bridge::channel()` + forwarding closure (same match on `LinkStillConnected`, same `PeripheralResult` handling).
- `services()` (sync trait method, `android.rs:408-462`): cache the service list on the `Peripheral` in `discover_services()` (fetch via `bridge::call("services")` right after `discover_services` resolves) and return the cache from `services()`. Removes the blocking `.expect` panic path. Kotlin `services` command stays.
- `check_permissions` (`android.rs:80`) → async bridge call sending **both** `askIfDenied` and `allowIbeacons` (fixes the existing bug where `allowIbeacons` was never sent).
- Remove `HANDLE`, `init(app, api)`, `tauri::*` imports.

### Kotlin refactor (`crates/blec/android/src/main/java/com/plugin/blec/`)

- New `Bridge.kt`: `object Bridge` with `@JvmStatic fun init(ctx: Context)`, `@JvmStatic fun setActivity(a: Activity)`, `@JvmStatic fun invoke(id: Long, cmd: String, args: String)` doing `when(cmd)` over today's 20 command names → calls on a `BleClientPlugin`-like class (renamed `BlecPlugin`, no `Plugin` superclass), and `@JvmStatic external fun nativeResolve/nativeReject/nativeChannelSend`. `init` registers `ActivityLifecycleCallbacks` on `ctx.applicationContext as Application` to track the resumed Activity and to re-run pending permission checks. If `ctx is Activity`, use it as the initial activity.
- New `Invoke.kt`: `class Invoke(val id: Long, val args: JSONObject) { fun resolve(obj: JSONObject? = null); fun reject(msg: String) }` (calls the natives, guards double-resolution) and `class Channel(val id: Long) { fun send(obj: JSONObject) }`. Use `org.json` (framework) instead of Tauri `JSObject`/`JSArray`/Jackson.
- Arg classes (`ConnectParams`, `NotifyParams`, `WriteParams`, `ReadParams`, `CheckPermissionsParams`, `MtuParams`, `BleClient.ScanParams`): drop `@InvokeArg`, add `companion object { fun from(j: JSONObject) }` (UUID parse, base64 decode for `data`, `Channel(j.getLong(..))`). Replace `invoke.parseArgs(X::class.java)` (BleClientPlugin.kt, BleClient.kt:119/228, Peripheral.kt:987/1033) with `X.from(invoke.args)`.
- `BleClient`/`Peripheral`: `activity: Activity` → `context: Context` (only `getSystemService`, `getSharedPreferences`, `connectGatt`, Toast need a Context). Replace `ContextCompat.getSystemService(activity, …)` → `context.getSystemService(BluetoothManager::class.java)`; `ActivityCompat.startActivityForResult(enableBtIntent)` (BleClient.kt:130) → `Bridge.currentActivity?.startActivityForResult(...) ?: context.startActivity(intent.addFlags(FLAG_ACTIVITY_NEW_TASK))`. This drops all androidx deps → smaller dex.
- Permissions (`BleClientPlugin.kt:266-318`): `hasBTPermissions()` via `context.checkSelfPermission(...) == PERMISSION_GRANTED` for the SDK-appropriate list (S+: `BLUETOOTH_SCAN`,`BLUETOOTH_CONNECT`; else `BLUETOOTH`,`BLUETOOTH_ADMIN`; `+ ACCESS_FINE_LOCATION` when `allowIbeacons`). `check_permissions`: if granted → resolve; else if an Activity is known → store `pendingPermissionInvoke`, `activity.requestPermissions(missing, REQ_CODE)`; on next `onActivityResumed` re-check and run today's `permissionsCallback` logic (Settings intent + Toast when `askIfDenied`), resolve. No Activity → reject `"no activity available to request permissions; call blec::android::set_activity"`.
- `sendEvent`/`reportLinkStillHeld`/notifications/scan results: `JSObject` → `JSONObject`, `channel.send(...)` unchanged in shape.
- Everything in `Peripheral.kt` (GATT callback, write queue, timeouts) stays.

### Dex build pipeline (`crates/blec/android/`)

- Gradle project with **one `com.android.application` module** (dummy app with no Activity) containing the Kotlin, `minSdk 26`, `compileSdk 34`, `isMinifyEnabled = true`, R8 rules: `-keep class com.plugin.blec.Bridge { *; }`, `-keepclasseswithmembernames class * { native <methods>; }`, `-dontobfuscate` (readable stack traces). Kotlin stdlib gets shrunk into the single `classes.dex` (expected ~150–300 KB). Pin Kotlin plugin version (`settings.gradle`) for reproducibility. Drop `:tauri-android` and androidx deps.
- `build-dex.sh`: `./gradlew assembleRelease && unzip -o -j build/outputs/apk/release/*.apk classes.dex -d ../src/android/` and fail if the APK has `classes2.dex` (multidex would need the `ByteBuffer[]` ctor / API 27).
- `classes.dex` is **committed**. No build.rs in the core (keeps `cargo check`/docs.rs working without an Android SDK). CI job (`ubuntu-latest` has an Android SDK) runs `build-dex.sh` and fails on `git diff --exit-code crates/blec/src/android/classes.dex` only when Kotlin sources changed (use `paths` filter), so drift is caught without forcing byte-identical output on unrelated changes.
- README section "Android internals" explaining the dex, how to rebuild, GrapheneOS caveat, main-Looper requirement.

## Part 2 – Tauri plugin `crates/tauri-plugin-blec`

- `Cargo.toml`: depends on `blec = { version = "…", path = "../blec" }`, `tauri`, `serde`, `tokio`, `uuid`; feature `mock = ["blec/mock"]`. `[package.metadata.docs.rs]` kept.
- `src/lib.rs`: `pub use blec::{Handler, OnDisconnectHandler, SubscriptionHandler, Timeouts, TimeoutsMs, Error, models, get_handler, check_permissions, ALLOW_IBEACONS}; #[cfg(feature="mock")] pub use blec::mock;` so existing `tauri_plugin_blec::get_handler()` users keep working. `try_init()` = `async_runtime::block_on(blec::init())?` then `Builder::new("blec").invoke_handler(...).setup(|app, _api| { #[cfg(android)] tauri::wry::prelude::dispatch(|env, activity, _| blec::android::set_activity(env.get_raw(), activity.as_raw())); Ok(()) }).on_event(exit → disconnect)` (`lib.rs:40-54` unchanged).
- `src/commands.rs`: unchanged except `check_permissions` → `async` and `set_android_mtu` → `blec::Handler::set_android_mtu_request` (already an associated fn).
- `android/`: keep only `build.gradle.kts` (com.android.library, namespace `com.plugin.blec.manifest`, no Kotlin plugin, no deps) and `src/main/AndroidManifest.xml` (current content). `settings.gradle` stays minimal (no `:tauri-android` include). `build.rs` unchanged (`android_path("android")`). Delete `android/build`, `.gradle`, `.tauri` leftovers from the tree and gitignore them.
- `guest-js/`, `permissions/`, `package.json`, `rollup.config.js`, `typedoc.json` move unchanged (JS API is not affected).
- Version bump: `tauri-plugin-blec` 0.15.0; `blec` 0.15.0 (jump past 0.3.4 so versions of the two crates line up); npm package stays as is.

## Part 3 – Dioxus example `examples/dioxus-example` (dx 0.7.x)

- Minimal app: `blec::init()` in a `use_resource`, scan button streaming `Vec<BleDevice>` into a `Signal`, connect + subscribe showing bytes, `check_permissions(true)` button.
- `Dioxus.toml`: `[android] min_sdk = 26`, `features = ["android.hardware.bluetooth_le"]`, `[android.permissions]` for `BLUETOOTH_SCAN` (note: `neverForLocation` flag needs `[android] manifest = "android/AndroidManifest.xml"` merge file – include one), `BLUETOOTH_CONNECT`, `BLUETOOTH` (maxSdk 30), `BLUETOOTH_ADMIN`, `ACCESS_FINE_LOCATION`.
- No `blec-dioxus` helper crate: under Dioxus 0.7 the ndk-context Context *is* the Activity, so nothing beyond `blec::init()` is needed; a `use_blec_scan()`-style hook would be ~20 lines of app code. Revisit only if real shared content appears.
- README for the example: `dx serve --platform android`, permission flow, desktop run.

## Implementation order

1. Workspace: create `crates/blec`, `crates/tauri-plugin-blec`; `git mv` files; root virtual manifest; fix example path deps; `cargo test` (desktop, mock) green before touching Android.
2. Core Android bridge: `bridge.rs`, re-target `android/mod.rs`, new `Error::Android`, `services()` caching, `check_permissions` async. `cargo check --target aarch64-linux-android -p blec` with a placeholder dex.
3. Kotlin: `Bridge.kt`, `Invoke.kt`, de-Tauri `BleClientPlugin`→`BlecPlugin`, `BleClient`, `Peripheral`; Gradle app module + R8 rules; `build-dex.sh`; commit dex.
4. Tauri layer: slim `lib.rs`, re-exports, manifest-only `android/`, `set_activity` via `tauri::wry::prelude::dispatch`.
5. Dioxus example; READMEs; CI (`cargo test`, `check --features mock`, `check --target aarch64-linux-android` for both crates, dex-drift job); version bumps.

## Verification

- Desktop: `cargo test --workspace` (handler tests via mock), `cargo check --workspace --features mock`, `cargo check -p blec -p tauri-plugin-blec --target aarch64-linux-android`.
- Dex: `crates/blec/android/build-dex.sh` produces a single `classes.dex`; `unzip -l` shows no `classes2.dex`.
- Tauri on device: `examples/plugin-blec-example` → `npm run tauri android dev`; scan, connect, read/write/notify against `examples/test-server`; deny permission → `check_permissions(true)` opens app settings and Toast; rotate/background the app and reconnect.
- Dioxus on device: `dx serve --platform android` in `examples/dioxus-example`; same smoke test; fresh install shows the system permission dialog on first scan.
- Stress: `examples/stress-client` scenarios against `stress-server` on Android to confirm the write queue/timeouts survived the bridge swap.

## Risks / open points

- Runtime-loaded dex is interpreted/JIT (not AOT) – negligible for this shim.
- If a future tao bump changes what ndk-context's `context()` is, the lifecycle-callback Activity tracking plus `set_activity` still cover permission requests.
- `check_permissions` becoming async is a Rust API change for direct Rust callers (JS unaffected).
- Kotlin `object` init runs on first `loadClass`/`init` call from Rust; all JNI calls must attach the current thread (tokio workers) – wrap in a helper.
- `blec` 0.3.4 users: publish 0.15.0 with a README note that the API is the former `tauri_plugin_blec` handler API.
