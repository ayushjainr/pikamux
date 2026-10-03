import Citadel
import Crypto
import Foundation
import NIOSSH
import Security
import Darwin

enum PairingError: Error, LocalizedError {
    case descriptor, unavailable, expired, pin, receipt, rejected, unknown, previous
    case connection(Int)
    case response(Int)
    var errorDescription: String? {
        switch self {
        case .descriptor: return "This is not a supported Pika pairing code."
        case .unavailable: return "Code scanned, but the pairing connection failed. Keep Connect phone open on the machine."
        case .connection(let code):
            switch code {
            case NSURLErrorTimedOut: return "Code scanned, but the machine did not respond. Keep Connect phone open and check the Tailscale connection."
            case NSURLErrorCannotConnectToHost: return "Code scanned, but the pairing service could not be reached. Open a fresh code on the machine."
            case NSURLErrorNotConnectedToInternet: return "Code scanned, but the phone has no permitted network route. Check Pika’s network access in Settings and Tailscale."
            default: return "Code scanned, but the secure connection failed (\(code)). No phone key was installed."
            }
        case .response(let code): return "Code scanned, but the machine could not provide pairing details (\(code)). Open a fresh code on the machine."
        case .expired: return "This pairing code expired. Choose Connect phone again on the machine."
        case .pin: return "The pairing machine's secure identity did not match the code. Nothing was trusted."
        case .receipt: return "The pairing receipt did not match this phone and machine."
        case .rejected: return "The machine did not accept this pairing code. Choose Connect phone again."
        case .unknown: return "Pairing may have completed. Check the original machine with this phone's saved key; do not repeat registration."
        case .previous: return "Finish the pending connection, or discard it before pairing another machine."
        }
    }
}

struct PairingDescriptor: Codable, Sendable, Equatable {
    let v: Int
    let address: String
    let ssh_port: Int
    let username: String
    let node_id: String
    let ssh_host_key: String
    let pair_port: Int
    let tls_sha256: String
    let token: String
    let expires_at: Double
    static func decode(_ data: Data) throws -> Self {
        guard data.count <= 4096 else { throw PairingError.descriptor }
        let value = try JSONDecoder().decode(Self.self, from: data)
        guard value.v == 1, (1...65535).contains(value.ssh_port), (1...65535).contains(value.pair_port),
              UUID(uuidString: value.node_id) != nil, !value.address.isEmpty, value.address.utf8.count <= 253,
              value.address.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || ".-:".contains($0)) }),
              !value.username.isEmpty, value.username.utf8.count <= 64,
              value.username.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || "_-.".contains($0)) }),
              value.tls_sha256.count == 64, value.tls_sha256.allSatisfy({ "0123456789abcdef".contains($0) }),
              base64url(value.token)?.count == 32, value.expires_at.isFinite,
              value.ssh_host_key.utf8.count <= 2048, !value.ssh_host_key.contains("\n") else { throw PairingError.descriptor }
        _ = try NIOSSHPublicKey(openSSHPublicKey: value.ssh_host_key)
        return value
    }
    static func base64url(_ text: String) -> Data? {
        guard !text.isEmpty, text.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || "-_".contains($0)) }) else { return nil }
        let base = text.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        return Data(base64Encoded: base + String(repeating: "=", count: (4 - base.count % 4) % 4))
    }
}

