import SwiftUI
import MarkdownUI

struct ThreadView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.verticalSizeClass) private var verticalSizeClass
    @Environment(\.dismiss) private var dismiss
    let item: BoardItem
    @State private var composing = false
    @State private var positioned = false
    @State private var nearBottom = true
    @State private var viewportHeight: CGFloat = 0
    @State private var bottomPosition: CGFloat = 0
    @StateObject private var historyScroll = HistoryReadingAnchor()
    @State private var readingGesture = false
    @State private var sendScrollRequest = 0
    @State private var controlPicker: ComposerPicker?
    @State private var showExperimentalNotice = false
    private var liveItem: BoardItem { model.board.first(where: { $0.identity == item.identity }) ?? item }
    private var machineName: String { model.machineName(for: item.identity.nodeId, fallback: liveItem.machine) }
    private var historyOnly: Bool { !model.conversationCapabilities["send"].bool && model.conversationCapabilities["readOnlyReason"].string != nil }
    private var experimentalNotice: String? {
        guard model.selected?.identity == item.identity,
            let text = model.conversationCapabilities["experimentalNotice"].string,
            !text.isEmpty else { return nil }
        return String(text.prefix(800))
    }
    var body: some View {
        ScrollViewReader { proxy in
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                TestContextBanner()
                if model.selected?.identity == item.identity, model.conversationCached { Text("Cached conversation · reconnect to verify current context").font(.caption).foregroundStyle(.secondary) }
                if model.selected?.identity == item.identity, model.conversationNeedsRefresh {
                    Text("Live reply updating · original snapshot preserved until the provider completes. Refresh to verify current output.").font(.caption).foregroundStyle(.secondary)
                    Button("Refresh original conversation") { Task { await attachVisible(force: true) } }.buttonStyle(.bordered)
                }
                if model.selected?.identity == item.identity, model.conversationCapabilities == .null {
                    Button(model.isConnected(node: item.identity.nodeId) ? "Recheck original conversation" : "Reconnect machine") {
                        Task { if model.isConnected(node: item.identity.nodeId) { await attachVisible(force: true) } else { model.retryConnection() } }
                    }.buttonStyle(.bordered)
                }
                if model.selected?.identity == item.identity, model.historyCursor != .null {
                    Button(model.historyLoading ? "Reading older context…" : "Load older context") {
                        let anchor = model.messages.first?.id
                        let token = model.conversationReadToken(for: item.identity)
                        let captured = anchor.map { historyScroll.capture(messageId: $0, token: token) } ?? false
                        nearBottom = false
                        Task {
                            await model.loadOlder(item)
                            // Allow native content size to reflect SwiftUI's prepend.
                            try? await Task.sleep(for: .milliseconds(50))
                            guard let token, model.conversationReadToken(for: item.identity) == token else { historyScroll.cancel(); return }
                            if captured { historyScroll.restore(token: token) }
                            else if let anchor { proxy.scrollTo(anchor, anchor: .top) }
                        }
                    }.font(.caption.weight(.medium)).buttonStyle(.bordered)
                        .buttonBorderShape(.capsule).frame(maxWidth: .infinity).disabled(model.historyLoading)
                }
                TimelineView(.periodic(from: .now, by: 15)) { _ in
                    Text((model.board.first(where: { $0.identity == item.identity }) ?? item).observationSummary)
                        .font(.caption).foregroundStyle(.secondary).frame(maxWidth: .infinity)
                }
                #if DEBUG
                if model.isFixture, ProcessInfo.processInfo.arguments.contains("--fixture-slow-assistant") {
                    Text("Fixture assistant completed: \(model.fixtureAssistantFinished ? "yes" : "no")").accessibilityIdentifier("fixtureAssistantFinished")
                }
                if model.isFixture { Text("Fixture send attempts: \(model.fixtureSendCount)").font(.caption2).foregroundStyle(.secondary).accessibilityIdentifier("fixtureSendCount") }
                if model.isFixture, item.state == "ASSISTANT" { Text("Fixture assistant opens: \(model.fixtureAssistantOpenCount)").font(.caption2).accessibilityIdentifier("fixtureAssistantOpenCount") }
                #endif
                NoticeCard()
                if model.pendingActions.values.contains(where: { $0.identity == item.identity && $0.state == "unknown" }) {
                    Button("Check original delivery receipt") { Task { await model.reconcile(item) } }.buttonStyle(.bordered)
                }
                if model.pendingActions.values.contains(where: { $0.identity == item.identity && $0.requestId != nil && ["pending", "unknown"].contains($0.state) }) {
                    Button("Check original request status") { Task { await model.reconcileRequests(item) } }.buttonStyle(.bordered)
                }
                if model.messages.isEmpty { Text(model.isConnected(node: item.identity.nodeId) ? "Reading the exact existing conversation…" : "Reconnect to read current context.").foregroundStyle(.secondary) }
                ForEach(model.selected?.identity == item.identity ? model.messages : []) { message in
                    Group {
                    if message.role == "user" {
                        HStack { Spacer(minLength: 36); Group {
                            if message.text.utf8.count > 64 * 1024 {
                                VStack(alignment: .leading, spacing: 8) {
                                    Text("Long message · full text in parts").font(.caption).foregroundStyle(.secondary)
                                    NativeTranscriptReader(text: message.text, monospaced: false, identifier: "large-user-content").frame(height: 320)
                                    Button("Copy original message") { UIPasteboard.general.string = message.text }.font(.caption.weight(.medium))
                                }
                            } else { Text(message.text).textSelection(.enabled).lineSpacing(4) }
                        }
                            .padding(.horizontal, 16).padding(.vertical, 12)
                            .background(PikaTheme.accent.opacity(0.09), in: RoundedRectangle(cornerRadius: 20)) }
                            .padding(.vertical, 6)
                    } else {
                        VStack(alignment: .leading, spacing: 12) {
                            HStack(spacing: 8) {
                                ProviderMark(provider: item.identity.provider)
                                Text(item.identity.provider.capitalized).font(.subheadline.weight(.semibold))
                            }
                            RichReply(text: message.text).equatable()
                        }.frame(maxWidth: .infinity, alignment: .leading).padding(.vertical, 8)
                    }
                    }.id(message.id)
                        .background {
                            if message.id == model.messages.first?.id || message.id == historyScroll.anchorMessageId {
                                HistoryScrollProbe(controller: historyScroll, messageId: message.id)
                            }
                        }
                }
                if model.selected?.identity == item.identity, let question = model.question { QuestionView(question: question, identity: item.identity).id(question.id + ":" + String(describing: question.requestId)) }
                if model.selected?.identity == item.identity, let request = model.approval, request.identity == item.identity {
                    ApprovalCard(method: request.method, params: request.params, item: request.item,
                        pending: model.pendingActions["approval:" + request.identity.draftKey + ":" + request.id] != nil) { decision in
                        Task { await model.approve(request, decision: decision) }
                    }.id(request.id)
                }
                if model.conversationCapabilities != .null, !model.conversationCapabilities["send"].bool {
                    Text(model.conversationCapabilities["readOnlyReason"].string ?? "This provider does not expose verified mobile replies for this existing conversation.")
                        .font(.callout).foregroundStyle(.secondary)
                }
                Color.clear.frame(height: 1).id("conversation-bottom")
                    .background(GeometryReader { geometry in Color.clear.preference(key: ConversationBottomKey.self, value: geometry.frame(in: .named("conversation-scroll")).maxY) })
            }.padding(20)
        }.coordinateSpace(name: "conversation-scroll").accessibilityIdentifier("conversation-" + item.identity.threadId)
            .background(GeometryReader { geometry in Color.clear.preference(key: ConversationViewportKey.self, value: geometry.size.height) })
            .onPreferenceChange(ConversationViewportKey.self) {
                viewportHeight = $0
                if positioned, nearBottom { Task { @MainActor in await Task.yield(); if nearBottom, !readingGesture { proxy.scrollTo("conversation-bottom", anchor: .bottom) } } }
            }
            .onPreferenceChange(ConversationBottomKey.self) {
                historyScroll.restoreIfAnchorMoved(token: model.conversationReadToken(for: item.identity))
                bottomPosition = $0
                if readingGesture { nearBottom = $0 <= viewportHeight + 80 }
                else if positioned, nearBottom, $0 > viewportHeight + 1 {
                    Task { @MainActor in
                        await Task.yield()
                        if nearBottom, !readingGesture { proxy.scrollTo("conversation-bottom", anchor: .bottom) }
                    }
                }
            }
            .simultaneousGesture(DragGesture().onChanged { _ in readingGesture = true }.onEnded { value in
                nearBottom = bottomPosition <= viewportHeight + 80; readingGesture = false
                if value.startLocation.x < 24, value.translation.width > 80,
                    value.translation.width > abs(value.translation.height) * 2 { dismiss() }
            })
            .onChange(of: contentVersion, initial: true) { _, _ in
                guard model.selected?.identity == item.identity, !model.historyLoading,
                    !positioned || nearBottom else { return }
                positioned = true
                Task { @MainActor in await Task.yield(); if nearBottom, !readingGesture { proxy.scrollTo("conversation-bottom", anchor: .bottom) } }
            }
            .onChange(of: sendScrollRequest) { _, _ in
                proxy.scrollTo("conversation-bottom", anchor: .bottom)
            }
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardDidChangeFrameNotification)) { _ in
                guard positioned, nearBottom, !readingGesture else { return }
                Task { @MainActor in
                    await Task.yield()
                    if nearBottom, !readingGesture { proxy.scrollTo("conversation-bottom", anchor: .bottom) }
                }
            }
            .scrollDismissesKeyboard(.interactively).background(PikaTheme.background)
            .onAppear { Task { await attachVisible() } }
            .sheet(item: $controlPicker) { picker in
                ComposerControls(item: item, picker: picker) { reference in
                    let draft = model.draft(item.identity)
                    let last = draft.split(whereSeparator: { $0.isWhitespace }).last.map(String.init) ?? ""
                    let prefix = last == "$" ? (draft.lastIndex(where: { $0.isWhitespace }).map { String(draft[...$0]) } ?? "") : draft + (draft.isEmpty || draft.last?.isWhitespace == true ? "" : " ")
                    model.setDraft(prefix + reference + " ", identity: item.identity)
                    model.referenceSkill(String(reference.dropFirst()), identity: item.identity)
                }
            }
            .onChange(of: model.draft(item.identity)) { _, draft in
                guard !composing, !historyOnly else { return }
                let trigger = draft.trimmingCharacters(in: .whitespacesAndNewlines)
                guard trigger == "/" || trigger == "/model" || draft.split(whereSeparator: { $0.isWhitespace }).last == "$" else { return }
                Task {
                    // Let normal paths and dollar-prefixed text continue typing
                    // before offering a standalone control trigger.
                    try? await Task.sleep(for: .milliseconds(350))
                    guard !Task.isCancelled, !composing, model.draft(item.identity) == draft, controlPicker == nil else { return }
                    controlPicker = trigger == "/model" ? .models : trigger == "/" ? .commands : .skills
                }
            }
            .safeAreaInset(edge: .bottom, spacing: 0) { composer }
            .safeAreaInset(edge: .top, spacing: 0) { threadHeader }
            .accessibilityAction(.escape) { dismiss() }
            .toolbar(.hidden, for: .tabBar)
            .toolbar(.hidden, for: .navigationBar)
        }
    }
    private var threadHeader: some View {
        HStack(spacing: 10) {
            DexSignals(states: model.isConnected(node: item.identity.nodeId) && liveItem.stale != true && !model.conversationCached
                ? Set([PikaTheme.state(liveItem.state)].filter { ["Needs you", "Working", "Ready"].contains($0) }) : [])
                .padding(.trailing, 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(liveItem.name).font(.subheadline.weight(.semibold)).lineLimit(1)
                Text("\(machineName) · \(PikaTheme.state(liveItem.state))")
                    .font(.caption2).foregroundStyle(.secondary).lineLimit(1)
            }.frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityElement(children: .combine).accessibilityIdentifier("conversationHeader")
            ProviderMark(provider: item.identity.provider, size: 32)
            #if DEBUG
            if model.isFixture {
                Button { model.suspend() } label: { Image(systemName: "wifi.slash").frame(width: 32, height: 44) }
                    .accessibilityLabel("Go offline").accessibilityIdentifier("fixtureOffline")
            }
            #endif
        }.foregroundStyle(Color.black.opacity(0.85)).padding(.horizontal, 16).frame(height: 50)
            .background {
                ZStack {
                    LinearGradient(colors: [PikaTheme.shellHighlight, PikaTheme.shell], startPoint: .leading, endPoint: .trailing).ignoresSafeArea(.container, edges: .top)
                    DexShoulder(compact: true).fill(PikaTheme.seam).offset(y: -2)
                    DexShoulder(compact: true).fill(PikaTheme.background)
                }
            }
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("threadCasingHeader")
    }
    private var contentVersion: String {
        guard model.selected?.identity == item.identity else { return "" }
        return "\(model.messages.count):\(model.messages.last?.id ?? ""):\(model.messages.last?.text ?? ""):\(model.question?.id ?? ""):\(model.approval?.id ?? "")"
    }
    private var composer: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 16) {
                Button("/ Commands") { controlPicker = .commands }.accessibilityIdentifier("composerCommands")
                Button("$ Skills") { controlPicker = .skills }.accessibilityIdentifier("composerSkills")
                Spacer()
                if experimentalNotice != nil {
                    Button { showExperimentalNotice = true } label: {
                        Label("Experimental", systemImage: "info.circle")
                    }.accessibilityIdentifier("experimentalConnectionInfo")
                }
            }.font(.caption).padding(.horizontal, 12).padding(.bottom, 6)
                .disabled(!model.isConnected(node: item.identity.nodeId) || model.selected?.identity != item.identity || model.conversationCached ||
                    historyOnly)
            HStack(alignment: .bottom, spacing: 8) {
                NativeComposer(text: Binding(get: { model.draft(item.identity) }, set: { model.setDraft($0, identity: item.identity) }),
                    composing: $composing, label: "Reply to \(liveItem.name) on \(machineName)", send: send)
                    .overlay(alignment: .topLeading) {
                        if model.draft(item.identity).isEmpty {
                            Text("Message…").foregroundStyle(.secondary).padding(.leading, 9).padding(.top, 10)
                                .allowsHitTesting(false).accessibilityHidden(true)
                        }
                    }
                Button(action: send) { PikaMark(size: 32).opacity(model.canSend(item, composing: composing) ? 1 : 0.35).frame(width: 44, height: 44) }
                    .background(model.canSend(item, composing: composing) ? PikaTheme.shell.opacity(0.10) : Color(uiColor: .tertiarySystemFill), in: Circle())
                    .foregroundStyle(model.canSend(item, composing: composing) ? PikaTheme.buttonText : .secondary)
                    .disabled(!model.canSend(item, composing: composing))
                    .accessibilityLabel("Send reply to \(liveItem.name) on \(machineName)").accessibilityIdentifier("sendReply")
            }.padding(8).background(PikaTheme.sheet, in: RoundedRectangle(cornerRadius: 28))
                .overlay(RoundedRectangle(cornerRadius: 28).strokeBorder(.primary.opacity(0.10), lineWidth: 0.5))
        }.padding(.horizontal, 16).padding(.vertical, verticalSizeClass == .compact ? 4 : 10).background(PikaTheme.background)
            .alert("Experimental connection", isPresented: $showExperimentalNotice) {
                Button("OK", role: .cancel) { }
            } message: {
                Text(experimentalNotice ?? "This connection is no longer active. Reopen the original conversation to verify its state.")
            }
    }
    private func send() {
        guard !historyOnly else { return }
        let trigger = model.draft(item.identity).trimmingCharacters(in: .whitespacesAndNewlines)
        if trigger == "/" || trigger == "/model" {
            controlPicker = trigger == "/model" ? .models : .commands
            return
        }
        guard model.canSend(item, composing: composing) else { return }
        nearBottom = true
        readingGesture = false
        sendScrollRequest += 1
        Task { await model.send(item, composing: composing) }
    }
    private func attachVisible(force: Bool = false) async {
        if item.state == "ASSISTANT" {
            if force || model.selected?.identity != item.identity { _ = await model.openAssistant() }
        } else { await model.open(item) }
    }
}

