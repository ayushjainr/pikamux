import SwiftUI

struct BoardView: View {
    @EnvironmentObject private var model: AppModel
    @State private var showConnection = false
    @State private var showAdd = false
    @State private var destination: BoardItem?
    @State private var flow: AddFlow?
    @State private var searchText = ""
    @State private var providerFilter: String?
    @FocusState private var searchFocused: Bool
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
                searchField
                HStack {
                    if !model.machines.isEmpty {
                        Picker("Machine", selection: $model.machineFilter) {
                            Text("All machines").tag(String?.none)
                            ForEach(model.machines) { machine in
                                Text(machine.displayName + (model.isConnected(node: machine.id) ? "" : " · Offline")).tag(Optional(machine.id))
                            }
                        }.pickerStyle(.menu).accessibilityIdentifier("machineFilter")
                    }
                    if !model.board.isEmpty {
                        Picker("Provider", selection: $providerFilter) {
                            Text("All providers").tag(String?.none)
                            ForEach(providers, id: \.self) { provider in
                                Text(provider == "opencode" ? "OpenCode" : provider.capitalized).tag(Optional(provider))
                            }
                        }.pickerStyle(.menu).accessibilityIdentifier("providerFilter")
                    }
                }.frame(maxWidth: .infinity, alignment: .leading)
                if let coverage = model.coverageNote { Text(coverage).font(.caption).foregroundStyle(.secondary) }
                if !model.board.isEmpty {
                    LazyVStack(spacing: 10) {
                        // One identity scope follows rows across freshness/state
                        // groups. Lazy child scopes must not retain old snapshots
                        // when a cached row becomes live again.
                        ForEach(orderedRows) { row in
                            Button { searchFocused = false; destination = row } label: { DexThreadCard(row: row) }
                                .buttonStyle(.plain)
                                .accessibilityIdentifier("thread-" + (ProcessInfo.processInfo.arguments.contains("--multi-machine-integration-test") ? row.identity.draftKey : row.identity.threadId))
                                .accessibilityLabel("Open \(row.name) on \(row.machine), \(row.identity.provider)")
                        }
                    }
                    if orderedRows.isEmpty {
                        ContentUnavailableView("No matching threads", systemImage: "magnifyingglass",
                            description: Text("Try another search or change the machine or provider filter."))
                            .accessibilityIdentifier("threadSearchEmpty")
                    }
                }
                if model.board.isEmpty {
                    ContentUnavailableView("Your work, here", systemImage: "desktopcomputer", description: Text("Choose Connect phone on your machine, then scan its code here. Manual login is also available. Keep Tailscale on both devices."))
                    Button("Add a machine") { showConnection = true }.buttonStyle(PikaPrimaryButtonStyle()).frame(maxWidth: .infinity)
                }
            }.padding(.horizontal, 16).padding(.bottom, 20)
        }.scrollDismissesKeyboard(.interactively).background(PikaTheme.background)
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
    private var searchField: some View {
        HStack(spacing: 10) {
            Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
            TextField("Search threads", text: $searchText)
                .focused($searchFocused)
                .textInputAutocapitalization(.never).autocorrectionDisabled()
                .submitLabel(.search).onSubmit { searchFocused = false }
                .accessibilityIdentifier("threadSearch")
                .accessibilityHint("Search by thread name, machine or provider")
            if !searchText.isEmpty {
                Button { searchText = "" } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                        .frame(width: 44, height: 44)
                }.buttonStyle(.plain).accessibilityLabel("Clear search")
                    .accessibilityIdentifier("clearThreadSearch")
            }
        }.padding(.leading, 14).padding(.trailing, searchText.isEmpty ? 14 : 0)
            .frame(minHeight: 44)
            .background(PikaTheme.sheet, in: RoundedRectangle(cornerRadius: 14))
            .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(PikaTheme.border, lineWidth: 1))
    }
    private var matchingRows: [BoardItem] {
        let terms = searchText.split(whereSeparator: { $0.isWhitespace }).map(String.init)
        return model.filteredBoard.filter { row in
            let fields = [row.name, row.machine, row.identity.provider]
            return (providerFilter == nil || row.identity.provider == providerFilter)
                && terms.allSatisfy { term in fields.contains { $0.localizedStandardContains(term) } }
        }
    }
    private var providers: [String] {
        Set(model.board.map { $0.identity.provider } + [providerFilter].compactMap { $0 }).sorted()
    }
    private var orderedRows: [BoardItem] {
        let known = ["Needs you", "Working", "Ready", "Parked", "Cached"]
        let matches = matchingRows
        return (known + ["Other"]).flatMap { state in
            matches.filter {
                let category = $0.stale == true ? "Cached" : PikaTheme.state($0.state)
                return state == "Other" ? !known.contains(category) : category == state
            }
        }
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
