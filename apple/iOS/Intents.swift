import AppIntents
#if !CONTROL_WIDGET
import UIKit
#endif

// Shortcuts actions, also run by the Control Center button (Control/SendControl.swift), Back Tap and the Action
// Button. Both open the app: only the app in front may use the clipboard, and only the app runs the node.
// The control's extension compiles this file with CONTROL_WIDGET set; there the intents do nothing, since the
// system runs them in the app.

/// Sends what's on the clipboard to the paired devices.
struct SendClipboardIntent: AppIntent {
    static var title: LocalizedStringResource = "Send Clipboard"
    static var description = IntentDescription("Sends what you copied to your paired devices.")
    static var openAppWhenRun: Bool = true

    @MainActor
    func perform() async throws -> some IntentResult {
        #if !CONTROL_WIDGET
        let model = ClipModel.shared
        await model.start()
        await model.waitForDevices()
        // Asks for permission unless "Paste from Other Apps" is set to Allow for this app.
        await model.send(UIPasteboard.general.itemProviders)
        #endif
        return .result()
    }
}

/// Puts the newest item from another device on the clipboard.
struct GetLatestClipIntent: AppIntent {
    static var title: LocalizedStringResource = "Get Latest Clip"
    static var description = IntentDescription("Puts the last item copied on your other devices on the clipboard.")
    static var openAppWhenRun: Bool = true

    @MainActor
    func perform() async throws -> some IntentResult {
        #if !CONTROL_WIDGET
        let model = ClipModel.shared
        await model.start()
        // Devices send their latest item when they connect.
        await model.waitForDevices()
        let ownId = model.node?.id()
        if let clip = model.history.first(where: { $0.from != ownId }) {
            Clipboard.write(clip)
        }
        #endif
        return .result()
    }
}
