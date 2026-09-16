// All this module does is merge the bluetooth permissions into the app's
// manifest. `dx` copies it into the generated gradle project because
// `#[manganis::ffi("android")]` in src/lib.rs points at this directory. The
// android code lives in the `blec` crate and is loaded at runtime from a dex
// embedded in the binary, so there is nothing to compile here: no kotlin
// plugin, no dependencies, no sources.
plugins {
    id("com.android.library")
}

android {
    namespace = "com.plugin.blec.manifest"
    compileSdk = 34

    defaultConfig {
        // The app's `[android] min_sdk` in Dioxus.toml must be at least this.
        minSdk = 26
    }
}
