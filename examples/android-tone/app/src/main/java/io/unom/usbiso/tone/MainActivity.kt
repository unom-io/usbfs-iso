package io.unom.usbiso.tone

import android.app.Activity
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import java.util.concurrent.Executors

/**
 * The reference integration: get a usbfs file descriptor from Android's USB stack and hand it to
 * Rust.
 *
 * The whole trick is three lines — [UsbManager.openDevice], [UsbDeviceConnection.getFileDescriptor],
 * and passing that `int` to [Native]. Everything else in this file is a user interface around it.
 *
 * ## Why this app exists at all
 *
 * Android's own audio framework will not play to some devices it can see perfectly well. The
 * motivating case is the DualSense, whose audio *output* is denylisted by VID/PID in AOSP's
 * `UsbAlsaManager`: the kernel enumerates the pad's 4-channel playback node and the framework then
 * throws it away, so there is no `AudioDeviceInfo` for `AAudioStreamBuilder_setDeviceId` to target.
 * The endpoint is still there. This app drives it directly.
 *
 * ## What to watch
 *
 * `adb logcat -s usb-iso` — the native side reports everything there.
 */
class MainActivity : Activity() {

    private lateinit var usbManager: UsbManager
    private lateinit var output: TextView
    private lateinit var buttons: LinearLayout

    /** Native calls block for seconds at a time; they must never run on the main thread. */
    private val worker = Executors.newSingleThreadExecutor()

    private var device: UsbDevice? = null

    private val permissionReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action != ACTION_USB_PERMISSION) return
            val granted = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
            if (granted) {
                log("permission granted")
                refresh()
            } else {
                log("permission DENIED — nothing can be done without it")
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        usbManager = getSystemService(Context.USB_SERVICE) as UsbManager

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 48, 32, 32)
        }
        buttons = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
        }
        output = TextView(this).apply {
            textSize = 12f
            setTextIsSelectable(true)
        }
        root.addView(buttons)
        root.addView(
            ScrollView(this).apply {
                addView(output)
                layoutParams = LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT,
                )
            }
        )
        setContentView(root)

        val filter = IntentFilter(ACTION_USB_PERMISSION)
        // Android 14 requires an explicit export flag on every runtime-registered receiver.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(permissionReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            registerReceiver(permissionReceiver, filter)
        }

        log("usb-iso reference app. Detail goes to logcat: adb logcat -s usb-iso")
        refresh()
    }

    override fun onDestroy() {
        unregisterReceiver(permissionReceiver)
        worker.shutdownNow()
        super.onDestroy()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        // An attach intent carries a persistent permission grant for that device.
        refresh()
    }

    /** Find an attached device with a USB Audio Class interface, and set up the UI for it. */
    private fun refresh() {
        buttons.removeAllViews()
        val candidates = usbManager.deviceList.values.filter { it.hasAudioInterface() }
        if (candidates.isEmpty()) {
            log("no attached device exposes a USB Audio Class interface")
            device = null
            return
        }
        val dev = candidates.first()
        device = dev
        log(
            "found ${dev.deviceName} %04x:%04x (%s), %d interfaces".format(
                dev.vendorId, dev.productId, dev.productName ?: "unnamed", dev.interfaceCount
            )
        )
        for (i in 0 until dev.interfaceCount) {
            val iface = dev.getInterface(i)
            log(
                "  if%d alt%d class 0x%02x/0x%02x, %d endpoints".format(
                    iface.id, iface.alternateSetting, iface.interfaceClass,
                    iface.interfaceSubclass, iface.endpointCount
                )
            )
        }

        if (!usbManager.hasPermission(dev)) {
            log("no permission yet")
            addButton("Request permission") { requestPermission(dev) }
            return
        }

        addButton("Probe (what is this device?)") { run("probe") { fd -> Native.probe(fd) } }
        addButton("Spike (WP0: claim + one URB)") { run("spike") { fd -> Native.spike(fd, -1, -1) } }
        addButton("Tone 200 Hz, all channels") {
            run("tone") { fd -> Native.tone(fd, -1, -1, 3, 200, 8, 0) }
        }
        addButton("Tone 60 Hz, channels 3+4 only (voice coils)") {
            // Channels are 0-based in the mask: bits 2 and 3 are the third and fourth channels,
            // which on a DualSense are the left and right voice coils.
            run("tone") { fd -> Native.tone(fd, -1, -1, 3, 60, 8, 0b1100) }
        }
        addButton("Sweep (WP7: find the latency floor)") {
            run("sweep") { fd -> Native.sweep(fd, -1, -1, 3) }
        }
    }

    private fun requestPermission(dev: UsbDevice) {
        val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        val intent = PendingIntent.getBroadcast(
            this, 0, Intent(ACTION_USB_PERMISSION).setPackage(packageName), flags
        )
        usbManager.requestPermission(dev, intent)
    }

    /**
     * Open the device, hand the descriptor to native code on a worker thread, and close it after.
     *
     * The `use`-style structure is the important part: the connection has to outlive the native
     * call, because the native side *borrows* the descriptor and never closes it.
     */
    private fun run(what: String, body: (Int) -> Int) {
        val dev = device ?: return
        setButtonsEnabled(false)
        log("--- $what ---")
        worker.execute {
            var connection: UsbDeviceConnection? = null
            try {
                connection = usbManager.openDevice(dev)
                if (connection == null) {
                    post("openDevice returned null — permission revoked, or the device is gone")
                    return@execute
                }
                val fd = connection.fileDescriptor
                if (fd < 0) {
                    post("no file descriptor from the connection")
                    return@execute
                }
                val code = body(fd)
                post("$what -> ${Status.describe(code)}")
            } catch (t: Throwable) {
                post("$what threw: $t")
            } finally {
                connection?.close()
                runOnUiThread { setButtonsEnabled(true) }
            }
        }
    }

    private fun setButtonsEnabled(enabled: Boolean) {
        for (i in 0 until buttons.childCount) buttons.getChildAt(i).isEnabled = enabled
    }

    private fun addButton(label: String, onClick: () -> Unit) {
        buttons.addView(
            Button(this).apply {
                text = label
                isAllCaps = false
                setOnClickListener { onClick() }
                layoutParams = LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                )
            }
        )
    }

    private fun post(line: String) = runOnUiThread { log(line) }

    private fun log(line: String) {
        output.append(line + "\n")
    }

    private fun UsbDevice.hasAudioInterface(): Boolean =
        (0 until interfaceCount).any { getInterface(it).interfaceClass == UsbConstants.USB_CLASS_AUDIO }

    private companion object {
        const val ACTION_USB_PERMISSION = "io.unom.usbiso.tone.USB_PERMISSION"
    }
}
