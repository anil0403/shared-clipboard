package com.noobsbot.sharedclip

import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.PersistableBundle
import android.provider.MediaStore
import android.text.Html
import android.util.Log
import android.webkit.MimeTypeMap
import androidx.core.content.FileProvider
import com.noobsbot.sharedclip.core.Clip
import com.noobsbot.sharedclip.core.Format
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File
import com.noobsbot.sharedclip.core.File as ClipFile

private const val LABEL = "Shared Clipboard"

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
