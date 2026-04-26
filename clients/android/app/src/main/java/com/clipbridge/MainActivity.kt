package com.clipbridge

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.AdminPanelSettings
import androidx.compose.material.icons.filled.BatteryChargingFull
import androidx.compose.material.icons.filled.Bolt
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.ChevronRight
import androidx.compose.material.icons.filled.CloudOff
import androidx.compose.material.icons.filled.Error
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.LinkOff
import androidx.compose.material.icons.filled.QrCodeScanner
import androidx.compose.material.icons.filled.RadioButtonUnchecked
import androidx.compose.material.icons.filled.Sync
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import kotlinx.coroutines.launch
import kotlinx.serialization.json.Json
import rikka.shizuku.Shizuku

class MainActivity : ComponentActivity() {

    private val shizukuPermissionListener =
        Shizuku.OnRequestPermissionResultListener { _, _ -> /* state listener picks it up */ }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Edge-to-edge: lets the app surface paint under the system status bar.
        // Light style = dark icons on a light background so they remain visible
        // when the TopAppBar is white.
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.light(
                Color.Transparent.toArgb(),
                Color.Transparent.toArgb(),
            ),
        )
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
    var configText by remember {
        mutableStateOf(existing?.let { json.encodeToString(PairingConfig.serializer(), it) } ?: "")
    }
    var error by remember { mutableStateOf<String?>(null) }
    var advancedOpen by remember { mutableStateOf(false) }
    var asEnabled by remember { mutableStateOf(isAccessibilityEnabled(context)) }
    var batteryOptDisabled by remember { mutableStateOf(isBatteryOptimizationDisabled(context)) }
    var shizukuState by remember { mutableStateOf(ShizukuBridge.state()) }
    val isPaired = existing != null

    val snackbarHostState = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    fun toast(message: String) {
        scope.launch { snackbarHostState.showSnackbar(message) }
    }

    val connState by ClipBridgeAccessibilityService.stateFlow.collectAsStateWithLifecycle()

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
        error = null
        runCatching {
            val cfg = json.decodeFromString(PairingConfig.serializer(), contents)
            require(cfg.keyBytes()?.size == 32) { "密钥长度必须为 32 字节" }
            onSave(cfg)
            toast("已配对，开始同步")
        }.onFailure {
            error = "配对信息无效：${it.message}"
            toast("配对失败：${it.message}")
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("ClipBridge", style = MaterialTheme.typography.titleLarge) },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                ),
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        Column(
            modifier = Modifier
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 16.dp, vertical = 8.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            ConnectionPill(
                state = connState,
                paired = isPaired,
                asEnabled = asEnabled,
            )

            ScanHero(
                paired = isPaired,
                onScan = {
                    scanLauncher.launch(
                        ScanOptions()
                            .setCaptureActivity(PortraitCaptureActivity::class.java)
                            .setOrientationLocked(true)
                            .setBeepEnabled(false)
                            .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                            .setPrompt("对准另一台设备显示的二维码"),
                    )
                },
            )

            StatusSection(
                shizukuState = shizukuState,
                asEnabled = asEnabled,
                batteryOptDisabled = batteryOptDisabled,
                onShizukuTap = {
                    when (shizukuState) {
                        ShizukuBridge.State.NOT_AUTHORIZED ->
                            ShizukuBridge.requestPermission(SHIZUKU_REQUEST_CODE)
                        else -> { /* refresh by lifecycle resume */ }
                    }
                },
                onAccessibilityTap = {
                    context.startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
                },
                onBatteryTap = {
                    if (!batteryOptDisabled) requestIgnoreBatteryOptimizations(context)
                },
            )

            AdvancedToggle(open = advancedOpen, onToggle = { advancedOpen = !advancedOpen })
            AnimatedVisibility(visible = advancedOpen) {
                AdvancedPanel(
                    configText = configText,
                    error = error,
                    onConfigChange = { configText = it; error = null },
                    onGenerate = {
                        val cfg = PairingConfig.makeNew()
                        configText = json.encodeToString(PairingConfig.serializer(), cfg)
                    },
                    onSave = {
                        error = null
                        runCatching {
                            val cfg = json.decodeFromString(
                                PairingConfig.serializer(),
                                configText,
                            )
                            require(cfg.keyBytes()?.size == 32) { "密钥长度必须为 32 字节" }
                            onSave(cfg)
                            toast("已保存，开始同步")
                        }.onFailure {
                            error = "配对信息无效：${it.message}"
                            toast("保存失败：${it.message}")
                        }
                    },
                    onReset = {
                        onClear()
                        configText = ""
                        error = null
                        toast("已重置配对")
                    },
                )
            }

            Spacer(Modifier.height(8.dp))
            Text(
                "默认中继 · ${DEFAULT_RELAY_URL.removePrefix("wss://")}",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.fillMaxWidth(),
            )
        }
    }
}

