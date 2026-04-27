package com.clipbridge

import android.Manifest
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.Image as ImageView
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import android.graphics.BitmapFactory
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
import androidx.compose.material.icons.filled.PhotoLibrary
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.Image
import androidx.compose.material.icons.filled.SaveAlt
import androidx.compose.material.icons.filled.Share
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
import androidx.compose.material3.IconButton
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
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
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import java.io.File
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
    var imageReadGranted by remember { mutableStateOf(isImageReadGranted(context)) }

    val imagePermLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted -> imageReadGranted = granted }

    // NEARBY_WIFI_DEVICES is required for any Wi-Fi-based peer discovery
    // (mDNS / NSD) on Android 13+. Ask once, opportunistically — if the
    // user denies we silently fall back to relay-only. This isn't gated
    // on a button press because the LAN feature is always-on; the dialog
    // shows up the first time MainActivity is opened on a 13+ device.
    val nearbyWifiLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { /* result is informational; LAN code degrades silently if denied */ }
    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val perm = "android.permission.NEARBY_WIFI_DEVICES"
            if (ContextCompat.checkSelfPermission(context, perm) !=
                PackageManager.PERMISSION_GRANTED
            ) {
                nearbyWifiLauncher.launch(perm)
            }
        }
    }

    val isPaired = existing != null

    val snackbarHostState = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    fun toast(message: String) {
        scope.launch { snackbarHostState.showSnackbar(message) }
    }

    val connState by ClipBridgeAccessibilityService.stateFlow.collectAsStateWithLifecycle()
    val lanPeerNames by ClipBridgeAccessibilityService.lanPeerNames.collectAsStateWithLifecycle()

    val lifecycle = LocalLifecycleOwner.current.lifecycle
    DisposableEffect(lifecycle) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                asEnabled = isAccessibilityEnabled(context)
                batteryOptDisabled = isBatteryOptimizationDisabled(context)
                shizukuState = ShizukuBridge.state()
                imageReadGranted = isImageReadGranted(context)
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

    val pickMediaLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.PickMultipleVisualMedia(maxItems = 9),
    ) { uris ->
        if (uris.isEmpty()) return@rememberLauncherForActivityResult
        for (uri in uris) {
            // PickVisualMedia hands us a content URI with temporary read
            // permission scoped to this activity. The accessibility
            // service runs in the same process, so it can re-use the
            // permission while the activity is alive — we read bytes
            // synchronously inside `sendImageFromUri`'s Dispatchers.IO
            // launch to keep the URI valid.
            ClipBridgeAccessibilityService.activeService()?.sendImageFromUri(uri)
                ?: scope.launch {
                    snackbarHostState.showSnackbar("无障碍服务未启动, 无法发送")
                }
        }
    }

    val imageHistory by ClipBridgeAccessibilityService.imageHistory.collectAsStateWithLifecycle()
    var selectedTab by remember { mutableStateOf(BottomTab.Sync) }

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
                title = {
                    Text(
                        when (selectedTab) {
                            BottomTab.Sync -> "ClipBridge"
                            BottomTab.Images -> "图片"
                        },
                        style = MaterialTheme.typography.titleLarge,
                    )
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                ),
            )
        },
        bottomBar = {
            NavigationBar {
                NavigationBarItem(
                    selected = selectedTab == BottomTab.Sync,
                    onClick = { selectedTab = BottomTab.Sync },
                    icon = { Icon(Icons.Filled.Sync, contentDescription = null) },
                    label = { Text("同步") },
                )
                NavigationBarItem(
                    selected = selectedTab == BottomTab.Images,
                    onClick = { selectedTab = BottomTab.Images },
                    icon = { Icon(Icons.Filled.PhotoLibrary, contentDescription = null) },
                    label = { Text("图片") },
                )
            }
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        when (selectedTab) {
            BottomTab.Sync -> SyncTabContent(
                padding = padding,
                connState = connState,
                lanPeerNames = lanPeerNames,
                isPaired = isPaired,
                asEnabled = asEnabled,
                shizukuState = shizukuState,
                batteryOptDisabled = batteryOptDisabled,
                imageReadGranted = imageReadGranted,
                advancedOpen = advancedOpen,
                configText = configText,
                error = error,
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
                onImageReadTap = {
                    if (!imageReadGranted) {
                        imagePermLauncher.launch(imageReadPermissionName())
                    }
                },
                onAdvancedToggle = { advancedOpen = !advancedOpen },
                onConfigChange = { configText = it; error = null },
                onGenerate = {
                    val cfg = PairingConfig.makeNew()
                    configText = json.encodeToString(PairingConfig.serializer(), cfg)
                },
                onSavePairing = {
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
                onResetPairing = {
                    onClear()
                    configText = ""
                    error = null
                    toast("已重置配对")
                },
            )
            BottomTab.Images -> ImagesTabContent(
                padding = padding,
                isPaired = isPaired,
                history = imageHistory,
                onPickFromGallery = {
                    pickMediaLauncher.launch(
                        PickVisualMediaRequest(
                            ActivityResultContracts.PickVisualMedia.ImageOnly,
                        ),
                    )
                },
                onSaveToGallery = { entry ->
                    scope.launch {
                        val uri = withContext(Dispatchers.IO) {
                            ImagePipeline.saveToGallery(context, entry.bytes, entry.mime)
                        }
                        snackbarHostState.showSnackbar(
                            if (uri != null) "已保存到「图片/ClipBridge」" else "保存失败"
                        )
                    }
                },
                onCopyToClipboard = { entry ->
                    val cb = context.getSystemService(Context.CLIPBOARD_SERVICE)
                        as android.content.ClipboardManager
                    val ok = ImagePipeline.writeImageToClipboard(
                        cb, context, entry.bytes, entry.mime,
                    )
                    scope.launch {
                        snackbarHostState.showSnackbar(
                            if (ok) "已复制到剪切板, 可在其它 app 粘贴" else "复制失败"
                        )
                    }
                },
                onShare = { entry -> shareImage(context, entry) },
            )
        }
    }
}

