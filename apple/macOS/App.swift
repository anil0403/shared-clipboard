import AppKit
import Combine
import ServiceManagement
import SwiftUI

/// A menu bar app: no Dock icon (LSUIElement), no main window.
@main
struct SharedClipApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        MenuBarExtra("Shared Clipboard", systemImage: "doc.on.clipboard") {
            MenuContent()
        }
    }
}

/// Starts the node, watches the clipboard and asks about pairing codes.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var model: ClipModel { .shared }
    private var lastChange = NSPasteboard.general.changeCount
    private var activity: NSObjectProtocol?
    private var subscriptions: Set<AnyCancellable> = []

    func applicationDidFinishLaunching(_ notification: Notification) {
        // The app is always in the background; without this, App Nap would slow the clipboard timer down.
        activity = ProcessInfo.processInfo.beginActivity(
            options: .userInitiatedAllowingIdleSystemSleep, reason: "Watching the clipboard")
        // macOS has no clipboard-changed event: check the change count twice a second (plan.md section 8).
        Timer.publish(every: 0.5, tolerance: 0.05, on: .main, in: .common).autoconnect()
            .sink { [weak self] _ in self?.check() }
            .store(in: &subscriptions)
        // RunLoop, not DispatchQueue: the alert's modal loop would otherwise hold up the main queue.
        model.$pairingCode.compactMap { $0 }.receive(on: RunLoop.main)
            .sink { [weak self] in self?.ask($0) }
            .store(in: &subscriptions)
        Task { await model.start() }
    }

    private func check() {
        let count = NSPasteboard.general.changeCount
        guard count != lastChange else { return }
        lastChange = count
        guard !model.paused, let item = Clipboard.read() else { return }
        Task { await model.send(item.formats, item.files) }
    }

    private func ask(_ request: PairingCode) {
        let alert = NSAlert()
        alert.messageText = "Pair with \(request.device.name)?"
        alert.informativeText = "Pair only if \(request.device.name) shows the same code:\n\n\(request.code)"
        alert.addButton(withTitle: "Pair")
        alert.addButton(withTitle: "Don't Pair")
        NSApp.activate()
        model.confirm(request, alert.runModal() == .alertFirstButtonReturn)
    }
}

/// The pairing window, made fresh (with a new code) each time it's opened.
@MainActor
enum PairingWindow {
    private static var window: NSWindow?

    static func show() {
        if window?.isVisible != true {
            let new = NSWindow(contentViewController: NSHostingController(rootView: PairingView()))
            new.title = "Pair a Device"
            new.isReleasedWhenClosed = false
            new.center()
            window = new
        }
        window?.makeKeyAndOrderFront(nil)
        NSApp.activate()
    }
}

struct MenuContent: View {
    @ObservedObject private var model = ClipModel.shared
    @State private var atLogin = SMAppService.mainApp.status == .enabled

    var body: some View {
        if let message = model.message {
            Text(message)
        }
        #if compiler(>=6.1) // Xcode 16.3, the first with the macOS 15.4 SDK
        if #available(macOS 15.4, *) {
            let behavior = NSPasteboard.general.accessBehavior
            if behavior == .ask || behavior == .alwaysDeny {
                Text("To send what you copy, allow Shared Clipboard to paste in System Settings > Privacy & Security.")
            }
        }
        #endif
        Section("Devices") {
            if model.devices.isEmpty {
                Text("No paired devices")
            }
            ForEach(model.devices, id: \.id) { device in
                Menu(device.online ? device.name : "\(device.name) (offline)") {
                    Button("Unpair") { Task { await model.unpair(device.id) } }
                }
            }
        }
        if !model.offers.isEmpty {
            Section("Click to Download") {
                ForEach(model.offers, id: \.id) { offer in
                    Button(offer.summary) { Task { await model.fetch(offer) } }
                }
            }
        }
        if !model.history.isEmpty {
            Section("History") {
                ForEach(model.history.prefix(10), id: \.id) { clip in
                    Button(clip.summary) { Task { await model.copy(clip) } }
                }
            }
        }
        Divider()
        Button("Pair a Device…") { PairingWindow.show() }
        Toggle("Pause", isOn: $model.paused)
        Toggle("Open at Login", isOn: $atLogin)
            .onChange(of: atLogin) { _, on in
                do {
                    if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
                } catch {
                    model.message = error.localizedDescription
                }
            }
        Divider()
        Button("Quit Shared Clipboard") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}
