import Foundation
import SwiftUI

struct CredentialPayload: Codable {
    let secret: Data
    let passphrase: String
}
struct PendingAction: Codable {
    let id: String
    let identity: ThreadIdentity
    let text: String
    var state: String
    var receiptCursor: String? = nil
    var requestId: JSONValue? = nil
    var turnId: String? = nil
    var itemId: String? = nil
}
struct ProviderQuestion: Identifiable {
    let id: String
    let requestId: JSONValue
    let turnId: String
    let itemId: String
    let questions: JSONValue
}
struct ProviderApproval: Identifiable {
    let id: String
    let identity: ThreadIdentity
    let requestId: JSONValue
    let method: String
    let params: JSONValue
    let item: JSONValue
}
struct MobileProject: Identifiable { let id: String; let name: String; let nodeId: String }
struct MobileNode: Identifiable { let id: String; let name: String }
struct ExistingCandidate: Identifiable { let identity: ThreadIdentity; let name: String; let project: String?; var id: ThreadIdentity { identity } }
struct CreationRecord: Codable { let id: String; let params: JSONValue; var state: String; var identity: ThreadIdentity? }

@MainActor
final class AppModel: ObservableObject {
    @Published var board: [BoardItem] = []
    @Published var selected: BoardItem?
    @Published var messages: [ChatMessage] = []
    @Published var question: ProviderQuestion?
    @Published var approval: ProviderApproval?
    @Published var capabilities: JSONValue = .null
    @Published var conversationCapabilities: JSONValue = .null
    @Published var conversationCached = false
    @Published var connected = false
    @Published var connecting = false
    @Published var observedAt: Date?
    private var noticeRequestAction: String?
    @Published var notice: String? { didSet { noticeDetails = nil; noticeRequestAction = nil } }
    @Published var noticeDetails: String?
    private let requestClosedNotice = "The original request closed. This does not confirm the decision was accepted."
    private func requestNotice(_ text: String, action: String) { notice = text; noticeRequestAction = action }

    func reportError(_ error: Error, action: String = "This action could not be completed. Reconnect and try again.") {
        switch error as? ConnectionError {
        case .remote: notice = action
        case .rejected: notice = "The machine rejected this action before dispatch. Nothing was sent."
        case .secureStorage: notice = "Secure login storage is unavailable. No login was saved."
        case .some: notice = error.localizedDescription
        case .none: notice = action
        }
        noticeDetails = error.localizedDescription
    }
    @Published var hostChallenge: HostChallenge?
    @Published var pendingPairingAvailable = false
    @Published var drafts: [String: String] = [:]
    private var draftSkills: [String: [String]] = [:]
    @Published var pendingActions: [String: PendingAction] = [:]
    private var receiptChecksInFlight: Set<ThreadIdentity> = []
    @Published var machines: [SavedMachine] = []
    @Published var activeNodeId: String?
    @Published var machineFilter: String?
    @Published private(set) var onlineNodes: Set<String> = []
    @Published private(set) var connectionCompletion = 0
    @Published var isFixture = false
    @Published var choices: [String: String] = [:]
    @Published var creations: [String: CreationRecord] = [:]
    @Published var mutationBusy = false
    @Published var fixtureSendCount = 0
    @Published var fixtureAssistantFinished = false
    @Published var fixtureAssistantOpenCount = 0
    @Published private var sourceCoverage: [String: Bool] = [:]
    var coverageNote: String? {
        let sources: [String]
        if let node = machineFilter {
            // A paired owner remains authoritative while offline. Never borrow
            // a healthy coordinator's coverage for its cached direct board.
            if machines.contains(where: { $0.id == node }) { sources = [node] }
            else if let source = route(for: node) { sources = [source] }
            else {
                // Retain uncertainty for cached or ambiguous coordinator routes.
                sources = sourceBoards.keys.filter { sourceBoards[$0]?.contains(where: { $0.identity.nodeId == node }) == true || sourceNodes[$0]?.contains(where: { $0.id == node }) == true }
            }
        } else { sources = Array(sourceCoverage.keys) }
        let partial = sources.filter { sourceCoverage[$0] == true }.sorted()
        guard !partial.isEmpty else { return nil }
        let names = partial.map { source in
            let name = machineName(for: source, fallback: sourceBoards[source]?.first(where: { $0.identity.nodeId == source })?.machine ?? "Machine")
            return name + ((!isFixture && !onlineNodes.contains(source)) ? " (cached)" : "")
        }.joined(separator: ", ")
        return "Partial board · " + names + " · some work is not included in this snapshot"
    }
    private let store = LocalStore()
    private var wire: (any MobileWire)?
    private var wires: [String: any MobileWire] = [:]
    private var listeners: [String: Task<Void, Never>] = [:]
    private var wireEpochs: [String: UUID] = [:]
    private struct RouteEpoch: Equatable { let source: String; let epoch: UUID }
    private func routeEpoch(for node: String) -> RouteEpoch? {
        if isFixture { return connected ? RouteEpoch(source: "fixture", epoch: generation) : nil }
        guard let source = route(for: node), let epoch = wireEpochs[source] else { return nil }
        return RouteEpoch(source: source, epoch: epoch)
    }
    private var nodeCapabilities: [String: JSONValue] = [:]
    private var sourceBoards: [String: [BoardItem]] = [:]
    private var sourceNodes: [String: [MobileNode]] = [:]
    private var resumeQueue: [SavedMachine] = []
    private var blockedReconnectNodes: Set<String> = []
    private var receivingNode: String?
    private var pendingBoardNode: String?
    private var previousPendingBoard: [BoardItem]?
    private var previousPendingCoverage: Bool?
    private var stagingByNode: [String: [BoardItem]] = [:]
    private var revisionByNode: [String: JSONValue] = [:]
    private var pageByNode: [String: Int] = [:]
    func isConnected(node: String) -> Bool { isFixture ? connected : route(for: node) != nil }
    func capabilities(for node: String?) -> JSONValue {
        guard let node else { return capabilities }
        if isFixture { return capabilities }
        return route(for: node).flatMap { nodeCapabilities[$0] } ?? .null
    }
    var filteredBoard: [BoardItem] { board.filter { machineFilter == nil || $0.identity.nodeId == machineFilter } }
    func selectMachine(node: String) {
        activeNodeId = node; wire = transport(for: node); capabilities = capabilities(for: node)
    }
    private func route(for node: String) -> String? {
        if machines.contains(where: { $0.id == node }) { return onlineNodes.contains(node) ? node : nil }
        let sources = onlineNodes.filter { sourceBoards[$0]?.contains(where: { $0.identity.nodeId == node }) == true || sourceNodes[$0]?.contains(where: { $0.id == node }) == true }
        return sources.count == 1 ? sources.first : nil
    }
    private func transport(for node: String) -> (any MobileWire)? { isFixture ? (connected ? wire : nil) : route(for: node).flatMap { wires[$0] } }
    func machineName(for node: String, fallback: String) -> String { machines.first(where: { $0.id == node })?.displayName ?? fallback }
    func renameMachine(_ node: String, nickname: String) {
        guard let index = machines.firstIndex(where: { $0.id == node }) else { return }
        var updated = machines
        let cleaned = nickname.trimmingCharacters(in: .whitespacesAndNewlines)
        guard cleaned.count <= 100 else { notice = "Use a machine nickname of 100 characters or fewer."; return }
        updated[index].nickname = cleaned.isEmpty ? nil : cleaned
        do { if !isFixture { try store.write(updated, name: "machines.json") }; machines = updated; rebuildBoard() }
        catch { notice = "Machine nickname could not be saved. Its connection identity is unchanged." }
    }
    private func rebuildBoard() {
        var rows: [ThreadIdentity: BoardItem] = [:]
        // Directly paired authoritative nodes win over coordinator copies.
        for source in sourceBoards.keys.sorted() {
            for row in sourceBoards[source] ?? [] where rows[row.identity] == nil || source == row.identity.nodeId {
                let direct = machines.first { $0.id == row.identity.nodeId }
                rows[row.identity] = BoardItem(identity: row.identity, name: row.name,
                    machine: direct?.displayName ?? row.machine, state: row.state, detail: row.detail,
                    observedAt: row.observedAt, cachedAt: row.cachedAt,
                    stale: row.stale == true || (!isFixture && !onlineNodes.contains(source)), unread: row.unread)
            }
        }
        board = rows.values.sorted { $0.identity.draftKey < $1.identity.draftKey }
    }
    private var connectingWire: SSHWire?
    private var connectionAttempt: ConnectionAttempt?
    private var eventsTask: Task<Void, Never>?
    private var connectionTask: Task<Void, Never>?
    private var verification: CheckedContinuation<Bool, Never>?
    private var generation = UUID()
    private var selectionGeneration = UUID()
    private var activeTurns: [ThreadIdentity: String] = [:]
    private var providerItems: [String: JSONValue] = [:]
    private var openingConversation: UUID?
    private var openingLiveItems = Set<String>()
    private var openingPartialItems = Set<String>()
    private var ambiguousLiveItems = Set<String>()
    private var snapshotBackedItems = Set<String>()
    @Published var conversationNeedsRefresh = false
    private var openingLiveTurn = false
    private var lastKnownMessages: [ThreadIdentity: [ChatMessage]] = [:]
    private var observedRequests: [String: (turn: String, item: String)] = [:]
    private var boardRevision: JSONValue = .null
    private var boardPage = 0
    private var boardStaging: [BoardItem] = []
    private var foreground = true
    private var reconnectAttempts = 0
    private var reconnectTask: Task<Void, Never>?
    private var firstBoardReceived = false
    private var boardReady: CheckedContinuation<Void, Error>?
    @Published var historyCursor: JSONValue = .null
    @Published var historyLoading = false
    private var awaitingAssistant = false
    private var assistantOpenToken: UUID?
    private var stagedAssistantEvents: [JSONValue] = []