struct PairingBootstrap: Sendable {
    let address: String
    let port: Int
    let pin: String
    let token: String
    static func parse(_ text: String) throws -> Self {
        guard text.utf8.count <= 1024, let code = URLComponents(string: text),
              code.scheme == "pika", code.host == "pair", code.query == nil,
              code.user == nil, code.password == nil, code.port == nil,
              code.path.hasPrefix("/v1/"), let fragment = code.fragment,
              let secret = PairingDescriptor.base64url(fragment), secret.count == 64,
              let authority = URLComponents(string: "https://" + code.path.dropFirst(4)),
              authority.user == nil, authority.password == nil, authority.path.isEmpty,
              authority.query == nil, authority.fragment == nil, let rawHost = authority.host,
              let port = authority.port, (1...65535).contains(port) else { throw PairingError.descriptor }
        let host = rawHost.trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        var ipv4 = in_addr(); var ipv6 = in6_addr()
        let is4 = host.withCString { inet_pton(AF_INET, $0, &ipv4) } == 1
        let is6 = host.withCString { inet_pton(AF_INET6, $0, &ipv6) } == 1
        let octets = host.split(separator: ".").compactMap { Int($0) }
        var privateAddress = is4 && octets.count == 4 &&
            (octets[0] == 10 || (octets[0] == 192 && octets[1] == 168) ||
             (octets[0] == 172 && (16...31).contains(octets[1])) ||
             (octets[0] == 100 && (64...127).contains(octets[1])))
        privateAddress = privateAddress || (is6 && withUnsafeBytes(of: ipv6) { $0[0] & 0xfe == 0xfc })
        #if DEBUG
        if ProcessInfo.processInfo.arguments.contains("--ssh-integration-test") {
            privateAddress = privateAddress || (is4 && octets[0] == 127) || host == "::1"
        }
        #endif
        guard privateAddress else { throw PairingError.descriptor }
        return Self(address: host, port: port,
                    pin: secret.prefix(32).map { String(format: "%02x", $0) }.joined(),
                    token: secret.suffix(32).base64EncodedString().replacingOccurrences(of: "+", with: "-")
                        .replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: ""))
    }
    func matches(_ descriptor: PairingDescriptor) -> Bool {
        descriptor.address == address && descriptor.pair_port == port && descriptor.tls_sha256 == pin && descriptor.token == token
    }
    func resolve() async throws -> PairingDescriptor {
        let data: Data
        do { data = try await PairingHTTP(address: address, port: port, pin: pin, path: "/descriptor", body: ["token": token]).run() }
        catch PairingError.unknown { throw PairingError.unavailable }
        let descriptor = try PairingDescriptor.decode(data)
        guard matches(descriptor) else { throw PairingError.receipt }
        return descriptor
    }
}

struct StagedPairing: Codable, Sendable {
    let descriptor: PairingDescriptor
    let secret: Data
    let publicKey: String
    var dispatched: Bool
    static let storageId = "pairing-stage"
    var keychainId: String { Self.storageId }
    static func pending() throws -> Self? {
        guard let bytes = try KeychainStore.loadIfPresent(id: storageId) else { return nil }
        return try JSONDecoder().decode(Self.self, from: bytes)
    }
    static func prepare(_ descriptor: PairingDescriptor) throws -> Self {
        if let existing = try pending() {
            if existing.descriptor == descriptor { return existing }
            let old = existing.descriptor
            guard old.node_id == descriptor.node_id, old.ssh_host_key == descriptor.ssh_host_key,
                  old.address == descriptor.address, old.ssh_port == descriptor.ssh_port,
                  old.username == descriptor.username else { throw PairingError.previous }
            guard descriptor.expires_at > Date.now.timeIntervalSince1970 else { throw PairingError.expired }
            // Only an explicit fresh scan can reauthorize the SAME key, never an automatic retry.
            let refreshed = Self(descriptor: descriptor, secret: existing.secret, publicKey: existing.publicKey, dispatched: false)
            try refreshed.persist(); return refreshed
        }
        guard descriptor.expires_at > Date.now.timeIntervalSince1970 else { throw PairingError.expired }
        let key = Curve25519.Signing.PrivateKey()
        let result = Self(descriptor: descriptor, secret: Data(key.makeSSHRepresentation().utf8),
                          publicKey: String(openSSHPublicKey: NIOSSHPrivateKey(ed25519Key: key).publicKey), dispatched: false)
        try result.persist()
        return result
    }
    func persist() throws { try KeychainStore.save(JSONEncoder().encode(self), id: keychainId) }
}

