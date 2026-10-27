package com.noobsbot.sharedclip

import android.content.ClipboardManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {
    private val clipboard by lazy { getSystemService(ClipboardManager::class.java) }
    private val onCopy = ClipboardManager.OnPrimaryClipChangedListener { autoSend() }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        service()
        setContent { MaterialTheme { Text(Shared.error ?: if (Shared.node == null) "Starting…" else "Running") } }
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

    private fun autoSend() {
        lifecycleScope.launch { sendClipboard(auto = true) }
    }
}
