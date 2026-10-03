import Foundation
import Security

/// Cache/drafts have no authority to mark a remote action delivered.
@MainActor
final class LocalStore {
    private let root: URL
    init() {
        var directory = "Pika"
        #if DEBUG
        if ProcessInfo.processInfo.arguments.contains("--ssh-integration-test"),
            let raw = ProcessInfo.processInfo.environment["PIKA_UI_TEST_STORE_ID"], let id = UUID(uuidString: raw) {
            directory = "PikaIntegration-" + id.uuidString
        }
        #endif
        root = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent(directory, isDirectory: true)
        try? FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }
    func read<T: Decodable>(_ name: String, as: T.Type) -> T? {
        guard let data = try? Data(contentsOf: root.appendingPathComponent(name)) else { return nil }
        return try? JSONDecoder().decode(T.self, from: data)
    }
    func write<T: Encodable>(_ value: T, name: String) throws {
        let data = try JSONEncoder().encode(value)
        try data.write(to: root.appendingPathComponent(name), options: [.atomic, .completeFileProtection])
    }
}

enum KeychainStore {
    static var service: String {
        #if DEBUG
        if ProcessInfo.processInfo.arguments.contains("--ssh-integration-test"),
            let raw = ProcessInfo.processInfo.environment["PIKA_UI_TEST_STORE_ID"], let id = UUID(uuidString: raw) {
            return "dev.pika.mobile.alpha.testcredentials." + id.uuidString
        }
        #endif
        return "dev.pika.mobile.alpha.credentials"
    }
    static func save(_ bytes: Data, id: String) throws {
        let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service, kSecAttrAccount as String: id,
            kSecAttrSynchronizable as String: false]
        let update = SecItemUpdate(query as CFDictionary, [kSecValueData as String: bytes] as CFDictionary)
        if update == errSecSuccess { return }
        guard update == errSecItemNotFound else { throw ConnectionError.secureStorage(update) }
        var add = query
        add[kSecValueData as String] = bytes
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess else { throw ConnectionError.secureStorage(status) }
    }
    static func load(id: String) throws -> Data {
        guard let data = try loadIfPresent(id: id) else { throw ConnectionError.credentials }
        return data
    }
    static func loadIfPresent(id: String) throws -> Data? {
        let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service, kSecAttrAccount as String: id,
            kSecAttrSynchronizable as String: false, kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else { throw ConnectionError.secureStorage(status) }
        return data
    }
    static func remove(id: String) throws {
        let status = SecItemDelete([kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service, kSecAttrAccount as String: id,
            kSecAttrSynchronizable as String: false] as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw ConnectionError.secureStorage(status) }
    }
}
