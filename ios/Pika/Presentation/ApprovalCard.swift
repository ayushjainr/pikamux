import SwiftUI

/// Displays an original provider request. Routing and pending-request lifetime
/// stay in AppModel; the closure must retain the exact view/thread/request tuple.
struct ApprovalCard: View {
    let method: String
    let params: JSONValue
    let item: JSONValue
    /// True only after a decision was dispatched and is awaiting resolution.
    let pending: Bool
    let onDecision: (String) -> Void

    private var commandRequest: Bool { method == "item/commandExecution/requestApproval" }
    private var changes: [JSONValue] { item["changes"].array }
    private var available: [String] { params["availableDecisions"].array.compactMap(\.string) }
    private func offered(_ decision: String) -> Bool {
        params["availableDecisions"] == .null || available.contains(decision)
    }
    private var hasExactDetail: Bool {
        if commandRequest {
            return (params["kind"] == .null || params["kind"].string == "command") &&
                !(params["command"].string ?? "").isEmpty
        }
        return method == "item/fileChange/requestApproval" &&
            item["id"] == params["itemId"] && !changes.isEmpty &&
            changes.allSatisfy { change in
                change["path"].string != nil && change["diff"].string != nil &&
                    ["add", "delete", "update"].contains(change["kind"]["type"].string ?? "")
            } &&
            // This field can request session-wide write access, not one action.
            // Do not mislabel it as a one-time grant.
            params["grantRoot"] == .null
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label(commandRequest ? "Run this command?" : "Apply these changes?", systemImage: "hand.raised")
                .font(.headline)
            if let reason = params["reason"].string { Text(reason).textSelection(.enabled) }
            if commandRequest, let command = params["command"].string {
                Text(command).font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(12)
                    .background(PikaTheme.background, in: RoundedRectangle(cornerRadius: 10))
                if let cwd = params["cwd"].string { Text("In \(cwd)").font(.caption).foregroundStyle(.secondary) }
            }
            ForEach(Array(changes.enumerated()), id: \.offset) { _, change in
                VStack(alignment: .leading, spacing: 6) {
                    Text(change["path"].string ?? "File change").font(.callout.bold()).textSelection(.enabled)
                    Text((change["kind"]["type"].string ?? "Unknown change").capitalized)
                        .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                    if let destination = change["kind"]["move_path"].string {
                        Text("Move to \(destination)").font(.caption).textSelection(.enabled)
                    }
                    if let diff = change["diff"].string {
                        Text(diff).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    }
                }
            }
            ForEach(["additionalPermissions", "networkApprovalContext", "grantRoot"], id: \.self) { key in
                if params[key] != .null {
                    VStack(alignment: .leading, spacing: 5) {
                        Text(key == "grantRoot" ? "Session write access requested" : "Additional access requested")
                            .font(.callout.bold())
                        Text(readable(params[key])).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    }
                }
            }
            if !hasExactDetail {
                Text("Review this request on your computer. Pika cannot show enough detail here to approve it accurately.")
                    .font(.callout).foregroundStyle(.secondary)
            } else if !offered("accept") && !offered("decline") {
                Text("This request needs a decision on your computer; the provider has not offered a one-time choice here.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                if offered("decline") {
                    Button("Decline") { onDecision("decline") }.buttonStyle(.bordered)
                        .accessibilityIdentifier("declineProviderRequest")
                }
                if hasExactDetail && offered("accept") {
                    Button("Allow once") { onDecision("accept") }.buttonStyle(PikaPrimaryButtonStyle())
                        .accessibilityIdentifier("acceptProviderRequest")
                }
            }.disabled(pending)
            if pending { Text("Checking the original request’s resolution…").font(.caption).foregroundStyle(.secondary) }
        }.padding(16).frame(maxWidth: .infinity, alignment: .leading)
            .background(PikaTheme.sheet, in: RoundedRectangle(cornerRadius: 16))
    }
    private func readable(_ value: JSONValue) -> String {
        if let text = value.string { return text }
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        return (try? encoder.encode(value)).flatMap { String(data: $0, encoding: .utf8) } ?? "Details unavailable"
    }
}
