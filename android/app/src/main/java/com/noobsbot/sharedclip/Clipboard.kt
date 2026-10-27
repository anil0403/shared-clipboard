package com.noobsbot.sharedclip

import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PersistableBundle
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.text.Html
import android.util.Log
import android.webkit.MimeTypeMap
import androidx.activity.ComponentActivity
import androidx.core.content.FileProvider
import androidx.core.content.IntentCompat
import androidx.lifecycle.lifecycleScope
import com.noobsbot.sharedclip.core.Clip
import com.noobsbot.sharedclip.core.Format
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import com.noobsbot.sharedclip.core.File as ClipFile

private const val LABEL = "Shared Clipboard"

// ClipDescription.EXTRA_IS_SENSITIVE. Password managers set it on older Androids too.
private const val SENSITIVE = "android.content.extra.IS_SENSITIVE"

/** Puts an item from another device on the clipboard: text and HTML as they are, images and files as URIs. */
suspend fun Context.writeClip(clip: Clip) {
    val html = clip.formats.firstOrNull { it.mime == "text/html" }?.data?.decodeToString()
    val text = clip.formats.firstOrNull { it.mime.startsWith("text/plain") }?.data?.decodeToString()
        ?: html?.let { Html.fromHtml(it, Html.FROM_HTML_MODE_COMPACT).toString() }
    // The formats are versions of the same item, so one image is enough.
    val image = clip.formats.firstOrNull { it.mime.startsWith("image/") }
    val uris = withContext(Dispatchers.IO) {
        listOfNotNull(image?.let { saveImage(clip.id, it) }) + clip.files.mapNotNull { saveDownload(it) }
    }
    val data = when {
        text != null && html != null -> ClipData.newHtmlText(LABEL, text, html)
        text != null -> ClipData.newPlainText(LABEL, text)
        uris.isNotEmpty() -> ClipData.newUri(contentResolver, LABEL, uris.first())
        else -> return
    }
    (if (text != null) uris else uris.drop(1)).forEach { data.addItem(contentResolver, ClipData.Item(it)) }
    // ClipDescription.EXTRA_IS_REMOTE_DEVICE: tells Android 13+ the item came from another device.
    data.description.extras = PersistableBundle().apply { putBoolean("android.content.extra.IS_REMOTE_DEVICE", true) }
    getSystemService(ClipboardManager::class.java).setPrimaryClip(data)
}

/** Saves an image where our FileProvider (res/xml/file_paths.xml) shares it from. */
private fun Context.saveImage(id: String, image: Format): Uri {
    val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(image.mime) ?: "img"
    val file = File(File(cacheDir, "clips").apply { mkdirs() }, "${id.filter { it.isLetterOrDigit() }.take(16)}.$ext")
    file.writeBytes(image.data)
    return FileProvider.getUriForFile(this, "$packageName.files", file)
}

/** Copies a received file into Downloads. */
private fun Context.saveDownload(file: ClipFile): Uri? = try {
    val values = ContentValues().apply {
        put(MediaStore.MediaColumns.DISPLAY_NAME, file.name)
        put(MediaStore.MediaColumns.IS_PENDING, 1)
    }
    contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)?.also { uri ->
        contentResolver.openOutputStream(uri)!!.use { out -> File(file.path).inputStream().use { it.copyTo(out) } }
        contentResolver.update(uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
    }
} catch (e: Exception) {
    Log.e("SharedClip", "Couldn't save ${file.name} to Downloads", e)
    null
}

/**
 * Sends what's on the clipboard, unless it's marked sensitive. Android lets only the app in focus read it.
 * [auto] sends (while our screen is open) skip an item already sent.
 */
