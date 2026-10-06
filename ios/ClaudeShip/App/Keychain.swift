import Foundation
import Security

/// Minimal generic-password wrapper for the one secret the app holds: the
/// hub's pairing token. Everything else is plain UserDefaults.
enum Keychain {
    private static let service = "com.python21.ClaudeShip"
    /// errSecMissingEntitlement: an unsigned build (the simulator, built
    /// with CODE_SIGNING_ALLOWED=NO) has no keychain at all. Only then does
    /// the secret fall back to UserDefaults; a signed build on a phone
    /// never takes that path.
    private static let unavailable: OSStatus = -34018
    private static func fallbackKey(_ account: String) -> String { "keychain-fallback.\(account)" }

    static func string(for account: String) -> String? {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
            kSecReturnData: true,
            kSecMatchLimit: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        guard status == errSecSuccess, let data = item as? Data else {
            if status == unavailable { return UserDefaults.standard.string(forKey: fallbackKey(account)) }
            if status != errSecItemNotFound { NSLog("ClaudeShip: keychain read failed: %d", status) }
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    @discardableResult
    static func set(_ value: String, for account: String) -> Bool {
        if value.isEmpty {
            delete(account)
            return true
        }
        let base: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
        ]
        let attributes: [CFString: Any] = [
            kSecValueData: Data(value.utf8),
            kSecAttrAccessible: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
        var status = SecItemUpdate(base as CFDictionary, attributes as CFDictionary)
        if status == errSecItemNotFound {
            status = SecItemAdd(base.merging(attributes) { $1 } as CFDictionary, nil)
        }
        if status == unavailable {
            UserDefaults.standard.set(value, forKey: fallbackKey(account))
            return true
        }
        if status != errSecSuccess { NSLog("ClaudeShip: keychain write failed: %d", status) }
        return status == errSecSuccess
    }

    static func delete(_ account: String) {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
        ]
        SecItemDelete(query as CFDictionary)
        UserDefaults.standard.removeObject(forKey: fallbackKey(account))
    }
}
