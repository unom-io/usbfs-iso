plugins {
    id("com.android.application")
}

android {
    namespace = "io.unom.usbiso.tone"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.unom.usbiso.tone"
        // 26 covers everything this needs. The interesting platform behaviour is at the top end,
        // not the bottom: arm64 Memory Tagging arrives with Android 14, and that is handled in the
        // ring's design rather than by a minSdk bump.
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    // The shared object built by cargo-ndk below.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    // No AndroidX, no Compose, no test frameworks: this app is a wrapper around four native calls.
    packaging {
        jniLibs.keepDebugSymbols.add("**/*.so")
    }
}

/**
 * Build the Rust cdylib for every ABI and drop the results straight into `jniLibs`.
 *
 * `cargo ndk` handles the linker configuration the NDK needs; without it, `cargo build
 * --target aarch64-linux-android` compiles but cannot link. Install it once with
 * `cargo install cargo-ndk`.
 */
val cargoNdk by tasks.registering(Exec::class) {
    group = "build"
    description = "Build usb-iso-tone for all Android ABIs via cargo-ndk"

    workingDir = file("../rust")
    val outDir = file("src/main/jniLibs")
    outputs.dir(outDir)
    inputs.dir(file("../rust/src"))
    inputs.file(file("../rust/Cargo.toml"))
    // The crates being demonstrated are path dependencies; a change in them must rebuild this.
    inputs.dir(file("../../../crates/usbfs-iso/src"))
    inputs.dir(file("../../../crates/uac-host/src"))

    doFirst {
        outDir.mkdirs()
        // cargo-ndk needs to find the NDK. Prefer an explicit env var, else the newest NDK in the
        // SDK, so a fresh checkout builds without any local configuration.
        if (System.getenv("ANDROID_NDK_HOME") == null) {
            val sdk = System.getenv("ANDROID_HOME")
                ?: System.getenv("ANDROID_SDK_ROOT")
                ?: "${System.getProperty("user.home")}/Library/Android/sdk"
            val newest = file("$sdk/ndk").listFiles()
                ?.filter { it.isDirectory }
                ?.maxByOrNull { it.name }
            requireNotNull(newest) {
                "No NDK found under $sdk/ndk. Install one, or set ANDROID_NDK_HOME."
            }
            environment("ANDROID_NDK_HOME", newest.absolutePath)
        }
    }

    commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-t", "armeabi-v7a",
        "-t", "x86_64",
        "-o", outDir.absolutePath,
        "build", "--release",
    )
}

tasks.named("preBuild") { dependsOn(cargoNdk) }