/// Presentation only: the original provider text remains unchanged in history.
private struct RichReply: View, Equatable {
    let text: String
    nonisolated static func == (lhs: RichReply, rhs: RichReply) -> Bool {
        lhs.text.utf8.elementsEqual(rhs.text.utf8)
    }
    @ViewBuilder
    var body: some View {
        if text.utf8.count > 1024 * 1024 {
            // A whole-message fallback also bounds documents with thousands
            // of individually small blocks; fence spelling cannot bypass it.
            VStack(alignment: .leading, spacing: 8) {
                Text("Long reply · original text mode · full text in parts")
                    .font(.caption).foregroundStyle(.secondary)
                NativeTranscriptReader(text: text, monospaced: false)
                    .frame(height: 320)
                Button("Copy original reply") { UIPasteboard.general.string = text }
                    .font(.caption.weight(.medium))
            }
        } else {
        Markdown(text)
            .markdownTextStyle(\.text) {
                FontSize(17)
                ForegroundColor(.primary)
                BackgroundColor(PikaTheme.background)
            }
            .markdownTextStyle(\.link) { ForegroundColor(PikaTheme.accent) }
            .markdownTextStyle(\.code) {
                FontFamilyVariant(.monospaced)
                FontSize(.em(0.88))
                BackgroundColor(PikaTheme.accent.opacity(0.08))
            }
            .markdownBlockStyle(\.heading1) { configuration in
                configuration.label.markdownTextStyle { FontWeight(.bold); FontSize(.em(1.5)) }
                    .markdownMargin(top: 20, bottom: 12)
            }
            .markdownBlockStyle(\.heading2) { configuration in
                configuration.label.markdownTextStyle { FontWeight(.semibold); FontSize(.em(1.25)) }
                    .markdownMargin(top: 18, bottom: 10)
            }
            .markdownBlockStyle(\.blockquote) { configuration in
                configuration.label.padding(.leading, 14).padding(.vertical, 6)
                    .overlay(alignment: .leading) { RoundedRectangle(cornerRadius: 2).fill(PikaTheme.buttonFill).frame(width: 3) }
                    .markdownMargin(top: 8, bottom: 14)
            }
            .markdownBlockStyle(\.codeBlock) { configuration in
                ReplyCodeBlock(code: configuration.content, language: configuration.language)
                    .markdownMargin(top: 8, bottom: 16)
            }
            .markdownBlockStyle(\.paragraph) { configuration in
                let paragraph = configuration.content.renderPlainText()
                if paragraph.utf8.count > 64 * 1024 {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Long paragraph · full text in parts").font(.caption).foregroundStyle(.secondary)
                        NativeTranscriptReader(text: paragraph, monospaced: false).frame(height: 320)
                    }.markdownMargin(top: 8, bottom: 16)
                } else {
                    configuration.label.fixedSize(horizontal: false, vertical: true)
                        .relativeLineSpacing(.em(0.25)).markdownMargin(top: 0, bottom: 16)
                }
            }
            .markdownBlockStyle(\.table) { configuration in
                ScrollView(.horizontal) {
                    configuration.label
                        .markdownTableBorderStyle(.init(color: .primary.opacity(0.12)))
                        .markdownTableBackgroundStyle(.alternatingRows(PikaTheme.background, PikaTheme.sheet))
                }.markdownMargin(top: 8, bottom: 16)
            }
            .markdownImageProvider(ReplyImagePlaceholder())
            .markdownInlineImageProvider(ReplyInlineImagePlaceholder())
            .textSelection(.enabled)
            .markdownTheme(.gitHub)
            .environment(\.openURL, OpenURLAction { url in
                // A displayed link is not permission to execute a custom scheme.
                ["https", "http", "mailto"].contains(url.scheme?.lowercased() ?? "") ? .systemAction : .discarded
            })
            if text.utf8.count > 64 * 1024 {
                Button("Copy original reply") { UIPasteboard.general.string = text }.font(.caption.weight(.medium))
            }
        }
    }
}

