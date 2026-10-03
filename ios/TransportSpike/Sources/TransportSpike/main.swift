import Citadel
import Crypto
import Foundation
import NIO
@preconcurrency import NIOSSH

enum SpikeFailure: Error { case assertion(String) }
func print(_ message: String) {
    FileHandle.standardOutput.write(Data((message + "\n").utf8))
}

struct FixtureAuth: NIOSSHServerUserAuthenticationDelegate {
    let key: NIOSSHPublicKey
    let supportedAuthenticationMethods: NIOSSHAvailableUserAuthenticationMethods = [.password, .publicKey]
    func requestReceived(request: NIOSSHUserAuthenticationRequest,
        responsePromise: EventLoopPromise<NIOSSHUserAuthenticationOutcome>) {
        guard request.username == "disposable" else { responsePromise.succeed(.failure); return }
        switch request.request {
        case .password(let value) where value.password == "fixture-only": responsePromise.succeed(.success)
        case .publicKey(let value) where value.publicKey == key: responsePromise.succeed(.success)
        default: responsePromise.succeed(.failure)
        }
    }
}

// Fixture only: callbacks are serialized by its one readability handler.
struct FixtureOutput: @unchecked Sendable { let value: ExecOutputHandler }

/// No shell, no files, no provider: an ephemeral loopback-only SSH fixture.
final class EchoExec: ExecDelegate, @unchecked Sendable {
    struct Context: ExecCommandContext {
        func terminate() async throws {}
    }
    func setEnvironmentValue(_ value: String, forKey key: String) async throws {
        throw SpikeFailure.assertion("environment not accepted")
    }
    func start(command: String, outputHandler: ExecOutputHandler) async throws -> ExecCommandContext {
        print("fixture received exec request")
        guard command == "pika _mobile-stdio-v1" else {
            throw SpikeFailure.assertion("command not allowlisted")
        }
        let input = outputHandler.stdinPipe.fileHandleForReading
        let output = FixtureOutput(value: outputHandler)
        input.readabilityHandler = { handle in
            let data = handle.availableData
            print("fixture stdin chunk bytes=\(data.count)")
            guard !data.isEmpty else {
                handle.readabilityHandler = nil
                try? output.value.stdoutPipe.fileHandleForWriting.close()
                output.value.succeed(exitCode: 0)
                return
            }
            output.value.stdoutPipe.fileHandleForWriting.write(data)
        }
        return Context()
    }
}

