import SwiftUI

struct DexSignals: View {
    var states: Set<String>
    var body: some View {
        HStack(spacing: 8) {
            ForEach(["Needs you", "Working", "Ready"], id: \.self) { state in
                Circle().fill(PikaTheme.color(state).gradient).frame(width: 9, height: 9)
                    .overlay(Circle().strokeBorder(PikaTheme.seam.opacity(0.6), lineWidth: 0.5))
                    .opacity(states.contains(state) ? 1 : 0.2)
                    .shadow(color: states.contains(state) ? PikaTheme.color(state).opacity(0.6) : .clear, radius: 2)
            }
        }.accessibilityElement(children: .ignore)
            .accessibilityLabel("Thread signal")
            .accessibilityValue(states.sorted().joined(separator: ", ").isEmpty ? "Inactive" : states.sorted().joined(separator: ", "))
            .accessibilityIdentifier("threadSignal")
    }
}

/// Decoration at the physical bottom edge, without consuming layout space.
struct DexBottomRim: View {
    var body: some View {
        GeometryReader { _ in
            VStack(spacing: 0) {
                Spacer(minLength: 0)
                DexLowerCasing().fill(PikaTheme.shell).frame(height: 82)
            }
        }.ignoresSafeArea(.container, edges: .bottom)
            .allowsHitTesting(false).accessibilityHidden(true)
    }
}

/// Device-like decoration only; it owns no board, connection or navigation state.
struct DexHeader: View {
    var add: (() -> Void)?
    var connections: (() -> Void)?
    var states: Set<String> = []
    var body: some View {
        ZStack(alignment: .topLeading) {
            LinearGradient(colors: [PikaTheme.shellHighlight, PikaTheme.shell], startPoint: .topLeading, endPoint: .bottomTrailing)
                .ignoresSafeArea(edges: .top)
            DexShoulder().fill(PikaTheme.seam).offset(y: -3)
            DexShoulder().fill(PikaTheme.background)
            HStack(spacing: 10) {
                if let connections {
                    Button(action: connections) { DexLens().frame(width: 48, height: 48) }
                        .accessibilityLabel("Machine connections").padding(.trailing, 4)
                } else {
                    DexLens().frame(width: 48, height: 48).padding(.trailing, 4).accessibilityHidden(true)
                }
                ForEach(["Needs you", "Working", "Ready"], id: \.self) { state in
                    Circle().fill(PikaTheme.color(state).gradient).frame(width: 11, height: 11)
                        .overlay(Circle().strokeBorder(PikaTheme.seam.opacity(0.8), lineWidth: 1))
                        .opacity(states.contains(state) ? 1 : 0.25)
                        .shadow(color: states.contains(state) ? PikaTheme.color(state).opacity(0.6) : .clear, radius: 3)
                        .accessibilityLabel("\(state): \(states.contains(state) ? "reported" : "not reported")")
                }
                Spacer(minLength: 4)
                HStack(spacing: 2) {
                    PikaMark(size: 32)
                    Text("Pika").font(.system(size: 27, weight: .bold, design: .rounded)).tracking(-1)
                }.fixedSize()
                Spacer(minLength: 4)
                if let add {
                    Button(action: add) {
                        Image(systemName: "plus").font(.system(size: 24, weight: .medium))
                            .foregroundStyle(.white).frame(width: 44, height: 44)
                            .background(LinearGradient(colors: [PikaTheme.shellHighlight, PikaTheme.shell], startPoint: .topLeading, endPoint: .bottomTrailing), in: Circle())
                            .overlay(Circle().strokeBorder(PikaTheme.seam, lineWidth: 1))
                    }
                        .accessibilityLabel("Add a thread").accessibilityIdentifier("addThread")
                }
            }.padding(.horizontal, 16).padding(.top, 10)
        }.frame(height: 76)
    }
}

