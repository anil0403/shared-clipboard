package com.noobsbot.sharedclip

import android.Manifest
import android.app.NotificationManager
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.edit
import com.google.zxing.BarcodeFormat
import com.google.zxing.qrcode.QRCodeWriter
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import com.noobsbot.sharedclip.core.Clip
import com.noobsbot.sharedclip.core.Device
import com.noobsbot.sharedclip.core.Platform
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {
    private val clipboard by lazy { getSystemService(ClipboardManager::class.java) }
    private val onCopy = ClipboardManager.OnPrimaryClipChangedListener { autoSend() }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        service()
        setContent {
            val dark = isSystemInDarkTheme()
            val colors = when {
                Build.VERSION.SDK_INT >= 31 -> if (dark) dynamicDarkColorScheme(this) else dynamicLightColorScheme(this)
                dark -> darkColorScheme()
                else -> lightColorScheme()
            }
            MaterialTheme(colors) { Surface { App() } }
        }
    }

    override fun onResume() {
        super.onResume()
        service(ClipService.DISCOVER)
        clipboard.addPrimaryClipChangedListener(onCopy)
    }

    override fun onPause() {
        super.onPause()
        clipboard.removePrimaryClipChangedListener(onCopy)
    }

    // While our screen has focus, Android lets us read the clipboard, so copies are sent on their own.
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) autoSend()
    }

    private fun autoSend() = sendClipboard(auto = true)
}

@Composable
private fun App() {
    var pairing by rememberSaveable { mutableStateOf(false) }
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {}
    LaunchedEffect(Unit) {
        // The camera permission is asked for by the QR scanner when it opens.
        val permissions = buildList {
            if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
            if (Build.VERSION.SDK_INT >= 37) add(Manifest.permission.ACCESS_LOCAL_NETWORK)
        }
        if (permissions.isNotEmpty()) ask.launch(permissions.toTypedArray())
    }
    CodeDialog()
    Box(Modifier.fillMaxSize().safeDrawingPadding().padding(horizontal = 16.dp)) {
        if (pairing) {
            BackHandler { pairing = false }
            PairScreen { pairing = false }
        } else {
            HomeScreen { pairing = true }
        }
    }
}

@Composable
private fun HomeScreen(onPair: () -> Unit) {
    val context = LocalContext.current
    val node = Shared.node
    val history = remember(node, Shared.historyVersion) { node?.let { runCatching { it.history() }.getOrNull() }.orEmpty() }
    LazyColumn(Modifier.fillMaxSize()) {
        item {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Shared Clipboard", Modifier.weight(1f).padding(vertical = 16.dp), style = MaterialTheme.typography.headlineSmall)
                Text("Pause", Modifier.padding(end = 8.dp))
                Switch(Shared.paused, { context.service(ClipService.PAUSE) })
            }
        }
        Shared.error?.let { item { Text("Couldn't start: $it", color = MaterialTheme.colorScheme.error) } }
        item { BatteryTip() }
        item { Header("Devices") { TextButton(onPair) { Text("Pair a device") } } }
        if (Shared.devices.isEmpty()) item { Text("No paired devices yet.") }
        items(Shared.devices, key = { it.id }) { DeviceRow(it) }
        item { Header("History") }
        if (history.isEmpty()) item { Text("Nothing copied yet.") }
        items(history) { HistoryRow(it) }
    }
}

