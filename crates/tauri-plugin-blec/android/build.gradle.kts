// All this module still does is merge the bluetooth permissions into the app's
// manifest. The plugin's android code lives in the `blec` crate and is loaded
// at runtime from a dex embedded in the binary, so there is nothing to compile
// here: no kotlin plugin, no dependencies, no sources.
plugins {
    id("com.android.library")
}

android {
    namespace = "com.plugin.blec.manifest"
    compileSdk = 34

    defaultConfig {
        minSdk = 26
    }
}