/**
 * Compact pill above the hero card showing live connection state.
 * Combines the AccessibilityService's connection-state flow with the local
 * pairing / accessibility-enabled flags so the user always sees the most
 * actionable status.
 */
@Composable
private fun ConnectionPill(
    state: UiConnState,
    paired: Boolean,
    asEnabled: Boolean,
) {
    data class Pill(
        val label: String,
        val icon: androidx.compose.ui.graphics.vector.ImageVector,
        val container: Color,
        val content: Color,
    )

    val cs = MaterialTheme.colorScheme
    val pill = when {
        !asEnabled -> Pill("无障碍未启用", Icons.Filled.Warning, cs.errorContainer, cs.onErrorContainer)
        !paired -> Pill("未配对", Icons.Filled.LinkOff, cs.surfaceVariant, cs.onSurfaceVariant)
        else -> when (state) {
            UiConnState.Idle -> Pill("等待启动", Icons.Filled.RadioButtonUnchecked, cs.surfaceVariant, cs.onSurfaceVariant)
            UiConnState.Connecting -> Pill("连接中…", Icons.Filled.Sync, cs.tertiaryContainer, cs.onTertiaryContainer)
            UiConnState.Connected -> Pill("已连接 · 同步中", Icons.Filled.CheckCircle, cs.primaryContainer, cs.onPrimaryContainer)
            UiConnState.Disconnected -> Pill("已断开,正在重连", Icons.Filled.CloudOff, cs.surfaceVariant, cs.onSurfaceVariant)
            is UiConnState.Error -> Pill("连接出错:${state.message}", Icons.Filled.Error, cs.errorContainer, cs.onErrorContainer)
        }
    }

    Surface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(14.dp),
        color = pill.container,
        contentColor = pill.content,
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                imageVector = pill.icon,
                contentDescription = null,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.width(8.dp))
            Text(
                pill.label,
                style = MaterialTheme.typography.labelLarge,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

@Composable
private fun ScanHero(paired: Boolean, onScan: () -> Unit) {
    val container = if (paired) {
        MaterialTheme.colorScheme.secondaryContainer
    } else {
        MaterialTheme.colorScheme.primaryContainer
    }
    val onContainer = if (paired) {
        MaterialTheme.colorScheme.onSecondaryContainer
    } else {
        MaterialTheme.colorScheme.onPrimaryContainer
    }
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clickable { onScan() },
        colors = CardDefaults.cardColors(containerColor = container, contentColor = onContainer),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        shape = RoundedCornerShape(20.dp),
    ) {
        Row(
            modifier = Modifier.padding(20.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier
                    .size(56.dp)
                    .clip(CircleShape)
                    .background(onContainer.copy(alpha = 0.12f)),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    imageVector = if (paired) Icons.Filled.CheckCircle else Icons.Filled.QrCodeScanner,
                    contentDescription = null,
                    modifier = Modifier.size(28.dp),
                )
            }
            Spacer(Modifier.width(16.dp))
            Column(Modifier.weight(1f)) {
                Text(
                    text = if (paired) "重新扫码配对" else "扫码配对",
                    style = MaterialTheme.typography.titleMedium,
                )
                Text(
                    text = if (paired) {
                        "当前已配对，点击扫描新的二维码"
                    } else {
                        "在另一台设备上生成二维码，点击扫描"
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = onContainer.copy(alpha = 0.75f),
                )
            }
            Icon(Icons.Filled.ChevronRight, contentDescription = null)
        }
    }
}

