// An application module, not a library: only an apk contains the single merged
// `classes.dex` that `blec` embeds. The app itself is never installed, it has
// no activity and no resources.
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.plugin.blec"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.plugin.blec"
        // InMemoryDexClassLoader needs API 26.
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "1.0"
    }

    buildTypes {
        release {
            // Shrinks the kotlin stdlib down to what the plugin actually uses,
            // which is what keeps the committed dex small.
            isMinifyEnabled = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    // No resources, no BuildConfig: nothing here renders anything.
    buildFeatures {
        buildConfig = false
        resValues = false
        shaders = false
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
}

// No dependencies at all: the plugin uses the android framework and org.json,
// both of which are part of the platform. The kotlin stdlib comes with the
// kotlin plugin and is shrunk into the dex.
dependencies {}
