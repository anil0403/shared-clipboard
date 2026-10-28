package com.noobsbot.sharedclip

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.BluetoothSocket
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Build
import android.os.ParcelUuid
import android.os.SystemClock
import android.util.Log
import androidx.core.content.ContextCompat
import com.noobsbot.sharedclip.core.BluetoothSink
import com.noobsbot.sharedclip.core.Node
import com.noobsbot.sharedclip.core.bluetoothPsmUuid
import com.noobsbot.sharedclip.core.bluetoothServiceUuid
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import java.io.IOException
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/** Why Bluetooth can't reach the desktops, for the home screen. */
enum class BluetoothProblem { OFF, NOT_ALLOWED }

/** The runtime permissions Bluetooth needs. Below Android 12 it stays off, so it never needs location. */
val bluetoothPermissions = if (Build.VERSION.SDK_INT >= 31) {
    arrayOf(Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT)
} else {
    emptyArray()
}

/**
 * Reaches desktops over Bluetooth LE while the core wants it (some paired device isn't on the network).
 * The phone is the central: it scans for the desktops' service, reads the L2CAP channel number from GATT,
 * connects, and pumps bytes between the socket and the core, which does the rest.
 * Main thread only, except the socket reads and writes.
 */
@SuppressLint("MissingPermission") // allowed() is checked before scanning, and a revoked permission is caught
class Bluetooth(private val context: Context) {
    private val scope = MainScope()
    private val adapter = context.getSystemService(BluetoothManager::class.java)?.adapter
    private val service = ParcelUuid.fromString(bluetoothServiceUuid())
    private val psmUuid = UUID.fromString(bluetoothPsmUuid())
    /** Desktops being connected or linked to, by address. */
    private val links = mutableSetOf<String>()
    /** Open sockets, so stop() can close them: that ends a blocked connect() or read(). */
    private val sockets = ConcurrentHashMap<String, BluetoothSocket>()
    /** When each desktop may be tried again, and the wait after that. */
    private val retry = mutableMapOf<String, Pair<Long, Long>>()
    private var scanning = false

    fun start() {
        ContextCompat.registerReceiver(
            context, stateReceiver, IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED), ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        // ponytail: polls, as the core doesn't say when bluetoothWanted() changes; device and network changes call update() too.
        scope.launch {
            while (true) {
                update()
                delay(30_000)
            }
        }
    }

    fun stop() {
        context.unregisterReceiver(stateReceiver)
        scope.cancel()
        off()
    }

    /** Scans while the core wants Bluetooth and it's on and allowed; stops scanning otherwise. */
    fun update() {
        Shared.bluetoothProblem = when {
            Build.VERSION.SDK_INT < 31 || adapter == null -> null
            !allowed() -> BluetoothProblem.NOT_ALLOWED
            !adapter.isEnabled -> BluetoothProblem.OFF
            else -> null
        }
        try {
            if (Shared.bluetoothProblem == null && adapter != null && Shared.node?.bluetoothWanted() == true) scan() else stopScan()
        } catch (e: SecurityException) {
            Log.w(TAG, "Bluetooth permission was taken away", e)
            off()
        }
    }

    private fun allowed() = Build.VERSION.SDK_INT >= 31 &&
        bluetoothPermissions.all { context.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }

    private fun scan() {
        if (scanning) return
        val scanner = adapter?.bluetoothLeScanner ?: return // null while Bluetooth is turning on or off
        val filter = ScanFilter.Builder().setServiceUuid(service).build()
        val settings = ScanSettings.Builder().setScanMode(ScanSettings.SCAN_MODE_LOW_POWER).build()
        scanner.startScan(listOf(filter), settings, scanCallback)
        scanning = true
    }

    private fun stopScan() {
        if (!scanning) return
        scanning = false
        // When Bluetooth is off there's no scanner, and the scan has already ended.
        runCatching { adapter?.bluetoothLeScanner?.stopScan(scanCallback) }
    }

    /** Stops scanning and closes every link. */
    private fun off() {
        runCatching { stopScan() }
        sockets.values.forEach { runCatching { it.close() } }
    }

    // Called on the main thread.
    private val scanCallback = object : ScanCallback() {
        override fun onScanResult(callbackType: Int, result: ScanResult) = found(result.device)

        override fun onBatchScanResults(results: MutableList<ScanResult>) = results.forEach { found(it.device) }

        override fun onScanFailed(errorCode: Int) {
            Log.w(TAG, "Bluetooth scan failed: $errorCode")
            // Tried again on the next update().
            if (errorCode != SCAN_FAILED_ALREADY_STARTED) scanning = false
        }
    }

