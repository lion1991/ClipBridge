package com.clipbridge

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.PowerManager
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
import rikka.shizuku.Shizuku

class MainActivity : ComponentActivity() {

    private val shizukuPermissionListener =
        Shizuku.OnRequestPermissionResultListener { _, _ -> /* state listener picks it up */ }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        ShizukuBridge.register()
        Shizuku.addRequestPermissionResultListener(shizukuPermissionListener)
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

    override fun onDestroy() {
        Shizuku.removeRequestPermissionResultListener(shizukuPermissionListener)
        super.onDestroy()
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
    var batteryOptDisabled by remember { mutableStateOf(isBatteryOptimizationDisabled(context)) }
    var shizukuState by remember { mutableStateOf(ShizukuBridge.state()) }

    // Re-check status flags every time we come back from a system Settings
    // activity (accessibility, battery optimization, etc).
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    DisposableEffect(lifecycle) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                asEnabled = isAccessibilityEnabled(context)
                batteryOptDisabled = isBatteryOptimizationDisabled(context)
                shizukuState = ShizukuBridge.state()
            }
        }
        lifecycle.addObserver(observer)
        onDispose { lifecycle.removeObserver(observer) }
    }

    DisposableEffect(Unit) {
        val l = ShizukuBridge.StateListener { s -> shizukuState = s }
        ShizukuBridge.addStateListener(l)
        onDispose { ShizukuBridge.removeStateListener(l) }
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
            ShizukuBanner(
                state = shizukuState,
                onRequest = { ShizukuBridge.requestPermission(SHIZUKU_REQUEST_CODE) },
                onRefresh = { shizukuState = ShizukuBridge.state() },
            )

            AccessibilityBanner(
                enabled = asEnabled,
                preferShizuku = shizukuState == ShizukuBridge.State.READY,
                onOpenSettings = {
                    context.startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
                },
                onRefresh = { asEnabled = isAccessibilityEnabled(context) },
            )

            BatteryOptBanner(
                disabled = batteryOptDisabled,
                onRequest = { requestIgnoreBatteryOptimizations(context) },
                onRefresh = { batteryOptDisabled = isBatteryOptimizationDisabled(context) },
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
private fun ShizukuBanner(
    state: ShizukuBridge.State,
    onRequest: () -> Unit,
    onRefresh: () -> Unit,
) {
    val (containerColor, contentColor) = when (state) {
        ShizukuBridge.State.READY ->
            MaterialTheme.colorScheme.secondaryContainer to MaterialTheme.colorScheme.onSecondaryContainer
        ShizukuBridge.State.NOT_AUTHORIZED ->
            MaterialTheme.colorScheme.tertiaryContainer to MaterialTheme.colorScheme.onTertiaryContainer
        ShizukuBridge.State.UNAVAILABLE ->
            MaterialTheme.colorScheme.surfaceVariant to MaterialTheme.colorScheme.onSurfaceVariant
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
                text = when (state) {
                    ShizukuBridge.State.READY -> "Shizuku: ready ✓"
                    ShizukuBridge.State.NOT_AUTHORIZED -> "Shizuku: needs permission"
                    ShizukuBridge.State.UNAVAILABLE -> "Shizuku: not available"
                },
                style = MaterialTheme.typography.titleSmall,
            )
            Text(
                text = when (state) {
                    ShizukuBridge.State.READY ->
                        "Reading clipboard via the privileged binder — covers external keyboards, programmatic copies, and more."
                    ShizukuBridge.State.NOT_AUTHORIZED ->
                        "Tap to grant ClipBridge permission. With it, accessibility-event tricks become a fallback only."
                    ShizukuBridge.State.UNAVAILABLE ->
                        "Install the Shizuku app and start its service (ADB or wireless ADB), then refresh. Optional — accessibility still works without it."
                },
                style = MaterialTheme.typography.bodySmall,
            )
            if (state == ShizukuBridge.State.NOT_AUTHORIZED) {
                Button(onClick = onRequest, modifier = Modifier.fillMaxWidth()) {
                    Text("Grant Shizuku permission")
                }
            }
            Button(onClick = onRefresh, modifier = Modifier.fillMaxWidth()) {
                Text("Refresh status")
            }
        }
    }
}

