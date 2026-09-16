# Research: Rust BLE on Android without a Gradle/Kotlin step for the consumer

Collected 2026-09-16 while planning the `blec` core split (see `plan-blec-core-split.md`).
Versions and URLs are as of that date. Everything marked *unverified* was not confirmed against a primary source.

## TL;DR

- A fully Kotlin-free GATT client is impossible: `BluetoothGattCallback` and `ScanCallback` are
  `public abstract class`es; `java.lang.reflect.Proxy` only implements interfaces; `BluetoothGatt`'s
  constructor is package-private and `IBluetoothGattCallback` is a hidden AIDL interface. Only the
  deprecated `BluetoothAdapter.LeScanCallback` (scan only) is an interface.
- Embedding a prebuilt `classes.dex` in the Rust crate and loading it with
  `dalvik.system.InMemoryDexClassLoader` + JNI `RegisterNatives` is proven and shipping in several
  Rust crates. It removes every consumer-side Gradle step; only manifest permissions remain.
- Dioxus 0.7.4+ can declare manifest permissions from `Dioxus.toml` and exposes the Activity via
  `ndk-context`. Tauri exposes the Application via `ndk-context` and the Activity via
  `tauri::wry::prelude::dispatch`.

## 1. btleplug on Android (0.13.0, 2026-08-31)

- Android deps: `jni = "0.22"` (0.22.4 resolves), `once_cell`. No `ndk-context`, no `JNI_OnLoad`,
  never asks for a Context: `BluetoothAdapter.getDefaultAdapter()`, `device.connectGatt(null, false, cb)`.
