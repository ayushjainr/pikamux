import Citadel
import Crypto
import Foundation
import NIO
@preconcurrency import NIOSSH

enum ConnectionError: Error, LocalizedError {
    case disconnected, changedHost, changedNode, credentials, malformed, timeout, remote(String), sharedConnectionRequired(String), rejected(String), secureStorage(Int32)
    var errorDescription: String? {
        switch self {
        case .disconnected: return "Connection lost. Your drafts are saved. Pending actions may have an unknown outcome."
        case .changedHost: return "This machine's SSH identity changed. It cannot inherit your saved trust."
        case .changedNode: return "This address is not the Pika machine you saved."
        case .credentials: return "The saved login is unavailable. Please authenticate again."
        case .malformed: return "The machine sent an incompatible or oversized response."
        case .timeout: return "No receipt arrived. The action's outcome may be unknown; it was not repeated."
        case .remote(let message): return message
        case .sharedConnectionRequired(let message): return message
        case .rejected(let message): return message
        case .secureStorage(let status): return "Secure credential storage failed (\(status)). No login was saved."
        }
    }
}

struct HostChallenge: Identifiable, Sendable {
    let id = UUID()
    let address: String
    let fingerprint: String
}

/// Verification completes before SSH can send authentication material.
final class HostVerifier: NIOSSHClientServerAuthenticationDelegate, @unchecked Sendable {
    let pinned: String?
    let address: String
    let verify: @Sendable (HostChallenge) async -> Bool
    private let lock = NSLock()
    private var accepted: String?
    init(pinned: String?, address: String, verify: @escaping @Sendable (HostChallenge) async -> Bool) {
        self.pinned = pinned; self.address = address; self.verify = verify
    }
    var verifiedKey: String? { lock.lock(); defer { lock.unlock() }; return accepted }
    func validateHostKey(hostKey: NIOSSHPublicKey, validationCompletePromise: EventLoopPromise<Void>) {
        let key = String(openSSHPublicKey: hostKey)
        if let pinned {
            guard pinned == key else { validationCompletePromise.fail(ConnectionError.changedHost); return }
            lock.lock(); accepted = key; lock.unlock()
            validationCompletePromise.succeed(())
            return
        }
        guard let encoded = key.split(separator: " ").last, let bytes = Data(base64Encoded: String(encoded)) else {
            validationCompletePromise.fail(ConnectionError.malformed); return
        }
        let fingerprint = "SHA256:" + Data(SHA256.hash(data: bytes)).base64EncodedString().replacingOccurrences(of: "=", with: "")
        Task {
            if await verify(HostChallenge(address: address, fingerprint: fingerprint)) {
                self.record(key)
                validationCompletePromise.succeed(())
            } else { validationCompletePromise.fail(ConnectionError.changedHost) }
        }
    }
    private func record(_ key: String) { lock.lock(); accepted = key; lock.unlock() }
}

protocol MobileWire: Sendable {
    var events: AsyncStream<JSONValue> { get }
    func request(_ method: String, params: JSONValue) async throws -> JSONValue
    func close() async
}

/// Exec stdin owns only its NIO Channel, not an async-stream buffer.
private struct SSHInput: Sendable {
    let channel: Channel
    @concurrent func write(_ bytes: Data) async throws {
        try await channel.writeAndFlush(SSHChannelData(type: .channel, data: .byteBuffer(ByteBuffer(bytes: bytes))))
    }
}

/// Direct library-owned NIO channels handle bounded exec/read/write/close.
/// SSH handlers are confined to their event loop, never transferred into the
/// application actor; reconnect remains explicit and application-owned.
private final class SSHSession: Sendable {
    private let channel: Channel
    init(channel: Channel) { self.channel = channel }
    @concurrent func run(writer: @escaping @Sendable (SSHInput) async -> Void,
        data: @escaping @Sendable (Data) async throws -> Void) async throws {
        let promise = channel.eventLoop.makePromise(of: Channel.self)
        channel.eventLoop.execute {
            do {
                let ssh = try self.channel.pipeline.syncOperations.handler(type: NIOSSHHandler.self)
                ssh.createChannel(promise, channelType: .session) { child, type in
                    guard type == .session else { return child.eventLoop.makeFailedFuture(ConnectionError.malformed) }
                    return child.setOption(ChannelOptions.autoRead, value: false).flatMap {
                        child.pipeline.addHandler(BoundedExecHandler(writer: writer, data: data))
                    }
                }
            } catch { promise.fail(error) }
        }
        let child = try await promise.futureResult.get()
        try await child.closeFuture.get()
    }
    @concurrent func close() async { try? await channel.close() }
}

