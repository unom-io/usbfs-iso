// AGP 9 has built-in Kotlin support, so modules do NOT apply org.jetbrains.kotlin.android — doing
// so is an error under AGP 9. This example uses no Compose and no AndroidX: it is a reference
// integration for a USB transport, and every dependency it does not have is one fewer thing
// between a reader and the part that matters.
plugins {
    id("com.android.application") version "9.3.1" apply false
}
