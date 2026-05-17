# ClipBridge — iOS 客户端 (TrollStore)

依赖 TrollStore 进行任意 entitlements 签名,因此**仅适用于 TrollStore 已经
安装的设备**(iOS 14.0 - 17.0,具体可在 https://ios.cfw.guide 查询当前
设备 + iOS 版本是否支持)。

## 为什么必须 TrollStore

iOS 14+ 限制后台 App 读 `UIPasteboard`,标准侧载 (Apple ID 7-day,
AltStore) 没办法绕过。TrollStore 用 CoreTrust bug 让我们能签上私有
entitlements:

```
com.apple.private.security.no-sandbox      解除沙箱限制
com.apple.private.security.no-container    无容器,可访问任意路径
platform-application                        提升为系统级 App 优先级
com.apple.UIKit.allow-paste-without-prompt  iOS 16+ 粘贴不再弹"已粘贴自"
```

后台 + 剪贴板 + 持久 WebSocket 全部解锁,体验接近 Mac/Windows 端。

## 编译

需要 macOS + Xcode 26+(Xcode 16 也可,只要 iOS SDK ≥ 15)+ Rust
工具链(rustup target add aarch64-apple-ios aarch64-apple-ios-sim)。

```bash
# 一次性
brew install xcodegen

# 一键打 TIPA(TrollStore IPA,扩展名让 iOS 直接路由到 TrollStore)
./scripts/build-ios-ipa.sh
# 输出: build/ios/ClipBridge.tipa
```

## 安装

1. 把 TIPA 弄到 iOS 设备(AirDrop / iCloud Drive / "文件" App)
2. 长按 TIPA → "在 TrollStore 中打开" (或在 TrollStore 里点 + 选择)
3. TrollStore 列表出现 ClipBridge → 点"安装"
4. 主屏幕出现图标,点开 → 扫码配对完成 → Mac 端复制立即同步到 iPhone

## 项目结构

```
clients/ios/
├── project.yml             # xcodegen 配置:Bundle ID、background modes、入口
├── Info.plist              # 由 xcodegen 从 project.yml 生成
├── ClipBridge.entitlements # TrollStore 私有 entitlements (手写,不由 xcodegen 覆盖)
├── ClipBridge.xcodeproj    # xcodegen 生成,Git 忽略
├── Sources/
│   ├── App.swift           # @main + AppDelegate + 协调器单例
│   ├── ContentView.swift   # 主屏:同步状态、文件/图片传输入口和历史
│   ├── FilePickerSheet.swift # 系统文件选择器
│   ├── FileTransferModels.swift # 文件传输 UI 模型和格式化
│   ├── PairingScreen.swift # Sheet 弹层:扫码 / 高级 JSON / 重置
│   ├── PairingConfig.swift # 配对配置(与 Mac/Win/Android 互通)
│   ├── BridgeCoordinator.swift # 包裹 Rust Client,UIPasteboard/文件传输协调,后台静默音频
│   └── ClipbridgeCore/
│       └── clipbridge_core.swift  # UniFFI 生成的 Swift 胶水,被编进 app 主模块
└── Resources/
    └── Assets.xcassets/    # 图标 (TODO: 加正式图标)
```

## 已知限制 / TODO

- **后台保活靠静默音频 hack**: AVAudioSession `.playback` + 0 音量 PCM 循环。
  TrollStore 私有 entitlements (no-sandbox) 通常已经够用,但加上音频路径作为
  双保险。如果将来发现 iOS 用户实际能稳定后台,可以考虑去掉。
- **图标是临时的**: `AppIcon.appiconset/icon-1024.png` 由 `clients/windows/icons/source.svg`
  用 rsvg-convert 渲染出来,凑合用。空的 AppIcon 会让 TrollStore 安装时
  LaunchServices 注册失败(error 181 / "将应用添加至图标缓存失败"),所以
  这里必须有一张 1024×1024、不带 alpha 通道的 PNG。换正式图标时记得保留
  这两个属性。
- **LAN 文件传输只覆盖主 App 前台**: 发送使用系统文件选择器,接收保存到
  `Documents/ClipBridge`,并通过 Files App / 分享面板打开。键盘扩展不参与
  文件传输,也不保证后台接收。
