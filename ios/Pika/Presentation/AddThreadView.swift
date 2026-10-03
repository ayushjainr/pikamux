import SwiftUI

enum AddFlow: String, Identifiable { case start, existing; var id: String { rawValue } }

struct AddThreadView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let flow: AddFlow
    let opened: (BoardItem) -> Void
    @State private var name = ""
    @State private var projects: [MobileProject] = []
    @State private var nodes: [MobileNode] = []
    @State private var nodeId = ""
    @State private var candidates: [ExistingCandidate] = []
    @State private var projectId = ""
    @State private var provider = "codex"
    @State private var error: String?
    @State private var loading = true
    @State private var filter = ""
    var body: some View {
        NavigationStack {
            Form {
                Section("Machine") {
                    Picker("Machine", selection: $nodeId) {
                        ForEach(nodes) { node in Text(node.name).tag(node.id) }
                    }.pickerStyle(.menu).accessibilityIdentifier("threadMachinePicker")
                    if model.creations.values.contains(where: { $0.state == "unknown" }) {
                        Button("Check original creation receipts") { Task { await model.reconcileCreations() } }
                    }
                }
                if !supported {
                    Section { Text("This machine does not yet expose verified mobile \(flow == .start ? "creation" : "admission"). No conversation will be created or changed.").foregroundStyle(.secondary) }
                } else if flow == .start {
                    Section("Thread") {
                        TextField("Name this thread", text: $name).accessibilityIdentifier("newThreadName")
                        Picker("Project", selection: $projectId) {
                            Text("Choose a project").tag("")
                            ForEach(projects) { project in Text(project.name).tag(project.id) }
                        }.pickerStyle(.menu).accessibilityIdentifier("threadProjectPicker")
                        Picker("Provider", selection: $provider) { Text("Codex").tag("codex") }
                        Text("Choose an available provider on this machine. Your existing conversations stay where they are.").font(.caption).foregroundStyle(.secondary)
                    }
                    Section {
                        Button(model.mutationBusy ? "Starting…" : "Start thread") {
                            guard let project = projects.first(where: { $0.id == projectId }) else { return }
                            Task { if let item = await model.create(name: name, project: project, provider: provider) { opened(item) } }
                        }.disabled(model.mutationBusy || name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || projectId.isEmpty || model.creations.values.contains(where: { $0.state == "unknown" || $0.state == "pending" }))
                            .accessibilityIdentifier("startThread")
                    }
                } else {
                    Section("Existing named conversations") {
                        TextField("Find a conversation", text: $filter)
                        ForEach(candidates.filter { filter.isEmpty || $0.name.localizedCaseInsensitiveContains(filter) }) { candidate in
                            Button {
                                Task { if let item = await model.adopt(candidate) { opened(item) } }
                            } label: {
                                VStack(alignment: .leading, spacing: 3) {
                                    Text(candidate.name)
                                    Text(candidate.identity.provider).font(.caption).foregroundStyle(.secondary)
                                }
                            }.disabled(model.mutationBusy).accessibilityIdentifier("candidate-" + candidate.identity.threadId)
                        }
                        if !loading, candidates.isEmpty { Text("No available conversations on this machine. Try another machine or reconnect.").foregroundStyle(.secondary) }
                    }
                }
                if loading && supported { ProgressView("Reading machine capabilities…") }
                if let error { Section { Text(error).font(.callout).foregroundStyle(.secondary) } }
                if model.notice != nil { Section { NoticeCard() } }
                if model.mutationBusy { Text("Closing this screen cannot undo an already dispatched action. Its receipt will be retained.").font(.caption).foregroundStyle(.secondary) }
            }.navigationTitle(flow == .start ? "Start a thread" : "Add an existing thread")
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button(model.mutationBusy ? "Close" : "Cancel") { dismiss() } } }
                .task {
                    do {
                        nodes = try await model.nodes()
                        nodeId = nodes.first(where: { $0.id == model.activeNodeId })?.id ?? nodes.first?.id ?? ""
                    } catch { model.reportError(error, action: "Machine choices are unavailable. Reconnect and try again."); loading = false }
                }
                .task(id: nodeId) {
                    defer { loading = false }
                    guard supported, !nodeId.isEmpty else { return }
                    loading = true; projects = []; candidates = []; projectId = ""
                    let selectedNode = nodeId
                    do {
                        if flow == .start {
                            let found = try await model.projects(node: selectedNode)
                            guard !Task.isCancelled, nodeId == selectedNode else { return }
                            projects = found
                            projectId = model.choices["project"] ?? ""
                            if !projects.contains(where: { $0.id == projectId }) { projectId = "" }
                            provider = model.choices["provider"] ?? "codex"
                        } else {
                            let found = try await model.candidates(node: selectedNode)
                            guard !Task.isCancelled, nodeId == selectedNode else { return }
                            candidates = found
                        }
                    } catch { model.reportError(error, action: "Thread choices are unavailable. Reconnect and try again.") }
                }
                .onChange(of: projectId) { _, value in model.remember(project: value, provider: provider) }
        }
    }
    private var supported: Bool { model.connected && model.capabilities[flow == .start ? "create" : "adopt"].bool }
}