private final class AuthenticationCompletion: ChannelInboundHandler, Sendable {
    typealias InboundIn = ByteBuffer
    let completion: EventLoopPromise<Void>
    init(_ completion: EventLoopPromise<Void>) { self.completion = completion }
    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        if event is UserAuthSuccessEvent { completion.succeed(()) }
        context.fireUserInboundEventTriggered(event)
    }
    func errorCaught(context: ChannelHandlerContext, error: Error) { completion.fail(error); context.close(promise: nil) }
    func channelInactive(context: ChannelHandlerContext) { completion.fail(ConnectionError.disconnected); context.fireChannelInactive() }
}

private func authenticatedChannel(address: String, port: Int,
    authentication: @escaping @Sendable () -> SSHAuthenticationMethod,
    verifier: any NIOSSHClientServerAuthenticationDelegate, attempt: ConnectionAttempt) async throws -> Channel {
    let loop = MultiThreadedEventLoopGroup.singleton.next()
    let completion = loop.makePromise(of: Void.self)
    let bootstrap = ClientBootstrap(group: loop).connectTimeout(.seconds(10)).channelInitializer { channel in
        do {
            try channel.pipeline.syncOperations.addHandlers(NIOSSHHandler(role: .client(.init(userAuthDelegate: authentication(), serverAuthDelegate: verifier)),
                allocator: channel.allocator, inboundChildChannelInitializer: nil), AuthenticationCompletion(completion))
            return channel.eventLoop.makeSucceededVoidFuture()
        } catch { completion.fail(error); return channel.eventLoop.makeFailedFuture(error) }
    }
    let channel: Channel
    do { channel = try await bootstrap.connect(host: address, port: port).get() }
    catch { completion.fail(error); throw error }
    do { try await attempt.attach(channel) }
    catch { completion.fail(error); try? await channel.close(); throw error }
    let timeout = loop.scheduleTask(in: .seconds(10)) { completion.fail(ConnectionError.timeout); channel.close(promise: nil) }
    defer { timeout.cancel() }
    do { try await completion.futureResult.get(); return channel }
    catch let error as SSHClientError {
        try? await channel.close()
        switch error {
        case .allAuthenticationOptionsFailed, .unsupportedPasswordAuthentication, .unsupportedPrivateKeyAuthentication, .unsupportedHostBasedAuthentication:
            throw ConnectionError.credentials
        default: throw error
        }
    } catch { try? await channel.close(); throw error }
}

/// NIO confines mutable counters to one event loop. Child reads are explicitly
/// resumed only after the consumer finishes; queued application bytes are
/// capped. This avoids Citadel's default unbounded AsyncThrowingStream.
private final class BoundedExecHandler: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = SSHChannelData
    let writer: @Sendable (SSHInput) async -> Void
    let data: @Sendable (Data) async throws -> Void
    private var queued = 0
    private var chunks: [Data] = []
    private var processing = false
    init(writer: @escaping @Sendable (SSHInput) async -> Void, data: @escaping @Sendable (Data) async throws -> Void) {
        self.writer = writer; self.data = data
    }
    func channelActive(context: ChannelHandlerContext) {
        let channel = context.channel
        context.triggerUserOutboundEvent(SSHChannelRequestEvent.ExecRequest(command: "pika _mobile", wantReply: true)).whenFailure { _ in channel.close(promise: nil) }
        context.read()
    }
    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        if event is ChannelSuccessEvent {
            let channel = context.channel
            Task { await writer(SSHInput(channel: channel)); channel.read() }
        } else if event is ChannelFailureEvent { context.close(promise: nil) }
        else { context.fireUserInboundEventTriggered(event) }
    }
    func channelRead(context: ChannelHandlerContext, data value: NIOAny) {
        let part = unwrapInboundIn(value)
        guard case .byteBuffer(let bytes) = part.data, bytes.readableBytes <= 1_048_576 - queued else {
            context.close(promise: nil); return
        }
        guard part.type == .channel else { context.read(); return }
        let payload = Data(bytes.readableBytesView), channel = context.channel
        queued += payload.count
        chunks.append(payload)
        guard !processing else { return }
        processing = true
        Task {
            do {
                while let next = try await channel.eventLoop.submit({ () -> Data? in
                    guard channel.isActive, !self.chunks.isEmpty else {
                        self.processing = false
                        if channel.isActive { channel.read() }
                        return nil
                    }
                    return self.chunks.removeFirst()
                }).get() {
                    try await data(next)
                    try await channel.eventLoop.submit { self.queued -= next.count }.get()
                }
            } catch { try? await channel.close() }
        }
    }
    func errorCaught(context: ChannelHandlerContext, error: Error) { context.close(promise: nil) }
}

