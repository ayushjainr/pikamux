import SwiftUI

struct RootView: View {
    @EnvironmentObject private var model: AppModel
    @State private var assistant: BoardItem?
    @State private var selectedTab = 0
    var body: some View {
        TabView(selection: $selectedTab) {
            NavigationStack {
                BoardView().safeAreaInset(edge: .bottom, spacing: 0) { DexTabBar(selection: $selectedTab) }
            }
                .tag(0)
            NavigationStack {
                VStack(spacing: 20) {
                    TestContextBanner()
                    PikaMark(size: 40).foregroundStyle(PikaTheme.accent)
                    Text("Your existing Pika").font(.title2.bold())
                    Text("This view attaches to the machine's existing assistant. It never creates a mobile substitute.")
                        .foregroundStyle(.secondary).multilineTextAlignment(.center)
                    Button("Open Pika") { Task { assistant = await model.openAssistant() } }.buttonStyle(PikaPrimaryButtonStyle())
                    NoticeCard()
                }.padding(28).frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(PikaTheme.background)
                    .safeAreaInset(edge: .top, spacing: 0) { DexHeader() }
                    .safeAreaInset(edge: .bottom, spacing: 0) { DexTabBar(selection: $selectedTab) }
                    .toolbar(.hidden, for: .navigationBar)
                    .toolbar(.hidden, for: .tabBar)
                    .navigationDestination(item: $assistant) { item in ThreadView(item: item) }
            }.tag(1)
        }
        .toolbar(.hidden, for: .tabBar)
        .overlay { DexBottomRim() }
    }
}

struct TestContextBanner: View {
    @EnvironmentObject private var model: AppModel
    var body: some View {
        Group {
            if model.isFixture {
                Text("UI TEST FIXTURE · no machine or provider connected")
                    .font(.caption2.bold()).frame(maxWidth: .infinity).padding(8)
                    .background(Color.orange.opacity(0.15)).accessibilityIdentifier("fixtureBanner")
            }
            #if DEBUG
            if !model.isFixture && ProcessInfo.processInfo.arguments.contains("--ssh-integration-test") {
                Text("DISPOSABLE SSH INTEGRATION · synthetic model, no quota")
                    .font(.caption2.bold()).frame(maxWidth: .infinity).padding(8).background(Color.orange.opacity(0.15))
                    .accessibilityIdentifier("sshIntegrationBanner")
            }
            #endif
        }.dynamicTypeSize(...DynamicTypeSize.xxxLarge)
    }
}
