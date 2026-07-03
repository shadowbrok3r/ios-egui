import SwiftUI
import EguiKit

@main
struct PluginsIosApp: App {
    var body: some Scene {
        WindowGroup {
            EguiAppView()
                .ignoresSafeArea()
        }
    }
}
