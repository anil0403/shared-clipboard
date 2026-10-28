package com.noobsbot.sharedclip

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.provider.Settings
import android.text.format.Formatter
import android.util.Log
import android.widget.Toast
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.core.content.edit
import com.noobsbot.sharedclip.core.Clip
import com.noobsbot.sharedclip.core.Config
import com.noobsbot.sharedclip.core.Device
import com.noobsbot.sharedclip.core.Events
import com.noobsbot.sharedclip.core.Node
import com.noobsbot.sharedclip.core.Offer
import com.noobsbot.sharedclip.core.Platform
import kotlinx.coroutines.Job
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.net.Inet6Address
import java.net.InetAddress

/** State shared by the service and the screens. Only touched on the main thread. */
object Shared {
    val scope = MainScope()
    var node by mutableStateOf<Node?>(null)
    var error by mutableStateOf<String?>(null)
    var devices by mutableStateOf(emptyList<Device>())
    /** Devices found on the network, by NSD service name. */
    val nearby = mutableStateMapOf<String, Nearby>()
    var pairing by mutableStateOf<Pair<Device, String>?>(null)
    var paused by mutableStateOf(false)
    /** Goes up when the history changes. */
    var historyVersion by mutableIntStateOf(0)
    /** When the last item we sent was copied, so the same copy isn't sent twice. */
    var lastSent = 0L
}

data class Nearby(val id: String, val name: String)

/** Starts the service, or hands it an action. */
fun Context.service(action: String? = null, id: String? = null) {
    startForegroundService(Intent(this, ClipService::class.java).setAction(action).putExtra("id", id))
}

/** Runs a core call, showing what went wrong as a toast. */
fun Context.call(block: suspend (Node) -> Unit) {
    val node = Shared.node ?: return toast("Shared Clipboard is still starting").also { service() }
    Shared.scope.launch {
        try {
            block(node)
        } catch (e: Exception) {
            toast(e.message ?: e.toString())
        }
    }
}

fun Context.toast(text: String) = Toast.makeText(this, text, Toast.LENGTH_SHORT).show()

