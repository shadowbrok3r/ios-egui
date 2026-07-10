import UIKit
@preconcurrency import UserNotifications

/// Stateless host capabilities: share sheet, notifications, haptics, and URL opening.
@MainActor
enum Capabilities {
    /// Find the top-most view controller to present from, starting at a view's window.
    static func presenter(from view: UIView?) -> UIViewController? {
        var vc = view?.window?.rootViewController
            ?? UIApplication.shared.connectedScenes
                .compactMap { ($0 as? UIWindowScene)?.keyWindow }
                .first?.rootViewController
        while let presented = vc?.presentedViewController {
            vc = presented
        }
        return vc
    }

    static func share(path: String, from view: UIView?) {
        guard !path.isEmpty else { return }
        let url = URL(fileURLWithPath: path)
        let av = UIActivityViewController(activityItems: [url], applicationActivities: nil)
        if let pop = av.popoverPresentationController {
            pop.sourceView = view
            pop.sourceRect = CGRect(x: (view?.bounds.midX ?? 0), y: (view?.bounds.midY ?? 0), width: 0, height: 0)
        }
        presenter(from: view)?.present(av, animated: true)
    }

    static func shareText(_ text: String, from view: UIView?) {
        guard !text.isEmpty else { return }
        let av = UIActivityViewController(activityItems: [text], applicationActivities: nil)
        if let pop = av.popoverPresentationController {
            pop.sourceView = view
            pop.sourceRect = CGRect(x: (view?.bounds.midX ?? 0), y: (view?.bounds.midY ?? 0), width: 0, height: 0)
        }
        presenter(from: view)?.present(av, animated: true)
    }

    static func openURL(_ string: String) {
        guard let url = URL(string: string) else { return }
        UIApplication.shared.open(url)
    }

    static func notify(title: String, body: String) {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
            guard granted else { return }
            let content = UNMutableNotificationContent()
            content.title = title
            content.body = body
            let request = UNNotificationRequest(
                identifier: UUID().uuidString,
                content: content,
                trigger: nil
            )
            center.add(request)
        }
    }

    static func haptic(_ kind: Int32) {
        switch kind {
        case 0: UIImpactFeedbackGenerator(style: .light).impactOccurred()
        case 1: UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        case 2: UIImpactFeedbackGenerator(style: .heavy).impactOccurred()
        case 3: UINotificationFeedbackGenerator().notificationOccurred(.success)
        case 4: UINotificationFeedbackGenerator().notificationOccurred(.warning)
        case 5: UINotificationFeedbackGenerator().notificationOccurred(.error)
        case 6: UISelectionFeedbackGenerator().selectionChanged()
        default: break
        }
    }
}
