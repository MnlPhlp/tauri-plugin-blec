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

Decisions already taken: core crate name `blec` (owned on crates.io, currently 0.3.4), Cargo workspace
in this repo (`crates/blec`, `crates/tauri-plugin-blec`), Dioxus 0.7.x example, Tauri plugin keeps a
manifest-only Android module.