private struct ReplyCodeBlock: View {
    let code: String
    let language: String?
    @State private var copied = false
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(language.flatMap { $0.isEmpty ? nil : $0 } ?? "Code").font(.caption.monospaced()).foregroundStyle(.secondary)
                Spacer()
                Button {
                    UIPasteboard.general.string = code
                    copied = true
                } label: { Label(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc").font(.caption.weight(.medium)) }
                    .accessibilityLabel(copied ? "Code copied" : "Copy code")
                    .buttonStyle(.plain).foregroundStyle(PikaTheme.accent)
                    .transaction { $0.animation = nil }
                    .frame(minHeight: 44)
            }.padding(.horizontal, 14)
            Divider().opacity(0.5)
            if code.utf8.count > 64 * 1024 {
                Text("Long code · full text in parts")
                    .font(.caption).foregroundStyle(.secondary).padding(.horizontal, 14).padding(.top, 8)
                NativeTranscriptReader(text: code, monospaced: true, identifier: "large-code-content")
                    .frame(height: 320)
            } else {
            ScrollView(.horizontal) {
                Text(code).font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                    .fixedSize(horizontal: true, vertical: false).padding(14)
            }
            }
        }.background(PikaTheme.sheet, in: RoundedRectangle(cornerRadius: 14))
            .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(.primary.opacity(0.08), lineWidth: 0.5))
            .onChange(of: code) { _, _ in copied = false }
    }
}

