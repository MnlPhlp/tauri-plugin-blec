# Notes: splitting out a host-agnostic `blec` core

Planning material from 2026-09-16. **Implemented** — see the commits on `decouple_from_tauri`,
from `split the crate into a blec core…` onwards. Kept as the record of why the design is what
it is.

- `plan-blec-core-split.md` — the plan: options considered, chosen design (Kotlin compiled to a
  committed `classes.dex`, loaded via `InMemoryDexClassLoader` + `RegisterNatives`), workspace layout,
  bridge protocol, Kotlin refactor, Tauri layer, Dioxus example, implementation order, verification, risks.
- `current-architecture-map.md` — how the crate is wired today: file map, Tauri coupling points,
  backend selection, full Rust ↔ Kotlin command/callback table with payload shapes, Kotlin structure,
  Gradle/manifest, CI, bugs to fix along the way.
- `research-android-without-gradle.md` — external research with sources: btleplug's Android backend,
  embedded-dex prior art and pitfalls, Dioxus 0.7 Android hooks (`Dioxus.toml`, ndk-context, manganis),
  how Tauri binds Kotlin plugins, other Rust BLE crates.

Corrections found while running it (the notes above still state the original assumption):

- **Tauri does not initialize `ndk-context`.** The plan and research say tao sets it up in
  `ndk_glue::onCreate` with the Application context; tao 0.35.3 has no `ndk_context` call at all
  (only a "TODO: use ndk-context" comment). Its `create` binding discards the JNI arguments and
  runs `run()` on a spawned thread, so the first `blec::init()` under Tauri panicked with
  "android context was not initialized". Fix (same day): `blec::android::init_with(env, context)`
  for hosts without `ndk-context`, called by the plugin from `wry::prelude::dispatch` on
  `RunEvent::Ready`, where the handler is initialized on Android instead of at plugin build time.
  Dioxus still initializes `ndk-context` itself and keeps using plain `blec::init()`.
- **The dex must not use the app's class loader as parent.** The plan said `context.getClassLoader()`.
  Parent-first loading then resolved `kotlin.*` from the Tauri app's own stdlib copy, while R8 (with
  `-allowaccessmodification` from `proguard-android-optimize.txt`) had inlined `mutableListOf` into
  `BlecPlugin` and widened the package-private `kotlin.collections.ArrayAsCollection` only in our copy:
  `IllegalAccessError` on the first command. Parent is now the boot class loader
  (`Object.class.getClassLoader()`), so the embedded stdlib is fully isolated from the app's.

Decisions already taken: core crate name `blec` (owned on crates.io, currently 0.3.4), Cargo workspace
in this repo (`crates/blec`, `crates/tauri-plugin-blec`), Dioxus 0.7.x example, Tauri plugin keeps a
manifest-only Android module.

Added 2026-09-16: `crates/dioxus-blec`, hooks on `blec` plus the same manifest-only Android module,
declared with `#[manganis::ffi("android")]` so `dx` merges the permissions into the app. Chosen over
`Dioxus.toml` settings because `[android] manifest = "<file>"` is a dead key in dx 0.7 (parsed, never
read) and `[android.permissions]` cannot express `neverForLocation`; details in
`research-android-without-gradle.md`, section 3.

Added 2026-09-17: the hosts now build the Kotlin themselves and the dex is opt-in. Loading a dex
from memory is dynamic code loading, which hardened ROMs (GrapheneOS has a per-app toggle) block,
so the default path is now the normal one: `crates/blec/android/lib` is an ordinary android library
module and `crates/tauri-plugin-blec/android` and `crates/dioxus-blec/android` are symlinks to it,
so tauri (`android_path`) and `dx` (`#[manganis::ffi]`) compile it into the app, and the bridge
looks `com.plugin.blec.Bridge` up in the app's class loader first. The dex build and the
`InMemoryDexClassLoader` path survive behind the `embedded-dex` cargo feature as the fallback for
hosts without a gradle build; the `:dex` module now depends on `:lib` instead of holding the
sources. Symlinks were chosen over copies after checking that `cargo package` (1.97, with and
without git) stores the linked files as regular files, that `dx` 0.7.9 copies the module with
`Path::is_dir` + `fs::copy` (both follow symlinks), and that gradle reads source sets through them.
Two things cargo does not do through the symlink: apply the module's `.gitignore` (hence the
`exclude` lists in the host crates' `Cargo.toml`) and, on Windows, materialize the link in a clone
without `core.symlinks`. The `consumer-rules.pro` in the module keeps the JNI entry points through
the app's R8; the dex build is byte-identical before and after the restructuring.