@Composable
private fun Header(title: String, action: @Composable () -> Unit = {}) {
    Row(Modifier.padding(top = 16.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(title, Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
        action()
    }
}

/** First-run tip: some phone makers stop background apps unless the user allows them. */
@Composable
private fun BatteryTip() {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("app", Context.MODE_PRIVATE) }
    var show by remember { mutableStateOf(!prefs.getBoolean("tipSeen", false)) }
    if (!show) return
    val uri = LocalUriHandler.current
    Card(Modifier.padding(vertical = 8.dp)) {
        Column(Modifier.padding(16.dp)) {
            Text("Keep it running", style = MaterialTheme.typography.titleMedium)
            Text("Some phones stop apps in the background, and then items stop arriving. dontkillmyapp.com shows how to allow Shared Clipboard on your phone.")
            Row {
                TextButton({ uri.openUri("https://dontkillmyapp.com") }) { Text("Open dontkillmyapp.com") }
                TextButton({
                    show = false
                    prefs.edit { putBoolean("tipSeen", true) }
                }) { Text("Got it") }
            }
        }
    }
}

@Composable
private fun DeviceRow(device: Device) {
    val context = LocalContext.current
    var confirm by remember { mutableStateOf(false) }
    ListItem(
        headlineContent = { Text(device.name) },
        supportingContent = { Text("${device.platform.label()} · ${if (device.online) "online" else "offline"}") },
        trailingContent = { TextButton({ confirm = true }) { Text("Unpair") } },
    )
    if (confirm) {
        AlertDialog(
            onDismissRequest = { confirm = false },
            title = { Text("Unpair ${device.name}?") },
            text = { Text("Your other devices forget it too.") },
            confirmButton = {
                TextButton({
                    confirm = false
                    context.call { it.unpair(device.id) }
                }) { Text("Unpair") }
            },
            dismissButton = { TextButton({ confirm = false }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun HistoryRow(clip: Clip) {
    val context = LocalContext.current
    val from = if (clip.from == Shared.node?.id()) "This device" else Shared.devices.find { it.id == clip.from }?.name
    ListItem(
        headlineContent = { Text(clip.preview(), maxLines = 2, overflow = TextOverflow.Ellipsis) },
        supportingContent = { Text(from ?: "Another device") },
        trailingContent = if (clip.files.isEmpty()) null else {
            { TextButton({ Shared.scope.launch { context.saveDownloads(clip.files) } }) { Text("Save") } }
        },
        modifier = Modifier.clickable(onClickLabel = "Copy") {
            Shared.scope.launch { runCatching { context.writeClip(clip) }.onFailure { context.toast("Couldn't copy it") } }
        },
    )
}

@Composable
private fun PairScreen(onDone: () -> Unit) {
    val context = LocalContext.current
    val node = Shared.node
    // A new link each time: it's valid for 10 minutes and one pairing.
    val qr = remember(node) { node?.let { runCatching { qrCode(it.pairingLink()) }.getOrNull() } }
    val scan = rememberLauncherForActivityResult(ScanContract()) { result ->
        val link = result.contents ?: return@rememberLauncherForActivityResult
        context.call {
            context.toast("Paired with ${it.pairLink(link).name}")
            onDone()
        }
    }
    val paired = Shared.devices.map { it.id }.toSet()
    val nearby = Shared.nearby.values.filter { it.id !in paired }
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        Header("Pair a device") { TextButton(onDone) { Text("Done") } }
        Text("Scan this code with your other device:")
        qr?.let {
            Image(it.asImageBitmap(), "Pairing QR code", Modifier.padding(16.dp).size(240.dp).align(Alignment.CenterHorizontally))
        }
        Button(
            {
                scan.launch(
                    ScanOptions()
                        .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                        .setPrompt("Scan the code shown on your other device")
                        .setBeepEnabled(false)
                        .setOrientationLocked(false),
                )
            },
            Modifier.align(Alignment.CenterHorizontally),
        ) { Text("Scan a code") }
        Header("Nearby")
        if (nearby.isEmpty()) Text("Looking for devices on this network…")
        nearby.forEach { device ->
            ListItem(
                headlineContent = { Text(device.name) },
                supportingContent = { Text("Tap to pair by comparing a code") },
                modifier = Modifier.clickable {
                    context.call {
                        context.toast("Paired with ${it.pairDevice(device.id).name}")
                        onDone()
                    }
                },
            )
        }
    }
}

/** The 6-digit code check for pairing without a QR code (Node.pairDevice). */
@Composable
private fun CodeDialog() {
    val (device, code) = Shared.pairing ?: return
    val context = LocalContext.current
    fun answer(accept: Boolean) {
        Shared.pairing = null
        context.getSystemService(NotificationManager::class.java).cancel(ClipService.PAIRING)
        runCatching { Shared.node?.confirmPairing(device.id, accept) }
    }
    AlertDialog(
        onDismissRequest = {},
        title = { Text("Pair with ${device.name}?") },
        text = {
            Column {
                Text("Accept only if ${device.name} shows the same code:")
                Text(code.chunked(3).joinToString(" "), Modifier.padding(top = 16.dp), style = MaterialTheme.typography.displaySmall)
            }
        },
        confirmButton = { TextButton({ answer(true) }) { Text("Accept") } },
        dismissButton = { TextButton({ answer(false) }) { Text("Reject") } },
    )
}

private fun Platform.label() = when (this) {
    Platform.LINUX -> "Linux"
    Platform.MACOS -> "macOS"
    Platform.ANDROID -> "Android"
    Platform.IOS -> "iOS"
}

private fun Clip.preview() =
    formats.firstOrNull { it.mime.startsWith("text/plain") }?.data?.decodeToString()?.take(300)
        ?: files.joinToString { it.name }.ifEmpty { if (formats.any { it.mime.startsWith("image/") }) "Image" else "Item" }

private fun qrCode(text: String): Bitmap {
    val m = QRCodeWriter().encode(text, BarcodeFormat.QR_CODE, 512, 512)
    val pixels = IntArray(m.width * m.height) { if (m[it % m.width, it / m.width]) Color.BLACK else Color.WHITE }
    return Bitmap.createBitmap(pixels, m.width, m.height, Bitmap.Config.ARGB_8888)
}