/// Layout itself is bounded, not merely the view's frame. Every original
/// character is available through explicit parts; surrounding Copy controls
/// always copy the complete original, never the currently displayed part.
private struct NativeTranscriptReader: View {
    let monospaced: Bool
    let identifier: String
    private let originalBytes: Data
    private let parts: [String]
    @State private var part = 0
    init(text: String, monospaced: Bool, identifier: String = "large-reply-content") {
        self.monospaced = monospaced
        self.identifier = identifier
        self.originalBytes = Data(text.utf8)
        var parts: [String] = []
        let scalars = text.unicodeScalars
        var start = scalars.startIndex
        var end = start
        var bytes = 0
        while end < scalars.endIndex {
            let scalar = scalars[end].value
            let width = scalar <= 0x7f ? 1 : scalar <= 0x7ff ? 2 : scalar <= 0xffff ? 3 : 4
            if bytes + width > 16 * 1024 {
                parts.append(String(text[start..<end]))
                start = end
                bytes = 0
            }
            bytes += width
            scalars.formIndex(after: &end)
        }
        if start < end { parts.append(String(text[start..<end])) }
        self.parts = parts.isEmpty ? [""] : parts
    }
    var body: some View {
        let selected = min(max(part, 0), parts.count - 1)
        VStack(spacing: 4) {
            NativeTranscriptPage(text: parts[selected], monospaced: monospaced, identifier: identifier)
            if parts.count > 1 {
                HStack {
                    Button("Previous") { part = max(0, selected - 1) }.disabled(selected == 0).accessibilityIdentifier(identifier + "-previous")
                    Spacer()
                    Text("Part \(selected + 1) of \(parts.count)").font(.caption).accessibilityIdentifier(identifier + "-page")
                    Spacer()
                    Button("Next") { part = min(parts.count - 1, selected + 1) }.disabled(selected + 1 >= parts.count).accessibilityIdentifier(identifier + "-next")
                }.font(.caption.weight(.medium)).padding(.horizontal, 14).frame(height: 44)
            }
        }.onChange(of: originalBytes) { _, _ in part = 0 }
    }
}

