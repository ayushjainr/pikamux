import SwiftUI
import UIKit

/// UIKit's real text system preserves selection, dictation and marked IME text.
struct NativeComposer: UIViewRepresentable {
    @Binding var text: String
    @Binding var composing: Bool
    let label: String
    let send: () -> Void
    func makeCoordinator() -> Coordinator { Coordinator(self) }
    func makeUIView(context: Context) -> ComposerTextView {
        let view = ComposerTextView()
        view.delegate = context.coordinator
        view.font = .preferredFont(forTextStyle: .body)
        view.adjustsFontForContentSizeCategory = true
        view.backgroundColor = .clear
        view.textContainerInset = UIEdgeInsets(top: 10, left: 4, bottom: 10, right: 4)
        view.returnKeyType = .default
        view.accessibilityIdentifier = "composer"
        view.shortcutSend = { [weak view] in
            guard view?.markedTextRange == nil else { return }
            context.coordinator.parent.send()
        }
        return view
    }
    func updateUIView(_ view: ComposerTextView, context: Context) {
        context.coordinator.parent = self
        context.coordinator.updating = true
        defer { context.coordinator.updating = false }
        view.accessibilityLabel = label
        // Updates elsewhere must not replace a live editor or move its caret.
        if view.text != text, view.markedTextRange == nil { view.text = text }
    }
    func sizeThatFits(_ proposal: ProposedViewSize, uiView: ComposerTextView, context: Context) -> CGSize? {
        let width = proposal.width ?? 300
        let limit = max(140, (uiView.font?.lineHeight ?? 20) * 5 + 20)
        // Large pasted drafts already need the scrolling viewport. Measuring
        // their entire document at infinite height can block the main thread
        // before the result is clamped, particularly for long Unicode lines.
        if uiView.text.utf8.count > 4096 {
            uiView.isScrollEnabled = true
            return CGSize(width: width, height: limit)
        }
        let ideal = uiView.sizeThatFits(CGSize(width: width, height: .greatestFiniteMagnitude))
        uiView.isScrollEnabled = ideal.height > limit
        return CGSize(width: width, height: min(max(46, ideal.height), limit))
    }
    final class Coordinator: NSObject, UITextViewDelegate {
        var parent: NativeComposer
        var updating = false
        init(_ parent: NativeComposer) { self.parent = parent }
        func textViewDidChange(_ textView: UITextView) {
            guard !updating else { return }
            if parent.text != textView.text { parent.text = textView.text }
            let marked = textView.markedTextRange != nil
            if parent.composing != marked { parent.composing = marked }
            textView.invalidateIntrinsicContentSize()
        }
        func textViewDidChangeSelection(_ textView: UITextView) {
            guard !updating else { return }
            let marked = textView.markedTextRange != nil
            if parent.composing != marked { parent.composing = marked }
        }
    }
}

final class ComposerTextView: UITextView {
    var shortcutSend: (() -> Void)?
    override var keyCommands: [UIKeyCommand]? {
        [UIKeyCommand(input: "\r", modifierFlags: .command, action: #selector(explicitSend)),
         UIKeyCommand(input: "\r", modifierFlags: .control, action: #selector(explicitSend))]
    }
    @objc private func explicitSend() {
        guard markedTextRange == nil else { return }
        shortcutSend?()
    }
}
