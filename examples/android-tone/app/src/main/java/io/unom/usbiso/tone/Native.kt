package io.unom.usbiso.tone

/**
 * The JNI surface of `usb-iso-tone`.
 *
 * Every call takes and returns primitives only; detail goes to logcat under the tag `usb-iso`.
 * That is a deliberate simplification — string marshalling is most of what makes hand-written JNI
 * unpleasant, and none of it is needed to demonstrate the transport.
 *
 * ## The one rule
 *
 * `fd` comes from [android.hardware.usb.UsbDeviceConnection.getFileDescriptor], and the connection
 * **must stay open for the whole call**. The native side borrows the descriptor; it never closes
 * it, because `UsbDeviceConnection.close()` owns that. Closing the connection while a native call
 * is running would pull the descriptor out from under an in-flight URB.
 */
object Native {
    init {
        System.loadLibrary("usb_iso_tone")
    }

    /** Log the device's identity, speed, capabilities and audio streams. Returns the number of
     *  playback streams found, or a negative [Status] code. */
    external fun probe(fd: Int): Int

    /**
     * WP0: force-claim the audio interface, move one isochronous URB, and give it back.
     *
     * Pass `-1` for [interface] or [alt] to let the native side choose the richest playback
     * stream. Returns [Status.OK] when the transport works.
     */
    external fun spike(fd: Int, interface_: Int, alt: Int): Int

    /**
     * Play a sine.
     *
     * [channelMask] is a bitmask of channel indices, or 0 for every channel. On a DualSense the
     * four channels are speaker L, speaker R, left voice coil, right voice coil — so `0b1100`
     * drives the haptic actuators alone, which at a low [hz] is the difference between hearing
     * something and feeling it.
     */
    external fun tone(
        fd: Int,
        interface_: Int,
        alt: Int,
        seconds: Int,
        hz: Int,
        depthMs: Int,
        channelMask: Int,
    ): Int

    /**
     * WP7: sweep in-flight depth against packets-per-URB and report the lowest underrun-free
     * configuration to logcat. Returns that depth in milliseconds, or a negative [Status] code.
     *
     * This is the measurement that decides whether the route can serve haptics, and it has to be
     * taken on the phone — a desktop number says nothing about a mobile scheduler.
     */
    external fun sweep(fd: Int, interface_: Int, alt: Int, secondsPerCell: Int): Int
}

/** Status codes returned by [Native]. Mirrors the constants at the top of the Rust `lib.rs`. */
object Status {
    const val OK = 0
    const val ERR_DESCRIPTORS = -1
    const val ERR_CLAIM_REFUSED = -2
    const val ERR_TRANSPORT = -3
    const val ERR_NO_STREAM = -4
    const val ERR_STREAM_HOLES = -5
    const val ERR_PANIC = -6

    fun describe(code: Int): String = when (code) {
        OK -> "OK"
        ERR_DESCRIPTORS -> "could not read or parse the device's descriptors"
        ERR_CLAIM_REFUSED ->
            "the kernel refused to release the interface. Some OEM kernels do this and there is " +
                "no app-side fix — a real consumer must detect it and fall back."
        ERR_TRANSPORT -> "a USB-level failure; see logcat"
        ERR_NO_STREAM -> "no suitable playback stream on this device"
        ERR_STREAM_HOLES -> "the stream ran but lost data — try a deeper buffer"
        ERR_PANIC -> "the native side panicked (caught at the JNI boundary)"
        else -> if (code > 0) "OK ($code)" else "unknown status $code"
    }
}