private struct NativeTranscriptPage: UIViewRepresentable {
    let text: String
    let monospaced: Bool
    var identifier = "large-reply-content"
    func makeUIView(context: Context) -> UITextView {
        // A zero-width initial text container can eagerly lay out a huge
        // single line as one glyph per line before SwiftUI supplies its frame.
        // Noncontiguous TextKit layout keeps work scoped to the native viewport.
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        layout.allowsNonContiguousLayout = true
        let container = NSTextContainer(size: CGSize(width: 320, height: CGFloat.greatestFiniteMagnitude))
        layout.addTextContainer(container)
        storage.addLayoutManager(layout)
        let view = TranscriptPageTextView(frame: CGRect(x: 0, y: 0, width: 320, height: 320), textContainer: container)
        view.isEditable = false
        view.isSelectable = true
        view.isScrollEnabled = true
        view.alwaysBounceVertical = true
        view.backgroundColor = .clear
        view.textColor = .label
        view.adjustsFontForContentSizeCategory = true
        let size = UIFont.preferredFont(forTextStyle: .callout).pointSize
        view.font = monospaced ? .monospacedSystemFont(ofSize: size, weight: .regular) : .preferredFont(forTextStyle: .body)
        view.textContainer.widthTracksTextView = true
        view.textContainerInset = UIEdgeInsets(top: 14, left: 14, bottom: 14, right: 14)
        view.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        view.accessibilityIdentifier = identifier
        // Each explicit, bounded page is one accessible text element. UIKit's
        // synthesized paragraph children otherwise repeatedly walk every text
        // attribute during snapshots, including long combining sequences.
        view.isAccessibilityElement = true
        view.accessibilityElements = []
        view.text = text
        return view
    }
    func updateUIView(_ view: UITextView, context: Context) {
        // Canonically equivalent Unicode strings can still have distinct
        // original bytes; the reader must not retain a previous normalization.
        if !view.text.utf8.elementsEqual(text.utf8) {
            view.text = text
            // A newly selected part starts at its beginning, while ordinary
            // parent updates preserve the reader's current scroll position.
            view.setContentOffset(.zero, animated: false)
        }
    }
}

