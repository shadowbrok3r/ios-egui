import SwiftUI
import EguiKit

@main
struct HelloApp: App {
    var body: some Scene {
        WindowGroup {
            EguiAppView()
                .ignoresSafeArea()
        }
    }
}
