import SwiftUI

enum ComposerPicker: String, Identifiable {
    case commands, models, skills
    var id: String { rawValue }
}

/// This sheet consumes the selected provider's catalog, never a phone-owned list.
struct ComposerControls: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let item: BoardItem
    @State var picker: ComposerPicker
    let insert: (String) -> Void
    @State private var catalog: JSONValue = .null
    @State private var filter = ""
    @State private var loading = true
    @State private var changing = false
    @State private var error: String?
    private var available: Bool {
        model.isConnected(node: item.identity.nodeId) && model.selected?.identity == item.identity && !model.conversationCached
    }
    private var models: [JSONValue] { catalog["models"]["data"].array.filter { !$0["hidden"].bool } }
    private var skills: [JSONValue] {
        catalog["skills"]["data"].array.flatMap { $0["skills"].array }.filter { $0["enabled"].bool }
    }
    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text("\(item.name) · \(model.machineName(for: item.identity.nodeId, fallback: item.machine))").font(.caption).foregroundStyle(.secondary)
                    if !available { Text("Reconnect and reopen this exact thread to use provider controls.") }
                    else if loading { ProgressView("Reading provider controls…") }
                    else if picker == .commands {
                        if !models.isEmpty {
                            Button { picker = .models } label: { Label("/model · Choose this thread's model", systemImage: "slider.horizontal.3") }
                                .accessibilityIdentifier("commandModel")
                        }
                        Text("Only controls supported by this provider connection are offered. /model is a control, not a message.").font(.caption).foregroundStyle(.secondary)
                        if models.isEmpty { Text("Model controls unavailable on this provider version.") }
                    } else if picker == .models {
                        Text("Current: \(catalog["currentModel"].string ?? "Unavailable")").accessibilityIdentifier("currentThreadModel")
                        Text("Applies only to this thread's subsequent turns. No message is sent.").font(.caption).foregroundStyle(.secondary)
                        ForEach(Array(models.enumerated()), id: \.offset) { _, entry in
                            if let id = entry["model"].string {
                                Button {
                                    changing = true
                                    Task {
                                        if await model.selectModel(id, item: item) {
                                            let trigger = model.draft(item.identity).trimmingCharacters(in: .whitespacesAndNewlines)
                                            if trigger == "/" || trigger == "/model" { model.setDraft("", identity: item.identity) }
                                            dismiss()
                                        } else { error = "The change was not confirmed. Reopen controls to read the actual setting before another change." }
                                        changing = false
                                    }
                                } label: {
                                    HStack {
                                        VStack(alignment: .leading) {
                                            Text(entry["displayName"].string ?? id)
                                            Text(id).font(.caption).foregroundStyle(.secondary)
                                        }
                                        Spacer()
                                        if catalog["currentModel"].string == id { Image(systemName: "checkmark").accessibilityLabel("Current model") }
                                    }
                                }.accessibilityIdentifier("modelChoice:" + id)
                            }
                        }
                        if let issue = catalog["modelsError"].string { Text(issue).font(.caption) }
                        if catalog["models"]["nextCursor"] != .null { Text("This provider has more models. This bounded picker shows its first page only.").font(.caption).foregroundStyle(.secondary) }
                    } else {
                        ForEach(Array(skills.enumerated()), id: \.offset) { _, skill in
                            if let name = skill["name"].string, filter.isEmpty || name.localizedCaseInsensitiveContains(filter) || (skill["description"].string ?? "").localizedCaseInsensitiveContains(filter) {
                                Button { insert("$" + name); dismiss() } label: {
                                    VStack(alignment: .leading, spacing: 4) {
                                        Text("$" + name)
                                        Text(skill["description"].string ?? "").font(.caption).foregroundStyle(.secondary).lineLimit(3)
                                    }
                                }.accessibilityIdentifier("skillChoice:" + name)
                            }
                        }
                        if skills.isEmpty { Text("No enabled skills were reported for this thread.") }
                        if let issue = catalog["skillsError"].string { Text(issue).font(.caption) }
                        if catalog["skills"]["data"].array.contains(where: { !$0["errors"].array.isEmpty }) { Text("Some skills could not be read by the provider; this list is incomplete.").font(.caption) }
                    }
                    if let error { Text(error).foregroundStyle(.secondary) }
                }.disabled(changing || !available || error != nil)
            }
            .modifier(ComposerSkillSearch(filter: $filter, enabled: picker == .skills))
            .navigationTitle(picker == .skills ? "Provider skills" : picker == .models ? "Thread model" : "Provider commands")
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Close") { dismiss() } } }
            .task {
                defer { loading = false }
                do { catalog = try await model.controls(item) }
                catch { self.error = "Provider controls are unavailable. Nothing was sent." }
            }
        }
    }
}

private struct ComposerSkillSearch: ViewModifier {
    @Binding var filter: String
    let enabled: Bool
    @ViewBuilder func body(content: Content) -> some View {
        if enabled { content.searchable(text: $filter, prompt: "Find a skill") }
        else { content }
    }
}
