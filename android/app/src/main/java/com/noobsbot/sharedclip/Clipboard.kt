package com.noobsbot.sharedclip

import android.annotation.SuppressLint
import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentResolver
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
import androidx.core.content.edit
import androidx.core.content.IntentCompat
import com.noobsbot.sharedclip.core.Clip
import com.noobsbot.sharedclip.core.Format
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.io.InputStream
import java.util.UUID
import com.noobsbot.sharedclip.core.File as ClipFile

private const val LABEL = "Shared Clipboard"

// ClipDescription.EXTRA_IS_SENSITIVE. Password managers set it on older Androids too.
private const val SENSITIVE = "android.content.extra.IS_SENSITIVE"

// ClipDescription.EXTRA_IS_REMOTE_DEVICE: tells Android 13+ the item came from another device.
private const val REMOTE = "android.content.extra.IS_REMOTE_DEVICE"

/**
 * Puts an item from another device on the clipboard: text and HTML as they are, images and files as URIs. Files are
 * shared from where the core keeps them; they go to Downloads only when the user asks ([saveDownloads]).
 */
suspend fun Context.writeClip(clip: Clip) {
    val html = clip.formats.firstOrNull { it.mime == "text/html" }?.data?.decodeToString()
    val text = clip.formats.firstOrNull { it.mime.startsWith("text/plain") }?.data?.decodeToString()
        ?: html?.let { Html.fromHtml(it, Html.FROM_HTML_MODE_COMPACT).toString() }
    // The formats are versions of the same item, so one image is enough.
    val image = clip.formats.firstOrNull { it.mime.startsWith("image/") }
    val uris = withContext(Dispatchers.IO) {
        listOfNotNull(image?.let { saveImage(clip.id, it) }) + clip.files.map { shareUri(File(it.path)) }
    }
    val data = when {
        text != null && html != null -> ClipData.newHtmlText(LABEL, text, html)
        text != null -> ClipData.newPlainText(LABEL, text)
        uris.isNotEmpty() -> ClipData.newUri(contentResolver, LABEL, uris.first())
        else -> return
    }
    (if (text != null) uris else uris.drop(1)).forEach { data.addItem(contentResolver, ClipData.Item(it)) }
    data.description.extras = PersistableBundle().apply { putBoolean(REMOTE, true) }
    getSystemService(ClipboardManager::class.java).setPrimaryClip(data)
}

/** Saves an image where our FileProvider (res/xml/file_paths.xml) shares it from. */
private fun Context.saveImage(id: String, image: Format): Uri {
    val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(image.mime) ?: "img"
    val file = File(File(cacheDir, "clips").apply { mkdirs() }, "${id.filter { it.isLetterOrDigit() }.take(16)}.$ext")
    file.writeBytes(image.data)
    return shareUri(file)
}

/** A URI other apps can read [file] through, from the folders in res/xml/file_paths.xml. */
private fun Context.shareUri(file: File): Uri = FileProvider.getUriForFile(this, "$packageName.files", file)

/** Copies an item's files into Downloads, when the user asks. */
suspend fun Context.saveDownloads(files: List<ClipFile>) {
    val failed = withContext(Dispatchers.IO) { files.count { !saveDownload(it) } }
    toast(if (failed == 0) "Saved to Downloads" else "Couldn't save $failed of ${files.size} to Downloads")
}

/** Copies a received file into Downloads, unless the same name and size is already there from an earlier save. */
private fun Context.saveDownload(file: ClipFile): Boolean = try {
    val downloads = MediaStore.Downloads.EXTERNAL_CONTENT_URI
    val size = File(file.path).length()
    // Without storage permission, this only finds files we saved ourselves.
    val saved = contentResolver.query(
        downloads, arrayOf(MediaStore.MediaColumns._ID),
        "${MediaStore.MediaColumns.DISPLAY_NAME} = ? AND ${MediaStore.MediaColumns.SIZE} = ?",
        arrayOf(file.name, size.toString()), null,
    )?.use { it.moveToFirst() } == true
    if (!saved) {
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, file.name)
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val uri = contentResolver.insert(downloads, values)!!
        contentResolver.openOutputStream(uri)!!.use { out -> File(file.path).inputStream().use { it.copyTo(out) } }
        contentResolver.update(uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
    }
    true
} catch (e: Exception) {
    Log.e("SharedClip", "Couldn't save ${file.name} to Downloads", e)
    false
}

/**
 * Sends what's on the clipboard, unless it's marked sensitive. Android lets only the app in focus read it.
 * [auto] sends (while our screen is open) skip items we put there and the item already sent, even after a restart.
 */
fun Context.sendClipboard(auto: Boolean) {
    val clip = getSystemService(ClipboardManager::class.java).primaryClip ?: return
    val desc = clip.description
    if (desc.extras?.getBoolean(SENSITIVE) == true) {
        if (!auto) toast("Not sent: the app that copied it marked it as private")
        return
    }
    val prefs = getSharedPreferences("app", Context.MODE_PRIVATE)
    // Ours came from another device. Android reads it back slightly different (text/plain;charset=utf-8 for
    // text/plain), so the core wouldn't see it's the same item and would send it back as a new one.
    val ours = desc.label?.toString() == LABEL || desc.extras?.getBoolean(REMOTE) == true
    if (auto && (ours || desc.timestamp == prefs.getLong("lastSent", 0))) return
    prefs.edit { putLong("lastSent", desc.timestamp) }
    val items = List(clip.itemCount) { clip.getItemAt(it) }
    send(items.firstNotNullOfOrNull { it.text }, items.firstNotNullOfOrNull { it.htmlText }, items.mapNotNull { it.uri })
}

