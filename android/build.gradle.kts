plugins {
    id("com.android.application") version "9.4.1" apply false
    // AGP compiles Kotlin itself; this only picks the Kotlin version, which the Compose plugin must match.
    id("org.jetbrains.kotlin.android") version "2.4.21" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.4.21" apply false
}