- `jni-utils` is vendored (`src/droidplug/jni_utils/`, Java `io.github.gedgygedgy.rust.**`) since 0.12.
- Min SDK 24 since 0.13 (PR #467).
- **jni version split:** btleplug wants jni 0.22; tauri 2.11 / tao 0.35 / wry 0.55 / dioxus 0.7 use
  jni 0.21.1. Both are already in this repo's `Cargo.lock`. `btleplug::platform::init(env: &mut jni::Env)`
  takes a 0.22 `Env`, so hosts on 0.21 must go through raw pointers.
- Java side is shipped as **source** in a Gradle library module (`src/droidplug/java/`, namespace
  `com.nonpolynomial.btleplug.android`, compileSdk 34, minSdk 24). No prebuilt jar/aar/dex.
  `scripts/build-java.sh` builds an AAR; the maintainer's advice is to copy the AAR into the app repo (issue #418).
  R8 keep rules needed: `-keep class com.nonpolynomial.** { *; }`, `-keep class io.github.gedgygedgy.** { *; }`.
- Init (`src/droidplug/jni/mod.rs`): `env.get_java_vm()`, `find_class(".../impl/Adapter")`,
  `register_native_methods` for `reportScanResult` and `onConnectionStateChanged`, pre-warms ~18 classes
  through `LoaderContext::default()` (thread-context ClassLoader then `FindClass`). Background threads
  `attach_current_thread()` and hit the cache. `jni 0.22` has `LoaderContext::Loader(&JClassLoader)`
  (the hook a dex design would use) but btleplug hardcodes the default.
- Known issues (github.com/deviceplug/btleplug/issues/N): #272 R8 stripping, #311 pending
  `ClassNotFoundException`, #291/#395/#417/#428 `unwrap on None` from an unpopulated class cache (root
  cause found in #428, fixed by vendoring in 0.12), #427 `ThreadDetached` from tokio workers, #359/#450
  "Droidplug has not been initialized"/class not found (Gradle wiring), #375/#399 NPEs, #416/#281 jni
  version mismatch with host. In-repo `docs/plan-fix-android-test-failures.md`: descriptor read/write
  futures never complete on Android; `ValueNotification.service_uuid` hardcoded. #421 "Consider using
  inMemoryDexClassLoader" closed without a PR.
- Verdict: reuse btleplug's `api` traits/types (as this plugin does) but not its Android backend.

Sources: raw.githubusercontent.com/deviceplug/btleplug/master/{Cargo.toml, src/droidplug/jni/mod.rs,
src/droidplug/java/build.gradle, scripts/build-java.sh}, github.com/deviceplug/btleplug/pull/462, issues/421.

## 2. Embedded dex + `InMemoryDexClassLoader` — prior art and pitfalls

Prior art (all Gradle-free for the consumer):

| Project | Version | Loader | Dex provenance | JNI crate |
|---|---|---|---|---|
| `android-ble` (wuwbobo2021) | 0.2.2, 2026-07 | `jni-min-helper::load_dex` | build.rs javac+d8 via `android-build` 0.1.4, **falls back to committed `java/classes.dex`** | jni 0.22.4, `bind_java_type!{ load_class = ... }` |
| `jni-min-helper` | 0.4.7, 2026-08 | API>=26 `InMemoryDexClassLoader`; <26 `DexClassLoader` in `codeCacheDir` | same | jni 0.22.4 |
| Slint android backend | master | identical to jni-min-helper (same author, PR #7204) | build.rs panics without SDK | jni 0.22 |
| `seamless-android` (ciyoxe) | 0.1.0, 2026-08, prototype | chained `InMemoryDexClassLoader`, parent = app loader via ndk-context | committed dex blobs | `jni-sys` only |
| `robius-authentication` | archived 2025 | `InMemoryDexClassLoader(buf, null)` | build.rs via `android-build` | jni |
| Dioxus `manganis::android::CallbackSystem` | 0.7.4+ | `InMemoryDexClassLoader(ByteBuffer, ClassLoader)`, `new_direct_byte_buffer`, `loadClass`, `register_native_methods` | caller supplies `&'static [u8]` | jni 0.21 |

`android-ble` is BLE-specific with a bluest-0.6-shaped API (scan, connect, discover, read/write, notify,
MTU, RSSI, pair), Android 7+, uses `ndk_context`, warns that `block_on` on the main thread blocks
forever (needs the main Looper), and has `request_permissions()` unimplemented.
https://github.com/wuwbobo2021/android-ble-rs

Java/JNI API facts:
- `InMemoryDexClassLoader(ByteBuffer, ClassLoader)` API 26; `(ByteBuffer[], ClassLoader)` API 27;
  `(ByteBuffer[], String librarySearchPath, ClassLoader)` API 29. `parent` may be null.
- `jni 0.22.4` (2026-03; 0.22.0/0.22.1 yanked): `unsafe fn register_native_methods<T>(&mut self, class: T, methods: &[NativeMethod])`,
  `NativeMethod::from_raw_parts(name: &JNIStr, sig: &JNIStr, fn_ptr: *mut c_void)`,
  `unsafe fn new_direct_byte_buffer(&mut self, data: *mut u8, len: usize)`, `GlobalRef` → `Global<T>`,
  `JNIEnv` → `Env`/`EnvUnowned`, `native_method!` (catch_unwind by default), `bind_java_type!`.
  https://docs.rs/jni/latest/jni/struct.Env.html

Pitfalls:
- **(a) `Java_*` exported symbols do not work for dex-loaded classes.** ART
  `Libraries::FindNativeMethodInternal` (runtime/jni/java_vm_ext.cc) only searches libraries loaded by
  the declaring class's ClassLoader. The `.so` belongs to the app's `PathClassLoader`, the shim to the
  in-memory loader → `UnsatisfiedLinkError`. `RegisterNatives` is mandatory.
- **(b) `FindClass` from native threads** sees only the system loader. Always go through the own
  loader's `loadClass`; keep the `JClassLoader` in a `static OnceLock<Global<...>>`.
- **(c) Threads:** `BluetoothGatt.runOrQueueCallback` runs callbacks on a Binder thread unless a Handler
  is passed via the API-26 `connectGatt(ctx, auto, cb, transport, phy, handler)`. `BluetoothLeScanner`
  posts every `onScanResult` to `new Handler(Looper.getMainLooper())` → the main thread must be
  pumping or no scan results arrive. Never block in callbacks; hand off via channels.
- **(d) Lifetime:** hold loader and classes as globals for process lifetime; guard against callbacks
  in flight during `Drop` (seamless-android documents a UAF in earlier `finalize()`-based designs).
  Use `panic=unwind` + `catch_unwind` at every native entry.
- **(e) R8/ProGuard:** the host's shrinker cannot touch bytes inside the `.so`; no keep rules for
  consumers. Shim references to `android.*` resolve via the boot loader; references to host-app
  classes only if the app loader is the parent. Runtime-loaded dex is not AOT-compiled (interpreted/JIT).
- **(f) Android 14 safer dynamic code loading:** the read-only-file check lives in
  `DexFile_openDexFileNative` only; `DexFile_openInMemoryDexFilesNative` has no such check, so
  `InMemoryDexClassLoader` is unaffected. A file-based `DexClassLoader` fallback on API>=34 needs
  `File.setReadOnly()` first.
- **(g) GrapheneOS** has a per-app "Dynamic code loading from memory" toggle (off by default); when
  on, `InMemoryDexClassLoader` throws `SecurityException`. Catch and report clearly.
- **(h) Google Play** forbids *downloading* executable code / self-updating. A dex embedded in the
  APK's `.so` is neither. No explicit Google statement blessing it was found (*unverified*).
- **(i) Abstract-with-empty-bodies trap:** `ScanCallback`/`BluetoothGattCallback` methods have
  concrete empty bodies; every override must be explicit.

Building the dex: `javac -source 8 -target 8 -cp android.jar` then
`d8 --min-api 26 --lib android.jar --output out/ classes/**/*.class` (or R8 via a Gradle application
module with `minifyEnabled`). `android-build` 0.1.4 (project-robius) wraps this for build.rs. Policies
in the wild: panic without SDK (Slint/robius, breaks `cargo check` downstream), build-or-fallback
(android-ble, recommended), committed-only (seamless). D8 output is not byte-reproducible across versions.

## 3. Dioxus mobile Android integration

- `dioxus`/`dioxus-cli` 0.7.10 stable (2026-07-30), 0.8.0-alpha.1. The Android overhaul is PR #4842,
  shipped in **0.7.4** (2026-03-28). Stack: wry 0.55.1, tao 0.35.2, **jni 0.21.1**, ndk-context 0.1.1.
- `dx build --platform android` renders `packages/cli/assets/android/gen/**.hbs` + `MainActivity.kt.hbs`
  (`package dev.dioxus.main; class MainActivity : WryActivity()`) into
  `target/dx/<app>/<profile>/android/` on **every build**; hand edits are overwritten.
  `WryActivity.kt`, `RustWebView.kt`, `Rust.kt`, `PermissionHelper.kt` come from wry's build.rs.
- `Dioxus.toml` (0.7.4+, schema: github.com/DioxusLabs/dioxus/blob/main/packages/cli/schema.json):

  ```toml
  [application]
  android_manifest = "android/AndroidManifest.xml"   # full replacement, legacy
  android_main_activity = "android/MainActivity.kt"  # must subclass WryActivity
  android_min_sdk_version = 24

  [android]
  min_sdk = 26
  manifest = "android/AndroidManifest.xml"           # merged
  gradle_dependencies = ["..."]
  gradle_plugins = [...]
  proguard_rules = [...]
  features = ["android.hardware.bluetooth_le"]
  [android.permissions]
  "android.permission.ACCESS_FINE_LOCATION" = { description = "..." }

  [permissions]
  bluetooth = { description = "..." }   # = BLUETOOTH_CONNECT + BLUETOOTH_SCAN only
  ```

  Caveat: `bluetooth` emits no `maxSdkVersion` legacy entries and no
  `usesPermissionFlags="neverForLocation"`; use `[android.permissions]` or a merged manifest file.
  In 0.6.x none of this existed. Third-party (mintlify) docs show wrong keys.
- Native plugins (Tauri-plugin analogue, 0.7.4+): `#[manganis::ffi("src/android")] unsafe extern "Kotlin" { ... }`.
  The dir must be a Gradle library module; `dx` copies it to `<root>/plugins/<name>/`, adds
  `include ':plugins:<name>'` and `implementation(project(":plugins:<name>"))`, AGP merges its manifest.
  Generated Rust constructs the class via `(Landroid/app/Activity;)V` inside
  `manganis::android::with_activity`. Still compiles Kotlin, but automatic for the consumer.
  https://docs.rs/manganis/latest/manganis/attr.ffi.html, example `examples/01-app-demos/geolocation-native-plugin/`.
- JNI env: `packages/desktop/src/mobile.rs` runs `tao::android_binding!` + `wry::android_binding!`;
  `android_setup` calls `ndk_context::initialize_android_context(vm, activity)` → under Dioxus
  **`android_context().context()` is the Activity**. `wry::prelude::dispatch(|env, activity, webview| ...)`
  runs on the main thread (must use dioxus's re-exported wry). `manganis::android::with_activity(|env, activity| ...)`
  runs synchronously on the calling thread. Flag: tao dev (0.37) re-added `initialize_android_context`
  with the *Application* context and ndk-context asserts on double init; a future tao bump may change
  what `context()` returns.
- Permissions today: community pattern from discussion #3475 (raw ndk_context + jni `call_method`),
  `WryActivity.requestPermissions(Array<String>, (Boolean?) -> Unit)` in wry's Kotlin,
  `manganis::android::java::{check_self_permission, request_permissions_via_helper}`. No
  `dioxus::permissions` Rust API; `dioxus-sdk` has no Android implementations. No Dioxus + BLE prior art found.

## 4. How Tauri binds Kotlin plugins (baseline to avoid replicating)

Verified on tauri 2.11.x, tauri-plugin 2.6.x, jni 0.21:
- Plugin build.rs: `tauri_plugin::Builder::new(COMMANDS).android_path("android").build()` needs
  `links = "..."`; `mobile::setup` copies `DEP_TAURI_ANDROID_LIBRARY_PATH` (the `app.tauri` library)
  into `<plugin>/android/.tauri/tauri-api` and prints `cargo:android_library_path=<abs plugin android dir>`.
- App build: `tauri_build::build()` → `mobile::generate_gradle_files` iterates `DEP_*_ANDROID_LIBRARY_PATH`
  and writes `gen/android/tauri.settings.gradle` (`include ':tauri-plugin-blec'` + projectDir) and
  `app/tauri.build.gradle.kts` (`implementation(project(":tauri-android"))`, `implementation(project(":tauri-plugin-blec"))`).
  `MainActivity : TauriActivity`; the CLI verifies the `.so` exports `Java_app_tauri_plugin_PluginManager_handlePluginResponse`.
- Runtime: Kotlin `@TauriPlugin(permissions=[...]) class X(activity): Plugin(activity)`,
  `@Command fun name(invoke: Invoke)`, `Invoke.parseArgs/resolve/reject`, `Channel(id){ send(JSObject) }`
  deserialized from `"__CHANNEL__:<id>"` strings by Jackson. Rust `PluginApi::register_android_plugin`
  → `activity.getAppClass` + `new_object(cls, "(Landroid/app/Activity;)V")` + `PluginManager.load` on
  the main thread via wry's `MainPipe`; `PluginHandle::run_mobile_plugin::<T>(cmd, payload)` →
  `PluginManager.runCommand(int,String,String,String)`; responses via
  `Java_app_tauri_plugin_PluginManager_{handlePluginResponse,sendChannelData}` generated by
  `tauri::android_binding!` inside `#[tauri::mobile_entry_point]`. `ndk-context` is initialised by
  tao's `ndk_glue::onCreate` with the **Application** context. Permissions:
  `requestPermissionForAliases(aliases, invoke, "cb")` → `@PermissionCallback fun cb(invoke)`.
- `tauri` re-exports `wry` and `tao`: `tauri/src/lib.rs:184` `pub use tauri_runtime_wry::{tao, wry};`.
  `wry::prelude::dispatch<F: FnOnce(&mut JNIEnv, &JObject /*activity*/, &JObject /*webview*/) + Send + 'static>(f)`
  (wry 0.55.1 `src/android/mod.rs:522`).
- A non-Tauri host reproducing this would need: a Gradle module, an Activity subclass exposing
  `getPluginManager()/getAppClass()`, two exported `Java_app_tauri_plugin_PluginManager_*` symbols, a
  main-thread JNI dispatcher, the `runCommand`/`__CHANNEL__` JSON protocol, a `tauri.conf.json` asset stub.

## 5. Other Rust BLE crates and how they ship Java

| Crate | Latest | Android | Delivery |
|---|---|---|---|
| `bluest` | 0.6.9 (2025-06) | README "planned"; `main` has scan-only java-spaghetti backend. **PR #46** (2026-06, open) swaps in `android-ble`, deletes the Java | would be Gradle-free via android-ble |
| `android-ble` | 0.2.2 | Android-only, bluest-compatible API | embedded dex |
| `seamless-android` | 0.1.0 | scan/GATT client/GATT server/advertising; `init_with_activity(vm, activity)`; needs `panic=unwind`; tested on one device | committed dex |
| SimpleBLE / `simplersble` | 1.1.1 / 1.1.1-dev34 | C++ JNI + Java bridge `org.simpleble.android.bridge.*` published as Maven AAR `org.simpleble:simpledroidbridge`, minSdk 31; **Rust bindings' build.rs panics on Android**; license BUSL-1.1 | Gradle required |
| `bluey`, `btleplug-kuyoonjo`, `bluetooth-rust` (classic only), `bleasy` (btleplug wrapper) | — | Gradle modules | Gradle |
| `ble-peripheral-rust`, `bluer`, `trouble-host`, `rumble`, `ble-central` | — | no Android central support | — |

`java-spaghetti` 0.2.0 generates proxy `.java` sources but has no dex/embedding mechanism.

## Unverified / open

- Where Dioxus's `CallbackSystem` gets its dex bytes (no `.dex` found committed in the dioxus repo).
- Whether `InMemoryDexClassLoader` copies or borrows the `ByteBuffer` (docs silent; `'static` `include_bytes!` sidesteps it).
- bluest PR #46 review status; whether btleplug #421 "completed" reflects any merged change.
- Which tao bump will change `ndk_context().context()` from Activity to Application under Dioxus.