struct DexShoulder: Shape {
    var compact = false
    func path(in rect: CGRect) -> Path {
        let w = rect.width
        let h = rect.height
        if compact {
            // A fixed signal bay keeps the seam clear of the dots on every width.
            var p = Path()
            p.move(to: CGPoint(x: 0, y: h))
            p.addQuadCurve(to: CGPoint(x: 14, y: h - 7), control: CGPoint(x: 0, y: h - 7))
            p.addLine(to: CGPoint(x: 55, y: h - 7))
            p.addCurve(to: CGPoint(x: 88, y: 0), control1: CGPoint(x: 73, y: h - 7), control2: CGPoint(x: 72, y: 0))
            p.addLine(to: CGPoint(x: w - 16, y: 0))
            p.addQuadCurve(to: CGPoint(x: w, y: 8), control: CGPoint(x: w, y: 0))
            p.addLine(to: CGPoint(x: w, y: h))
            p.closeSubpath()
            return p
        }
        let shelf: CGFloat = compact ? 0.08 : 0.29
        let end: CGFloat = compact ? 0.19 : 0.47
        var p = Path()
        p.move(to: CGPoint(x: 0, y: h))
        p.addQuadCurve(to: CGPoint(x: 16, y: h * 0.84), control: CGPoint(x: 0, y: h * 0.84))
        p.addLine(to: CGPoint(x: w * shelf, y: h * 0.84))
        p.addCurve(to: CGPoint(x: w * end, y: h * 0.10), control1: CGPoint(x: w * (shelf + 0.07), y: h * 0.84), control2: CGPoint(x: w * (end - 0.06), y: h * 0.10))
        p.addLine(to: CGPoint(x: w - 18, y: h * 0.10))
        p.addQuadCurve(to: CGPoint(x: w, y: h * 0.24), control: CGPoint(x: w, y: h * 0.10))
        p.addLine(to: CGPoint(x: w, y: rect.height))
        p.addLine(to: CGPoint(x: 0, y: rect.height))
        p.closeSubpath()
        return p
    }
}

struct DexLens: View {
    var body: some View {
        Circle().fill(RadialGradient(colors: [Color(red: 0.22, green: 0.70, blue: 0.93), Color(red: 0.01, green: 0.36, blue: 0.58), Color(red: 0.01, green: 0.19, blue: 0.30)], center: .topLeading, startRadius: 1, endRadius: 50))
            .overlay(alignment: .topLeading) { Circle().fill(.white.opacity(0.9)).frame(width: 9, height: 9).offset(x: 11, y: 9) }
            .overlay(Circle().strokeBorder(Color.white.opacity(0.9), lineWidth: 3))
            .padding(1.5).background(Circle().fill(Color(red: 0.36, green: 0.37, blue: 0.34)))
            .shadow(color: .black.opacity(0.25), radius: 1, y: 2)
    }
}

struct DexMark: View {
    var body: some View {
        RoundedRectangle(cornerRadius: 5).fill(PikaTheme.shell.gradient)
            .overlay(alignment: .topLeading) {
                Circle().fill(Color.cyan.gradient).frame(width: 7, height: 7)
                    .overlay(Circle().strokeBorder(.white, lineWidth: 1)).padding(3)
            }
            .overlay(alignment: .bottom) {
                RoundedRectangle(cornerRadius: 2).fill(Color(red: 0.95, green: 0.91, blue: 0.80))
                    .frame(width: 16, height: 12).padding(.bottom, 4)
            }.frame(width: 25, height: 31).accessibilityHidden(true)
    }
}

struct DexTabBar: View {
    @Binding var selection: Int
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var tailAngle = 0.0
    @State private var flick: Task<Void, Never>?
    var body: some View {
        HStack(spacing: 0) {
            tab("Dex", index: 0) { DexMark() }
            tab("Pika", index: 1) {
                PikaMark(size: 32).opacity(selection == 1 ? 1 : 0.7)
                    .rotationEffect(.degrees(tailAngle), anchor: .bottom)
            }
        }.padding(.top, 10).padding(.bottom, 8)
            .background(PikaTheme.background)
            .overlay(alignment: .top) { Rectangle().fill(PikaTheme.border).frame(height: 0.5) }
            .onAppear { if selection == 1 { animateTail() } }
            .onChange(of: reduceMotion) { _, reduced in if reduced { flick?.cancel(); tailAngle = 0 } }
            .onDisappear { flick?.cancel(); tailAngle = 0 }
    }
    private func tab<Icon: View>(_ title: String, index: Int, @ViewBuilder icon: () -> Icon) -> some View {
        Button {
            if selection == index, index == 1 { animateTail() }
            selection = index
        } label: {
            VStack(spacing: 4) { icon().frame(width: 34, height: 34); Text(title).font(.caption.weight(selection == index ? .semibold : .regular)) }
                .frame(maxWidth: .infinity).frame(minHeight: 54)
                .foregroundStyle(selection == index ? PikaTheme.accent : .secondary)
        }.accessibilityIdentifier(index == 0 ? "dexTab" : "pikaTab")
            .accessibilityAddTraits(selection == index ? .isSelected : [])
    }
    private func animateTail() {
        flick?.cancel(); tailAngle = 0
        guard !reduceMotion else { return }
        flick = Task { @MainActor in
            for angle in [-15.0, 12, -6, 0] {
                guard !Task.isCancelled, !reduceMotion else { return }
                withAnimation(.easeInOut(duration: 0.09)) { tailAngle = angle }
                do { try await Task.sleep(for: .milliseconds(90)) } catch { return }
            }
        }
    }
}