private final class TranscriptPageTextView: UITextView {
    // The page is literal plain text. UIKit's generic attributed-value path
    // re-resolves thousands of font/color runs on every accessibility query.
    // Expose exactly the displayed native text, without inventing a full-source
    // value or hiding the explicit Previous/Next parts from accessibility.
    override var accessibilityAttributedValue: NSAttributedString? {
        get { NSAttributedString(string: text ?? "") }
        set { }
    }
}

private struct ReplyImagePlaceholder: ImageProvider {
    func makeImage(url: URL?) -> some View {
        Label("Image · not loaded", systemImage: "photo").font(.caption).foregroundStyle(.secondary)
    }
}
private struct ReplyInlineImagePlaceholder: InlineImageProvider {
    func image(with url: URL, label: String) async throws -> Image { Image(systemName: "photo") }
}

private struct ConversationBottomKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}
private struct ConversationViewportKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}
/// Presentation-only native offset capture: no connection or transcript ownership.
@MainActor private final class HistoryReadingAnchor: ObservableObject {
    weak var scrollView: UIScrollView?
    private weak var anchorView: UIView?
    private var registeredId: String?
    @Published private(set) var anchorMessageId: String?
    private var saved: (offset: CGPoint, y: CGFloat, token: String)?
    func register(view: UIView, messageId: String, scroll: UIScrollView) {
        guard anchorMessageId == nil || anchorMessageId == messageId else { return }
        anchorView = view; registeredId = messageId; scrollView = scroll
    }
    func capture(messageId: String, token: String?) -> Bool {
        guard let token, let scrollView, let anchorView, registeredId == messageId else { return false }
        saved = (scrollView.contentOffset, anchorView.convert(anchorView.bounds, to: scrollView).minY, token)
        anchorMessageId = messageId
        return true
    }
    func restoreIfAnchorMoved(token: String?) {
        guard let saved else { return }
        guard token == saved.token else { cancel(); return }
        guard let scrollView, let anchorView,
            abs(anchorView.convert(anchorView.bounds, to: scrollView).minY - saved.y) > 1 else { return }
        restore(token: token)
    }
    func restore(token: String?) {
        guard let saved else { return }
        guard token == saved.token else { cancel(); return }
        guard let scrollView, let anchorView else { cancel(); return }
        scrollView.layoutIfNeeded()
        let delta = anchorView.convert(anchorView.bounds, to: scrollView).minY - saved.y
        cancel()
        scrollView.setContentOffset(CGPoint(x: saved.offset.x,
            y: saved.offset.y + delta), animated: false)
    }
    func cancel() { saved = nil; anchorMessageId = nil }
}
private struct HistoryScrollProbe: UIViewRepresentable {
    let controller: HistoryReadingAnchor
    let messageId: String
    func makeUIView(context: Context) -> Probe {
        let view = Probe(); view.controller = controller; view.messageId = messageId; view.isUserInteractionEnabled = false
        return view
    }
    func updateUIView(_ uiView: Probe, context: Context) { uiView.connect() }
    final class Probe: UIView {
        weak var controller: HistoryReadingAnchor?
        var messageId = ""
        override func didMoveToWindow() { super.didMoveToWindow(); connect() }
        func connect() {
            var ancestor = superview
            while let view = ancestor {
                if let scroll = view as? UIScrollView { controller?.register(view: self, messageId: messageId, scroll: scroll); return }
                ancestor = view.superview
            }
        }
    }
}

