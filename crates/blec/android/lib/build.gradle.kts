// The android side of `blec`: the bluetooth permissions in the manifest and the
// kotlin that drives the platform bluetooth stack.
//
// This one directory is what every host builds. `crates/tauri-plugin-blec/android`
// and `crates/dioxus-blec/android` are symlinks to it, so tauri (through
// `tauri_plugin::Builder::android_path`) and `dx` (through `#[manganis::ffi]`)
// compile it as an ordinary gradle library module of the app, and the classes
// end up in the apk like any other. The `:dex` module next to it builds the same
// sources into the `classes.dex` that the `embedded-dex` cargo feature loads at
// runtime for hosts without a gradle build.
//
// No plugin versions here: the app's root project supplies them (tauri and dx
// both put the android and kotlin gradle plugins on the buildscript classpath;
// dx strips versions from this file anyway) and the standalone dex build pins
// them in ../settings.gradle.kts.
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.plugin.blec"
    compileSdk = 34

    defaultConfig {
        minSdk = 26
        // Keeps the entry points the rust side reaches through JNI alive in
        // the app's R8 pass; AGP merges these rules into the app's own.
        consumerProguardFiles("consumer-rules.pro")
    }

    // No resources and no BuildConfig: nothing here renders anything.
    buildFeatures {
        buildConfig = false
        resValues = false
        shaders = false
    }

    // Kotlin refuses to build when its jvm target differs from java's, and
    // the hosts default to different ones, so both are set here.
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
}

// The kotlin side uses the android framework and org.json, both part of the
// platform; the kotlin stdlib comes with the kotlin plugin.
dependencies {}