/** Keeps the core running: owns the Node, finds devices with NSD and keeps the sockets on the Wi-Fi. */
class ClipService : Service() {
    private val scope = MainScope()
    private val nm by lazy { getSystemService(NotificationManager::class.java) }
    private val cm by lazy { getSystemService(ConnectivityManager::class.java) }
    private val nsd by lazy { getSystemService(NsdManager::class.java) }
    private val multicast by lazy {
        getSystemService(WifiManager::class.java).createMulticastLock(TAG).apply { setReferenceCounted(false) }
    }
    private val prefs by lazy { getSharedPreferences("app", MODE_PRIVATE) }
    private val name by lazy { Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME) ?: Build.MODEL }
    private var wifi: Network? = null
    private var wifiCaps: NetworkCapabilities? = null
    private var discovering = false
    private var discoveryTimer: Job? = null

    override fun onBind(intent: Intent?) = null

    override fun onCreate() {
        super.onCreate()
        nm.createNotificationChannels(
            listOf(
                NotificationChannel(STATUS, "Connection status", NotificationManager.IMPORTANCE_LOW),
                NotificationChannel(ALERTS, "Pairing and large items", NotificationManager.IMPORTANCE_HIGH),
            ),
        )
        foreground()
        Shared.paused = prefs.getBoolean("paused", false)
        // The core opens its sockets in start(), so they must already be on the Wi-Fi.
        @Suppress("DEPRECATION")
        cm.allNetworks.firstOrNull { cm.getNetworkCapabilities(it)?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true }
            ?.let { cm.bindProcessToNetwork(it) }
        scope.launch {
            try {
                val node = Node.start(Config(filesDir.path, name, Platform.ANDROID, 0u), events)
                node.setPaused(Shared.paused)
                Shared.devices = node.devices()
                advertise(node)
                Shared.node = node
                Shared.error = null
            } catch (e: Exception) {
                Log.e(TAG, "The core didn't start", e)
                Shared.error = e.message ?: e.toString()
                stopSelf()
                return@launch
            }
            cm.registerNetworkCallback(wifiRequest, wifiCallback, Handler(mainLooper))
            updateMetered()
            update()
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        foreground() // Android wants this after every startForegroundService().
        when (intent?.action) {
            PAUSE -> pause(!Shared.paused)
            DISCOVER -> if (Shared.node != null) discover()
            FETCH -> intent.getStringExtra("id")?.let { id ->
                nm.cancel(id.hashCode())
                call {
                    writeClip(it.fetch(id))
                    Shared.historyVersion++
                }
            }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        scope.cancel()
        Shared.node?.let { node ->
            Shared.node = null
            cm.unregisterNetworkCallback(wifiCallback)
            nsd.unregisterService(registration)
            stopDiscovery()
            cm.bindProcessToNetwork(null)
            Shared.scope.launch {
                runCatching { node.stop() }
                node.close()
            }
        }
        super.onDestroy()
    }

    // The core calls these on its own threads.
    private val events = object : Events {
        override fun onClip(clip: Clip) {
            Shared.scope.launch {
                runCatching { writeClip(clip) }.onFailure { Log.e(TAG, "Couldn't write the clipboard", it) }
                Shared.historyVersion++
            }
        }

        override fun onOffer(offer: Offer) {
            Shared.scope.launch { notifyOffer(offer) }
        }

        override fun onDevices(devices: List<Device>) {
            Shared.scope.launch {
                Shared.devices = devices
                update()
            }
        }

        override fun onPairingCode(device: Device, code: String) {
            Shared.scope.launch {
                Shared.pairing = device to code
                nm.notify(
                    PAIRING,
                    Notification.Builder(this@ClipService, ALERTS)
                        .setSmallIcon(R.drawable.ic_clip)
                        .setContentTitle("Pair with ${device.name}?")
                        .setContentText("Code $code. Tap to answer.")
                        .setContentIntent(activity(MainActivity::class.java))
                        .setAutoCancel(true)
                        .build(),
                )
            }
        }
    }

    private fun pause(paused: Boolean) {
        Shared.paused = paused
        prefs.edit { putBoolean("paused", paused) }
        Shared.node?.setPaused(paused)
        update()
    }

    // Notifications

    private fun foreground() = startForeground(STATUS_ID, status(), ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)

    private fun update() = nm.notify(STATUS_ID, status())

    private fun status(): Notification {
        val online = Shared.devices.count { it.online }
        val title = when {
            Shared.node == null -> "Starting…"
            Shared.paused -> "Paused"
            else -> "Connected to $online ${if (online == 1) "device" else "devices"}"
        }
        return Notification.Builder(this, STATUS)
            .setSmallIcon(R.drawable.ic_clip)
            .setContentTitle(title)
            .setOngoing(true)
            .setContentIntent(activity(MainActivity::class.java))
            .addAction(action("Send clipboard", activity(SendActivity::class.java)))
            .addAction(action(if (Shared.paused) "Resume" else "Pause", serviceIntent(PAUSE, null)))
            .build()
    }

    private fun notifyOffer(offer: Offer) {
        val from = Shared.devices.find { it.id == offer.from }?.name ?: "another device"
        val what = offer.fileNames.joinToString().ifEmpty { offer.mimes.firstOrNull() ?: "An item" }
        nm.notify(
            offer.id.hashCode(),
            Notification.Builder(this, ALERTS)
                .setSmallIcon(R.drawable.ic_clip)
                .setContentTitle("$what from $from")
                .setContentText(Formatter.formatShortFileSize(this, offer.size.toLong()))
                .addAction(action("Download", serviceIntent(FETCH, offer.id)))
                .build(),
        )
    }

    private fun activity(c: Class<*>) =
        PendingIntent.getActivity(this, 0, Intent(this, c).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK), PendingIntent.FLAG_IMMUTABLE)

    private fun serviceIntent(action: String, id: String?) = PendingIntent.getForegroundService(
        this, id.hashCode(), Intent(this, ClipService::class.java).setAction(action).putExtra("id", id),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    private fun action(title: String, intent: PendingIntent) = Notification.Action.Builder(null, title, intent).build()

    // Discovery (docs/protocol.md, "Finding devices")

    private fun advertise(node: Node) {
        val info = NsdServiceInfo().apply {
            serviceName = name
            serviceType = TYPE
            port = node.port().toInt()
            setAttribute("id", node.id())
            setAttribute("name", name)
            setAttribute("platform", "android")
        }
        nsd.registerService(info, NsdManager.PROTOCOL_DNS_SD, registration)
    }

    /** Looks for devices for two minutes, holding the multicast lock only meanwhile. */
    private fun discover() {
        discoveryTimer?.cancel()
        discoveryTimer = scope.launch {
            delay(120_000)
            stopDiscovery()
        }
        if (discovering) return
        discovering = true
        multicast.acquire()
        nsd.discoverServices(TYPE, NsdManager.PROTOCOL_DNS_SD, discovery)
    }

    private fun stopDiscovery() {
        if (!discovering) return
        discovering = false
        runCatching { nsd.stopServiceDiscovery(discovery) } // throws if starting it had failed
        multicast.release()
    }

    @Suppress("DEPRECATION") // its replacement needs Android 14
    private fun resolve(info: NsdServiceInfo): Unit = nsd.resolveService(
        info,
        object : NsdManager.ResolveListener {
            override fun onResolveFailed(info: NsdServiceInfo, error: Int) {
                // Older Androids resolve one service at a time.
                if (error == NsdManager.FAILURE_ALREADY_ACTIVE) {
                    Shared.scope.launch {
                        delay(300)
                        resolve(info)
                    }
                }
            }

            override fun onServiceResolved(info: NsdServiceInfo) {
                Shared.scope.launch { found(info) }
            }
        },
    )

    private fun found(info: NsdServiceInfo) {
        val node = Shared.node ?: return
        val id = info.attributes["id"]?.decodeToString() ?: return
        if (id == node.id()) return
        @Suppress("DEPRECATION")
        val hosts = if (Build.VERSION.SDK_INT >= 34) info.hostAddresses else listOfNotNull(info.host)
        hosts.forEach { node.found(id, address(it, info.port)) }
        Shared.nearby[info.serviceName] = Nearby(id, info.attributes["name"]?.decodeToString() ?: info.serviceName)
    }

    /** "ip:port" as Rust parses it: IPv6 in brackets, with a numeric scope. */
    private fun address(host: InetAddress, port: Int) = if (host is Inet6Address) {
        val scope = if (host.scopeId != 0) "%${host.scopeId}" else ""
        "[${host.hostAddress?.substringBefore('%')}$scope]:$port"
    } else {
        "${host.hostAddress}:$port"
    }

    private val registration = object : NsdManager.RegistrationListener {
        override fun onServiceRegistered(info: NsdServiceInfo) {}
        override fun onServiceUnregistered(info: NsdServiceInfo) {}
        override fun onRegistrationFailed(info: NsdServiceInfo, error: Int) {
            Log.w(TAG, "NSD register failed: $error")
        }
        override fun onUnregistrationFailed(info: NsdServiceInfo, error: Int) {}
    }

    private val discovery = object : NsdManager.DiscoveryListener {
        override fun onServiceFound(info: NsdServiceInfo) = resolve(info)
        override fun onServiceLost(info: NsdServiceInfo) {
            Shared.scope.launch { Shared.nearby.remove(info.serviceName) }
        }
        override fun onDiscoveryStarted(type: String) {}
        override fun onDiscoveryStopped(type: String) {}
        override fun onStartDiscoveryFailed(type: String, error: Int) {
            Log.w(TAG, "NSD discovery failed: $error")
        }
        override fun onStopDiscoveryFailed(type: String, error: Int) {}
    }

    // Network

    private val wifiRequest = NetworkRequest.Builder()
        .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
        .removeCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) // Wi-Fi without internet too
        .build()

    // ponytail: the whole process is bound to the Wi-Fi, because the core's iroh sockets can't use
    // Network.bindSocket, and Android sends traffic over mobile data when the Wi-Fi has no internet.
    // Revisit when internet sync arrives: the core will then need mobile data too.
    private val wifiCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            wifi = network
            cm.bindProcessToNetwork(network)
            discover()
        }

        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            if (network != wifi) return
            wifiCaps = caps
            updateMetered()
        }

        override fun onLost(network: Network) {
            if (network != wifi) return
            wifi = null
            wifiCaps = null
            cm.bindProcessToNetwork(null)
            updateMetered()
        }
    }

    /** Metered Wi-Fi and mobile data count as metered. Data Saver doesn't: it limits other apps' background use. */
    private fun updateMetered() {
        val metered = wifiCaps?.let { !it.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED) }
            ?: cm.isActiveNetworkMetered
        Shared.node?.setMetered(metered)
    }

    companion object {
        const val PAUSE = "pause"
        const val DISCOVER = "discover"
        const val FETCH = "fetch"
        const val PAIRING = 2
        private const val STATUS_ID = 1
        private const val STATUS = "status"
        private const val ALERTS = "alerts"
        private const val TYPE = "_sharedclip._udp"
        private const val TAG = "SharedClip"
    }
}

/** Starts the service after a reboot. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == Intent.ACTION_BOOT_COMPLETED) context.service()
    }
}