@main
struct TransportSpike {
    nonisolated static func main() async throws {
        let watchdog = DispatchWorkItem { print("FAIL fixture exceeded 15-second deadline"); exit(124) }
        DispatchQueue.global().asyncAfter(deadline: .now() + 15, execute: watchdog)
        defer { watchdog.cancel() }
        if CommandLine.arguments.count == 5 { try await runOpenSSH() }
        else { try await runFixture() }
    }
    @concurrent
    static func runOpenSSH() async throws {
        let imported = try Curve25519.Signing.PrivateKey(sshEd25519:
            Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1])))
        let pinned = try NIOSSHPublicKey(openSSHPublicKey:
            String(contentsOfFile: CommandLine.arguments[2], encoding: .utf8))
        guard let port = Int(CommandLine.arguments[3]) else { throw SpikeFailure.assertion("port") }
        let username = CommandLine.arguments[4]
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        let began = ContinuousClock.now
        let client = try await SSHClient.connect(host: "127.0.0.1", port: port,
            authenticationMethod: .ed25519(username: username, privateKey: imported),
            hostKeyValidator: .trustedKeys([pinned]), reconnect: .never,
            group: group, connectTimeout: .seconds(3))
        print("PASS OpenSSH imported-key pinned-host handshake; elapsed=\(began.duration(to: .now))")
        let parts = ["{\"schema\":1,", "\"id\":\"one\"}\n{\"schema\":1,\"id\":\"two\"}\n", "{\"schema\":1,\"id\":\"three\"}\n"]
        let expected = parts.joined()
        var received = ByteBuffer()
        try await client.withExec("pika _mobile-stdio-v1") { inbound, outbound in
            for part in parts {
                try await outbound.write(ByteBuffer(string: part))
                try await Task.sleep(for: .milliseconds(20))
            }
            for try await chunk in inbound {
                if case .stdout(let buffer) = chunk {
                    guard received.readableBytes + buffer.readableBytes <= 4096 else {
                        throw SpikeFailure.assertion("frame bound")
                    }
                    received.writeImmutableBuffer(buffer)
                    if received.readableBytes >= expected.utf8.count { break }
                }
            }
        }
        guard String(buffer: received) == expected else { throw SpikeFailure.assertion("stdio bytes") }
        print("PASS OpenSSH sustained split/coalesced three-frame bidirectional non-PTY exec")
        try await client.close()
        guard !client.isConnected else { throw SpikeFailure.assertion("disconnect") }
        print("PASS explicit disconnect")
        do {
            let wrong = NIOSSHPrivateKey(ed25519Key: .init()).publicKey
            let bad = try await SSHClient.connect(host: "127.0.0.1", port: port,
                authenticationMethod: .ed25519(username: username, privateKey: imported),
                hostKeyValidator: .trustedKeys([wrong]), reconnect: .never,
                group: group, connectTimeout: .seconds(3))
            try await bad.close()
            throw SpikeFailure.assertion("changed host accepted")
        } catch is InvalidHostKey { print("PASS mismatch rejected specifically as InvalidHostKey") }
        let retry = try await SSHClient.connect(host: "127.0.0.1", port: port,
            authenticationMethod: .ed25519(username: username, privateKey: imported),
            hostKeyValidator: .trustedKeys([pinned]), reconnect: .never,
            group: group, connectTimeout: .seconds(3))
        try await retry.close()
        print("PASS valid pinned-host reconnect immediately after mismatch")
        try await group.shutdownGracefully()
    }
    @concurrent
    static func runFixture() async throws {
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        let hostKey = NIOSSHPrivateKey(ed25519Key: .init())
        // Accept only an explicitly supplied disposable fixture key; never find user keys.
        guard CommandLine.arguments.count == 2 else { throw SpikeFailure.assertion("supply disposable key path") }
        let imported = try Curve25519.Signing.PrivateKey(sshEd25519:
            Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1])))
        let auth = FixtureAuth(key: NIOSSHPrivateKey(ed25519Key: imported).publicKey)
        // Citadel does not publicly expose its chosen port for port=0.
        let port = Int.random(in: 49152...65535)
        let server = try await SSHServer.host(host: "127.0.0.1", port: port,
            hostKeys: [hostKey], authenticationDelegate: auth, group: group)
        print("fixture listening on ephemeral loopback port")
        server.enableExec(withDelegate: EchoExec())
        let started = ContinuousClock.now
        do {
            let client = try await SSHClient.connect(host: "127.0.0.1", port: port,
                authenticationMethod: .passwordBased(username: "disposable", password: "fixture-only"),
                hostKeyValidator: .trustedKeys([hostKey.publicKey]), reconnect: .never,
                group: group, connectTimeout: .seconds(3))
            print("PASS pinned-host password handshake; elapsed=\(started.duration(to: .now))")
            let parts = ["{\"schema\":1,", "\"request_id\":\"fixture\",\"operation\":\"hello\"}\n{\"schema\":1,\"event\":\"one\"}\n", "{\"schema\":1,\"event\":\"two\"}\n"]
            let request = parts.joined()
            var received = ByteBuffer()
            do {
                try await client.withExec("pika _mobile-stdio-v1") { inbound, outbound in
                    print("client exec ready")
                    for part in parts {
                        try await outbound.write(ByteBuffer(string: part))
                        try await Task.sleep(for: .milliseconds(20))
                    }
                    for try await chunk in inbound {
                        if case .stdout(let buffer) = chunk {
                            guard received.readableBytes + buffer.readableBytes <= 4096 else {
                                throw SpikeFailure.assertion("frame size")
                            }
                            received.writeImmutableBuffer(buffer)
                            if received.readableBytes >= request.utf8.count { break }
                        }
                    }
                }
            } catch let error as ChannelError where error == .alreadyClosed {
                // Citadel closes the exec channel after the server already closed it.
            }
            guard String(buffer: received) == request else {
                throw SpikeFailure.assertion("stdio changed bytes")
            }
            print("PASS exec bidirectional versioned fixture bytes (not a Pika endpoint)")
            try await client.close()
            guard !client.isConnected else { throw SpikeFailure.assertion("disconnect") }
            print("PASS explicit disconnect")
            let keyed = try await SSHClient.connect(host: "127.0.0.1", port: port,
                authenticationMethod: .ed25519(username: "disposable", privateKey: imported),
                hostKeyValidator: .trustedKeys([hostKey.publicKey]), reconnect: .never,
                group: group, connectTimeout: .seconds(3))
            try await keyed.close()
            print("PASS imported OpenSSH Ed25519 authentication")
            do {
                let wrong = NIOSSHPrivateKey(ed25519Key: .init())
                let rejected = try await SSHClient.connect(host: "127.0.0.1", port: port,
                    authenticationMethod: .passwordBased(username: "disposable", password: "fixture-only"),
                    hostKeyValidator: .trustedKeys([wrong.publicKey]), reconnect: .never,
                    group: group, connectTimeout: .seconds(3))
                try await rejected.close()
                throw SpikeFailure.assertion("host-key mismatch accepted")
            } catch is InvalidHostKey { print("PASS changed host key rejected: InvalidHostKey") }
            let reconnected = try await SSHClient.connect(host: "127.0.0.1", port: port,
                authenticationMethod: .passwordBased(username: "disposable", password: "fixture-only"),
                hostKeyValidator: .trustedKeys([hostKey.publicKey]), reconnect: .never,
                group: group, connectTimeout: .seconds(3))
            try await reconnected.close()
            print("PASS valid pinned-host reconnect immediately after mismatch")
            try await server.close()
            try await group.shutdownGracefully()
        } catch {
            try? await server.close()
            try? await group.shutdownGracefully()
            throw error
        }
    }
}