private enum class BottomTab { Sync, Images }

@Composable
private fun SyncTabContent(
    padding: androidx.compose.foundation.layout.PaddingValues,
    connState: UiConnState,
    lanPeerNames: List<String>,
    isPaired: Boolean,
    asEnabled: Boolean,
    shizukuState: ShizukuBridge.State,
    batteryOptDisabled: Boolean,
    imageReadGranted: Boolean,
    advancedOpen: Boolean,
    configText: String,
    error: String?,
    onScan: () -> Unit,
    onShizukuTap: () -> Unit,
    onAccessibilityTap: () -> Unit,
    onBatteryTap: () -> Unit,
    onImageReadTap: () -> Unit,
    onAdvancedToggle: () -> Unit,
    onConfigChange: (String) -> Unit,
    onGenerate: () -> Unit,
    onSavePairing: () -> Unit,
    onResetPairing: () -> Unit,
) {
    Column(
        modifier = Modifier
            .padding(padding)
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 16.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        ConnectionPill(state = connState, paired = isPaired, asEnabled = asEnabled, lanPeerNames = lanPeerNames)
        ScanHero(paired = isPaired, onScan = onScan)
        StatusSection(
            shizukuState = shizukuState,
            asEnabled = asEnabled,
            batteryOptDisabled = batteryOptDisabled,
            imageReadGranted = imageReadGranted,
            onShizukuTap = onShizukuTap,
            onAccessibilityTap = onAccessibilityTap,
            onBatteryTap = onBatteryTap,
            onImageReadTap = onImageReadTap,
        )
        AdvancedToggle(open = advancedOpen, onToggle = onAdvancedToggle)
        AnimatedVisibility(visible = advancedOpen) {
            AdvancedPanel(
                configText = configText,
                error = error,
                onConfigChange = onConfigChange,
                onGenerate = onGenerate,
                onSave = onSavePairing,
                onReset = onResetPairing,
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

@Composable
private fun ImagesTabContent(
    padding: androidx.compose.foundation.layout.PaddingValues,
    isPaired: Boolean,
    history: List<ImageHistoryEntry>,
    onPickFromGallery: () -> Unit,
    onSaveToGallery: (ImageHistoryEntry) -> Unit,
    onCopyToClipboard: (ImageHistoryEntry) -> Unit,
    onShare: (ImageHistoryEntry) -> Unit,
) {
    Column(
        modifier = Modifier
            .padding(padding)
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 16.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        if (!isPaired) {
            // Pre-pairing nudge — pairing lives on the sync tab.
            Card(
                modifier = Modifier.fillMaxWidth(),
                colors = CardDefaults.cardColors(
                    containerColor = MaterialTheme.colorScheme.surfaceVariant,
                ),
                elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
                shape = RoundedCornerShape(20.dp),
            ) {
                Row(
                    modifier = Modifier.padding(20.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        Icons.Filled.QrCodeScanner,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.width(12.dp))
                    Text(
                        "先到「同步」标签完成配对",
                        style = MaterialTheme.typography.bodyMedium,
                    )
                }
            }
            return@Column
        }

        // Two sub-sections: received and sent. Keeps the user mental model
        // aligned with iOS's "最近收到 / 最近发送" cards.
        val received = history.filter { it.direction == ImageHistoryEntry.Direction.RECEIVED }
        val sent = history.filter { it.direction == ImageHistoryEntry.Direction.SENT }

        ImageTransferCard(
            title = "最近收到",
            emptyMessage = "暂无 — 等其他设备发图过来",
            history = received,
            onPickFromGallery = onPickFromGallery,
            showPicker = true,
            onSaveToGallery = onSaveToGallery,
            onCopyToClipboard = onCopyToClipboard,
            onShare = onShare,
        )
        ImageTransferCard(
            title = "最近发送",
            emptyMessage = "暂无 — 选图发送或本机复制图片后会出现",
            history = sent,
            onPickFromGallery = onPickFromGallery,
            showPicker = false,
            onSaveToGallery = onSaveToGallery,
            onCopyToClipboard = onCopyToClipboard,
            onShare = onShare,
        )
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
    lanPeerNames: List<String>,
) {
    data class Pill(
        val label: String,
        val icon: androidx.compose.ui.graphics.vector.ImageVector,
        val container: Color,
        val content: Color,
    )

    val cs = MaterialTheme.colorScheme
    // Only worth surfacing the transport hint once we're actually connected
    // — before that the user cares about why pairing/connection isn't up,
    // not which transport will be used when it does.
    val transportSuffix = if (paired && asEnabled && state is UiConnState.Connected) {
        if (lanPeerNames.isNotEmpty()) {
            " · 局域网 ${lanPeerNames.size} (${lanPeerNames.joinToString(", ")})"
        } else " · 仅中继"
    } else ""
    val pill = when {
        !asEnabled -> Pill("无障碍未启用", Icons.Filled.Warning, cs.errorContainer, cs.onErrorContainer)
        !paired -> Pill("未配对", Icons.Filled.LinkOff, cs.surfaceVariant, cs.onSurfaceVariant)
        else -> when (state) {
            UiConnState.Idle -> Pill("等待启动", Icons.Filled.RadioButtonUnchecked, cs.surfaceVariant, cs.onSurfaceVariant)
            UiConnState.Connecting -> Pill("连接中…", Icons.Filled.Sync, cs.tertiaryContainer, cs.onTertiaryContainer)
            UiConnState.Connected -> Pill("已连接 · 同步中$transportSuffix", Icons.Filled.CheckCircle, cs.primaryContainer, cs.onPrimaryContainer)
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
    imageReadGranted: Boolean,
    onShizukuTap: () -> Unit,
    onAccessibilityTap: () -> Unit,
    onBatteryTap: () -> Unit,
    onImageReadTap: () -> Unit,
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
                icon = Icons.Filled.PhotoLibrary,
                title = "图片读取权限",
                subtitle = if (imageReadGranted) {
                    "已授权 · 复制相册图片可同步"
                } else {
                    "点击授予 · 否则相册复制图只能传出标题"
                },
                state = if (imageReadGranted) RowState.OK else RowState.WARN,
                onClick = onImageReadTap,
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

/// Permission name swaps at API 33: READ_MEDIA_IMAGES is the new
/// granular replacement for READ_EXTERNAL_STORAGE on Tiramisu+.
private fun imageReadPermissionName(): String =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        Manifest.permission.READ_MEDIA_IMAGES
    } else {
        Manifest.permission.READ_EXTERNAL_STORAGE
    }

private fun isImageReadGranted(context: Context): Boolean {
    val perm = imageReadPermissionName()
    return ContextCompat.checkSelfPermission(context, perm) ==
        PackageManager.PERMISSION_GRANTED
}

/**
 * Image transfer area: send-from-gallery button + horizontally-scrolling
 * thumbnails of recent images (received and sent). Each thumbnail offers
 * "保存到相册" and "分享" via tap-to-expand actions.
 *
 * No image text body — that's covered in the existing pasteboard sync.
 * This card is for explicit image traffic the user wants to act on.
 */
@Composable
private fun ImageTransferCard(
    title: String,
    emptyMessage: String,
    history: List<ImageHistoryEntry>,
    onPickFromGallery: () -> Unit,
    /// True only on the "最近收到" card so the picker button doesn't
    /// appear twice (would be confusing — it's the same action either way).
    showPicker: Boolean,
    onSaveToGallery: (ImageHistoryEntry) -> Unit,
    onCopyToClipboard: (ImageHistoryEntry) -> Unit,
    onShare: (ImageHistoryEntry) -> Unit,
) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.surfaceVariant,
        ),
        elevation = CardDefaults.cardElevation(defaultElevation = 0.dp),
        shape = RoundedCornerShape(20.dp),
    ) {
        Column(
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 14.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(
                    imageVector = Icons.Filled.Image,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.width(10.dp))
                Text(
                    title,
                    style = MaterialTheme.typography.titleSmall,
                    modifier = Modifier.weight(1f),
                )
                if (showPicker) {
                    TextButton(onClick = onPickFromGallery) {
                        Text("从相册选图")
                    }
                }
            }
            if (history.isEmpty()) {
                Text(
                    emptyMessage,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                LazyRow(
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    items(history, key = { it.id }) { entry ->
                        ImageThumbCell(
                            entry = entry,
                            onSave = { onSaveToGallery(entry) },
                            onCopy = { onCopyToClipboard(entry) },
                            onShare = { onShare(entry) },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun ImageThumbCell(
    entry: ImageHistoryEntry,
    onSave: () -> Unit,
    onCopy: () -> Unit,
    onShare: () -> Unit,
) {
    // Decode bytes to a Bitmap on first composition. NSCache-style bounded
    // map would be nicer but Compose's `remember(entry.id)` keying gets us
    // ~free re-decode skip on scrolls.
    val bitmap = remember(entry.id) {
        runCatching {
            BitmapFactory.decodeByteArray(entry.bytes, 0, entry.bytes.size)
        }.getOrNull()
    }
    Column(
        modifier = Modifier.width(120.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(120.dp)
                .clip(RoundedCornerShape(10.dp))
                .background(MaterialTheme.colorScheme.surface),
            contentAlignment = Alignment.Center,
        ) {
            if (bitmap != null) {
                ImageView(
                    bitmap = bitmap.asImageBitmap(),
                    contentDescription = null,
                    modifier = Modifier.fillMaxSize(),
                    contentScale = ContentScale.Crop,
                )
            } else {
                Icon(
                    imageVector = Icons.Filled.Image,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.outline,
                )
            }
        }
        Text(
            "${entry.width}×${entry.height} · ${entry.sizeLabel}",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Text(
            entry.deviceName,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
            IconButton(onClick = onCopy, modifier = Modifier.size(28.dp)) {
                Icon(
                    Icons.Filled.ContentCopy,
                    contentDescription = "复制到剪切板",
                    modifier = Modifier.size(18.dp),
                )
            }
            IconButton(onClick = onSave, modifier = Modifier.size(28.dp)) {
                Icon(
                    Icons.Filled.SaveAlt,
                    contentDescription = "保存到相册",
                    modifier = Modifier.size(18.dp),
                )
            }
            IconButton(onClick = onShare, modifier = Modifier.size(28.dp)) {
                Icon(
                    Icons.Filled.Share,
                    contentDescription = "分享",
                    modifier = Modifier.size(18.dp),
                )
            }
        }
    }
}

/**
 * Share via Android's standard chooser (ACTION_SEND with the entry's
 * image mime). Routes through our FileProvider so any picked target —
 * Files, Photos, messenger — can read the bytes via its temporary URI
 * permission.
 */
private fun shareImage(context: Context, entry: ImageHistoryEntry) {
    val ext = when (entry.mime) {
        "image/png" -> "png"
        "image/jpeg" -> "jpg"
        else -> "img"
    }
    val cacheDir = File(context.cacheDir, "clipbridge_images").apply { mkdirs() }
    val file = File(cacheDir, "share_${entry.id.take(16)}.$ext")
    if (!file.exists() || file.length() != entry.bytes.size.toLong()) {
        file.writeBytes(entry.bytes)
    }
    val uri = FileProvider.getUriForFile(
        context,
        "${context.packageName}.fileprovider",
        file,
    )
    val send = Intent(Intent.ACTION_SEND).apply {
        type = entry.mime
        putExtra(Intent.EXTRA_STREAM, uri)
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
    context.startActivity(Intent.createChooser(send, "分享图片"))
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