/// A single bounded claim. Pin validation happens during TLS, before HTTP body delivery.
final class PairingHTTP: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    private let address: String
    private let port: Int
    private let pin: String
    private let path: String
    private let body: Data
    private let lock = NSLock()
    private var buffer = Data()
    private var continuation: CheckedContinuation<Data, Error>?
    private var task: URLSessionDataTask?
    private var session: URLSession?
    init(address: String, port: Int, pin: String, path: String, body: [String: String]) throws {
        self.address = address; self.port = port; self.pin = pin; self.path = path
        self.body = try JSONEncoder().encode(body)
    }
    static func claim(_ descriptor: PairingDescriptor, publicKey: String) async throws {
        let data = try await Self(address: descriptor.address, port: descriptor.pair_port, pin: descriptor.tls_sha256,
                                  path: "/pair", body: ["token": descriptor.token, "public_key": publicKey]).run()
        let value = try JSONDecoder().decode(JSONValue.self, from: data)
        guard value["v"].number == 1, value["state"].string == "paired",
              value["node_id"].string == descriptor.node_id, value["public_key"].string == publicKey,
              value["ssh_host_key"].string == descriptor.ssh_host_key else { throw PairingError.receipt }
    }
    func run() async throws -> Data {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (pending: CheckedContinuation<Data, Error>) in
                var parts = URLComponents(); parts.scheme = "https"
                parts.host = address.contains(":") ? "[" + address + "]" : address
                parts.port = port; parts.path = path
                guard let url = parts.url else { pending.resume(throwing: PairingError.descriptor); return }
                var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 15)
                request.httpMethod = "POST"; request.setValue("application/json", forHTTPHeaderField: "Content-Type")
                request.httpBody = body
                let config = URLSessionConfiguration.ephemeral
                config.httpCookieStorage = nil; config.urlCache = nil
                config.timeoutIntervalForRequest = 15; config.timeoutIntervalForResource = 20
                let session = URLSession(configuration: config, delegate: self, delegateQueue: nil)
                let task = session.dataTask(with: request)
                lock.withLock { self.continuation = pending; self.session = session; self.task = task }
                if Task.isCancelled { finish(CancellationError()) } else { task.resume() }
            }
        } onCancel: { self.finish(CancellationError()) }
    }
    private func finish(_ error: Error?) {
        let values = lock.withLock { () -> (CheckedContinuation<Data, Error>?, URLSession?, URLSessionDataTask?, Data) in
            let values = (continuation, session, task, buffer)
            continuation = nil; session = nil; task = nil
            return values
        }
        values.2?.cancel(); values.1?.invalidateAndCancel()
        if let error { values.0?.resume(throwing: error) } else { values.0?.resume(returning: values.3) }
    }
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
              challenge.protectionSpace.host.trimmingCharacters(in: CharacterSet(charactersIn: "[]")) == address,
              let trust = challenge.protectionSpace.serverTrust,
              let chain = SecTrustCopyCertificateChain(trust) as? [SecCertificate], let leaf = chain.first else {
            completionHandler(.cancelAuthenticationChallenge, nil); finish(PairingError.pin); return
        }
        let digest = SHA256.hash(data: SecCertificateCopyData(leaf) as Data).map { String(format: "%02x", $0) }.joined()
        guard digest == pin else {
            completionHandler(.cancelAuthenticationChallenge, nil); finish(PairingError.pin); return
        }
        completionHandler(.useCredential, URLCredential(trust: trust))
    }
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
        completionHandler(nil); finish(PairingError.unknown)
    }
    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
                    completionHandler: @escaping (URLSession.ResponseDisposition) -> Void) {
        guard let response = response as? HTTPURLResponse, response.statusCode == 200,
              response.expectedContentLength <= 16_384 else {
            completionHandler(.cancel)
            finish(path == "/descriptor" ? PairingError.response((response as? HTTPURLResponse)?.statusCode ?? 0) : PairingError.unknown)
            return
        }
        completionHandler(.allow)
    }
    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        let valid = lock.withLock { () -> Bool in
            guard buffer.count + data.count <= 16_384 else { return false }
            buffer.append(data); return true
        }
        if !valid { finish(PairingError.receipt) }
    }
    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        if let error {
            finish(path == "/descriptor" ? PairingError.connection((error as NSError).code) : PairingError.unknown)
            return
        }
        finish(nil)
    }
}