@Composable
private fun AccessibilityBanner(
    enabled: Boolean,
    preferShizuku: Boolean,
    onOpenSettings: () -> Unit,
    onRefresh: () -> Unit,
) {
    // When Shizuku is doing the heavy lifting, AS becomes optional. Tone the
    // banner down (warning instead of error) so the user isn't pushed to
    // enable both.
    val (containerColor, contentColor) = when {
        enabled -> MaterialTheme.colorScheme.secondaryContainer to MaterialTheme.colorScheme.onSecondaryContainer
        preferShizuku -> MaterialTheme.colorScheme.surfaceVariant to MaterialTheme.colorScheme.onSurfaceVariant
        else -> MaterialTheme.colorScheme.errorContainer to MaterialTheme.colorScheme.onErrorContainer
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
                text = when {
                    enabled -> "Accessibility: enabled ✓"
                    preferShizuku -> "Accessibility: optional (Shizuku is active)"
                    else -> "Accessibility: NOT enabled"
                },
                style = MaterialTheme.typography.titleSmall,
            )
            Text(
                text = when {
                    enabled -> "Used as a fallback for copy detection. Safe to leave on."
                    preferShizuku -> "Shizuku already covers clipboard reads. Enable accessibility only if you want toast-based detection too."
                    else -> "Tap below and enable ClipBridge under Installed services — needed when Shizuku is not available."
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

private const val SHIZUKU_REQUEST_CODE = 0x5817

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

private fun isBatteryOptimizationDisabled(context: Context): Boolean {
    val pm = context.getSystemService(Context.POWER_SERVICE) as PowerManager
    return pm.isIgnoringBatteryOptimizations(context.packageName)
}

@Suppress("BatteryLife") // we sideload — Play Store policy doesn't apply
private fun requestIgnoreBatteryOptimizations(context: Context) {
    val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
        data = Uri.parse("package:${context.packageName}")
    }
    runCatching { context.startActivity(intent) }.onFailure {
        // Some OEM ROMs (e.g. heavily-stripped Samsung variants) reject the
        // direct intent. Fall back to the optimization list page so the user
        // can pick ClipBridge manually.
        runCatching {
            context.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
        }
    }
}

@Composable
private fun BatteryOptBanner(
    disabled: Boolean,
    onRequest: () -> Unit,
    onRefresh: () -> Unit,
) {
    val (containerColor, contentColor) = if (disabled) {
        MaterialTheme.colorScheme.secondaryContainer to MaterialTheme.colorScheme.onSecondaryContainer
    } else {
        MaterialTheme.colorScheme.tertiaryContainer to MaterialTheme.colorScheme.onTertiaryContainer
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
                text = if (disabled) {
                    "Battery optimization: disabled ✓"
                } else {
                    "Battery optimization: ON (recommended to disable)"
                },
                style = MaterialTheme.typography.titleSmall,
            )
            Text(
                text = if (disabled) {
                    "Android won't suspend ClipBridge while idle. Sync stays alive overnight."
                } else {
                    "Android may suspend the connection after the screen is off for a while. " +
                            "On Samsung you may also want Settings → Apps → ClipBridge → Battery → Unrestricted."
                },
                style = MaterialTheme.typography.bodySmall,
            )
            if (!disabled) {
                Button(onClick = onRequest, modifier = Modifier.fillMaxWidth()) {
                    Text("Disable battery optimization")
                }
            }
            Button(onClick = onRefresh, modifier = Modifier.fillMaxWidth()) {
                Text("Refresh status")
            }
        }
    }
}
