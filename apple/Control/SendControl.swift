import AppIntents
import SwiftUI
import WidgetKit

@main
struct Controls: WidgetBundle {
    var body: some Widget {
        SendClipboardControl()
    }
}

/// A Control Center button (also for the Lock Screen and Action Button) that opens the app and sends the clipboard.
struct SendClipboardControl: ControlWidget {
    var body: some ControlWidgetConfiguration {
        StaticControlConfiguration(kind: "com.noobsbot.sharedclip.send") {
            ControlWidgetButton(action: SendClipboardIntent()) {
                Label("Send Clipboard", systemImage: "doc.on.clipboard")
            }
        }
        .displayName("Send Clipboard")
        .description("Opens Shared Clipboard and sends what you copied.")
    }
}