suspend fun Context.sendClipboard(auto: Boolean) {
    val clip = getSystemService(ClipboardManager::class.java).primaryClip ?: return
    val desc = clip.description
    if (desc.extras?.getBoolean(SENSITIVE) == true) return
    if (auto && (desc.timestamp == Shared.lastSent || Shared.node == null)) return
    Shared.lastSent = desc.timestamp
    val items = List(clip.itemCount) { clip.getItemAt(it) }
    send(items.firstNotNullOfOrNull { it.text }, items.firstNotNullOfOrNull { it.htmlText }, items.mapNotNull { it.uri })
}

/** Reads the text, HTML and content URIs and hands them to the core. Reads them now, while we may. */
suspend fun Context.send(text: CharSequence?, html: String?, uris: List<Uri>) {
    val formats = mutableListOf<Format>()
    val files = mutableListOf<ClipFile>()
    try {
        withContext(Dispatchers.IO) {
            text?.let { formats += Format("text/plain;charset=utf-8", it.toString().encodeToByteArray()) }
            html?.let { formats += Format("text/html", it.encodeToByteArray()) }
            for (uri in uris) {
                val mime = contentResolver.getType(uri)
                if (mime == "image/png" || mime == "image/jpeg") {
                    formats += Format(mime, contentResolver.openInputStream(uri)!!.use { it.readBytes() })
                } else {
                    files += cacheFile(uri)
                }
            }
        }
    } catch (e: Exception) {
        return toast("Couldn't read it: ${e.message}")
    }
    if (formats.isNotEmpty() || files.isNotEmpty()) call { it.copy(formats, files) }
}

/** Copies a shared file into the cache, because the core reads files by path. */
// ponytail: a later file with the same name replaces it, and Android may clear the cache before an
// offered item is fetched. Copy into filesDir instead if the core doesn't keep its own copy.
private fun Context.cacheFile(uri: Uri): ClipFile {
    val name = contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
        ?.use { if (it.moveToFirst()) it.getString(0) else null }
    // The name comes from another app: keep the file inside our folder.
    val safe = File(name ?: "file").name.takeUnless { it.trim('.').isBlank() } ?: "file"
    val file = File(File(cacheDir, "send").apply { mkdirs() }, safe)
    contentResolver.openInputStream(uri)!!.use { input -> file.outputStream().use { input.copyTo(it) } }
    return ClipFile(safe, file.path, file.length().toULong())
}

/** See-through screen that sends the clipboard (tile, notification), a share or selected text, then closes. */
class SendActivity : ComponentActivity() {
    private var started = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val i = intent
        when (i.action) {
            Intent.ACTION_SEND -> once {
                send(
                    i.getCharSequenceExtra(Intent.EXTRA_TEXT),
                    i.getStringExtra(Intent.EXTRA_HTML_TEXT),
                    listOfNotNull(IntentCompat.getParcelableExtra(i, Intent.EXTRA_STREAM, Uri::class.java)),
                )
            }
            Intent.ACTION_SEND_MULTIPLE -> once {
                send(null, null, IntentCompat.getParcelableArrayListExtra(i, Intent.EXTRA_STREAM, Uri::class.java).orEmpty())
            }
            Intent.ACTION_PROCESS_TEXT -> once { send(i.getCharSequenceExtra(Intent.EXTRA_PROCESS_TEXT), null, emptyList()) }
        }
    }

    // The clipboard can only be read once the window has focus.
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && intent.action == null) once { sendClipboard(auto = false) }
    }

    private fun once(block: suspend () -> Unit) {
        if (started) return
        started = true
        lifecycleScope.launch {
            block()
            finish()
        }
    }
}

/** Quick Settings tile: one tap sends the clipboard. */
class SendTile : TileService() {
    override fun onStartListening() {
        qsTile?.run {
            state = Tile.STATE_INACTIVE
            updateTile()
        }
    }

    override fun onClick() {
        val intent = Intent(this, SendActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (Build.VERSION.SDK_INT >= 34) {
            startActivityAndCollapse(PendingIntent.getActivity(this, 0, intent, PendingIntent.FLAG_IMMUTABLE))
        } else {
            @Suppress("DEPRECATION")
            startActivityAndCollapse(intent)
        }
    }
}