actor ConnectionAttempt {
    private var channel: Channel?
    private var cancelled = false
    func attach(_ value: Channel) async throws {
        guard !cancelled else { try? await value.close(); throw CancellationError() }
        channel = value
    }
    func cancel() async { cancelled = true; try? await channel?.close(); channel = nil }
}

private struct HostProbeComplete: Error {}
private final class HostProbe: NIOSSHClientServerAuthenticationDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private var key: String?
    var hostKey: String? { lock.lock(); defer { lock.unlock() }; return key }
    func validateHostKey(hostKey: NIOSSHPublicKey, validationCompletePromise: EventLoopPromise<Void>) {
        lock.lock(); key = String(openSSHPublicKey: hostKey); lock.unlock()
        // End before authentication. Human verification has no live socket deadline.
        validationCompletePromise.fail(HostProbeComplete())
    }
}
/// One SSH session and one fixed exec. No arbitrary command API and no replay.
actor SSHWire: MobileWire {
    nonisolated let events: AsyncStream<JSONValue>
    private let eventSink: AsyncStream<JSONValue>.Continuation
    private let session: SSHSession
    private var writer: SSHInput?
    private var reader: Task<Void, Never>?
    private var pending: [String: CheckedContinuation<JSONValue, Error>] = [:]
    private var outbound: [(String, Data)] = []
    private var writing = false
    private var buffered = Data()
    private var ready: CheckedContinuation<Void, Error>?
    private var closed = false
    let verifiedHostKey: String
    static func connect(address: String, port: Int, username: String, secret: Data,
        privateKey: Bool, passphrase: String, pinnedHost: String?,
        attempt: ConnectionAttempt,
        verify: @escaping @Sendable (HostChallenge) async -> Bool) async throws -> SSHWire {
        guard !address.isEmpty, address.count <= 253, !address.contains(where: { $0.isWhitespace || $0.isNewline }),
            (1...65535).contains(port), !username.isEmpty else { throw ConnectionError.malformed }
        var pinned = pinnedHost
        if pinned == nil {
            let probe = HostProbe()
            do {
                let channel = try await authenticatedChannel(address: address, port: port,
                    authentication: { .passwordBased(username: "host-key-probe", password: "unused") }, verifier: probe, attempt: attempt)
                try? await channel.close(); throw ConnectionError.malformed
            } catch is HostProbeComplete {}
            guard let key = probe.hostKey, let encoded = key.split(separator: " ").last,
                let bytes = Data(base64Encoded: String(encoded)), !Task.isCancelled else { throw ConnectionError.changedHost }
            let fingerprint = "SHA256:" + Data(SHA256.hash(data: bytes)).base64EncodedString().replacingOccurrences(of: "=", with: "")
            guard await verify(HostChallenge(address: address, fingerprint: fingerprint)), !Task.isCancelled else { throw CancellationError() }
            pinned = key
        }
        let verifier = HostVerifier(pinned: pinned, address: address, verify: verify)
        let authentication: @Sendable () -> SSHAuthenticationMethod
        if privateKey {
            let key = try Curve25519.Signing.PrivateKey(sshEd25519: secret,
                decryptionKey: passphrase.isEmpty ? nil : Data(passphrase.utf8))
            authentication = { .ed25519(username: username, privateKey: key) }
        } else {
            guard let password = String(data: secret, encoding: .utf8) else { throw ConnectionError.credentials }
            authentication = { .passwordBased(username: username, password: password) }
        }
        let channel = try await authenticatedChannel(address: address, port: port, authentication: authentication, verifier: verifier, attempt: attempt)
        guard !Task.isCancelled else { try? await channel.close(); throw CancellationError() }
        guard let key = verifier.verifiedKey else { try? await channel.close(); throw ConnectionError.changedHost }
        let wire = SSHWire(channel: channel, verifiedHostKey: key)
        try await wire.start()
        return wire
    }
    init(channel: Channel, verifiedHostKey: String) {
        self.session = SSHSession(channel: channel); self.verifiedHostKey = verifiedHostKey
        var sink: AsyncStream<JSONValue>.Continuation!
        events = AsyncStream(bufferingPolicy: .bufferingOldest(256)) { sink = $0 }
        eventSink = sink
    }
    private func start() async throws {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                ready = continuation
                reader = Task { await self.read() }
                Task {
                    try? await Task.sleep(for: .seconds(20))
                    if self.ready != nil { await self.close() }
                }
            }
        } onCancel: {
            Task { await self.close() }
        }
    }
    private func read() async {
        do {
            try await session.run(writer: { await self.setWriter($0) }, data: { try await self.receive($0) })
        } catch { /* Fail pending requests conservatively, never retry here. */ }
        await close()
    }
    private func setWriter(_ output: SSHInput) {
        writer = output; ready?.resume(); ready = nil
    }
    func request(_ method: String, params: JSONValue = .object([:])) async throws -> JSONValue {
        guard !closed, writer != nil, pending.count < 32 else { throw ConnectionError.disconnected }
        let id = UUID().uuidString
        let frame: JSONValue = .object(["v": .number(1), "id": .string(id), "method": .string(method), "params": params])
        var bytes = try JSONEncoder().encode(frame)
        guard bytes.count <= 131_072 else { throw ConnectionError.malformed }
        bytes.append(10)
        return try await withCheckedThrowingContinuation { continuation in
            pending[id] = continuation
            outbound.append((id, bytes))
            if !writing { writing = true; Task { await self.drainOutbound() } }
            Task {
                try? await Task.sleep(for: .seconds(20))
                self.fail(id, error: ConnectionError.timeout)
            }
        }
    }
    private func drainOutbound() async {
        defer { writing = false }
        while !closed, let writer, !outbound.isEmpty {
            let (id, bytes) = outbound.removeFirst()
            // A timed-out queued request must not be dispatched later.
            guard pending[id] != nil else { continue }
            do { try await writer.write(bytes) }
            catch { await close(); return }
        }
    }
    private func receive(_ bytes: Data) throws {
        buffered.append(bytes)
        while let newline = buffered.firstIndex(of: 10) {
            let line = buffered[..<newline]
            guard line.count <= 16 * 1_048_576 else { throw ConnectionError.malformed }
            buffered.removeSubrange(...newline)
            let frame = try JSONDecoder().decode(JSONValue.self, from: line)
            guard frame["v"].number == 1 else { throw ConnectionError.malformed }
            if let id = frame["id"].string {
                guard !frame.has("event"), !frame.has("method"), frame.has("result") != frame.has("error") else {
                    throw ConnectionError.malformed
                }
                guard let continuation = pending.removeValue(forKey: id) else { continue }
                if frame.has("error") {
                    let message = frame["error"]["message"].string ?? "The machine could not confirm this request."
                    let code = frame["error"]["code"].string ?? ""
                    let definitive = code == "rejected_before_dispatch"
                    continuation.resume(throwing: definitive ? ConnectionError.rejected(message) : code == "shared_connection_required" ? ConnectionError.sharedConnectionRequired(message) : ConnectionError.remote(message))
                } else { continuation.resume(returning: frame["result"]) }
            } else if frame["event"].string != nil, frame.has("params"), !frame.has("result"), !frame.has("error") {
                if case .dropped = eventSink.yield(frame) { throw ConnectionError.malformed }
            } else { throw ConnectionError.malformed }
        }
        guard buffered.count <= 16 * 1_048_576 else { throw ConnectionError.malformed }
    }
    private func fail(_ id: String, error: Error) { pending.removeValue(forKey: id)?.resume(throwing: error) }
    func close() async {
        guard !closed else { return }
        closed = true
        ready?.resume(throwing: ConnectionError.disconnected); ready = nil
        let waiting = pending.values; pending.removeAll()
        for continuation in waiting { continuation.resume(throwing: ConnectionError.disconnected) }
        writer = nil
        outbound = []
        reader?.cancel()
        await session.close()
        eventSink.finish()
    }
}