struct QuestionView: View {
    @EnvironmentObject private var model: AppModel
    let question: ProviderQuestion
    let identity: ThreadIdentity
    @State private var answers: [String: [String]] = [:]
    @State private var submitted = false
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("The original provider is asking").font(.caption).foregroundStyle(.secondary)
            ForEach(Array(question.questions.array.enumerated()), id: \.offset) { index, row in
                let id = row["id"].string ?? "question-\(index)"
                Text(row["question"].string ?? "Question").font(.body.weight(.medium))
                ForEach(Array(row["options"].array.enumerated()), id: \.offset) { _, option in
                    let label = option["label"].string ?? "Option"
                    Button {
                        answers[id] = [label]
                    } label: {
                        HStack {
                            VStack(alignment: .leading, spacing: 4) {
                                Text(label)
                                if let detail = option["description"].string { Text(detail).font(.caption).foregroundStyle(.secondary) }
                            }
                            Spacer(); Image(systemName: answers[id] == [label] ? "checkmark.circle.fill" : "circle")
                        }.padding(12).frame(minHeight: 44).background(PikaTheme.background, in: RoundedRectangle(cornerRadius: 12))
                    }.disabled(submitted).accessibilityIdentifier("questionOption:" + id + ":" + label)
                }
                // The provider's request accepts answer strings, including custom
                // input. Do not invent approval controls for other request types.
                if row["isSecret"].bool {
                    SecureField("Your answer", text: Binding(get: { answers[id]?.first ?? "" }, set: { answers[id] = [$0] })).disabled(submitted)
                } else {
                    TextField("Your answer", text: Binding(get: { answers[id]?.first ?? "" }, set: { answers[id] = [$0] }), axis: .vertical)
                        .textFieldStyle(.roundedBorder).disabled(submitted)
                }
            }
            Button(submitted ? "Awaiting resolution" : "Submit answer") {
                guard !submitted else { return }
                submitted = true
                Task {
                    await model.answer(question, identity: identity, answers: answers)
                    submitted = model.pendingActions["answer:" + identity.draftKey + ":" + question.id] != nil
                }
            }.buttonStyle(PikaPrimaryButtonStyle()).disabled(submitted || answers.count != question.questions.array.count || answers.values.contains(where: { $0.first?.isEmpty != false }))
                .accessibilityIdentifier("submitAnswer")
        }.padding(16).background(PikaTheme.sheet, in: RoundedRectangle(cornerRadius: 18))
            .onAppear { submitted = model.pendingActions["answer:" + identity.draftKey + ":" + question.id] != nil }
    }
}
