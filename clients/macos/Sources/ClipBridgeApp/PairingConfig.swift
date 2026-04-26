import Foundation

/// Default relay endpoint used when generating new pairings. Hidden from the
/// pairing UI so the user doesn't have to know about server URLs at all.
/// (The pairing JSON / QR still carries the URL for forward compatibility,
/// in case we ever ship a "use a different relay" advanced setting.)
let DEFAULT_RELAY_URL = "wss://clip.wrlog.cn"

/// Mirror of Rust core::group::GroupConfig — what the QR code or paste-buffer
/// transports between devices to bootstrap a sync group.
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
    private static let key = "com.clipbridge.pairing.v1"

    static func load() -> PairingConfig? {
        guard let data = UserDefaults.standard.data(forKey: key) else { return nil }
        return try? JSONDecoder().decode(PairingConfig.self, from: data)
    }

    static func save(_ config: PairingConfig) {
        if let data = try? JSONEncoder().encode(config) {
            UserDefaults.standard.set(data, forKey: key)
        }
    }

    static func clear() {
        UserDefaults.standard.removeObject(forKey: key)
    }
}