    init() {
        #if DEBUG
        if ProcessInfo.processInfo.arguments.contains("--ui-fixture") {
            isFixture = true
            let fixture = FixtureWire()
            wire = fixture
            connected = true
            activeNodeId = "fixture-node"
            capabilities = .object(["board": .bool(true), "codexShared": .bool(true), "create": .bool(false), "adopt": .bool(false), "assistant": .bool(false)])
            if ProcessInfo.processInfo.arguments.contains("--fixture-mutations") {
                capabilities = .object(["board": .bool(true), "codexShared": .bool(true), "create": .bool(true), "adopt": .bool(true), "assistant": .bool(false)])
            }
            board = FixtureWire.items
            if ProcessInfo.processInfo.arguments.contains("--fixture-search") {
                machines = board.map { row in
                    SavedMachine(id: row.identity.nodeId, address: "fixture.invalid", port: 22,
                        username: "fixture", hostKey: "", credentialId: "", keyAuthentication: true, name: row.machine)
                }
            }
            observedAt = .now
            listen(fixture)
            return
        }
        #endif
        machines = store.read("machines.json", as: [SavedMachine].self) ?? []
        pendingPairingAvailable = (try? StagedPairing.pending()) != nil
        board = store.read("board.json", as: [BoardItem].self) ?? []
        sourceBoards = store.read("machine-boards.json", as: [String: [BoardItem]].self) ?? [:]
        sourceCoverage = store.read("machine-coverage.json", as: [String: Bool].self) ?? [:]
        if sourceBoards.isEmpty { for row in board { sourceBoards[row.identity.nodeId, default: []].append(row) } }
        rebuildBoard()
        observedAt = store.read("observed.json", as: Date.self)
        drafts = store.read("drafts.json", as: [String: String].self) ?? [:]
        draftSkills = store.read("draft-skills.json", as: [String: [String]].self) ?? [:]
        for key in Array(draftSkills.keys) {
            let words = Set((drafts[key] ?? "").split(whereSeparator: { $0.isWhitespace }).map(String.init))
            draftSkills[key] = draftSkills[key]?.filter { words.contains("$" + $0) }
        }
        pendingActions = store.read("pending.json", as: [String: PendingAction].self) ?? [:]
        choices = store.read("choices.json", as: [String: String].self) ?? [:]
        creations = store.read("creations.json", as: [String: CreationRecord].self) ?? [:]
        for id in creations.keys where creations[id]?.state == "pending" { creations[id]?.state = "unknown" }
        for id in pendingActions.keys where pendingActions[id]?.state == "pending" { pendingActions[id]?.state = "unknown" }
        try? store.write(pendingActions, name: "pending.json")
    }
    var freshness: String {
        guard let observedAt else { return connected ? "Connected · waiting for board" : "No saved board" }
        let age = max(0, Int(Date.now.timeIntervalSince(observedAt)))
        return "\(connected ? "Observed" : "Last known") \(age < 60 ? "\(age)s" : "\(age / 60)m") ago"
    }
    func setDraft(_ text: String, identity: ThreadIdentity) {
        drafts[identity.draftKey] = text
        let tokens = Set(text.split(whereSeparator: { $0.isWhitespace }).map(String.init))
        let retained = (draftSkills[identity.draftKey] ?? []).filter { tokens.contains("$" + $0) }
        if retained.isEmpty { draftSkills.removeValue(forKey: identity.draftKey) }
        else { draftSkills[identity.draftKey] = retained }
        if !isFixture {
            do { try store.write(drafts, name: "drafts.json"); try store.write(draftSkills, name: "draft-skills.json") }
            catch { notice = "Draft or selected skills could not be saved. Keep this app open until storage is available." }
        }
    }
    func draft(_ identity: ThreadIdentity) -> String { drafts[identity.draftKey] ?? "" }
    func referenceSkill(_ name: String, identity: ThreadIdentity) {
        let clean = name.hasPrefix("$") ? String(name.dropFirst()) : name
        guard !clean.isEmpty, clean.count <= 200, !clean.contains(where: { $0.isWhitespace }) else { return }
        var references = draftSkills[identity.draftKey] ?? []
        if !references.contains(clean) { references.append(clean) }
        guard references.count <= 32 else { notice = "Use at most 32 selected skills in one reply."; return }
        do {
            var updated = draftSkills; updated[identity.draftKey] = references
            if !isFixture { try store.write(updated, name: "draft-skills.json") }
            draftSkills = updated
        } catch { notice = "The skill selection could not be saved. Select it again before sending." }
    }
    func beginPairing(_ code: String) {
        beginPairingTask(code)
    }
    func finishPairing() { beginPairingTask(nil) }
    func discardPairing() {
        guard !connecting else { return }
        do {
            try KeychainStore.remove(id: StagedPairing.storageId)
            pendingPairingAvailable = false
            notice = "Unfinished pairing removed from this phone. Its key may still be authorized on the machine; remove it there if necessary."
        } catch { reportError(error) }
    }
    private func beginPairingTask(_ code: String?) {
        cancelConnection()
        let token = UUID(); generation = token; connecting = true; notice = "Pairing this phone…"
        connectionTask = Task {
            do {
                guard generation == token, !Task.isCancelled, foreground else { return }
                var staged: StagedPairing
                if let code {
                    let bootstrap = try PairingBootstrap.parse(code)
                    let descriptor: PairingDescriptor
                    if let previous = try StagedPairing.pending(), bootstrap.matches(previous.descriptor) { descriptor = previous.descriptor }
                    else { descriptor = try await bootstrap.resolve() }
                    guard generation == token, !Task.isCancelled, foreground else { return }
                    staged = try StagedPairing.prepare(descriptor)
                }
                else {
                    guard let pending = try StagedPairing.pending() else { throw PairingError.descriptor }
                    staged = pending
                }
                pendingPairingAvailable = true
                let descriptor = staged.descriptor
                if !staged.dispatched {
                    guard descriptor.expires_at > Date.now.timeIntervalSince1970 else { throw PairingError.expired }
                    staged.dispatched = true
                    try staged.persist() // Must survive a lost receipt/process exit before any HTTP write.
                    guard generation == token, !Task.isCancelled, foreground else { return }
                    do { try await PairingHTTP.claim(descriptor, publicKey: staged.publicKey) }
                    catch PairingError.unknown { /* Only exact-pinned SSH below reconciles; never repeat claim. */ }
                }
                guard generation == token, !Task.isCancelled else { return }
                connectionTask = nil
                beginConnection(address: descriptor.address, port: descriptor.ssh_port, username: descriptor.username,
                    secret: staged.secret, key: true, passphrase: "", expectedHost: descriptor.ssh_host_key,
                    expectedNode: descriptor.node_id, pairingStageId: staged.keychainId)
            } catch {
                guard generation == token, !Task.isCancelled else { return }
                connecting = false
                notice = (error as? PairingError)?.localizedDescription ??
                    (pendingPairingAvailable ? "Pairing is not confirmed. Finish the pending connection with this phone's saved key, or scan a fresh code for the same machine." :
                     "Could not reach pairing. Keep Tailscale on and choose Connect phone again.")
                noticeDetails = error.localizedDescription
            }
        }
    }
    func beginConnection(address: String, port: Int, username: String, secret: Data, key: Bool, passphrase: String, saved: SavedMachine? = nil,
                         expectedHost: String? = nil, expectedNode: String? = nil, pairingStageId: String? = nil) {
        cancelConnection()
        if saved == nil { reconnectAttempts = 0 }
        let token = UUID(); generation = token; connecting = true; notice = nil
        let attempt = ConnectionAttempt(); connectionAttempt = attempt
        connectionTask = Task {
            do {
                let transport = try await SSHWire.connect(address: address, port: port, username: username,
                    secret: secret, privateKey: key, passphrase: passphrase, pinnedHost: expectedHost ?? saved?.hostKey, attempt: attempt) { challenge in
                        await self.verify(challenge, token: token)
                    }
                guard generation == token, !Task.isCancelled else { await transport.close(); return }
                connectingWire = transport
                let hello = try await transport.request("hello", params: .object([:]))
                guard let node = hello["nodeId"].string, !node.isEmpty else { await transport.close(); throw ConnectionError.malformed }
                guard saved == nil || saved?.id == node else { await transport.close(); throw ConnectionError.changedNode }
                guard expectedNode == nil || expectedNode == node else { await transport.close(); throw ConnectionError.changedNode }
                guard generation == token, !Task.isCancelled else { await transport.close(); return }
                // Replacement is an explicit offline boundary. Never leave an
                // old online wire without its subscription or leak its socket.
                if let previous = wires.removeValue(forKey: node) {
                    listeners[node]?.cancel(); listeners.removeValue(forKey: node)
                    onlineNodes.remove(node); connected = !onlineNodes.isEmpty
                    if activeNodeId == node { wire = nil; conversationCapabilities = .null; conversationCached = !messages.isEmpty }
                    rebuildBoard()
                    await previous.close()
                    guard generation == token, !Task.isCancelled else { await transport.close(); return }
                }
                firstBoardReceived = false
                pendingBoardNode = node
                previousPendingBoard = sourceBoards[node]
                previousPendingCoverage = sourceCoverage[node]
                listen(transport, node: node)
                _ = try await transport.request("board/subscribe", params: .object([:]))
                try await waitForFirstBoard(token: token)
                guard generation == token, !Task.isCancelled else { await transport.close(); return }
                guard firstBoardReceived else { await transport.close(); throw ConnectionError.disconnected }
                let credentialId = saved?.credentialId ?? UUID().uuidString
                let payload = try JSONEncoder().encode(CredentialPayload(secret: secret, passphrase: passphrase))
                try KeychainStore.save(payload, id: credentialId)
                let machine = SavedMachine(id: node, address: address, port: port, username: username,
                    hostKey: transport.verifiedHostKey, credentialId: credentialId, keyAuthentication: key,
                    name: hello["name"].string ?? hello["hostname"].string ?? saved?.name,
                    nickname: machines.first(where: { $0.id == node })?.nickname)
                var updated = machines.filter { $0.id != node }; updated.append(machine)
                try store.write(updated, name: "machines.json")
                if let pairingStageId {
                    try? KeychainStore.remove(id: pairingStageId)
                    pendingPairingAvailable = (try? StagedPairing.pending()) != nil
                }
                machines = updated
                let restoresSelection = selected.map { !isConnected(node: $0.identity.nodeId) } ?? false
                wires[node] = transport; onlineNodes.insert(node); nodeCapabilities[node] = hello["capabilities"]
                blockedReconnectNodes.remove(node)
                wire = transport; connected = true; connecting = false; activeNodeId = node
                rebuildBoard()
                connectionCompletion += 1
                reconnectAttempts = 0
                connectingWire = nil
                pendingBoardNode = nil
                previousPendingBoard = nil
                previousPendingCoverage = nil
                try? store.write(sourceBoards, name: "machine-boards.json")
                try? store.write(sourceCoverage, name: "machine-coverage.json")
                connectionAttempt = nil
                capabilities = hello["capabilities"]
                if let selected {
                    selectMachine(node: selected.identity.nodeId)
                    if restoresSelection, isConnected(node: selected.identity.nodeId) {
                        if selected.state == "ASSISTANT" { _ = await openAssistant() }
                        else { await open(selected) }
                    }
                }
                guard generation == token else { return }
                resumeNextMachine()
            } catch {
                guard generation == token else { return }
                connecting = false; connected = !onlineNodes.isEmpty; reportError(error, action: "Could not connect to this machine. Check the address and login, then try again.")
                let unfinished = connectingWire; connectingWire = nil
                if let pending = pendingBoardNode {
                    listeners[pending]?.cancel(); listeners.removeValue(forKey: pending)
                    sourceBoards[pending] = previousPendingBoard; sourceCoverage[pending] = previousPendingCoverage; rebuildBoard()
                }
                pendingBoardNode = nil
                previousPendingBoard = nil
                previousPendingCoverage = nil
                if let unfinished { await unfinished.close() }
                guard generation == token else { return }
                if saved != nil {
                    switch error {
                    case ConnectionError.changedHost, ConnectionError.changedNode, ConnectionError.credentials,
                        ConnectionError.secureStorage, ConnectionError.malformed: if let saved { blockedReconnectNodes.insert(saved.id) }
                    default: scheduleReconnect(token: token)
                    }
                }
                resumeNextMachine()
            }
        }
    }
    private func verify(_ challenge: HostChallenge, token: UUID) async -> Bool {
        guard generation == token, connecting else { return false }
        return await withCheckedContinuation { continuation in
            verification?.resume(returning: false)
            verification = continuation; hostChallenge = challenge
        }
    }
    private func waitForFirstBoard(token: UUID) async throws {
        if firstBoardReceived { return }
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                boardReady = continuation
                Task {
                    try? await Task.sleep(for: .seconds(20))
                    guard generation == token, let waiting = boardReady else { return }
                    boardReady = nil; waiting.resume(throwing: ConnectionError.timeout)
                }
            }
        } onCancel: {
            Task { @MainActor in
                guard self.generation == token else { return }
                self.boardReady?.resume(throwing: CancellationError()); self.boardReady = nil
            }
        }
    }
    func verifyHost(_ accepted: Bool) {
        verification?.resume(returning: accepted); verification = nil; hostChallenge = nil
    }
    func cancelConnection() {
        reconnectTask?.cancel(); reconnectTask = nil
        boardReady?.resume(throwing: CancellationError()); boardReady = nil
        generation = UUID(); connectionTask?.cancel(); connectionTask = nil
        connecting = false; verifyHost(false)
        if let unfinished = connectingWire { Task { await unfinished.close() }; connectingWire = nil }
        if let pending = pendingBoardNode {
            listeners[pending]?.cancel(); listeners.removeValue(forKey: pending)
            sourceBoards[pending] = previousPendingBoard; sourceCoverage[pending] = previousPendingCoverage; rebuildBoard()
        }; pendingBoardNode = nil; previousPendingBoard = nil; previousPendingCoverage = nil
        if let attempt = connectionAttempt { Task { await attempt.cancel() }; connectionAttempt = nil }
    }
    func resume() {
        if !foreground { reconnectAttempts = 0 }
        foreground = true
        guard !isFixture, !connecting else { return }
        resumeQueue = machines.filter { !onlineNodes.contains($0.id) && !blockedReconnectNodes.contains($0.id) }
        resumeNextMachine()
    }
    private func resumeNextMachine() {
        guard foreground, !connecting else { return }
        guard !resumeQueue.isEmpty else {
            if machines.contains(where: { !onlineNodes.contains($0.id) && !blockedReconnectNodes.contains($0.id) }) { scheduleReconnect(token: generation) }
            return
        }
        let saved = resumeQueue.removeFirst()
        do {
            let credential = try JSONDecoder().decode(CredentialPayload.self, from: KeychainStore.load(id: saved.credentialId))
            beginConnection(address: saved.address, port: saved.port, username: saved.username,
                secret: credential.secret, key: saved.keyAuthentication, passphrase: credential.passphrase, saved: saved)
        } catch { blockedReconnectNodes.insert(saved.id); notice = "Saved login unavailable for \(saved.displayName). Add this machine again to authenticate."; resumeNextMachine() }
    }
    func retryConnection() { reconnectAttempts = 0; blockedReconnectNodes = []; resume() }
    private func scheduleReconnect(token: UUID) {
        guard foreground, !isFixture, reconnectTask == nil, !machines.isEmpty else { return }
        reconnectAttempts = min(6, reconnectAttempts + 1)
        let delay = min(30, 1 << reconnectAttempts)
        reconnectTask = Task {
            defer { if generation == token { reconnectTask = nil } }
            try? await Task.sleep(for: .seconds(delay))
            guard !Task.isCancelled, foreground, !connecting else { return }
            reconnectTask = nil; resume()
        }
    }
    func suspend() {
        foreground = false
        cancelConnection(); connected = false; eventsTask?.cancel()
        resumeQueue = []
        for task in listeners.values { task.cancel() }; listeners = [:]
        for transport in wires.values { Task { await transport.close() } }; wires = [:]; onlineNodes = []
        rebuildBoard()
        conversationCapabilities = .null; conversationCached = !messages.isEmpty
        boardStaging = []; boardPage = 0; boardRevision = .null
        if let wire { Task { await wire.close() } }
        wire = nil
    }
    private func listen(_ transport: any MobileWire, node: String? = nil) {
        let source = node ?? "fixture-node"
        listeners[source]?.cancel()
        let epoch = UUID(); wireEpochs[source] = epoch; sourceNodes[source] = nil
        let token = generation
        listeners[source] = Task {
            for await frame in transport.events {
                guard !Task.isCancelled, wireEpochs[source] == epoch else { return }
                // A paired transport may project fleet rows, but cannot impersonate
                // another directly paired node's conversation stream.
                // The DEBUG-only in-memory fixture deliberately multiplexes
                // synthetic nodes on one wire; real SSH wires must prove routing.
                if !isFixture, frame["event"].string?.hasPrefix("conversation/") == true,
                    let owner = frame["params"]["identity"]["nodeId"].string,
                    owner != source, route(for: owner) != source { continue }
                receivingNode = source
                boardStaging = stagingByNode[source] ?? []; boardRevision = revisionByNode[source] ?? .null; boardPage = pageByNode[source] ?? 0
                handleEvent(frame)
                stagingByNode[source] = boardStaging; revisionByNode[source] = boardRevision; pageByNode[source] = boardPage
                receivingNode = nil
            }
            guard !Task.isCancelled, wireEpochs[source] == epoch else { return }
            if pendingBoardNode == source {
                firstBoardReceived = false
                boardReady?.resume(throwing: ConnectionError.disconnected); boardReady = nil
            }
            wires.removeValue(forKey: source); onlineNodes.remove(source); connected = !onlineNodes.isEmpty
            if activeNodeId == source { wire = nil; conversationCapabilities = .null; conversationCached = !messages.isEmpty }
            rebuildBoard()
            notice = "Connection lost. Last-known content and drafts are still here. Uncertain actions are not repeated."
            boardStaging = []; boardPage = 0; boardRevision = .null
            scheduleReconnect(token: token)
        }
    }
    private func identity(_ value: JSONValue) -> ThreadIdentity? {
        guard let node = value["nodeId"].string, let provider = value["provider"].string, let thread = value["threadId"].string,
            !node.isEmpty, !provider.isEmpty, !thread.isEmpty else { return nil }
        return ThreadIdentity(nodeId: node, provider: provider, threadId: thread)
    }
    private func handleEvent(_ frame: JSONValue) {
        let params = frame["params"]
        var resolution: (turn: String, item: String)?
        if frame["event"].string == "conversation/event", let owner = identity(params["identity"]),
            params["requestId"] != .null, let turn = params["params"]["turnId"].string,
            let item = params["params"]["itemId"].string {
            if observedRequests.count >= 256 { observedRequests = [:] }
            observedRequests[owner.draftKey + ":" + String(describing: params["requestId"])] = (turn, item)
        }
        if frame["event"].string == "conversation/disconnected", identity(params["identity"]) == selected?.identity {
            conversationCapabilities = .null; conversationCached = !messages.isEmpty
            notice = "The original provider connection ended. Reopen to verify its state."
            noticeDetails = params["message"].string
            return
        }
        if frame["event"].string == "connection/error" {
            if let selected,
                (isFixture || receivingNode == route(for: selected.identity.nodeId)),
                (params["nodeId"] == .null || params["nodeId"].string == selected.identity.nodeId) {
                conversationCapabilities = .null; conversationCached = !messages.isEmpty
            }
            notice = "A machine connection failed. Last-known content is preserved."
            noticeDetails = params["message"].string
            return
        }
        if frame["event"].string == "conversation/event", params["method"].string == "serverRequest/resolved",
            let owner = identity(params["identity"]), params["params"]["requestId"] != .null {
            let request = params["params"]["requestId"]
            resolution = observedRequests.removeValue(forKey: owner.draftKey + ":" + String(describing: request))
            if let turn = params["params"]["turnId"].string, let item = params["params"]["itemId"].string { resolution = (turn, item) }
            for action in Array(pendingActions.values) where action.identity == owner && action.requestId == request &&
                resolution != nil && action.turnId == resolution?.turn && action.itemId == resolution?.item {
                pendingActions[action.id]?.state = "resolved"
                if selected?.identity == owner, noticeRequestAction == action.id { requestNotice(requestClosedNotice, action: action.id) }
            }
            if !isFixture { try? store.write(pendingActions, name: "pending.json") }
        }
        #if DEBUG
        if isFixture, frame["event"].string == "fixture/sendCount" {
            fixtureSendCount = Int(params["count"].number ?? 0); return
        }
        if isFixture, frame["event"].string == "fixture/assistantFinished" { fixtureAssistantFinished = true; return }
        #endif
        if frame["event"].string == "board/snapshot" {
            let items = params["items"].array.compactMap { row -> BoardItem? in
                guard let id = identity(row["identity"]), let name = row["name"].string else { return nil }
                return BoardItem(identity: id, name: name, machine: row["machine"].string ?? "Machine",
                    state: row["status"].string ?? "UNKNOWN", detail: "", observedAt: row["observedAt"].number,
                    cachedAt: row["cachedAt"].number, stale: row["stale"].bool, unread: row["unread"].bool)
            }
            guard items.count == params["items"].array.count, params["items"].hasArrayShape else {
                if receivingNode == pendingBoardNode {
                    boardReady?.resume(throwing: ConnectionError.malformed); boardReady = nil
                }
                notice = "Invalid board snapshot. Previous board preserved."; return
            }
            // The verified selected node exposes its explicitly permitted fleet;
            // remote row ownership stays exact, not rewritten to the coordinator.
            if params["page"] != .null {
                guard let index = params["page"]["index"].number, index >= 0, index <= 100_000,
                    index.rounded(.towardZero) == index else { notice = "Invalid board page. The previous board is preserved."; return }
                if index == 0 { boardRevision = params["revision"]; boardPage = 0; boardStaging = [] }
                guard params["revision"] == boardRevision, Int(index) == boardPage,
                    boardStaging.count + items.count <= 100_000 else { boardStaging = []; notice = "Incomplete board update. The previous board is preserved."; return }
                boardStaging.append(contentsOf: items); boardPage += 1
                guard params["page"]["complete"].bool else { return }
                guard Set(boardStaging.map(\.identity)).count == boardStaging.count else { boardStaging = []; notice = "Duplicate identity in board update. Previous board preserved."; return }
                board = boardStaging; boardStaging = []
            } else { board = items }
            if let source = receivingNode {
                sourceBoards[source] = board
                sourceCoverage[source] = params["coverage"]["partial"].bool
                rebuildBoard()
                if !isFixture, source != pendingBoardNode {
                    try? store.write(sourceBoards, name: "machine-boards.json")
                    try? store.write(sourceCoverage, name: "machine-coverage.json")
                }
            }
            if connectingWire != nil, receivingNode == pendingBoardNode {
                firstBoardReceived = true; boardReady?.resume(); boardReady = nil
            } else if isFixture { firstBoardReceived = true }
            observedAt = params["observedAt"].number.map { Date(timeIntervalSince1970: $0) }
            if !isFixture, receivingNode != pendingBoardNode { try? store.write(board, name: "board.json"); try? store.write(observedAt, name: "observed.json") }
            if !params["health"].array.isEmpty { notice = "Some machine information is unavailable. Last-known content is preserved."; noticeDetails = params["health"].array.compactMap(\.string).joined(separator: "; ") }
        } else if frame["event"].string == "conversation/event", awaitingAssistant, assistantOpenToken == selectionGeneration {
            if stagedAssistantEvents.count < 128 { stagedAssistantEvents.append(frame) }
            else { notice = "Assistant events exceeded the safe handoff limit. Reopen the existing assistant." }
        } else if frame["event"].string == "conversation/event", identity(params["identity"]) == selected?.identity {
            let method = params["method"].string ?? ""
            let event = params["params"]
            if openingConversation == selectionGeneration {
                // A request referring to an item is not a newer copy of that
                // item. Keep its snapshot detail so approvals retain their diff.
                if ["item/started", "item/completed"].contains(method), let id = event["item"]["id"].string {
                    openingLiveItems.insert(id)
                }
                if method == "item/agentMessage/delta", let id = event["itemId"].string {
                    openingLiveItems.insert(id); openingPartialItems.insert(id)
                }
                if method == "item/completed", let id = event["item"]["id"].string { openingPartialItems.remove(id) }
                if method == "turn/started" || method == "turn/completed" { openingLiveTurn = true }
            }
            if ["item/started", "item/completed"].contains(method), let id = event["item"]["id"].string {
                providerItems[id] = event["item"]
                if let current = approval, current.params["itemId"].string == id {
                    approval = ProviderApproval(id: current.id, identity: current.identity, requestId: current.requestId,
                        method: current.method, params: current.params, item: event["item"])
                }
            }
            if method == "turn/started", let selected, let id = event["turn"]["id"].string {
                activeTurns[selected.identity] = id
            } else if method == "turn/completed", let selected, let id = event["turn"]["id"].string,
                activeTurns[selected.identity] == id {
                activeTurns.removeValue(forKey: selected.identity)
            } else if method == "item/agentMessage/delta", let text = event["delta"].string {
                let id = event["itemId"].string ?? "stream"
                // No snapshot/event watermark exists on legacy endpoints. Wait
                // for a full item rather than duplicating or dropping a prefix.
                if snapshotBackedItems.contains(id) { ambiguousLiveItems.insert(id); conversationNeedsRefresh = true }
                guard !ambiguousLiveItems.contains(id) else { return }
                if let index = messages.firstIndex(where: { $0.id == id }) {
                    let old = messages[index]; messages[index] = ChatMessage(id: old.id, role: old.role, text: old.text + text)
                } else { messages.append(ChatMessage(id: id, role: "assistant", text: text)) }
            } else if method == "item/completed", let type = event["item"]["type"].string,
                type == "userMessage" || type == "agentMessage", let id = event["item"]["id"].string {
                let entry = event["item"]
                ambiguousLiveItems.remove(id); snapshotBackedItems.remove(id); conversationNeedsRefresh = !ambiguousLiveItems.isEmpty
                let text = entry["text"].string ?? entry["content"].array.compactMap { $0["text"].string }.joined(separator: "\n")
                let message = ChatMessage(id: id, role: type == "userMessage" ? "user" : "assistant", text: text)
                if let index = messages.firstIndex(where: { $0.id == id }) { messages[index] = message }
                else { messages.append(message) }
                if id.hasPrefix("claude-channel-reply:"), let selected, selected.identity.provider == "claude" {
                    Task { await self.reconcile(selected) }
                }
            } else if method == "item/tool/requestUserInput", event["questions"].array.count > 0,
                let turnId = event["turnId"].string, let itemId = event["itemId"].string,
                params["requestId"] != .null, event["questions"].array.allSatisfy({ $0["id"].string?.isEmpty == false }) {
                question = ProviderQuestion(id: itemId + ":" + String(describing: params["requestId"]), requestId: params["requestId"], turnId: turnId,
                    itemId: itemId, questions: event["questions"])
            } else if ["item/commandExecution/requestApproval", "item/fileChange/requestApproval"].contains(method),
                let selected, let itemId = event["itemId"].string, event["turnId"].string != nil, params["requestId"] != .null {
                approval = ProviderApproval(id: itemId + ":" + String(describing: params["requestId"]), identity: selected.identity,
                    requestId: params["requestId"], method: method, params: event, item: providerItems[itemId] ?? .null)
            } else if method == "serverRequest/resolved", let shown = approval, event["requestId"] == shown.requestId,
                resolution?.turn == shown.params["turnId"].string, resolution?.item == shown.params["itemId"].string {
                pendingActions["approval:" + shown.identity.draftKey + ":" + shown.id]?.state = "resolved"
                if !isFixture { try? store.write(pendingActions, name: "pending.json") }
                approval = nil
            } else if method == "serverRequest/resolved", let shown = question,
                event["requestId"] == shown.requestId, resolution?.turn == shown.turnId, resolution?.item == shown.itemId {
                pendingActions["answer:" + (selected?.identity.draftKey ?? "") + ":" + shown.id]?.state = "resolved"
                if !isFixture { try? store.write(pendingActions, name: "pending.json") }
                question = nil
            }
        }
    }
    func open(_ item: BoardItem) async {
        if !isFixture { selectMachine(node: item.identity.nodeId) }
        if selected?.identity != item.identity, noticeRequestAction != nil { notice = nil }
        if let previous = selected { lastKnownMessages[previous.identity] = Array(messages.suffix(500)) }
        selected = item; messages = lastKnownMessages[item.identity] ?? []; question = nil; approval = nil; providerItems = [:]; conversationCapabilities = .null; historyLoading = false; historyCursor = .null
        conversationCached = !messages.isEmpty
        if lastKnownMessages.count > 20, let eviction = lastKnownMessages.keys.first(where: { $0 != item.identity }) { lastKnownMessages.removeValue(forKey: eviction) }
        let token = UUID(); selectionGeneration = token
        openingConversation = token; openingLiveItems = []; openingPartialItems = []; openingLiveTurn = false
        ambiguousLiveItems = []; snapshotBackedItems = []; conversationNeedsRefresh = false
        defer { if openingConversation == token { openingConversation = nil; openingLiveItems = []; openingPartialItems = []; openingLiveTurn = false } }
        let connectionToken = routeEpoch(for: item.identity.nodeId)
        guard let wire = transport(for: item.identity.nodeId) else { notice = "Reconnect to read this exact conversation. Your draft is saved."; return }
        do {
            let result = try await wire.request("conversation/open", params: .object(["identity": item.identity.json]))
            guard routeEpoch(for: item.identity.nodeId) == connectionToken, selectionGeneration == token, selected?.identity == item.identity else { return }
            guard identity(result["identity"]) == item.identity else { throw ConnectionError.changedNode }
            conversationCapabilities = result["capabilities"]
            if !openingLiveTurn { activeTurns[item.identity] = result["activeTurnId"].string }
            historyCursor = result["turns"]["nextCursor"]
            let turns = chronologicalTurns(result, provider: item.identity.provider)
            snapshotBackedItems = activeSnapshotItems(turns, activeTurnId: result["activeTurnId"].string)
            for entry in turns.flatMap({ $0["items"].array }) { if let id = entry["id"].string, !openingLiveItems.contains(id) { providerItems[id] = entry } }
            if let current = approval, let id = current.params["itemId"].string, let original = providerItems[id] {
                approval = ProviderApproval(id: current.id, identity: current.identity, requestId: current.requestId,
                    method: current.method, params: current.params, item: original)
            }
            let live = messages.filter { openingLiveItems.contains($0.id) }
            var snapshot = turns.flatMap { turn in turn["items"].array.compactMap { entry -> ChatMessage? in
                guard let type = entry["type"].string, type == "agentMessage" || type == "userMessage" else { return nil }
                let text = entry["text"].string ?? entry["content"].array.compactMap { $0["text"].string }.joined(separator: "\n")
                return ChatMessage(id: entry["id"].string ?? UUID().uuidString, role: type == "userMessage" ? "user" : "assistant", text: text)
            } }
            for message in live {
                if let index = snapshot.firstIndex(where: { $0.id == message.id }) {
                    if openingPartialItems.contains(message.id) { ambiguousLiveItems.insert(message.id) }
                    else { snapshot[index] = message }
                }
                else { snapshot.append(message) }
            }
            conversationNeedsRefresh = !ambiguousLiveItems.isEmpty
            messages = snapshot
            lastKnownMessages[item.identity] = Array(messages.suffix(500))
            conversationCached = false
            // Acknowledge only the successfully installed exact server snapshot.
            // The owning server freezes its event before reading and compares it
            // atomically; cached/failed opens and newer output remain unread.
            if let acknowledgement = result["readAcknowledgement"].string {
                do {
                    let receipt = try await wire.request("conversation/acknowledge", params: .object([
                        "identity": item.identity.json, "readAcknowledgement": .string(acknowledgement)
                    ]))
                    guard identity(receipt["identity"]) == item.identity,
                        receipt["acknowledged"] == .bool(true) || receipt["acknowledged"] == .bool(false) else {
                        throw ConnectionError.remote("Unread acknowledgement outcome is unconfirmed; newer unread activity was not cleared locally.")
                    }
                } catch {
                    if routeEpoch(for: item.identity.nodeId) == connectionToken, selectionGeneration == token {
                        reportError(error, action: "Conversation opened, but unread acknowledgement was not confirmed.")
                    }
                }
            }
        } catch { if routeEpoch(for: item.identity.nodeId) == connectionToken, selectionGeneration == token { reportError(error, action: "Could not read this conversation. Reconnect to check its original state.") } }
    }
    func loadOlder(_ item: BoardItem) async {
        guard selected?.identity == item.identity, historyCursor != .null, !historyLoading, let wire = transport(for: item.identity.nodeId) else { return }
        let token = selectionGeneration, cursor = historyCursor
        let connectionToken = routeEpoch(for: item.identity.nodeId)
        historyLoading = true
        defer { if selectionGeneration == token, routeEpoch(for: item.identity.nodeId) == connectionToken { historyLoading = false } }
        do {
            let result = try await wire.request("conversation/history", params: .object(["identity": item.identity.json, "cursor": cursor]))
            guard routeEpoch(for: item.identity.nodeId) == connectionToken, selectionGeneration == token, selected?.identity == item.identity else { return }
            guard identity(result["identity"]) == item.identity else { throw ConnectionError.changedNode }
            let previous = chronologicalTurns(result, provider: item.identity.provider).flatMap { $0["items"].array.compactMap { entry -> ChatMessage? in
                guard let id = entry["id"].string, let type = entry["type"].string, ["userMessage", "agentMessage"].contains(type) else { return nil }
                return ChatMessage(id: id, role: type == "userMessage" ? "user" : "assistant",
                    text: entry["text"].string ?? entry["content"].array.compactMap { $0["text"].string }.joined(separator: "\n"))
            } }
            var ids = Set(messages.map(\.id))
            messages = previous.filter { ids.insert($0.id).inserted } + messages
            historyCursor = result["turns"]["nextCursor"]
        } catch { if routeEpoch(for: item.identity.nodeId) == connectionToken, selectionGeneration == token { reportError(error, action: "Older context is unavailable. Try again after reconnecting.") } }
    }
    func conversationReadToken(for identity: ThreadIdentity) -> String? {
        guard selected?.identity == identity, let route = routeEpoch(for: identity.nodeId) else { return nil }
        return selectionGeneration.uuidString + ":" + route.source + ":" + route.epoch.uuidString
    }
    private func chronologicalTurns(_ result: JSONValue, provider: String) -> [JSONValue] {
        guard result["turns"]["data"].hasArrayShape else { return result["thread"]["turns"].array }
        let turns = result["turns"]["data"].array
        // Older Codex mobile endpoints expose turns/list's descending default.
        // Items within each turn already retain their original order.
        if result["turns"]["order"].string == "descending" ||
            (result["turns"]["order"].string == nil && provider == "codex") { return Array(turns.reversed()) }
        return turns
    }
    private func activeSnapshotItems(_ turns: [JSONValue], activeTurnId: String?) -> Set<String> {
        Set(turns.filter { $0["status"].string == "inProgress" || (activeTurnId != nil && $0["id"].string == activeTurnId) }
            .flatMap { $0["items"].array }.filter { $0["type"].string == "agentMessage" }.compactMap { $0["id"].string })
    }
    func canSend(_ item: BoardItem, composing: Bool) -> Bool {
        isConnected(node: item.identity.nodeId) && selected?.identity == item.identity && conversationCapabilities["send"].bool && !composing && !draft(item.identity).trimmingCharacters(in: .whitespacesAndNewlines).isEmpty &&
            !pendingActions.values.contains { $0.identity == item.identity && !$0.id.hasPrefix("answer:") && !$0.id.hasPrefix("approval:") && ($0.state == "pending" || $0.state == "unknown") }
    }
    func send(_ item: BoardItem, composing: Bool) async {
        guard canSend(item, composing: composing), let wire = transport(for: item.identity.nodeId) else { return }
        let text = draft(item.identity), id = UUID().uuidString
        pendingActions[id] = PendingAction(id: id, identity: item.identity, text: text, state: "pending")
        if !isFixture {
            do { try store.write(pendingActions, name: "pending.json") }
            catch { pendingActions.removeValue(forKey: id); notice = "Not sent. Your outgoing text could not be saved safely."; return }
        }
        let token = routeEpoch(for: item.identity.nodeId)
        do {
            var params: [String: JSONValue] = ["identity": item.identity.json,
                "clientMessageId": .string(id), "text": .string(text)]
            let words = Set(text.split(whereSeparator: { $0.isWhitespace }).map(String.init))
            let skills = (draftSkills[item.identity.draftKey] ?? []).filter { words.contains("$" + $0) }
            if !skills.isEmpty { params["skills"] = .array(skills.map(JSONValue.string)) }
            if let turn = activeTurns[item.identity] { params["expectedTurnId"] = .string(turn) }
            let result = try await wire.request("conversation/send", params: .object(params))
            guard identity(result["identity"]) == item.identity, result["clientMessageId"].string == id else { throw ConnectionError.malformed }
            let receipt = result["state"].string ?? "unknown"
            let state = pendingActions[id]?.state == "accepted" || ["accepted", "delivered"].contains(receipt) ? "accepted" : (receipt == "rejected" ? "rejected" : "unknown")
            pendingActions[id]?.state = state
            if state == "accepted", draft(item.identity) == text { setDraft("", identity: item.identity) }
            if routeEpoch(for: item.identity.nodeId) == token, selected?.identity == item.identity {
                notice = state == "accepted" ? (isFixture ? "Accepted by the UI fixture only." : ProcessInfo.processInfo.arguments.contains("--multi-machine-integration-test") ? "Accepted by the disposable SSH protocol fixture only." : "Accepted by the existing provider conversation.") :
                    (state == "rejected" ? "The provider rejected this message. Your draft is preserved." : "Outcome unknown. This message will not be repeated automatically.")
            }
        } catch ConnectionError.rejected(let message) {
            pendingActions[id]?.state = "rejected"
            if routeEpoch(for: item.identity.nodeId) == token, selected?.identity == item.identity { notice = "The machine rejected this message. Your draft is preserved."; noticeDetails = message }
        } catch {
            pendingActions[id]?.state = "unknown"
            if routeEpoch(for: item.identity.nodeId) == token, selected?.identity == item.identity { notice = "Outcome unknown. Your text is saved; it was not repeated." }
        }
        if !isFixture {
            do { try store.write(pendingActions, name: "pending.json") }
            catch { notice = "The machine's outcome could not be saved locally. Reconnect to check the durable receipt before any resend." }
        }
    }
    func reconcile(_ item: BoardItem) async {
        guard let wire = transport(for: item.identity.nodeId) else { return }
        guard receiptChecksInFlight.insert(item.identity).inserted else { return }
        defer { receiptChecksInFlight.remove(item.identity) }
        let unknown = pendingActions.values.filter { $0.identity == item.identity && ($0.state == "unknown" || (item.identity.provider == "claude" && $0.state == "pending")) && !$0.id.hasPrefix("answer:") && !$0.id.hasPrefix("approval:") }
        for action in unknown {
            do {
                var params: [String: JSONValue] = ["identity": item.identity.json, "clientMessageId": .string(action.id)]
                if let cursor = action.receiptCursor { params["cursor"] = .string(cursor) }
                let result = try await wire.request("conversation/receipt", params: .object(params))
                guard identity(result["identity"]) == item.identity, result["clientMessageId"].string == action.id else { throw ConnectionError.malformed }
                if ["accepted", "delivered"].contains(result["state"].string ?? "") {
                    pendingActions[action.id]?.state = "accepted"
                    if draft(item.identity) == action.text { setDraft("", identity: item.identity) }
                    notice = "The original machine confirmed this message was accepted. It was not sent again."
                } else if result["state"].string == "rejected" {
                    pendingActions[action.id]?.state = "rejected"
                    notice = "The original machine confirmed rejection. The draft is preserved."
                } else {
                    pendingActions[action.id]?.receiptCursor = result["nextCursor"].string
                    notice = result["nextCursor"].string == nil ? "Delivery remains uncertain. Nothing was repeated." : "More original history remains. Check again to continue the read-only receipt search."
                }
            } catch { notice = "The original machine cannot confirm the outcome yet. The message was not repeated." }
        }
        if !isFixture { try? store.write(pendingActions, name: "pending.json") }
    }
    func answer(_ question: ProviderQuestion, identity: ThreadIdentity, answers: [String: [String]]) async {
        guard connected, selected?.identity == identity, self.question?.id == question.id,
            self.question?.requestId == question.requestId, conversationCapabilities["answer"].bool, let item = selected, let wire = transport(for: identity.nodeId) else { return }
        let actionId = "answer:" + item.identity.draftKey + ":" + question.id
        let token = routeEpoch(for: identity.nodeId)
        guard pendingActions[actionId] == nil else { notice = "This answer is already pending or uncertain. It will not be repeated."; return }
        pendingActions[actionId] = PendingAction(id: actionId, identity: item.identity, text: "Structured answer", state: "pending",
            requestId: question.requestId, turnId: question.turnId, itemId: question.itemId)
        if !isFixture {
            do { try store.write(pendingActions, name: "pending.json") }
            catch { pendingActions.removeValue(forKey: actionId); notice = "Not submitted. The pending answer could not be saved safely."; return }
        }
        requestNotice("Submitting the answer to the original request…", action: actionId)
        do {
            let mapped = answers.mapValues { JSONValue.object(["answers": .array($0.map(JSONValue.string))]) }
            let result = try await wire.request("conversation/answer", params: .object(["identity": item.identity.json,
                "requestId": question.requestId, "turnId": .string(question.turnId), "itemId": .string(question.itemId), "answers": .object(mapped)]))
            if result["state"].string != "submitted", pendingActions[actionId]?.state != "resolved" { pendingActions[actionId]?.state = result["state"].string == "rejected" ? "rejected" : "unknown" }
            if routeEpoch(for: identity.nodeId) == token, selected?.identity == identity, noticeRequestAction == actionId {
                requestNotice(pendingActions[actionId]?.state == "resolved" ? requestClosedNotice : result["state"].string == "submitted" ? "Answer submitted. Waiting for the original request to resolve." : result["state"].string == "rejected" ? "The machine rejected this answer. It was not repeated." : "Answer outcome unknown. It was not repeated.", action: actionId)
            }
        } catch ConnectionError.rejected(let details) {
            if pendingActions[actionId]?.state != "resolved" { pendingActions[actionId]?.state = "rejected" }
            if routeEpoch(for: identity.nodeId) == token, selected?.identity == identity, noticeRequestAction == actionId { requestNotice(pendingActions[actionId]?.state == "resolved" ? requestClosedNotice : "The machine rejected this answer before dispatch. Nothing was submitted.", action: actionId); noticeDetails = details }
        } catch {
            if pendingActions[actionId]?.state != "resolved" { pendingActions[actionId]?.state = "unknown" }
            if routeEpoch(for: identity.nodeId) == token, selected?.identity == identity, noticeRequestAction == actionId { requestNotice(pendingActions[actionId]?.state == "resolved" ? requestClosedNotice : "Answer outcome unknown. It was not repeated.", action: actionId) }
        }
        if !isFixture { try? store.write(pendingActions, name: "pending.json") }
    }
    func approve(_ request: ProviderApproval, decision: String) async {
        guard connected, selected?.identity == request.identity, approval?.id == request.id,
            approval?.requestId == request.requestId, ["accept", "decline"].contains(decision), let wire = transport(for: request.identity.nodeId) else { return }
        let id = "approval:" + request.identity.draftKey + ":" + request.id
        let token = routeEpoch(for: request.identity.nodeId)
        guard pendingActions[id] == nil else { return }
        pendingActions[id] = PendingAction(id: id, identity: request.identity, text: "One-time provider decision", state: "pending",
            requestId: request.requestId, turnId: request.params["turnId"].string, itemId: request.params["itemId"].string)
        do {
            if !isFixture { try store.write(pendingActions, name: "pending.json") }
        } catch { pendingActions.removeValue(forKey: id); notice = "Not submitted. The decision could not be saved safely."; return }
        requestNotice("Submitting the decision to the original request…", action: id)
        do {
            let result = try await wire.request("conversation/approve", params: .object(["identity": request.identity.json,
                "requestId": request.requestId, "turnId": request.params["turnId"], "itemId": request.params["itemId"], "decision": .string(decision)]))
            if pendingActions[id]?.state != "resolved" { pendingActions[id]?.state = result["state"].string == "submitted" ? "pending" : result["state"].string == "rejected" ? "rejected" : "unknown" }
            if routeEpoch(for: request.identity.nodeId) == token, selected?.identity == request.identity, noticeRequestAction == id {
                requestNotice(pendingActions[id]?.state == "resolved" ? requestClosedNotice : result["state"].string == "submitted" ? "Decision submitted. Waiting for the original request to close." : result["state"].string == "rejected" ? "The machine rejected this decision. It was not repeated." : "Decision outcome unknown. It was not repeated.", action: id)
            }
        } catch ConnectionError.rejected(let details) {
            if pendingActions[id]?.state != "resolved" { pendingActions[id]?.state = "rejected" }
            if routeEpoch(for: request.identity.nodeId) == token, selected?.identity == request.identity, noticeRequestAction == id { requestNotice(pendingActions[id]?.state == "resolved" ? requestClosedNotice : "The machine rejected this decision before dispatch. Nothing was submitted.", action: id); noticeDetails = details }
        } catch {
            if pendingActions[id]?.state != "resolved" { pendingActions[id]?.state = "unknown" }
            if routeEpoch(for: request.identity.nodeId) == token, selected?.identity == request.identity, noticeRequestAction == id { requestNotice(pendingActions[id]?.state == "resolved" ? requestClosedNotice : "Decision outcome unknown. It was not repeated.", action: id); noticeDetails = error.localizedDescription }
        }
        if !isFixture { try? store.write(pendingActions, name: "pending.json") }
    }
    func reconcileRequests(_ item: BoardItem) async {
        guard selected?.identity == item.identity, let wire = transport(for: item.identity.nodeId) else { return }
        let token = routeEpoch(for: item.identity.nodeId)
        let actions = pendingActions.values.filter { $0.identity == item.identity && $0.requestId != nil && ["pending", "unknown"].contains($0.state) }
        var ownsNotice = true
        for action in actions {
            guard let request = action.requestId, let turn = action.turnId, let entry = action.itemId else { continue }
            if ownsNotice, routeEpoch(for: item.identity.nodeId) == token, selected?.identity == item.identity { requestNotice("Checking the original request…", action: action.id) }
            do {
                let result = try await wire.request("conversation/requestStatus", params: .object([
                    "identity": item.identity.json, "requestId": request, "turnId": .string(turn), "itemId": .string(entry)]))
                guard identity(result["identity"]) == item.identity, result["requestId"] == request,
                    result["turnId"].string == turn, result["itemId"].string == entry else { throw ConnectionError.malformed }
                if result["state"].string == "resolved" {
                    pendingActions[action.id]?.state = "resolved"
                    if routeEpoch(for: item.identity.nodeId) == token, selected?.identity == item.identity {
                        if question?.requestId == request, question?.turnId == turn, question?.itemId == entry { question = nil }
                        if approval?.identity == item.identity, approval?.requestId == request,
                            approval?.params["turnId"].string == turn, approval?.params["itemId"].string == entry { approval = nil }
                    }
                }
                if ownsNotice, routeEpoch(for: item.identity.nodeId) == token, selected?.identity == item.identity, noticeRequestAction == action.id {
                    requestNotice(pendingActions[action.id]?.state == "resolved" ? requestClosedNotice :
                        "The decision remains pending or uncertain. It will not be repeated and does not block unrelated replies.", action: action.id)
                } else { ownsNotice = false }
            } catch {
                if ownsNotice, routeEpoch(for: item.identity.nodeId) == token, selected?.identity == item.identity, noticeRequestAction == action.id {
                    requestNotice(pendingActions[action.id]?.state == "resolved" ? requestClosedNotice : "The original request status is unknown. Nothing was repeated.", action: action.id)
                } else { ownsNotice = false }
            }
        }
        if !isFixture { try? store.write(pendingActions, name: "pending.json") }
    }
    func openAssistant() async -> BoardItem? {
        guard let requestedNode = activeNodeId, let wire = transport(for: requestedNode) else { notice = "Reconnect to the machine that owns your Pika assistant."; return nil }
        let selectionToken = UUID(); selectionGeneration = selectionToken; assistantOpenToken = selectionToken
        awaitingAssistant = true; stagedAssistantEvents = []
        defer {
            if assistantOpenToken == selectionToken { awaitingAssistant = false; stagedAssistantEvents = []; assistantOpenToken = nil }
        }
        let token = routeEpoch(for: requestedNode)
        do {
            let result = try await wire.request("assistant/open", params: .object([:]))
            guard routeEpoch(for: requestedNode) == token, selectionGeneration == selectionToken else { return nil }
            guard let identity = identity(result["identity"]), identity.nodeId == requestedNode,
                result["assistant"]["profileId"].string?.isEmpty == false else { throw ConnectionError.changedNode }
            if selected?.identity != identity, noticeRequestAction != nil { notice = nil }
            let item = BoardItem(identity: identity, name: "Pika", machine: machines.first(where: { $0.id == identity.nodeId })?.displayName ?? "Machine", state: "ASSISTANT", detail: "")
            selected = item; question = nil; approval = nil; providerItems = [:]; conversationCapabilities = result["capabilities"]
            ambiguousLiveItems = []; snapshotBackedItems = []; conversationNeedsRefresh = false
            conversationCached = false; historyLoading = false
            historyCursor = result["turns"]["nextCursor"]
            activeTurns[identity] = result["activeTurnId"].string
            let turns = chronologicalTurns(result, provider: identity.provider)
            snapshotBackedItems = activeSnapshotItems(turns, activeTurnId: result["activeTurnId"].string)
            for entry in turns.flatMap({ $0["items"].array }) { if let id = entry["id"].string { providerItems[id] = entry } }
            messages = turns.flatMap { $0["items"].array.compactMap { entry -> ChatMessage? in
                guard let type = entry["type"].string, type == "agentMessage" || type == "userMessage", let id = entry["id"].string else { return nil }
                let text = entry["text"].string ?? entry["content"].array.compactMap { $0["text"].string }.joined(separator: "\n")
                return ChatMessage(id: id, role: type == "userMessage" ? "user" : "assistant", text: text)
            } }
            lastKnownMessages[identity] = Array(messages.suffix(500))
            let staged = stagedAssistantEvents
            awaitingAssistant = false
            for frame in staged { handleEvent(frame) }
            if isFixture { fixtureAssistantOpenCount += 1 }
            notice = nil
            return item
        } catch {
            if routeEpoch(for: requestedNode) == token, selectionGeneration == selectionToken {
                let legacySharedConnection = error.localizedDescription.contains("The assistant is not on its private shared connection yet.")
                if case .sharedConnectionRequired = error as? ConnectionError {
                    reportError(error, action: "")
                } else if legacySharedConnection {
                    notice = "On the assistant's owning machine, exit its terminal normally, then reopen with pika pika. This resumes the same assistant and memory. Reconnecting the phone will not enable its private shared connection."
                    noticeDetails = error.localizedDescription
                } else {
                    reportError(error, action: "The existing Pika assistant is unavailable. Check its owning machine; no replacement was started.")
                }
            }
            return nil
        }
    }
    func controls(_ item: BoardItem) async throws -> JSONValue {
        guard selected?.identity == item.identity, let wire = transport(for: item.identity.nodeId) else { throw ConnectionError.disconnected }
        let token = selectionGeneration
        let connectionToken = routeEpoch(for: item.identity.nodeId)
        let result = try await wire.request("conversation/controls", params: .object(["identity": item.identity.json]))
        guard token == selectionGeneration, routeEpoch(for: item.identity.nodeId) == connectionToken, selected?.identity == item.identity,
            identity(result["identity"]) == item.identity else { throw ConnectionError.changedNode }
        return result
    }
    func selectModel(_ id: String, item: BoardItem) async -> Bool {
        guard selected?.identity == item.identity, let wire = transport(for: item.identity.nodeId) else { return false }
        let token = selectionGeneration
        let connectionToken = routeEpoch(for: item.identity.nodeId)
        do {
            let result = try await wire.request("conversation/model", params: .object(["identity": item.identity.json, "model": .string(id)]))
            guard token == selectionGeneration, routeEpoch(for: item.identity.nodeId) == connectionToken, selected?.identity == item.identity,
                identity(result["identity"]) == item.identity else { throw ConnectionError.changedNode }
            guard result["state"].string == "accepted" else {
                notice = result["state"].string == "rejected" ? "The model change was rejected." : "Model change outcome unknown. Reopen controls to read its current setting; it will not be repeated."
                return false
            }
            return true
        } catch {
            if token == selectionGeneration, routeEpoch(for: item.identity.nodeId) == connectionToken, selected?.identity == item.identity {
                reportError(error, action: "Model change outcome unknown. Reopen controls to check its current setting; it will not be repeated.")
            }
            return false
        }
    }
    func explainUnavailable(_ action: String) { notice = "\(action) is not yet supported by this machine's mobile connection. No conversation was created or changed." }
    func nodes() async throws -> [MobileNode] {
        if !isFixture {
            var merged = Dictionary(uniqueKeysWithValues: machines.map { ($0.id, MobileNode(id: $0.id, name: $0.displayName)) })
            for source in onlineNodes.sorted() {
                guard let transport = wires[source] else { continue }
                let epoch = wireEpochs[source]
                do {
                    let result = try await transport.request("nodes/list", params: .object([:]))
                    guard onlineNodes.contains(source), wireEpochs[source] == epoch, result["items"].hasArrayShape,
                        result["items"].array.count <= 10_000 else { continue }
                    let claims = result["items"].array.compactMap { row -> MobileNode? in
                        guard let id = row["nodeId"].string, !id.isEmpty,
                            let name = row["name"].string, !name.isEmpty else { return nil }
                        return MobileNode(id: id, name: name)
                    }
                    guard claims.count == result["items"].array.count, Set(claims.map(\.id)).count == claims.count else { continue }
                    sourceNodes[source] = claims
                    for claim in claims where merged[claim.id] == nil { merged[claim.id] = claim }
                } catch { /* Saved/offline machines remain selectable; no invented route. */ }
            }
            return merged.values.sorted { $0.name < $1.name }
        }
        guard connected, let wire else { throw ConnectionError.disconnected }
        let result = try await wire.request("nodes/list", params: .object([:]))
        return result["items"].array.compactMap { row in
            guard let id = row["nodeId"].string, let name = row["name"].string else { return nil }
            return MobileNode(id: id, name: name)
        }
    }
    private func selector(_ method: String, node: String?) async throws -> [JSONValue] {
        guard let target = node ?? activeNodeId, let wire = transport(for: target) else { throw ConnectionError.disconnected }
        var rows: [JSONValue] = [], cursor: String?, seen = Set<String>()
        repeat {
            var params: [String: JSONValue] = ["limit": .number(256)]
            if let node { params["nodeId"] = .string(node) }
            if let cursor { params["cursor"] = .string(cursor) }
            let result = try await wire.request(method, params: .object(params))
            rows.append(contentsOf: result["items"].array)
            guard rows.count <= 100_000 else { throw ConnectionError.malformed }
            cursor = result["nextCursor"].string
            if let cursor, !seen.insert(cursor).inserted { throw ConnectionError.malformed }
        } while cursor != nil
        return rows
    }
    func projects(node: String? = nil) async throws -> [MobileProject] {
        return try await selector("projects/list", node: node).compactMap { row in
            guard let id = row["id"].string, let name = row["name"].string, let node = row["nodeId"].string else { return nil }
            return MobileProject(id: id, name: name, nodeId: node)
        }
    }
    func candidates(node: String? = nil) async throws -> [ExistingCandidate] {
        return try await selector("conversation/candidates", node: node).compactMap { row in
            guard let identity = identity(row["identity"]), let name = row["name"].string else { return nil }
            return ExistingCandidate(identity: identity, name: name, project: row["project"].string)
        }
    }
    func remember(project: String, provider: String) {
        choices["project"] = project; choices["provider"] = provider
        if !isFixture { try? store.write(choices, name: "choices.json") }
    }
    var savedAssistantMachine: String? { choices["assistantMachine"].flatMap { node in machines.contains(where: { $0.id == node }) ? node : nil } }
    func rememberAssistantMachine(_ node: String?) {
        var updated = choices
        updated["assistantMachine"] = node
        do { if !isFixture { try store.write(updated, name: "choices.json") }; choices = updated }
        catch { notice = "The assistant machine choice could not be saved." }
    }
    func hasUnresolvedCreation(on node: String) -> Bool {
        creations.values.contains {
            ($0.state == "unknown" || $0.state == "pending")
                && (($0.params["nodeId"].string ?? "").isEmpty || $0.params["nodeId"].string == node)
        }
    }
    func create(name: String, project: MobileProject, provider: String) async -> BoardItem? {
        guard !mutationBusy, capabilities(for: project.nodeId)["create"].bool, let wire = transport(for: project.nodeId),
            !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            !hasUnresolvedCreation(on: project.nodeId) else {
            notice = "Creation is unavailable or an earlier creation needs its original receipt checked first."; return nil
        }
        mutationBusy = true; defer { mutationBusy = false }
        let id = UUID().uuidString
        let params: JSONValue = .object(["clientOperationId": .string(id), "name": .string(name), "projectId": .string(project.id),
            "provider": .string(provider), "nodeId": .string(project.nodeId)])
        creations[id] = CreationRecord(id: id, params: params, state: "pending", identity: nil)
        if !isFixture {
            do { try store.write(creations, name: "creations.json") }
            catch { creations.removeValue(forKey: id); notice = "Not started. The creation intent could not be saved safely."; return nil }
        }
        do {
            let result = try await wire.request("conversation/create", params: params)
            guard result["state"].string == "created", let identity = identity(result["identity"]),
                identity.nodeId == project.nodeId, identity.provider == provider else {
                creations[id]?.state = result["state"].string == "rejected" ? "rejected" : "unknown"
                if !isFixture { try? store.write(creations, name: "creations.json") }
                notice = result["state"].string == "rejected" ? "The machine rejected creation. No conversation was created." : "Creation not confirmed. It will not be repeated automatically."
                noticeDetails = result["message"].string
                return nil
            }
            creations[id]?.state = "created"; creations[id]?.identity = identity
            if !isFixture { try store.write(creations, name: "creations.json") }
            remember(project: project.id, provider: provider)
            return BoardItem(identity: identity, name: name, machine: machines.first(where: { $0.id == identity.nodeId })?.displayName ?? "Machine", state: "STARTING", detail: "")
        } catch ConnectionError.rejected(let message) {
            creations[id]?.state = "rejected"; notice = "The machine rejected creation. No conversation was created."; noticeDetails = message
        } catch { creations[id]?.state = "unknown"; notice = "Creation outcome unknown. No replacement was launched." }
        if !isFixture { try? store.write(creations, name: "creations.json") }
        return nil
    }
    func reconcileCreations() async {
        guard connected else { return }
        for record in creations.values.filter({ $0.state == "unknown" }) {
            guard let node = record.params["nodeId"].string, let wire = transport(for: node) else { continue }
            do {
                let result = try await wire.request("conversation/receipt", params: .object([
                    "nodeId": record.params["nodeId"], "clientOperationId": .string(record.id)]))
                if result["state"].string == "created", result["clientOperationId"].string == record.id,
                    let found = identity(result["identity"]), found.nodeId == record.params["nodeId"].string,
                    found.provider == record.params["provider"].string {
                    creations[record.id]?.state = "created"; creations[record.id]?.identity = found
                    notice = "The original creation is confirmed. No replacement was launched."
                } else if result["state"].string == "rejected" { creations[record.id]?.state = "rejected" }
            } catch { notice = "Creation remains uncertain. No replacement was launched." }
        }
        if !isFixture { try? store.write(creations, name: "creations.json") }
    }
    func adopt(_ candidate: ExistingCandidate) async -> BoardItem? {
        guard !mutationBusy, capabilities(for: candidate.identity.nodeId)["adopt"].bool, let wire = transport(for: candidate.identity.nodeId) else { return nil }
        mutationBusy = true; defer { mutationBusy = false }
        do {
            let result = try await wire.request("conversation/adopt", params: .object(["identity": candidate.identity.json]))
            guard result["state"].string == "added", identity(result["identity"]) == candidate.identity else { throw ConnectionError.malformed }
            return BoardItem(identity: candidate.identity, name: candidate.name, machine: machines.first(where: { $0.id == candidate.identity.nodeId })?.displayName ?? "Machine", state: "UNKNOWN", detail: "")
        } catch { notice = "Addition could not be confirmed. No replacement agent was requested; an uncertain addition is not repeated automatically."; return nil }
    }
}
