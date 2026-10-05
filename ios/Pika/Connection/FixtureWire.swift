#if DEBUG
import Foundation

/// Explicitly labelled in-app endpoint double. Not a provider/network E2E proof.
actor FixtureWire: MobileWire {
    static let items = [
        BoardItem(identity: ThreadIdentity(nodeId: "fixture-node", provider: ProcessInfo.processInfo.arguments.contains("--fixture-channel-receipt") ? "claude" : "codex", threadId: "fixture-one"), name: "master_quant", machine: "Fixture Alpha", state: "NEEDS YOU", detail: ""),
        BoardItem(identity: ThreadIdentity(nodeId: "fixture-node-two", provider: "codex", threadId: "fixture-two"), name: "master_quant", machine: "Fixture Beta", state: "WORKING", detail: "")
    ]
    nonisolated let events: AsyncStream<JSONValue>
    private let sink: AsyncStream<JSONValue>.Continuation
    private var selected: ThreadIdentity?
    private var closed = false
    private var sent: Set<String> = []
    private var sendAttempts = 0
    private var approvalAttempts = 0
    private var receiptChecks = 0
    private var creations: [String: JSONValue] = [:]
    private var threadModels: [ThreadIdentity: String] = [:]
    private var replies: [ThreadIdentity: [JSONValue]] = [:]
    init() {
        var continuation: AsyncStream<JSONValue>.Continuation!
        events = AsyncStream { continuation = $0 }; sink = continuation
    }
    func request(_ method: String, params: JSONValue) async throws -> JSONValue {
        guard !closed else { throw ConnectionError.disconnected }
        switch method {
        case "conversation/controls":
            guard let selected, params["identity"] == selected.json else { throw ConnectionError.changedNode }
            if ProcessInfo.processInfo.arguments.contains("--fixture-controls-unsupported") { throw ConnectionError.remote("Unsupported mobile method") }
            return .object(["identity": selected.json, "currentModel": .string(threadModels[selected] ?? "fixture-current"),
                "models": .object(["data": .array(["fixture-current", "fixture-alternative"].map { .object(["model": .string($0), "displayName": .string($0)]) })]),
                "skills": .object(["data": .array([.object(["cwd": .string("/fixture"), "errors": .array([]), "skills": .array([
                    .object(["name": .string("fixture-review"), "description": .string("Disposable review skill"), "path": .string("/fixture/skills/review/SKILL.md"), "enabled": .bool(true)])])])])])])
        case "conversation/model":
            guard let selected, params["identity"] == selected.json, let value = params["model"].string,
                ["fixture-current", "fixture-alternative"].contains(value) else { throw ConnectionError.changedNode }
            if ProcessInfo.processInfo.arguments.contains("--fixture-model-unknown") { return .object(["identity": selected.json, "state": .string("unknown")]) }
            threadModels[selected] = value
            return .object(["identity": selected.json, "state": .string("accepted"), "model": .string(value)])
        case "nodes/list":
            return .object(["items": .array([.object(["nodeId": .string("fixture-node"), "name": .string("Fixture Alpha")]),
                .object(["nodeId": .string("fixture-node-two"), "name": .string("Fixture Beta")])])])
        case "projects/list":
            let node = params["nodeId"].string ?? "fixture-node"
            let page2 = params["cursor"].string != nil
            return .object(["items": .array([.object(["id": .string(page2 ? "/fixture/selected-project" : "/fixture/first-project"),
                "name": .string(page2 ? "Second page project" : "First page project"), "nodeId": .string(node)])]),
                "nextCursor": page2 ? .null : .string("fixture-project-page2")])
        case "conversation/create":
            guard let id = params["clientOperationId"].string else { throw ConnectionError.malformed }
            let owner = ThreadIdentity(nodeId: params["nodeId"].string ?? "", provider: "codex", threadId: "fixture-created-" + id)
            creations[id] = .object(["state": .string("created"), "clientOperationId": .string(id), "identity": owner.json])
            return .object(["state": .string("unknown"), "clientOperationId": .string(id)])
        case "conversation/open", "assistant/open":
            let assistant = method == "assistant/open"
            let decoded: ThreadIdentity
            if assistant { decoded = ThreadIdentity(nodeId: "fixture-node", provider: "codex", threadId: "fixture-assistant") }
            else { decoded = try JSONDecoder().decode(ThreadIdentity.self, from: JSONEncoder().encode(params["identity"])) }
            guard assistant || Self.items.contains(where: { $0.identity == decoded }) else { throw ConnectionError.changedNode }
            selected = decoded
            if ProcessInfo.processInfo.arguments.contains("--fixture-delta-before-open") || ProcessInfo.processInfo.arguments.contains("--fixture-delta-after-open") {
                if ProcessInfo.processInfo.arguments.contains("--fixture-delta-before-open") {
                    emit(decoded, "item/agentMessage/delta", .object(["itemId": .string("snapshot-item"), "delta": .string(" world")]))
                    try await Task.sleep(for: .milliseconds(400))
                }
                Task {
                    try? await Task.sleep(for: .milliseconds(300))
                    emit(decoded, "item/agentMessage/delta", .object(["itemId": .string("snapshot-item"), "delta": .string(" world")]))
                    try? await Task.sleep(for: .seconds(8))
                    emit(decoded, "item/completed", .object(["item": .object(["id": .string("snapshot-item"), "type": .string("agentMessage"), "text": .string("Hello world · complete original reply")])]))
                }
                return .object(["identity": decoded.json, "activeTurnId": .string("snapshot-turn"), "capabilities": .object(["read": .bool(true), "send": .bool(true)]),
                    "turns": .object(["order": .string("chronological"), "nextCursor": .null, "data": .array([.object(["id": .string("snapshot-turn"), "status": .string("inProgress"), "items": .array([.object(["id": .string("snapshot-item"), "type": .string("agentMessage"), "text": .string("Hello world")])])])])])])
            }
            if ProcessInfo.processInfo.arguments.contains("--fixture-turn-pages") {
                let turns = (20..<30).map { fixtureTurn($0) } + (replies[decoded] ?? []).map { .object(["items": .array([$0])]) }
                if ProcessInfo.processInfo.arguments.contains("--fixture-open-race") {
                    emit(decoded, "item/completed", .object(["item": .object(["id": .string("open-live"), "type": .string("agentMessage"), "text": .string("Live output while original history opens")])]))
                    try await Task.sleep(for: .milliseconds(400))
                }
                let legacy = ProcessInfo.processInfo.arguments.contains("--fixture-legacy-order")
                return .object(["identity": decoded.json, "capabilities": .object(["read": .bool(true), "send": .bool(true)]),
                    "turns": .object(["order": legacy ? .null : .string("chronological"), "data": .array(legacy ? Array(turns.reversed()) : turns), "nextCursor": .string("page-10")])])
            }
            if assistant, ProcessInfo.processInfo.arguments.contains("--fixture-slow-assistant") {
                try await Task.sleep(for: .seconds(4))
                sink.yield(.object(["v": .number(1), "event": .string("fixture/assistantFinished"), "params": .object([:])]))
            }
            var replayItems: [JSONValue] = []
            if ProcessInfo.processInfo.arguments.contains("--fixture-approval") {
                let file = assistant || decoded.threadId == "fixture-two"
                let original: JSONValue = .object(["id": .string("fixture-approval-item"), "type": .string(file ? "fileChange" : "commandExecution"),
                    "changes": file ? .array([.object(["path": .string("/fixture/notes.txt"), "kind": .object(["type": .string("update")]), "diff": .string("-old\n+reviewed fixture")])]) : .array([])])
                if file { replayItems = [original] }
                else { emit(decoded, "item/started", .object(["item": original])) }
                emit(decoded, file ? "item/fileChange/requestApproval" : "item/commandExecution/requestApproval",
                    .object(["turnId": .string("fixture-turn"), "itemId": .string("fixture-approval-item"),
                        "command": file ? .null : .string("printf 'disposable fixture'"), "reason": .string("Explicit UI fixture approval"),
                        "cwd": .string("/fixture"), "availableDecisions": .array([.string("accept"), .string("decline")])]), requestId: .string("fixture-request"))
            }
            let richText = """
            ## Ready for your review

            The connection is **working**. Your conversation stays on the *original machine*.

            - Open the existing thread
              - Keep its history intact
            - Send a reply from your phone

            > A small interface, the same conversation.

            | Machine | Status |
            | --- | --- |
            | Test host | Connected |

            See the [guide](https://example.com) and use `pika` to return.

            ```sh
            printf 'hello from Pika\\n'
            echo 'ready'
            ```
            """
            let entry: JSONValue = .object(["id": .string("fixture-context"), "type": .string("agentMessage"),
                "text": .string(ProcessInfo.processInfo.arguments.contains("--fixture-markdown") ? richText : "This is disposable UI fixture context for \(Self.items.first(where: { $0.identity == decoded })?.machine ?? "Fixture Assistant"). No real provider is attached.")])
            let history: [JSONValue] = ProcessInfo.processInfo.arguments.contains("--fixture-long-history") ? (0..<30).map { index in
                .object(["id": .string("long-\(index)"), "type": .string("agentMessage"), "text": .string("Original fixture context \(index)\nA sufficiently long original message to exercise native reading and history anchors.")])
            } : []
            return .object(["identity": decoded.json, "capabilities": .object(["read": .bool(true), "send": .bool(!ProcessInfo.processInfo.arguments.contains("--fixture-read-only")), "answer": .bool(true),
                "readOnlyReason": .string("This original provider supports verified history only; mobile replies are not available."),
                "experimentalNotice": ProcessInfo.processInfo.arguments.contains("--fixture-experimental") ? .string("Experimental connection. Native permissions still apply; an uncertain message is never sent twice.") : .null]),
                "assistant": assistant ? .object(["profileId": .string("fixture-profile"), "scope": .string("private"), "memoryEpoch": .string("fixture-epoch")]) : .null,
                "turns": .object(["order": .string("chronological"), "data": .array([.object(["items": .array([entry] + history + replayItems)])]), "nextCursor": .string("fixture-older")])])
        case "conversation/requestStatus":
            if ProcessInfo.processInfo.arguments.contains("--fixture-status-race"), let selected {
                emit(selected, "serverRequest/resolved", .object(["requestId": params["requestId"], "turnId": params["turnId"], "itemId": params["itemId"]]))
                try await Task.sleep(for: .milliseconds(100))
            }
            return .object(["identity": params["identity"], "requestId": params["requestId"], "turnId": params["turnId"], "itemId": params["itemId"], "state": .string("unknown")])
        case "conversation/history":
            if ProcessInfo.processInfo.arguments.contains("--fixture-turn-pages") {
                if ProcessInfo.processInfo.arguments.contains("--fixture-history-live-append"), let selected {
                    emit(selected, "item/completed", .object(["item": .object(["id": .string("history-live-append"), "type": .string("agentMessage"),
                        "text": .string("Live output appended while older context loads.\n\nNew output below must not move the original reading anchor above.")])]))
                    try await Task.sleep(for: .milliseconds(400))
                }
                let start = params["cursor"].string == "page-10" ? 10 : 0
                let legacy = ProcessInfo.processInfo.arguments.contains("--fixture-legacy-order")
                let turns = (start...start + 10).map { fixtureTurn($0) }
                return .object(["identity": params["identity"], "turns": .object(["order": legacy ? .null : .string("chronological"),
                    "data": .array(legacy ? Array(turns.reversed()) : turns), "nextCursor": start == 10 ? .string("page-0") : .null])])
            }
            return .object(["identity": params["identity"], "turns": .object(["nextCursor": .null,
                "data": .array([.object(["items": .array([.object(["id": .string("fixture-old"), "type": .string("agentMessage"), "text": .string("Older exact fixture history")])])])])])])
        case "conversation/approve":
            approvalAttempts += 1
            guard approvalAttempts == 1 else { throw ConnectionError.malformed }
            if ProcessInfo.processInfo.arguments.contains("--fixture-fast-resolution"), let selected {
                emit(selected, "serverRequest/resolved", .object(["requestId": .string("fixture-request")]))
                try await Task.sleep(for: .milliseconds(100))
            }
            return .object(["state": .string("submitted")])
        case "conversation/receipt":
            if let id = params["clientOperationId"].string { return creations[id] ?? .object(["state": .string("unknown")]) }
            if ProcessInfo.processInfo.arguments.contains("--fixture-channel-receipt") {
                return .object(["identity": params["identity"], "clientMessageId": params["clientMessageId"], "state": .string("delivered")])
            }
            receiptChecks += 1
            return .object(["identity": params["identity"], "clientMessageId": params["clientMessageId"],
                "state": .string(receiptChecks == 1 ? "unknown" : "delivered"),
                "nextCursor": receiptChecks == 1 ? .string("fixture-receipt-page2") : .null])
        case "conversation/send":
            sendAttempts += 1
            if ProcessInfo.processInfo.arguments.contains("--fixture-live-header") {
                sink.yield(.object(["v": .number(1), "event": .string("board/snapshot"), "params": .object([
                    "items": .array(Self.items.map { row in .object(["identity": row.identity.json,
                        "name": .string(row.identity == selected ? "Updated original thread" : row.name),
                        "machine": .string(row.machine), "status": .string(row.identity == selected ? "READY" : row.state)]) }),
                    "observedAt": .number(Date().timeIntervalSince1970), "health": .array([])])]))
            }
            sink.yield(.object(["v": .number(1), "event": .string("fixture/sendCount"), "params": .object(["count": .number(Double(sendAttempts))])]))
            guard let selected, params["identity"] == selected.json, let id = params["clientMessageId"].string,
                let text = params["text"].string, !sent.contains(id) else { throw ConnectionError.malformed }
            sent.insert(id)
            replies[selected, default: []].append(.object(["id": .string(id), "type": .string("userMessage"), "text": .string(text)]))
            sink.yield(.object(["v": .number(1), "event": .string("conversation/event"), "params": .object([
                "identity": selected.json, "method": .string("item/completed"), "params": .object([
                    "item": .object(["id": .string(id), "type": .string("userMessage"), "content": .array([.object(["type": .string("text"), "text": .string(text)])])])])])]))
            if ProcessInfo.processInfo.arguments.contains("--fixture-turn-pages") {
                let reply: JSONValue = .object(["id": .string("reply-" + id), "type": .string("agentMessage"), "text": .string("Latest persisted fixture reply")])
                replies[selected, default: []].append(reply)
                emit(selected, "item/completed", .object(["item": reply]))
            }
            if ProcessInfo.processInfo.arguments.contains("--fixture-channel-receipt") {
                Task {
                    try? await Task.sleep(for: .seconds(2))
                    emit(selected, "item/completed", .object(["item": .object([
                        "id": .string("claude-channel-reply:toolu_fixture"), "type": .string("agentMessage"),
                        "text": .string("Fixture channel reply — not a native provider proof")])]))
                }
                return .object(["identity": selected.json, "clientMessageId": .string(id), "state": .string("unknown")])
            }
            if ProcessInfo.processInfo.arguments.contains("--fixture-unknown-outcome") { throw ConnectionError.timeout }
            if ProcessInfo.processInfo.arguments.contains("--fixture-long-history") {
                emit(selected, "item/completed", .object(["item": .object(["id": .string("live-" + id), "type": .string("agentMessage"), "text": .string("Live fixture output")])]))
            }
            if ProcessInfo.processInfo.arguments.contains("--fixture-streaming-reply") {
                let attempt = sendAttempts
                Task {
                    for index in 1...12 {
                        try? await Task.sleep(for: .seconds(1))
                        guard !closed, self.selected == selected else { return }
                        emit(selected, "item/agentMessage/delta", .object([
                            "itemId": .string("stream-" + id),
                            "delta": .string("\n\nReply \(attempt) paragraph \(index). This is delayed disposable output that grows beyond the visible conversation area.")]))
                    }
                }
            }
            return .object(["identity": selected.json, "clientMessageId": .string(id), "state": .string("accepted")])
        default: throw ConnectionError.remote("This feature is unavailable in the disposable UI fixture; no real conversation was created or changed.")
        }
    }
    private func emit(_ identity: ThreadIdentity, _ method: String, _ params: JSONValue, requestId: JSONValue = .null) {
        sink.yield(.object(["v": .number(1), "event": .string("conversation/event"),
            "params": .object(["identity": identity.json, "method": .string(method), "params": params, "requestId": requestId])]))
    }
    private func fixtureTurn(_ index: Int) -> JSONValue {
        .object(["id": .string("turn-\(index)"), "items": .array([
            .object(["id": .string("turn-user-\(index)"), "type": .string("userMessage"), "text": .string("Original user turn \(index)")]),
            .object(["id": .string("turn-agent-\(index)"), "type": .string("agentMessage"), "text": .string("Original assistant turn \(index). Original context remains on its exact machine.\n\nThis disposable turn is long enough to require scrolling through native history.")])])])
    }
    func close() async { closed = true; sink.finish() }
}
#endif