/** A shared file or image, opened while we were allowed to read it. */
private class Part(val mime: String?, val name: String?, val input: InputStream)

/**
 * Hands text, HTML and content URIs to the core. Opens the URIs now, while we may read them, and copies them in the
 * background, so the screen that called this can close right away.
 */
fun Context.send(text: CharSequence?, html: String?, uris: List<Uri>) {
    val parts = mutableListOf<Part>()
    try {
        for (uri in uris) {
            // Other apps can name any URI, and we'd read it with our own rights: file:// could be our device key.
            if (uri.scheme != ContentResolver.SCHEME_CONTENT || uri.authority == "$packageName.files") {
                throw SecurityException("only shared files can be sent")
            }
            val name = contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
                ?.use { if (it.moveToFirst()) it.getString(0) else null }
            parts += Part(contentResolver.getType(uri), name, contentResolver.openInputStream(uri)!!)
        }
    } catch (e: Exception) {
        parts.forEach { it.input.close() }
        return toast("Couldn't read it: ${e.message}")
    }
    val app = applicationContext
    Shared.scope.launch {
        val formats = mutableListOf<Format>()
        val files = mutableListOf<ClipFile>()
        try {
            withContext(Dispatchers.IO) {
                text?.let { formats += Format("text/plain;charset=utf-8", it.toString().encodeToByteArray()) }
                html?.let { formats += Format("text/html", it.encodeToByteArray()) }
                for (part in parts) {
                    if (part.mime == "image/png" || part.mime == "image/jpeg") {
                        formats += Format(part.mime, part.input.readBytes())
                    } else {
                        files += app.keepFile(part)
                    }
                }
            }
        } catch (e: Exception) {
            return@launch app.toast("Couldn't read it: ${e.message}")
        } finally {
            parts.forEach { runCatching { it.input.close() } }
        }
        if (formats.isNotEmpty() || files.isNotEmpty()) app.call { it.copy(formats, files) }
    }
}

/**
 * Copies a shared file into a folder of its own in filesDir/send, because the core reads files by path for as long
 * as the item is in the history. [deleteUnsent] deletes the folder once it isn't.
 */
private fun Context.keepFile(part: Part): ClipFile {
    // The name comes from another app: keep the file inside our folder.
    val name = File(part.name ?: "file").name.takeUnless { it.trim('.').isBlank() } ?: "file"
    val dir = File(File(filesDir, "send"), UUID.randomUUID().toString()).apply { mkdirs() }
    val file = File(dir, name)
    file.outputStream().use { part.input.copyTo(it) }
    return ClipFile(name, file.path, file.length().toULong())
}

/** Deletes the shared files that aren't in the history any more; [kept] are the paths of those that are. */
fun Context.deleteUnsent(kept: List<String>) {
    val dirs = kept.mapTo(HashSet()) { File(it).parent }
    // ponytail: spares the last hour's folders, as a send may still be copying into one before the core has the
    // item. Track the sends in progress if an hour is ever too short.
    val old = System.currentTimeMillis() - 3_600_000
    File(filesDir, "send").listFiles()?.forEach { if (it.path !in dirs && it.lastModified() < old) it.deleteRecursively() }
}

/** Opens SendActivity to send the clipboard. Only we can: the alias isn't exported (AndroidManifest.xml). */
fun Context.sendClipboardIntent() = Intent().setClassName(this, "com.noobsbot.sharedclip.SendClipboard")

/** See-through screen that sends the clipboard (tile, notification), a share or selected text, then closes. */
class SendActivity : ComponentActivity() {
    private var started = false
    private val clipboard get() = componentName.className.endsWith(".SendClipboard")

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Re-created (after a rotation, say) once it had sent: don't send again.
        started = savedInstanceState?.getBoolean("started") == true
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
            else -> if (!clipboard) finish()
        }
    }

    // The clipboard can only be read once the window has focus.
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && clipboard) once { sendClipboard(auto = false) }
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putBoolean("started", started)
    }

    /** Sends once and closes right away: [send] only opens what it sends, the copying goes on in the background. */
    private fun once(send: () -> Unit) {
        // Always close: an invisible window left open would block the screen.
        try {
            if (!started) {
                started = true
                send()
            }
        } finally {
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
        val intent = sendClipboardIntent().addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (Build.VERSION.SDK_INT >= 34) {
            startActivityAndCollapse(PendingIntent.getActivity(this, 0, intent, PendingIntent.FLAG_IMMUTABLE))
        } else {
            @Suppress("DEPRECATION")
            @SuppressLint("StartActivityAndCollapseDeprecated") // only below Android 14, where it's the only way
            startActivityAndCollapse(intent)
        }
    }
}
