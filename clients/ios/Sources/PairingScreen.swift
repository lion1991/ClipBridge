import AVFoundation
import SwiftUI
import UIKit

struct PairingScreen: View {
    @Binding var isPresented: Bool
    let coordinator: BridgeCoordinator

    @State private var configJson: String = ""
    @State private var error: String?
    @State private var showAdvanced: Bool = false
    @State private var showScanner: Bool = false
    @State private var showResetConfirm: Bool = false

    private static let json: JSONEncoder = {
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        return enc
    }()

    var body: some View {
        NavigationView {
            Form {
                Section {
                    Button {
                        showScanner = true
                    } label: {
                        Label("扫描另一台设备的二维码", systemImage: "qrcode.viewfinder")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                    .listRowBackground(Color.clear)
                    .listRowInsets(EdgeInsets())
                }

                Section {
                    DisclosureGroup(isExpanded: $showAdvanced) {
                        VStack(alignment: .leading, spacing: 10) {
                            Text("粘贴另一台设备的配对 JSON,或在本机生成新配对(其他设备扫码)。")
                                .font(.caption)
                                .foregroundColor(.secondary)

                            TextEditor(text: $configJson)
                                .font(.system(.caption, design: .monospaced))
                                .frame(height: 130)
                                .padding(6)
                                .background(
                                    RoundedRectangle(cornerRadius: 8)
                                        .fill(Color(.systemBackground))
                                )
                                .overlay(
                                    RoundedRectangle(cornerRadius: 8)
                                        .stroke(Color(.separator), lineWidth: 1)
                                )

                            if let error {
                                Text(error)
                                    .font(.caption)
                                    .foregroundColor(.red)
                            }

                            HStack(spacing: 8) {
                                Button("生成新配对", action: generate)
                                    .buttonStyle(.bordered)
                                Button("保存", action: save)
                                    .buttonStyle(.borderedProminent)
                                Spacer()
                                Button("复制", action: copyJSON)
                                    .buttonStyle(.bordered)
                            }
                        }
                        .padding(.top, 6)
                    } label: {
                        Text("高级选项").font(.subheadline.weight(.medium))
                    }
                }

                if coordinator.hasPairing {
                    Section {
                        Button(role: .destructive) {
                            showResetConfirm = true
                        } label: {
                            Text("重置配对")
                                .frame(maxWidth: .infinity)
                        }
                        .confirmationDialog(
                            "重置后所有已配对设备都需要重新配对",
                            isPresented: $showResetConfirm,
                            titleVisibility: .visible
                        ) {
                            Button("确认重置", role: .destructive) {
                                coordinator.resetPairing()
                                configJson = ""
                                isPresented = false
                            }
                            Button("取消", role: .cancel) {}
                        }
                    }
                }
            }
            .navigationTitle("配对")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("关闭") { isPresented = false }
                }
            }
            .sheet(isPresented: $showScanner) {
                QRScannerView { code in
                    showScanner = false
                    configJson = code
                    save()
                    if error == nil { isPresented = false }
                }
            }
            .onAppear {
                if let existing = PairingStore.load() {
                    configJson = (try? Self.encode(existing)) ?? ""
                }
            }
        }
        .navigationViewStyle(.stack)
    }

    private func generate() {
        let cfg = PairingConfig.makeNew()
        configJson = (try? Self.encode(cfg)) ?? ""
        error = nil
    }

    private func save() {
        let raw = configJson.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !raw.isEmpty,
              let data = raw.data(using: .utf8),
              let cfg = try? JSONDecoder().decode(PairingConfig.self, from: data),
              cfg.keyData != nil
        else {
            error = "配对信息无效:需要 relay_url、group_id、key (32 字节 base64url)"
            return
        }
        error = nil
        coordinator.savePairing(cfg)
    }

    private func copyJSON() {
        UIPasteboard.general.string = configJson
    }

    private static func encode(_ cfg: PairingConfig) throws -> String {
        let data = try json.encode(cfg)
        return String(data: data, encoding: .utf8) ?? ""
    }
}

// MARK: - QR scanner (AVFoundation-based, no third-party deps)

struct QRScannerView: UIViewControllerRepresentable {
    let onResult: (String) -> Void

    func makeUIViewController(context: Context) -> QRScannerVC {
        let vc = QRScannerVC()
        vc.onResult = onResult
        return vc
    }

    func updateUIViewController(_ uiViewController: QRScannerVC, context: Context) {}
}

final class QRScannerVC: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    var onResult: ((String) -> Void)?

    private let session = AVCaptureSession()
    private var preview: AVCaptureVideoPreviewLayer?

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        configureSession()

        let cancel = UIButton(type: .system)
        cancel.setTitle("取消", for: .normal)
        cancel.setTitleColor(.white, for: .normal)
        cancel.titleLabel?.font = .systemFont(ofSize: 16, weight: .medium)
        cancel.addTarget(self, action: #selector(dismissSelf), for: .touchUpInside)
        cancel.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(cancel)
        NSLayoutConstraint.activate([
            cancel.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 12),
            cancel.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 16),
        ])

        let prompt = UILabel()
        prompt.text = "对准另一台设备显示的二维码"
        prompt.textColor = .white
        prompt.font = .systemFont(ofSize: 15, weight: .medium)
        prompt.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(prompt)
        NSLayoutConstraint.activate([
            prompt.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor, constant: -32),
            prompt.centerXAnchor.constraint(equalTo: view.centerXAnchor),
        ])
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        if !session.isRunning {
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                self?.session.startRunning()
            }
        }
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        if session.isRunning {
            session.stopRunning()
        }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        preview?.frame = view.bounds
    }

    private func configureSession() {
        guard
            let device = AVCaptureDevice.default(for: .video),
            let input = try? AVCaptureDeviceInput(device: device),
            session.canAddInput(input)
        else { return }
        session.addInput(input)

        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else { return }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        output.metadataObjectTypes = [.qr]

        let layer = AVCaptureVideoPreviewLayer(session: session)
        layer.videoGravity = .resizeAspectFill
        layer.frame = view.bounds
        view.layer.insertSublayer(layer, at: 0)
        preview = layer
    }

    @objc private func dismissSelf() {
        dismiss(animated: true)
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard
            let obj = metadataObjects.first as? AVMetadataMachineReadableCodeObject,
            obj.type == .qr,
            let value = obj.stringValue
        else { return }
        session.stopRunning()
        onResult?(value)
        dismiss(animated: true)
    }
}
