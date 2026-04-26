package com.clipbridge

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.provider.Settings
import android.view.accessibility.AccessibilityManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import kotlinx.serialization.json.Json

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    PairingScreen(
                        existing = PairingStore.load(this),
                        onSave = { config -> PairingStore.save(this, config) },
                        onClear = { PairingStore.clear(this) },
                    )
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PairingScreen(
    existing: PairingConfig?,
    onSave: (PairingConfig) -> Unit,
    onClear: () -> Unit,
) {
    val context = LocalContext.current
    val json = remember { Json { prettyPrint = true; ignoreUnknownKeys = true } }
    var relay by remember { mutableStateOf(existing?.relayUrl ?: "ws://10.0.2.2:8787") }
    var configText by remember {
        mutableStateOf(existing?.let { json.encodeToString(PairingConfig.serializer(), it) } ?: "")
    }
    var error by remember { mutableStateOf<String?>(null) }
    var asEnabled by remember { mutableStateOf(isAccessibilityEnabled(context)) }

    // Re-check the accessibility-enabled flag every time we come back from
    // the system Settings activity.
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    DisposableEffect(lifecycle) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                asEnabled = isAccessibilityEnabled(context)
            }
        }
        lifecycle.addObserver(observer)
        onDispose { lifecycle.removeObserver(observer) }
    }

    val scanLauncher = rememberLauncherForActivityResult(ScanContract()) { result ->
        val contents = result?.contents ?: return@rememberLauncherForActivityResult
        configText = contents
        runCatching {
            val cfg = json.decodeFromString(PairingConfig.serializer(), contents)
            relay = cfg.relayUrl
        }
    }

    Scaffold(
        topBar = { TopAppBar(title = { Text("ClipBridge pairing") }) },
    ) { padding ->
        Column(
            modifier = Modifier
                .padding(padding)
                .padding(16.dp)
                .fillMaxSize(),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            AccessibilityBanner(
                enabled = asEnabled,
                onOpenSettings = {
                    context.startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
                },
                onRefresh = { asEnabled = isAccessibilityEnabled(context) },
            )

            OutlinedTextField(
                value = relay,
                onValueChange = { relay = it },
                label = { Text("Relay URL") },
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = {
                    scanLauncher.launch(
                        ScanOptions()
                            .setOrientationLocked(false)
                            .setBeepEnabled(false)
                            .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                            .setPrompt("Aim at the QR shown by the Mac"),
                    )
                },
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text("Scan QR from Mac")
            }
            Button(onClick = {
                val cfg = PairingConfig.makeNew(relay)
                configText = json.encodeToString(PairingConfig.serializer(), cfg)
            }) {
                Text("Generate new pairing (this device)")
            }
            OutlinedTextField(
                value = configText,
                onValueChange = { configText = it },
                label = { Text("Pairing JSON (auto-filled by scan)") },
                modifier = Modifier.fillMaxWidth().height(220.dp),
            )
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            Spacer(Modifier.height(8.dp))
            Button(onClick = {
                error = null
                runCatching {
                    val cfg: PairingConfig = json.decodeFromString(
                        PairingConfig.serializer(),
                        configText,
                    )
                    require(cfg.keyBytes()?.size == 32) { "key must be 32 bytes" }
                    onSave(cfg)
                }.onFailure { error = "Invalid config: ${it.message}" }
            }) {
                Text("Save & start syncing")
            }
            Button(onClick = {
                onClear()
                configText = ""
            }) {
                Text("Reset pairing")
            }
        }
    }
}

@Composable
private fun AccessibilityBanner(
    enabled: Boolean,
    onOpenSettings: () -> Unit,
    onRefresh: () -> Unit,
) {
    val (containerColor, contentColor) = if (enabled) {
        MaterialTheme.colorScheme.secondaryContainer to MaterialTheme.colorScheme.onSecondaryContainer
    } else {
        MaterialTheme.colorScheme.errorContainer to MaterialTheme.colorScheme.onErrorContainer
    }
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = containerColor, contentColor = contentColor),
    ) {
        Column(
            modifier = Modifier.padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(
                text = if (enabled) "Accessibility: enabled ✓" else "Accessibility: NOT enabled",
                style = MaterialTheme.typography.titleSmall,
            )
            Text(
                text = if (enabled) {
                    "ClipBridge can read your clipboard and sync it."
                } else {
                    "Tap the button below, then enable ClipBridge under Installed services. " +
                            "Without this, the app cannot read what you copy on this phone."
                },
                style = MaterialTheme.typography.bodySmall,
            )
            if (!enabled) {
                Button(onClick = onOpenSettings, modifier = Modifier.fillMaxWidth()) {
                    Text("Open Accessibility settings")
                }
            }
            Button(onClick = onRefresh, modifier = Modifier.fillMaxWidth()) {
                Text("Refresh status")
            }
        }
    }
}

private fun isAccessibilityEnabled(context: Context): Boolean {
    val expected = ComponentName(context, ClipBridgeAccessibilityService::class.java)
        .flattenToString()
    val enabled = Settings.Secure.getString(
        context.contentResolver,
        Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
    ).orEmpty()
    return enabled
        .split(':')
        .any { it.equals(expected, ignoreCase = true) }
}