@Composable
private fun StatusSection(
    shizukuState: ShizukuBridge.State,
    asEnabled: Boolean,
    batteryOptDisabled: Boolean,
    onShizukuTap: () -> Unit,
    onAccessibilityTap: () -> Unit,
    onBatteryTap: () -> Unit,
) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        shape = RoundedCornerShape(20.dp),
    ) {
        Column(modifier = Modifier.padding(vertical = 4.dp)) {
            StatusRow(
                icon = Icons.Filled.AdminPanelSettings,
                title = "Shizuku",
                subtitle = when (shizukuState) {
                    ShizukuBridge.State.READY -> "已启用高权限剪贴板读取"
                    ShizukuBridge.State.NOT_AUTHORIZED -> "点击授予权限"
                    ShizukuBridge.State.UNAVAILABLE -> "未检测到 Shizuku 服务"
                },
                state = when (shizukuState) {
                    ShizukuBridge.State.READY -> RowState.OK
                    ShizukuBridge.State.NOT_AUTHORIZED -> RowState.WARN
                    ShizukuBridge.State.UNAVAILABLE -> RowState.NEUTRAL
                },
                onClick = onShizukuTap,
            )
            Divider()
            StatusRow(
                icon = Icons.Filled.Visibility,
                title = "无障碍服务",
                subtitle = when {
                    asEnabled -> "可捕获选择菜单的复制操作"
                    shizukuState == ShizukuBridge.State.READY -> "可选 · Shizuku 已覆盖"
                    else -> "点击进入系统设置开启"
                },
                state = when {
                    asEnabled -> RowState.OK
                    shizukuState == ShizukuBridge.State.READY -> RowState.NEUTRAL
                    else -> RowState.WARN
                },
                onClick = onAccessibilityTap,
            )
            Divider()
            StatusRow(
                icon = Icons.Filled.BatteryChargingFull,
                title = "电池优化",
                subtitle = if (batteryOptDisabled) {
                    "已豁免 · 后台连接更稳"
                } else {
                    "点击设为不限制后台"
                },
                state = if (batteryOptDisabled) RowState.OK else RowState.WARN,
                onClick = onBatteryTap,
            )
        }
    }
}

private enum class RowState { OK, WARN, NEUTRAL }

@Composable
private fun StatusRow(
    icon: ImageVector,
    title: String,
    subtitle: String,
    state: RowState,
    onClick: () -> Unit,
) {
    val (badgeColor, badgeIcon) = when (state) {
        RowState.OK -> MaterialTheme.colorScheme.primary to Icons.Filled.CheckCircle
        RowState.WARN -> MaterialTheme.colorScheme.error to Icons.Filled.Warning
        RowState.NEUTRAL -> MaterialTheme.colorScheme.outline to Icons.Filled.Bolt
    }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable { onClick() }
            .padding(horizontal = 16.dp, vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            imageVector = icon,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.size(22.dp),
        )
        Spacer(Modifier.width(14.dp))
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.titleSmall)
            Text(
                subtitle,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Icon(
            imageVector = badgeIcon,
            contentDescription = null,
            tint = badgeColor,
            modifier = Modifier.size(20.dp),
        )
    }
}

@Composable
private fun Divider() {
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(1.dp)
            .padding(start = 52.dp)
            .background(MaterialTheme.colorScheme.outlineVariant.copy(alpha = 0.5f)),
    )
}

@Composable
private fun AdvancedToggle(open: Boolean, onToggle: () -> Unit) {
    TextButton(
        onClick = onToggle,
        modifier = Modifier.fillMaxWidth(),
    ) {
        Text(
            "高级",
            style = MaterialTheme.typography.labelLarge,
            modifier = Modifier.weight(1f),
        )
        Icon(
            imageVector = if (open) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
            contentDescription = null,
        )
    }
}

@Composable
private fun AdvancedPanel(
    configText: String,
    error: String?,
    onConfigChange: (String) -> Unit,
    onGenerate: () -> Unit,
    onSave: () -> Unit,
    onReset: () -> Unit,
) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surface),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        shape = RoundedCornerShape(20.dp),
    ) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(
                "在本机生成二维码，或粘贴另一台设备的配对 JSON。",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            OutlinedButton(onClick = onGenerate, modifier = Modifier.fillMaxWidth()) {
                Text("在本机生成新配对")
            }
            OutlinedTextField(
                value = configText,
                onValueChange = onConfigChange,
                label = { Text("配对 JSON") },
                modifier = Modifier
                    .fillMaxWidth()
                    .height(180.dp),
                textStyle = MaterialTheme.typography.bodySmall,
            )
            error?.let {
                Text(
                    it,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            Button(
                onClick = onSave,
                modifier = Modifier.fillMaxWidth(),
                enabled = configText.isNotBlank(),
            ) {
                Text("保存并开始同步")
            }
            TextButton(
                onClick = onReset,
                modifier = Modifier.fillMaxWidth(),
                colors = ButtonDefaults.textButtonColors(
                    contentColor = MaterialTheme.colorScheme.error,
                ),
            ) {
                Text("重置配对")
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
        runCatching {
            context.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
        }
    }
}