private struct DexLowerCasing: Shape {
    func path(in rect: CGRect) -> Path {
        var p = Path()
        p.move(to: CGPoint(x: rect.maxX, y: 0))
        p.addCurve(to: CGPoint(x: rect.maxX - 12, y: 30), control1: CGPoint(x: rect.maxX, y: 12), control2: CGPoint(x: rect.maxX - 12, y: 14))
        p.addQuadCurve(to: CGPoint(x: rect.maxX - 26, y: rect.maxY - 8), control: CGPoint(x: rect.maxX - 12, y: rect.maxY - 8))
        p.addLine(to: CGPoint(x: 0, y: rect.maxY - 8))
        p.addLine(to: CGPoint(x: 0, y: rect.maxY))
        p.addLine(to: CGPoint(x: rect.maxX, y: rect.maxY)); p.closeSubpath()
        return p
    }
}

struct PikaMark: View {
    var size: CGFloat = 22
    var body: some View {
        Image("PikaMark").renderingMode(.original).resizable().scaledToFit()
            .frame(width: size, height: size)
            .accessibilityHidden(true)
    }
}

struct NoticeCard: View {
    @EnvironmentObject private var model: AppModel
    var body: some View {
        if let notice = model.notice {
            VStack(alignment: .leading, spacing: 8) {
                Text(notice).font(.callout)
                if let details = model.noticeDetails, details != notice {
                    DisclosureGroup("Details") {
                        Text(details).font(.caption.monospaced()).textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }.font(.caption)
                }
            }.foregroundStyle(.secondary).padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(PikaTheme.sheet, in: RoundedRectangle(cornerRadius: 12))
        }
    }
}

enum PikaTheme {
    static let accent = adaptive(light: 0xAD332D, dark: 0xFF9B85)
    static let shell = Color(red: 0.88, green: 0.27, blue: 0.23)
    static let shellHighlight = Color(red: 0.96, green: 0.38, blue: 0.32)
    static let seam = Color(red: 0.56, green: 0.22, blue: 0.18)
    static let border = adaptive(light: 0xDCD6CB, dark: 0x494238)
    static let buttonFill = Color(red: 1, green: 188.0 / 255, blue: 22.0 / 255)
    static let buttonText = Color(red: 0.161, green: 0.145, blue: 0.122)
    static let attention = adaptive(light: 0xB83C32, dark: 0xFF988B)
    static let ready = adaptive(light: 0x28743C, dark: 0x90D596)
    static let working = adaptive(light: 0x916000, dark: 0xF5C15D)
    static let background = adaptive(light: 0xF5F1E7, dark: 0x211E19)
    static let sheet = adaptive(light: 0xFCF9F2, dark: 0x302B24)
    private static func adaptive(light: UInt32, dark: UInt32) -> Color {
        Color(uiColor: UIColor { traits in
            let rgb = traits.userInterfaceStyle == .dark ? dark : light
            return UIColor(red: CGFloat((rgb >> 16) & 255) / 255,
                           green: CGFloat((rgb >> 8) & 255) / 255,
                           blue: CGFloat(rgb & 255) / 255, alpha: 1)
        })
    }
    static func state(_ raw: String) -> String {
        let normalized = raw.lowercased().replacingOccurrences(of: "_", with: " ")
        switch normalized {
        case "needs you", "attention": return "Needs you"
        case "ready": return "Ready"
        case "working": return "Working"
        case "parked": return "Parked"
        case "assistant": return "Your assistant"
        case "cached", "stale": return "Cached"
        default: return raw
        }
    }
    static func color(_ state: String) -> Color {
        switch self.state(state) {
        case "Needs you": return attention
        case "Ready": return ready
        case "Working": return working
        default: return .secondary
        }
    }
}

struct PikaPrimaryButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var enabled
    func makeBody(configuration: Configuration) -> some View {
        configuration.label.padding(.horizontal, 16).padding(.vertical, 10)
            .foregroundStyle(PikaTheme.buttonText)
            .background(PikaTheme.buttonFill.opacity(enabled ? (configuration.isPressed ? 0.75 : 1) : 0.4), in: Capsule())
            .opacity(enabled ? 1 : 0.6)
    }
}

struct ProviderMark: View {
    let provider: String
    var size: CGFloat = 34
    var body: some View {
        Image(asset).resizable().scaledToFit().padding(7)
            .frame(width: size, height: size)
            .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 10))
            .foregroundStyle(.primary)
            .accessibilityLabel(provider)
    }
    private var asset: String {
        switch provider.lowercased() {
        case "codex", "openai": return "ProviderOpenAI"
        case "claude": return "ProviderClaude"
        case "opencode": return "ProviderOpenCode"
        case "muse": return "ProviderMuse"
        default: return "PikaMark"
        }
    }
}
