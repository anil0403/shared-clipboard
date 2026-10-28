import javax.inject.Inject

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.noobsbot.sharedclip"
    compileSdk {
        version = release(37) { minorApiLevel = 2 }
    }
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "com.noobsbot.sharedclip"
        minSdk = 29
        targetSdk = 37
        // Set by CI (.github/set-version.sh), never by hand: see VERSIONING.md.
        versionCode = 1000099
        versionName = "1.0.0"
        // The core is only built for these (see RustCore), so leave out JNA's other ABIs too.
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.09.00"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")
    // The core's Kotlin bindings load it through JNA.
    implementation("net.java.dev.jna:jna:5.19.1@aar")
    implementation("com.journeyapps:zxing-android-embedded:4.3.0")
    implementation("com.google.zxing:core:3.5.4")
}

/** Builds the Rust core (`core/`) for Android with cargo-ndk, then generates its Kotlin bindings. */
abstract class RustCore @Inject constructor(private val exec: ExecOperations) : DefaultTask() {
    @get:OutputDirectory abstract val jniLibs: DirectoryProperty
    @get:OutputDirectory abstract val bindings: DirectoryProperty
    @get:Internal abstract val repo: DirectoryProperty
    @get:Internal abstract val ndk: DirectoryProperty

    init {
        // Cargo knows when the core changed, so always ask it; it's quick when nothing did.
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val libs = jniLibs.get().asFile
        cargo("ndk", "-t", "arm64-v8a", "-t", "x86_64", "-P", "29", "-o", libs.path,
            "build", "--release", "-p", "sharedclip-core")
        // iroh and iroh-relay are cdylibs too and cargo-ndk copies them, but nothing loads them.
        libs.walk().filter { it.isFile && it.name != "libsharedclip.so" }.forEach { it.delete() }
        cargo("run", "-p", "sharedclip-core", "--features", "bindgen", "--bin", "uniffi-bindgen", "--",
            "generate", "--library", libs.resolve("arm64-v8a/libsharedclip.so").path,
            "--language", "kotlin", "--out-dir", bindings.get().asFile.path)
    }

    private fun cargo(vararg args: String) = exec.exec {
        workingDir = repo.get().asFile
        environment("ANDROID_NDK_HOME", ndk.get().asFile.path)
        // Keep the symbols uniffi-bindgen reads; AGP strips the .so when it packages the APK.
        environment("CARGO_PROFILE_RELEASE_STRIP", "debuginfo")
        commandLine("cargo", *args)
    }
}

androidComponents.onVariants { variant ->
    val task = tasks.register<RustCore>("rustCore${variant.name.replaceFirstChar { it.uppercase() }}") {
        repo.set(rootProject.layout.projectDirectory.dir(".."))
        ndk.set(androidComponents.sdkComponents.ndkDirectory)
    }
    variant.sources.jniLibs?.addGeneratedSourceDirectory(task, RustCore::jniLibs)
    variant.sources.kotlin?.addGeneratedSourceDirectory(task, RustCore::bindings)
}
