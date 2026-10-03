import SwiftUI

@main
struct PikaApp: App {
    @StateObject private var model = AppModel()
    @Environment(\.scenePhase) private var phase
    var body: some Scene {
        WindowGroup {
            RootView().environmentObject(model).tint(PikaTheme.accent)
                .preferredColorScheme(.light)
                .task { model.resume() }
                .onChange(of: phase) { _, value in
                    if value == .active { model.resume() }
                    else if value == .background { model.suspend() }
                }
        }
    }
}
