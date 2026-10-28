package com.noobsbot.sharedclip

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.core.content.IntentCompat
import androidx.core.net.toUri
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL

/** A release on GitHub newer than this app. */
data class Update(val version: String, val apk: String, val size: Long)

/**
 * Finds newer releases on GitHub and installs them with Android's installer. Android only installs an update signed
 * with the same key as this app, so a changed download can't replace it.
 */
object Updates {
    private const val LATEST = "https://api.github.com/repos/noobsbot-team/shared-clipboard/releases/latest"
    private const val TAG = "SharedClip"
    const val CHANNEL = "updates"
    const val NOTIFICATION = 3

    /** Asks GitHub for the latest release, and sets Shared.update to it if it's newer. Null if not, or offline. */
    suspend fun check(context: Context): Update? {
        val update = runCatching { withContext(Dispatchers.IO) { latest(context) } }
            .onFailure { Log.i(TAG, "Couldn't check for updates", it) }
            .getOrNull()
        Shared.update = update
        return update
    }

    private fun latest(context: Context): Update? {
        val release = JSONObject(get(LATEST).bufferedReader().use { it.readText() })
        val version = release.getString("tag_name").removePrefix("v")
        val installed = context.packageManager.getPackageInfo(context.packageName, 0).longVersionCode
        if (build(version) <= installed) return null
        val assets = release.getJSONArray("assets")
        val apk = (0 until assets.length()).map { assets.getJSONObject(it) }
            .find { it.getString("name").endsWith("-android.apk") } ?: return null
        return Update(version, apk.getString("browser_download_url"), apk.getLong("size"))
    }

    /** The build number CI gives a release (.github/set-version.sh); 0 for anything else. */
    fun build(version: String): Long {
        val parts = version.split(".").map { it.toLongOrNull() ?: return 0 }
        if (parts.size != 3) return 0
        val (major, minor, patch) = parts
        return major * 1_000_000 + minor * 10_000 + patch * 100 + 99
    }

    /**
     * Downloads the update into an install session and commits it; UpdateReceiver hears how it went. Android asks the
     * user to confirm unless this app installed the current version itself (Android 12 and newer).
     */
    suspend fun install(context: Context, update: Update) {
        if (Shared.updating != null) return
        if (!context.packageManager.canRequestPackageInstalls()) {
            context.toast("Allow Shared Clipboard to install apps, then tap Update again")
            context.startActivity(
                Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, "package:${context.packageName}".toUri()),
            )
            return
        }
        Shared.updating = 0f
        val installer = context.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
            setAppPackageName(context.packageName) // only this app, never another one
            setSize(update.size)
            if (Build.VERSION.SDK_INT >= 31) setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
        }
        val id = installer.createSession(params)
        try {
            installer.openSession(id).use { session ->
                withContext(Dispatchers.IO) {
                    get(update.apk).use { input ->
                        session.openWrite("app.apk", 0, update.size).use { output ->
                            val buffer = ByteArray(64 * 1024)
                            var done = 0L
                            var shown = 0
                            while (true) {
                                val n = input.read(buffer)
                                if (n < 0) break
                                output.write(buffer, 0, n)
                                done += n
                                val percent = (done * 100 / update.size.coerceAtLeast(1)).toInt()
                                if (percent != shown) {
                                    shown = percent
                                    Shared.scope.launch { Shared.updating = percent / 100f }
                                }
                            }
                            session.fsync(output)
                        }
                    }
                }
                Shared.updating = 1f
                val status = PendingIntent.getBroadcast(
                    context, id, Intent(context, UpdateReceiver::class.java),
                    PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                )
                session.commit(status.intentSender)
            }
        } catch (e: Exception) {
            Log.e(TAG, "The update didn't download", e)
            installer.abandonSession(id)
            Shared.updating = null
            context.toast("The update didn't download: ${e.message}")
        }
    }

    private fun get(url: String): InputStream {
        val connection = URL(url).openConnection() as HttpURLConnection
        connection.connectTimeout = 15_000
        connection.readTimeout = 30_000
        connection.setRequestProperty("User-Agent", "SharedClipboard-Android")
        if (connection.responseCode != HttpURLConnection.HTTP_OK) {
            connection.disconnect()
            error("GitHub answered ${connection.responseCode}")
        }
        return connection.inputStream
    }
}

/** Hears how an install went. On success Android stops this app, and BootReceiver starts it again. */
class UpdateReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                val confirm = IntentCompat.getParcelableExtra(intent, Intent.EXTRA_INTENT, Intent::class.java) ?: return
                context.startActivity(confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            }
            PackageInstaller.STATUS_SUCCESS -> {}
            PackageInstaller.STATUS_FAILURE_ABORTED -> Shared.updating = null
            else -> {
                Shared.updating = null
                context.toast("The update didn't install: ${intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)}")
            }
        }
    }
}
