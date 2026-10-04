import Foundation

enum JSONValue: Codable, Sendable, Equatable {
    case object([String: JSONValue]), array([JSONValue]), string(String), number(Double), bool(Bool), null
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let x = try? c.decode(Bool.self) { self = .bool(x) }
        else if let x = try? c.decode(String.self) { self = .string(x) }
        else if let x = try? c.decode(Double.self) { self = .number(x) }
        else if let x = try? c.decode([JSONValue].self) { self = .array(x) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .object(let x): try c.encode(x)
        case .array(let x): try c.encode(x)
        case .string(let x): try c.encode(x)
        case .number(let x): try c.encode(x)
        case .bool(let x): try c.encode(x)
        case .null: try c.encodeNil()
        }
    }
    subscript(_ key: String) -> JSONValue { if case .object(let x) = self { return x[key] ?? .null }; return .null }
    func has(_ key: String) -> Bool { if case .object(let x) = self { return x[key] != nil }; return false }
    var string: String? { if case .string(let x) = self { return x }; return nil }
    var array: [JSONValue] { if case .array(let x) = self { return x }; return [] }
    var hasArrayShape: Bool { if case .array = self { return true }; return false }
    var bool: Bool { if case .bool(let x) = self { return x }; return false }
    var number: Double? { if case .number(let x) = self { return x }; return nil }
}

struct ThreadIdentity: Codable, Hashable, Sendable {
    let nodeId: String
    let provider: String
    let threadId: String
    var draftKey: String { [nodeId, provider, threadId].map { Data($0.utf8).base64EncodedString() }.joined(separator: ":") }
    var json: JSONValue { .object(["nodeId": .string(nodeId), "provider": .string(provider), "threadId": .string(threadId)]) }
}

struct BoardItem: Codable, Identifiable, Hashable, Sendable {
    let identity: ThreadIdentity
    let name: String
    let machine: String
    let state: String
    let detail: String
    var observedAt: Double? = nil
    var cachedAt: Double? = nil
    var stale: Bool? = nil
    var unread: Bool? = nil
    var id: ThreadIdentity { identity }
    var observationSummary: String {
        guard let observedAt, observedAt <= Date.now.timeIntervalSince1970 else { return "Source observation time unknown" }
        let seconds = max(0, Int(Date.now.timeIntervalSince1970 - observedAt))
        let age = seconds < 60 ? "\(seconds)s" : "\(seconds / 60)m"
        return (stale == true ? "Cached · " : "Observed · ") + age + " ago"
    }
}

struct ChatMessage: Identifiable, Sendable {
    let id: String
    let role: String
    let text: String
}

struct SavedMachine: Codable, Identifiable, Sendable {
    let id: String
    let address: String
    let port: Int
    let username: String
    let hostKey: String
    let credentialId: String
    let keyAuthentication: Bool
    var name: String? = nil
    var nickname: String? = nil
    var displayName: String { nickname?.isEmpty == false ? nickname! : (name?.isEmpty == false ? name! : address) }
}
