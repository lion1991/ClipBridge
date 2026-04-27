import Foundation

/// Mirror of the wire format used by Mac, Android, and Windows clients —
/// scanning any of their QRs on iOS just works.
let DEFAULT_RELAY_URL = "wss://clip.wrlog.cn"

struct PairingConfig: Codable, Equatable {
    var relayUrl: String
    var groupId: String
    /// 32-byte ChaCha20-Poly1305 key, base64url (no padding) on the wire.
    var keyBase64Url: String

    enum CodingKeys: String, CodingKey {
        case relayUrl = "relay_url"
        case groupId = "group_id"
        case keyBase64Url = "key"
    }

    var keyData: Data? {
        Data(base64URLNoPad: keyBase64Url)
    }

    static func makeNew(relayUrl: String = DEFAULT_RELAY_URL) -> PairingConfig {
        var keyBytes = [UInt8](repeating: 0, count: 32)
        let result = SecRandomCopyBytes(kSecRandomDefault, keyBytes.count, &keyBytes)
        precondition(result == errSecSuccess, "SecRandomCopyBytes failed")
        let keyData = Data(keyBytes)
        return PairingConfig(
            relayUrl: relayUrl,
            groupId: UUID().uuidString.lowercased(),
            keyBase64Url: keyData.base64URLNoPadString
        )
    }
}

extension Data {
    init?(base64URLNoPad input: String) {
        var s = input.replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        let pad = (4 - s.count % 4) % 4
        s.append(String(repeating: "=", count: pad))
        guard let data = Data(base64Encoded: s) else { return nil }
        self = data
    }

    var base64URLNoPadString: String {
        base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}

enum PairingStore {
    /// Shared between the main app and the keyboard extension. Must match
    /// the value in both targets' entitlements (`application-groups`).
    static let appGroupId = "group.com.clipbridge.shared"

    private static let pairingKey = "com.clipbridge.pairing.v1"
    private static let deviceIdKey = "com.clipbridge.device_id"

    /// Falls back to `.standard` only if the App Group container hasn't been
    /// granted yet (eg. user is on a build that predates this entitlement).
    /// Keyboard-extension reads will silently fail in that case, which is
    /// acceptable — they'll start working as soon as the user re-installs
    /// the IPA with both entitlements files signed in.
    private static var defaults: UserDefaults {
        UserDefaults(suiteName: appGroupId) ?? .standard
    }

    static func load() -> PairingConfig? {
        if let data = defaults.data(forKey: pairingKey),
           let cfg = try? JSONDecoder().decode(PairingConfig.self, from: data) {
            return cfg
        }
        // One-shot migration: pre-keyboard-extension installs stored the
        // pairing in `.standard`. Lift it into the App Group container so the
        // keyboard can see it, then clear the legacy copy.
        guard
            let legacyData = UserDefaults.standard.data(forKey: pairingKey),
            let cfg = try? JSONDecoder().decode(PairingConfig.self, from: legacyData)
        else { return nil }
        defaults.set(legacyData, forKey: pairingKey)
        UserDefaults.standard.removeObject(forKey: pairingKey)
        return cfg
    }

    static func save(_ config: PairingConfig) {
        if let data = try? JSONEncoder().encode(config) {
            defaults.set(data, forKey: pairingKey)
        }
    }

    static func clear() {
        defaults.removeObject(forKey: pairingKey)
    }

    static func deviceId() -> String {
        if let id = defaults.string(forKey: deviceIdKey) { return id }
        // Same migration story as pairing: legacy device_id lived in
        // .standard. Lift it before minting a fresh UUID, otherwise the
        // device looks like a brand new client to the relay.
        if let legacy = UserDefaults.standard.string(forKey: deviceIdKey) {
            defaults.set(legacy, forKey: deviceIdKey)
            UserDefaults.standard.removeObject(forKey: deviceIdKey)
            return legacy
        }
        let id = UUID().uuidString
        defaults.set(id, forKey: deviceIdKey)
        return id
    }
}
