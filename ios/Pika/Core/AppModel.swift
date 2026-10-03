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
    @Published var pendingActions: [String: PendingAction] = [:]
    @Published var machines: [SavedMachine] = []
    @Published var activeNodeId: String?
    @Published var isFixture = false
    @Published var choices: [String: String] = [:]
    @Published var creations: [String: CreationRecord] = [:]
    @Published var mutationBusy = false
    @Published var fixtureSendCount = 0
    @Published var fixtureAssistantFinished = false
    @Published var fixtureAssistantOpenCount = 0
    @Published var coverageNote: String?
    private let store = LocalStore()
    private var wire: (any MobileWire)?
    private var connectingWire: SSHWire?
    private var connectionAttempt: ConnectionAttempt?
    private var eventsTask: Task<Void, Never>?
    private var connectionTask: Task<Void, Never>?
    private var verification: CheckedContinuation<Bool, Never>?
    private var generation = UUID()
    private var selectionGeneration = UUID()
    private var activeTurns: [ThreadIdentity: String] = [:]
    private var providerItems: [String: JSONValue] = [:]
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
            observedAt = .now
            listen(fixture)
            return
        }
        #endif
        machines = store.read("machines.json", as: [SavedMachine].self) ?? []
        pendingPairingAvailable = (try? StagedPairing.pending()) != nil
        board = store.read("board.json", as: [BoardItem].self) ?? []
        observedAt = store.read("observed.json", as: Date.self)
        drafts = store.read("drafts.json", as: [String: String].self) ?? [:]
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
        if !isFixture { do { try store.write(drafts, name: "drafts.json") } catch { notice = "Draft could not be saved. Keep this app open until storage is available." } }
    }
    func draft(_ identity: ThreadIdentity) -> String { drafts[identity.draftKey] ?? "" }
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
        eventsTask?.cancel(); connected = false; conversationCapabilities = .null
        if let previous = wire { Task { await previous.close() } }
        wire = nil
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
        eventsTask?.cancel(); connected = false; conversationCapabilities = .null
        observedRequests = [:]
        if let previous = wire { Task { await previous.close() } }
        wire = nil
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
                firstBoardReceived = false
                listen(transport)
                _ = try await transport.request("board/subscribe", params: .object([:]))
                try await waitForFirstBoard(token: token)
                guard generation == token, !Task.isCancelled else { await transport.close(); return }
                let credentialId = saved?.credentialId ?? UUID().uuidString
                let payload = try JSONEncoder().encode(CredentialPayload(secret: secret, passphrase: passphrase))
                try KeychainStore.save(payload, id: credentialId)
                let machine = SavedMachine(id: node, address: address, port: port, username: username,
                    hostKey: transport.verifiedHostKey, credentialId: credentialId, keyAuthentication: key)
                var updated = machines.filter { $0.id != node }; updated.append(machine)
                try store.write(updated, name: "machines.json")
                if let pairingStageId {
                    try? KeychainStore.remove(id: pairingStageId)
                    pendingPairingAvailable = (try? StagedPairing.pending()) != nil
                }
                machines = updated
                wire = transport; connected = true; connecting = false; activeNodeId = node
                reconnectAttempts = 0
                connectingWire = nil
                connectionAttempt = nil
                capabilities = hello["capabilities"]
                if let selected {
                    if selected.state == "ASSISTANT" { _ = await openAssistant() }
                    else { await open(selected) }
                }
            } catch {
                guard generation == token else { return }
                connecting = false; connected = false; reportError(error, action: "Could not connect to this machine. Check the address and login, then try again.")
                if let unfinished = connectingWire { await unfinished.close(); connectingWire = nil }
                if saved != nil {
                    switch error {
                    case ConnectionError.changedHost, ConnectionError.changedNode, ConnectionError.credentials,
                        ConnectionError.secureStorage, ConnectionError.malformed: break
                    default: scheduleReconnect(token: token)
                    }
                }
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
        if let attempt = connectionAttempt { Task { await attempt.cancel() }; connectionAttempt = nil }
    }
    func resume() {
        if !foreground { reconnectAttempts = 0 }
        foreground = true
        guard !isFixture, !connected, !connecting, let saved = machines.last else { return }
        do {
            let credential = try JSONDecoder().decode(CredentialPayload.self, from: KeychainStore.load(id: saved.credentialId))
            beginConnection(address: saved.address, port: saved.port, username: saved.username,
                secret: credential.secret, key: saved.keyAuthentication, passphrase: credential.passphrase, saved: saved)
        } catch { notice = "Saved login unavailable. Add this machine again to authenticate." }
    }
    func retryConnection() { reconnectAttempts = 0; resume() }
    private func scheduleReconnect(token: UUID) {
        guard foreground, !isFixture, reconnectTask == nil, !machines.isEmpty else { return }
        reconnectAttempts = min(6, reconnectAttempts + 1)
        let delay = min(30, 1 << reconnectAttempts)
        reconnectTask = Task {
            defer { if generation == token { reconnectTask = nil } }
            try? await Task.sleep(for: .seconds(delay))
            guard !Task.isCancelled, generation == token, foreground, !connected, !connecting else { return }
            reconnectTask = nil; resume()
        }
    }
    func suspend() {
        foreground = false
        cancelConnection(); connected = false; eventsTask?.cancel()
        conversationCapabilities = .null; conversationCached = !messages.isEmpty
        boardStaging = []; boardPage = 0; boardRevision = .null
        if let wire { Task { await wire.close() } }
        wire = nil
    }
    private func listen(_ transport: any MobileWire) {
        eventsTask?.cancel()
        let token = generation
        eventsTask = Task {
            for await frame in transport.events {
                guard !Task.isCancelled, generation == token else { return }
                handleEvent(frame)
            }
            guard !Task.isCancelled, generation == token else { return }
            boardReady?.resume(throwing: ConnectionError.disconnected); boardReady = nil
            connected = false; conversationCapabilities = .null
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
            if params["nodeId"] == .null || params["nodeId"].string == selected?.identity.nodeId {
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
                boardReady?.resume(throwing: ConnectionError.malformed); boardReady = nil
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
            firstBoardReceived = true; boardReady?.resume(); boardReady = nil
            observedAt = params["observedAt"].number.map { Date(timeIntervalSince1970: $0) }
            coverageNote = params["coverage"]["partial"].bool ? "Partial board · some work is not included in this snapshot" : nil
            if !isFixture { try? store.write(board, name: "board.json"); try? store.write(observedAt, name: "observed.json") }
            if !params["health"].array.isEmpty { notice = "Some machine information is unavailable. Last-known content is preserved."; noticeDetails = params["health"].array.compactMap(\.string).joined(separator: "; ") }
        } else if frame["event"].string == "conversation/event", awaitingAssistant, assistantOpenToken == selectionGeneration {
            if stagedAssistantEvents.count < 128 { stagedAssistantEvents.append(frame) }
            else { notice = "Assistant events exceeded the safe handoff limit. Reopen the existing assistant." }
        } else if frame["event"].string == "conversation/event", identity(params["identity"]) == selected?.identity {
            let method = params["method"].string ?? ""
            let event = params["params"]
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
                if let index = messages.firstIndex(where: { $0.id == id }) {
                    let old = messages[index]; messages[index] = ChatMessage(id: old.id, role: old.role, text: old.text + text)
                } else { messages.append(ChatMessage(id: id, role: "assistant", text: text)) }
            } else if method == "item/completed", let type = event["item"]["type"].string,
                type == "userMessage" || type == "agentMessage", let id = event["item"]["id"].string {
                let entry = event["item"]
                let text = entry["text"].string ?? entry["content"].array.compactMap { $0["text"].string }.joined(separator: "\n")
                let message = ChatMessage(id: id, role: type == "userMessage" ? "user" : "assistant", text: text)
                if let index = messages.firstIndex(where: { $0.id == id }) { messages[index] = message }
                else { messages.append(message) }
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
        if selected?.identity != item.identity, noticeRequestAction != nil { notice = nil }
        if let previous = selected { lastKnownMessages[previous.identity] = Array(messages.suffix(500)) }
        selected = item; messages = lastKnownMessages[item.identity] ?? []; question = nil; approval = nil; providerItems = [:]; conversationCapabilities = .null
        conversationCached = !messages.isEmpty
        if lastKnownMessages.count > 20, let eviction = lastKnownMessages.keys.first(where: { $0 != item.identity }) { lastKnownMessages.removeValue(forKey: eviction) }
        let token = UUID(); selectionGeneration = token
        let connectionToken = generation
        guard connected, let wire else { notice = "Reconnect to read this exact conversation. Your draft is saved."; return }
        do {
            let result = try await wire.request("conversation/open", params: .object(["identity": item.identity.json]))
            guard generation == connectionToken, selectionGeneration == token, selected?.identity == item.identity else { return }
            guard identity(result["identity"]) == item.identity else { throw ConnectionError.changedNode }
            conversationCapabilities = result["capabilities"]
            activeTurns[item.identity] = result["activeTurnId"].string
            historyCursor = result["turns"]["nextCursor"]
            let turns = result["turns"]["data"].array.isEmpty ? result["thread"]["turns"].array : result["turns"]["data"].array
            for entry in turns.flatMap({ $0["items"].array }) { if let id = entry["id"].string { providerItems[id] = entry } }
            if let current = approval, let id = current.params["itemId"].string, let original = providerItems[id] {
                approval = ProviderApproval(id: current.id, identity: current.identity, requestId: current.requestId,
                    method: current.method, params: current.params, item: original)
            }
            messages = turns.flatMap { turn in turn["items"].array.compactMap { entry -> ChatMessage? in
                guard let type = entry["type"].string, type == "agentMessage" || type == "userMessage" else { return nil }
                let text = entry["text"].string ?? entry["content"].array.compactMap { $0["text"].string }.joined(separator: "\n")
                return ChatMessage(id: entry["id"].string ?? UUID().uuidString, role: type == "userMessage" ? "user" : "assistant", text: text)
            } }
            lastKnownMessages[item.identity] = Array(messages.suffix(500))
            conversationCached = false
        } catch { if generation == connectionToken, selectionGeneration == token { reportError(error, action: "Could not read this conversation. Reconnect to check its original state.") } }
    }
    func loadOlder(_ item: BoardItem) async {
        guard connected, selected?.identity == item.identity, historyCursor != .null, !historyLoading, let wire else { return }
        let token = selectionGeneration, cursor = historyCursor
        let connectionToken = generation
        historyLoading = true
        defer { historyLoading = false }
        do {
            let result = try await wire.request("conversation/history", params: .object(["identity": item.identity.json, "cursor": cursor]))
            guard generation == connectionToken, selectionGeneration == token, selected?.identity == item.identity else { return }
            guard identity(result["identity"]) == item.identity else { throw ConnectionError.changedNode }
            let previous = result["turns"]["data"].array.flatMap { $0["items"].array.compactMap { entry -> ChatMessage? in
                guard let id = entry["id"].string, let type = entry["type"].string, ["userMessage", "agentMessage"].contains(type) else { return nil }
                return ChatMessage(id: id, role: type == "userMessage" ? "user" : "assistant",
                    text: entry["text"].string ?? entry["content"].array.compactMap { $0["text"].string }.joined(separator: "\n"))
            } }
            let ids = Set(messages.map(\.id))
            messages = previous.filter { !ids.contains($0.id) } + messages
            historyCursor = result["turns"]["nextCursor"]
        } catch { if generation == connectionToken, selectionGeneration == token { reportError(error, action: "Older context is unavailable. Try again after reconnecting.") } }
    }
    func canSend(_ item: BoardItem, composing: Bool) -> Bool {
        connected && selected?.identity == item.identity && conversationCapabilities["send"].bool && !composing && !draft(item.identity).trimmingCharacters(in: .whitespacesAndNewlines).isEmpty &&
            !pendingActions.values.contains { $0.identity == item.identity && !$0.id.hasPrefix("answer:") && !$0.id.hasPrefix("approval:") && ($0.state == "pending" || $0.state == "unknown") }
    }
    func send(_ item: BoardItem, composing: Bool) async {
        guard canSend(item, composing: composing), let wire else { return }
        let text = draft(item.identity), id = UUID().uuidString
        pendingActions[id] = PendingAction(id: id, identity: item.identity, text: text, state: "pending")
        if !isFixture {
            do { try store.write(pendingActions, name: "pending.json") }
            catch { pendingActions.removeValue(forKey: id); notice = "Not sent. Your outgoing text could not be saved safely."; return }
        }
        let token = generation
        do {
            var params: [String: JSONValue] = ["identity": item.identity.json,
                "clientMessageId": .string(id), "text": .string(text)]
            if let turn = activeTurns[item.identity] { params["expectedTurnId"] = .string(turn) }
            let result = try await wire.request("conversation/send", params: .object(params))
            guard identity(result["identity"]) == item.identity, result["clientMessageId"].string == id else { throw ConnectionError.malformed }
            let receipt = result["state"].string ?? "unknown"
            let state = ["accepted", "delivered"].contains(receipt) ? "accepted" : (receipt == "rejected" ? "rejected" : "unknown")
            pendingActions[id]?.state = state
            if state == "accepted", draft(item.identity) == text { setDraft("", identity: item.identity) }
            if generation == token, selected?.identity == item.identity {
                notice = state == "accepted" ? (isFixture ? "Accepted by the UI fixture only." : "Accepted by the existing provider conversation.") :
                    (state == "rejected" ? "The provider rejected this message. Your draft is preserved." : "Outcome unknown. This message will not be repeated automatically.")
            }
        } catch ConnectionError.rejected(let message) {
            pendingActions[id]?.state = "rejected"
            if generation == token, selected?.identity == item.identity { notice = "The machine rejected this message. Your draft is preserved."; noticeDetails = message }
        } catch {
            pendingActions[id]?.state = "unknown"
            if generation == token, selected?.identity == item.identity { notice = "Outcome unknown. Your text is saved; it was not repeated." }
        }
        if !isFixture {
            do { try store.write(pendingActions, name: "pending.json") }
            catch { notice = "The machine's outcome could not be saved locally. Reconnect to check the durable receipt before any resend." }
        }
    }
    func reconcile(_ item: BoardItem) async {
        guard connected, let wire else { return }
        let unknown = pendingActions.values.filter { $0.identity == item.identity && $0.state == "unknown" && !$0.id.hasPrefix("answer:") && !$0.id.hasPrefix("approval:") }
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
            self.question?.requestId == question.requestId, conversationCapabilities["answer"].bool, let item = selected, let wire else { return }
        let actionId = "answer:" + item.identity.draftKey + ":" + question.id
        let token = generation
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
            if generation == token, selected?.identity == identity, noticeRequestAction == actionId {
                requestNotice(pendingActions[actionId]?.state == "resolved" ? requestClosedNotice : result["state"].string == "submitted" ? "Answer submitted. Waiting for the original request to resolve." : result["state"].string == "rejected" ? "The machine rejected this answer. It was not repeated." : "Answer outcome unknown. It was not repeated.", action: actionId)
            }
        } catch ConnectionError.rejected(let details) {
            if pendingActions[actionId]?.state != "resolved" { pendingActions[actionId]?.state = "rejected" }
            if generation == token, selected?.identity == identity, noticeRequestAction == actionId { requestNotice(pendingActions[actionId]?.state == "resolved" ? requestClosedNotice : "The machine rejected this answer before dispatch. Nothing was submitted.", action: actionId); noticeDetails = details }
        } catch {
            if pendingActions[actionId]?.state != "resolved" { pendingActions[actionId]?.state = "unknown" }
            if generation == token, selected?.identity == identity, noticeRequestAction == actionId { requestNotice(pendingActions[actionId]?.state == "resolved" ? requestClosedNotice : "Answer outcome unknown. It was not repeated.", action: actionId) }
        }
        if !isFixture { try? store.write(pendingActions, name: "pending.json") }
    }
    func approve(_ request: ProviderApproval, decision: String) async {
        guard connected, selected?.identity == request.identity, approval?.id == request.id,
            approval?.requestId == request.requestId, ["accept", "decline"].contains(decision), let wire else { return }
        let id = "approval:" + request.identity.draftKey + ":" + request.id
        let token = generation
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
            if generation == token, selected?.identity == request.identity, noticeRequestAction == id {
                requestNotice(pendingActions[id]?.state == "resolved" ? requestClosedNotice : result["state"].string == "submitted" ? "Decision submitted. Waiting for the original request to close." : result["state"].string == "rejected" ? "The machine rejected this decision. It was not repeated." : "Decision outcome unknown. It was not repeated.", action: id)
            }
        } catch ConnectionError.rejected(let details) {
            if pendingActions[id]?.state != "resolved" { pendingActions[id]?.state = "rejected" }
            if generation == token, selected?.identity == request.identity, noticeRequestAction == id { requestNotice(pendingActions[id]?.state == "resolved" ? requestClosedNotice : "The machine rejected this decision before dispatch. Nothing was submitted.", action: id); noticeDetails = details }
        } catch {
            if pendingActions[id]?.state != "resolved" { pendingActions[id]?.state = "unknown" }
            if generation == token, selected?.identity == request.identity, noticeRequestAction == id { requestNotice(pendingActions[id]?.state == "resolved" ? requestClosedNotice : "Decision outcome unknown. It was not repeated.", action: id); noticeDetails = error.localizedDescription }
        }
        if !isFixture { try? store.write(pendingActions, name: "pending.json") }
    }
    func reconcileRequests(_ item: BoardItem) async {
        guard connected, selected?.identity == item.identity, let wire else { return }
        let token = generation
        let actions = pendingActions.values.filter { $0.identity == item.identity && $0.requestId != nil && ["pending", "unknown"].contains($0.state) }
        var ownsNotice = true
        for action in actions {
            guard let request = action.requestId, let turn = action.turnId, let entry = action.itemId else { continue }
            if ownsNotice, generation == token, selected?.identity == item.identity { requestNotice("Checking the original request…", action: action.id) }
            do {
                let result = try await wire.request("conversation/requestStatus", params: .object([
                    "identity": item.identity.json, "requestId": request, "turnId": .string(turn), "itemId": .string(entry)]))
                guard identity(result["identity"]) == item.identity, result["requestId"] == request,
                    result["turnId"].string == turn, result["itemId"].string == entry else { throw ConnectionError.malformed }
                if result["state"].string == "resolved" {
                    pendingActions[action.id]?.state = "resolved"
                    if generation == token, selected?.identity == item.identity {
                        if question?.requestId == request, question?.turnId == turn, question?.itemId == entry { question = nil }
                        if approval?.identity == item.identity, approval?.requestId == request,
                            approval?.params["turnId"].string == turn, approval?.params["itemId"].string == entry { approval = nil }
                    }
                }
                if ownsNotice, generation == token, selected?.identity == item.identity, noticeRequestAction == action.id {
                    requestNotice(pendingActions[action.id]?.state == "resolved" ? requestClosedNotice :
                        "The decision remains pending or uncertain. It will not be repeated and does not block unrelated replies.", action: action.id)
                } else { ownsNotice = false }
            } catch {
                if ownsNotice, generation == token, selected?.identity == item.identity, noticeRequestAction == action.id {
                    requestNotice(pendingActions[action.id]?.state == "resolved" ? requestClosedNotice : "The original request status is unknown. Nothing was repeated.", action: action.id)
                } else { ownsNotice = false }
            }
        }
        if !isFixture { try? store.write(pendingActions, name: "pending.json") }
    }
    func openAssistant() async -> BoardItem? {
        guard let wire, connected else { notice = "Reconnect to the machine that owns your Pika assistant."; return nil }
        let selectionToken = UUID(); selectionGeneration = selectionToken; assistantOpenToken = selectionToken
        awaitingAssistant = true; stagedAssistantEvents = []
        defer {
            if assistantOpenToken == selectionToken { awaitingAssistant = false; stagedAssistantEvents = []; assistantOpenToken = nil }
        }
        let token = generation
        do {
            let result = try await wire.request("assistant/open", params: .object([:]))
            guard generation == token, selectionGeneration == selectionToken else { return nil }
            guard let identity = identity(result["identity"]), identity.nodeId == activeNodeId,
                result["assistant"]["profileId"].string?.isEmpty == false else { throw ConnectionError.changedNode }
            if selected?.identity != identity, noticeRequestAction != nil { notice = nil }
            let item = BoardItem(identity: identity, name: "Pika", machine: machines.first(where: { $0.id == identity.nodeId })?.address ?? "Machine", state: "ASSISTANT", detail: "")
            selected = item; question = nil; approval = nil; providerItems = [:]; conversationCapabilities = result["capabilities"]
            conversationCached = false; historyLoading = false
            historyCursor = result["turns"]["nextCursor"]
            activeTurns[identity] = result["activeTurnId"].string
            let turns = result["turns"]["data"].array.isEmpty ? result["thread"]["turns"].array : result["turns"]["data"].array
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
        } catch { if generation == token, selectionGeneration == selectionToken { reportError(error, action: "The existing Pika assistant is unavailable. Reconnect to its machine and try again.") }; return nil }
    }
    func explainUnavailable(_ action: String) { notice = "\(action) is not yet supported by this machine's mobile connection. No conversation was created or changed." }
    func nodes() async throws -> [MobileNode] {
        guard connected, let wire else { throw ConnectionError.disconnected }
        let result = try await wire.request("nodes/list", params: .object([:]))
        return result["items"].array.compactMap { row in
            guard let id = row["nodeId"].string, let name = row["name"].string else { return nil }
            return MobileNode(id: id, name: name)
        }
    }
    private func selector(_ method: String, node: String?) async throws -> [JSONValue] {
        guard connected, let wire else { throw ConnectionError.disconnected }
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
        choices = ["project": project, "provider": provider]
        if !isFixture { try? store.write(choices, name: "choices.json") }
    }
    func create(name: String, project: MobileProject, provider: String) async -> BoardItem? {
        guard !mutationBusy, connected, capabilities["create"].bool, let wire,
            !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            !creations.values.contains(where: { $0.state == "unknown" || $0.state == "pending" }) else {
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
            return BoardItem(identity: identity, name: name, machine: machines.first(where: { $0.id == identity.nodeId })?.address ?? "Machine", state: "STARTING", detail: "")
        } catch ConnectionError.rejected(let message) {
            creations[id]?.state = "rejected"; notice = "The machine rejected creation. No conversation was created."; noticeDetails = message
        } catch { creations[id]?.state = "unknown"; notice = "Creation outcome unknown. No replacement was launched." }
        if !isFixture { try? store.write(creations, name: "creations.json") }
        return nil
    }
    func reconcileCreations() async {
        guard connected, let wire else { return }
        for record in creations.values.filter({ $0.state == "unknown" }) {
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
        guard !mutationBusy, connected, capabilities["adopt"].bool, let wire else { return nil }
        mutationBusy = true; defer { mutationBusy = false }
        do {
            let result = try await wire.request("conversation/adopt", params: .object(["identity": candidate.identity.json]))
            guard result["state"].string == "added", identity(result["identity"]) == candidate.identity else { throw ConnectionError.malformed }
            return BoardItem(identity: candidate.identity, name: candidate.name, machine: machines.first(where: { $0.id == candidate.identity.nodeId })?.address ?? "Machine", state: "UNKNOWN", detail: "")
        } catch { notice = "Addition could not be confirmed. No replacement agent was requested; an uncertain addition is not repeated automatically."; return nil }
    }
}
