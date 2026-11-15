import Foundation
import Security

/// Debrid API keys in the iOS Keychain (generic passwords, this device only).
/// The engine only ever gets a key in memory at runtime.
enum Keychain {
    private static let serviceName = "torrenter.debrid"

    private static func query(_ service: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: serviceName,
            kSecAttrAccount as String: service,
        ]
    }

    static func read(_ service: String) -> String? {
        var query = query(service)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data,
              let key = String(data: data, encoding: .utf8),
              !key.isEmpty
        else {
            return nil
        }
        return key
    }

    /// False only when the item is provably gone (`errSecItemNotFound`); any
    /// other failure (e.g. before first unlock) counts as still there.
    static func exists(_ service: String) -> Bool {
        var query = query(service)
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        return SecItemCopyMatching(query as CFDictionary, nil) != errSecItemNotFound
    }

    @discardableResult
    static func save(_ service: String, key: String) -> Bool {
        let data = Data(key.utf8)
        let update: [String: Any] = [kSecValueData as String: data]
        let status = SecItemUpdate(query(service) as CFDictionary, update as CFDictionary)
        if status == errSecSuccess {
            return true
        }
        guard status == errSecItemNotFound else {
            return false
        }
        var add = query(service)
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    static func delete(_ service: String) {
        SecItemDelete(query(service) as CFDictionary)
    }
}
