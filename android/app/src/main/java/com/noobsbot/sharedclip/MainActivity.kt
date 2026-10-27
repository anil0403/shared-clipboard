package com.noobsbot.sharedclip

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        service()
        setContent { MaterialTheme { Text(Shared.error ?: if (Shared.node == null) "Starting…" else "Running") } }
    }

    override fun onResume() {
        super.onResume()
        service(ClipService.DISCOVER)
    }
}
