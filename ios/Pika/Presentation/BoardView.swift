import SwiftUI

struct BoardView: View {
    @EnvironmentObject private var model: AppModel
    @State private var showConnection = false
    @State private var showAdd = false
    @State private var destination: BoardItem?
    @State private var flow: AddFlow?
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                TestContextBanner()
                HStack {
                    TimelineView(.periodic(from: .now, by: 15)) { _ in
                        Label(model.freshness, systemImage: model.connected ? "circle.fill" : "wifi.slash")
                            .font(.caption).foregroundStyle(.secondary).accessibilityIdentifier("connectionStatus")
                    }
                    Spacer(minLength: 8)
                }
                NoticeCard()
                if !model.machines.isEmpty {
                    Picker("Machine", selection: $model.machineFilter) {
                        Text("All machines").tag(String?.none)
                        ForEach(model.machines) { machine in
                            Text(machine.displayName + (model.isConnected(node: machine.id) ? "" : " · Offline")).tag(Optional(machine.id))
                        }
                    }.pickerStyle(.menu).accessibilityIdentifier("machineFilter")
                }
                if let coverage = model.coverageNote { Text(coverage).font(.caption).foregroundStyle(.secondary) }
                if !model.board.isEmpty {
                    LazyVStack(spacing: 10) {
                        // One identity scope follows rows across freshness/state
                        // groups. Lazy child scopes must not retain old snapshots
                        // when a cached row becomes live again.
                        ForEach(orderedRows) { row in
                            Button { destination = row } label: { DexThreadCard(row: row) }
                                .buttonStyle(.plain)
                                .accessibilityIdentifier("thread-" + (ProcessInfo.processInfo.arguments.contains("--multi-machine-integration-test") ? row.identity.draftKey : row.identity.threadId))
                                .accessibilityLabel("Open \(row.name) on \(row.machine), \(row.identity.provider)")
                        }
                    }
                }
                if model.board.isEmpty {
                    ContentUnavailableView("Your work, here", systemImage: "desktopcomputer", description: Text("Choose Connect phone on your machine, then scan its code here. Manual login is also available. Keep Tailscale on both devices."))
                    Button("Add a machine") { showConnection = true }.buttonStyle(PikaPrimaryButtonStyle()).frame(maxWidth: .infinity)
                }
            }.padding(.horizontal, 16).padding(.bottom, 20)
        }.background(PikaTheme.background)
            .safeAreaInset(edge: .top, spacing: 0) {
                DexHeader(add: { showAdd = true }, connections: { showConnection = true },
                    states: Set(model.connected ? model.board.filter { $0.stale != true }.map { PikaTheme.state($0.state) } : []))
            }
            .navigationTitle("Dex").navigationBarTitleDisplayMode(.inline)
            .toolbar(.hidden, for: .navigationBar)
            .toolbar(.hidden, for: .tabBar)
            .sheet(isPresented: $showConnection, onDismiss: {
                if model.connecting { model.cancelConnection() }
            }) { ConnectionView() }
            .confirmationDialog("Add to your Dex", isPresented: $showAdd, titleVisibility: .visible) {
                Button("Start a thread") { flow = .start }
                Button("Add an existing thread") { flow = .existing }
                Button("Cancel", role: .cancel) {}
            }
            .sheet(item: $flow) { selected in AddThreadView(flow: selected) { item in flow = nil; destination = item } }
            .navigationDestination(item: $destination) { item in ThreadView(item: item) }
    }
    private func rows(for state: String) -> [BoardItem] {
        let known = ["Needs you", "Working", "Ready", "Parked", "Cached"]
        return model.filteredBoard.filter {
            let category = $0.stale == true ? "Cached" : PikaTheme.state($0.state)
            return state == "Other" ? !known.contains(category) : category == state
        }
    }
    private var orderedRows: [BoardItem] {
        ["Needs you", "Working", "Ready", "Parked", "Cached", "Other"].flatMap { rows(for: $0) }
    }
}

private struct DexThreadCard: View {
    let row: BoardItem
    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            ProviderMark(provider: row.identity.provider, size: 48)
            VStack(alignment: .leading, spacing: 7) {
                ViewThatFits(in: .horizontal) {
                    HStack(alignment: .firstTextBaseline) { name; Spacer(minLength: 5); badge }
                    VStack(alignment: .leading, spacing: 6) { name; badge }
                }
                Text("\(row.machine) · \(row.identity.provider.capitalized)").font(.subheadline).foregroundStyle(.secondary)
                Text(row.observationSummary).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("boardFreshness-" + row.identity.draftKey)
                if row.unread == true { Text("Unread").font(.caption2.weight(.semibold)).foregroundStyle(PikaTheme.accent) }
            }
            Image(systemName: "chevron.right").font(.caption).foregroundStyle(.tertiary).frame(maxHeight: .infinity)
        }.padding(14).frame(maxWidth: .infinity, minHeight: 108, alignment: .leading)
            .background(PikaTheme.sheet, in: RoundedRectangle(cornerRadius: 16))
            .overlay(RoundedRectangle(cornerRadius: 16).strokeBorder(PikaTheme.border.opacity(0.85), lineWidth: 1))
    }
    private var name: some View { Text(row.name).font(.headline).foregroundStyle(.primary).lineLimit(2) }
    private var badge: some View {
        let color = PikaTheme.color(row.stale == true ? "CACHED" : row.state)
        return HStack(spacing: 5) {
            Circle().fill(color).frame(width: 7, height: 7)
            Text((row.stale == true ? "Cached · " : "") + PikaTheme.state(row.state)).font(.caption.weight(.semibold))
        }.foregroundStyle(color).padding(.horizontal, 9).padding(.vertical, 6)
            .background(color.opacity(0.08), in: Capsule()).fixedSize()
    }
}
