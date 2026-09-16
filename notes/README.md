# Notes: splitting out a host-agnostic `blec` core

Planning material from 2026-09-16. **Implemented** — see the five commits on `decouple_from_tauri`
(`split the crate into a blec core…` through `add a dioxus example and a dex drift check`).
Kept as the record of why the design is what it is.

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

Decisions already taken: core crate name `blec` (owned on crates.io, currently 0.3.4), Cargo workspace
in this repo (`crates/blec`, `crates/tauri-plugin-blec`), Dioxus 0.7.x example, Tauri plugin keeps a
manifest-only Android module.