    private val stateReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            // The scan ends when Bluetooth goes off, and the sockets fail on their own.
            if (intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, 0) != BluetoothAdapter.STATE_ON) scanning = false
            update()
        }
    }

    private fun found(device: BluetoothDevice) {
        val address = device.address
        val node = Shared.node ?: return
        if (address in links || SystemClock.elapsedRealtime() < (retry[address]?.first ?: 0L)) return
        links += address
        scope.launch {
            val started = SystemClock.elapsedRealtime()
            try {
                link(node, device)
            } catch (e: SecurityException) {
                Log.w(TAG, "Bluetooth permission was taken away", e)
                off()
            } catch (e: Exception) {
                Log.i(TAG, "Bluetooth link to $address ended: $e")
            } finally {
                sockets.remove(address)?.let { runCatching { it.close() } }
                links -= address
                // Wait 5 s, then twice as long each time up to a minute, unless the link had stayed up a while.
                val now = SystemClock.elapsedRealtime()
                val wait = if (now - started > MAX_RETRY) MIN_RETRY else retry[address]?.second ?: MIN_RETRY
                retry[address] = now + wait to (wait * 2).coerceAtMost(MAX_RETRY)
            }
        }
    }

    private suspend fun link(node: Node, device: BluetoothDevice) {
        val psm = withTimeout(GATT_TIMEOUT) { readPsm(device) }
        val socket = device.createInsecureL2capChannel(psm)
        sockets[device.address] = socket
        // connect() has no timeout of its own; closing the socket ends it.
        val timeout = scope.launch {
            delay(CONNECT_TIMEOUT)
            runCatching { socket.close() }
        }
        try {
            withContext(Dispatchers.IO) { socket.connect() }
        } finally {
            timeout.cancel()
        }
        val link = node.bluetoothOpen(Sink(socket), initiator = true)
        try {
            withContext(Dispatchers.IO) {
                val input = socket.inputStream
                val buffer = ByteArray(65_536)
                while (true) {
                    val n = input.read(buffer)
                    if (n < 0) break
                    link.receive(buffer.copyOf(n))
                }
            }
        } finally {
            link.closed()
            link.destroy()
        }
    }

    /** Connects to the desktop's GATT server just to read its L2CAP channel number (u16 little-endian). */
    private suspend fun readPsm(device: BluetoothDevice): Int {
        val psm = CompletableDeferred<Int>()
        fun got(value: ByteArray?, status: Int) {
            if (status == BluetoothGatt.GATT_SUCCESS && value != null && value.size >= 2) {
                psm.complete((value[0].toInt() and 0xFF) or ((value[1].toInt() and 0xFF) shl 8))
            } else {
                psm.completeExceptionally(IOException("Couldn't read the channel number: $status"))
            }
        }
        // Called on Binder threads. An exception there would crash the app, so it goes to psm instead.
        val callback = object : BluetoothGattCallback() {
            override fun onConnectionStateChange(gatt: BluetoothGatt, status: Int, newState: Int) {
                try {
                    when {
                        status != BluetoothGatt.GATT_SUCCESS -> throw IOException("GATT connection failed: $status")
                        newState == BluetoothProfile.STATE_CONNECTED -> if (!gatt.discoverServices()) throw IOException("Couldn't discover services")
                        newState == BluetoothProfile.STATE_DISCONNECTED -> throw IOException("GATT disconnected")
                    }
                } catch (e: Exception) {
                    psm.completeExceptionally(e)
                }
            }

            override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
                try {
                    val characteristic = gatt.getService(service.uuid)?.getCharacteristic(psmUuid)
                        ?: throw IOException("No channel number characteristic")
                    if (!gatt.readCharacteristic(characteristic)) throw IOException("Couldn't read the channel number")
                } catch (e: Exception) {
                    psm.completeExceptionally(e)
                }
            }

            override fun onCharacteristicRead(gatt: BluetoothGatt, characteristic: BluetoothGattCharacteristic, value: ByteArray, status: Int) =
                got(value, status)

            @Deprecated("Android 13 calls the one above instead")
            override fun onCharacteristicRead(gatt: BluetoothGatt, characteristic: BluetoothGattCharacteristic, status: Int) {
                @Suppress("DEPRECATION")
                got(characteristic.value, status)
            }
        }
        @Suppress("DEPRECATION") // its replacement is much newer than our minSdk
        val gatt = device.connectGatt(context, false, callback, BluetoothDevice.TRANSPORT_LE)
            ?: throw IOException("Couldn't connect to GATT")
        try {
            return psm.await()
        } finally {
            gatt.close()
        }
    }

    /** The core writes through this, one call at a time, on a thread where it may block. */
    private class Sink(private val socket: BluetoothSocket) : BluetoothSink {
        override fun send(data: ByteArray) {
            // An exception would reach the core as a panic, so close instead: the reader then ends the link.
            try {
                socket.outputStream.write(data)
            } catch (e: Exception) {
                Log.i(TAG, "Bluetooth write failed: $e")
                runCatching { socket.close() }
            }
        }

        // Also called after we tell the core the socket closed; closing again does nothing.
        override fun closeSocket() {
            runCatching { socket.close() }
        }
    }

    private companion object {
        const val TAG = "SharedClip"
        const val GATT_TIMEOUT = 15_000L
        const val CONNECT_TIMEOUT = 20_000L
        const val MIN_RETRY = 5_000L
        const val MAX_RETRY = 60_000L
    }
}
