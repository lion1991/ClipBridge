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

# 一键打 IPA
./scripts/build-ios-ipa.sh
# 输出: build/ios/ClipBridge.ipa
```

## 安装

1. 把 IPA 弄到 iOS 设备(AirDrop / iCloud Drive / "文件" App)
2. 长按 IPA → "在 TrollStore 中打开" (或在 TrollStore 里点 + 选择)
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
│   ├── ContentView.swift   # 主屏:状态 pill + 配对入口卡片
│   ├── PairingScreen.swift # Sheet 弹层:扫码 / 高级 JSON / 重置
│   ├── PairingConfig.swift # 配对配置(与 Mac/Win/Android 互通)
│   ├── BridgeCoordinator.swift # 包裹 Rust Client,UIPasteboard 轮询,后台静默音频
│   └── ClipbridgeCore/
│       └── clipbridge_core.swift  # UniFFI 生成的 Swift 胶水,被编进 app 主模块
└── Resources/
    └── Assets.xcassets/    # 图标 (TODO: 加正式图标)
```

## 已知限制 / TODO

- **后台保活靠静默音频 hack**: AVAudioSession `.playback` + 0 音量 PCM 循环。
  TrollStore 私有 entitlements (no-sandbox) 通常已经够用,但加上音频路径作为
  双保险。如果将来发现 iOS 用户实际能稳定后台,可以考虑去掉。
- **图标缺失**: Assets.xcassets 里只有占位 contents.json,没有 1024×1024
  的 PNG。安装后会显示灰色默认图标。补一个 PNG 进 AppIcon.appiconset
  即可。
- **同 LAN 直连**还没做(Mac/Win 也都没做),全部走公网中继。后续如果做
  LAN bypass,iOS 上 `NSLocalNetworkUsageDescription` + Bonjour
  service 这套 entitlements TrollStore 也覆盖。
