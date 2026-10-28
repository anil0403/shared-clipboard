import AppIntents
import SwiftUI
import UniformTypeIdentifiers

@main
struct SharedClipApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}

struct ContentView: View {
    @ObservedObject private var model = ClipModel.shared
    @Environment(\.scenePhase) private var phase

    var body: some View {
        NavigationStack {
            List {
                // iOS asks before an app reads the clipboard, except through the system's Paste button.
                Section {
                    PasteButton(supportedContentTypes: [.plainText, .url, .image, .data]) { providers in
                        Task { @MainActor in await model.send(providers) }
                    }
                } header: {
                    Text("Send clipboard")
                } footer: {
                    if let message = model.message { Text(message) }
                }
                Section {
                    if model.devices.isEmpty {
                        Text("No paired devices yet. Tap Pair.")
                    }
                    ForEach(model.devices, id: \.id) { device in
                        LabeledContent(device.name, value: device.online ? "Connected" : "Offline")
                    }
                    .onDelete { rows in
                        let ids = rows.map { model.devices[$0].id }
                        Task { for id in ids { await model.unpair(id) } }
                    }
                } header: {
                    HStack {
                        Text("Devices")
                        Spacer()
                        NavigationLink("Pair") { PairingView() }
                            .textCase(nil)
                    }
                }
                if !model.offers.isEmpty {
                    Section("Tap to download") {
                        ForEach(model.offers, id: \.id) { offer in
                            Button(offer.summary) { Task { await model.fetch(offer) } }
                        }
                    }
                }
                Section("History (tap to copy)") {
                    // A long history scrolls inside its own panel, so the rest of the screen stays in reach.
                    if model.history.count > Self.historyRows {
                        ScrollView {
                            LazyVStack(spacing: 0) {
                                ForEach(model.history, id: \.id) { clip in
                                    historyRow(clip)
                                    Divider()
                                }
                            }
                        }
                        .frame(height: CGFloat(Self.historyRows) * 44)
                    } else {
                        ForEach(model.history, id: \.id) { clip in
                            Button(clip.summary) { Task { await model.copy(clip) } }
                        }
                    }
                }
                Toggle("Pause", isOn: $model.paused)
            }
            .navigationTitle("Shared Clipboard")
        }
        // iOS stops the app's connections in the background: start over whenever it comes back, and sync the
        // clipboard both ways.
        .onChange(of: phase, initial: true) { _, now in
            if now == .active { Task { await model.open() } }
        }
        .alert("Compare codes", isPresented: $model.askingCode, presenting: model.pairingCode) { request in
            Button("Pair") { model.confirm(request, true) }
            Button("Don't Pair", role: .cancel) { model.confirm(request, false) }
        } message: { request in
            Text("Pair only if \(request.device.name) shows the same code:\n\n\(request.code)")
        }
    }

    /// How many history items show before the history scrolls on its own.
    private static let historyRows = 6

    /// One line per item, so the panel's height fits a whole number of rows.
    private func historyRow(_ clip: Summary) -> some View {
        Button { Task { await model.copy(clip) } } label: {
            Text(clip.summary)
                .lineLimit(1)
                .frame(maxWidth: .infinity, minHeight: 43, alignment: .leading)
                .contentShape(Rectangle())
        }
        // Without this, a list row with many buttons fires them all on one tap.
        .buttonStyle(.borderless)
    }
}

/// Siri and Shortcuts phrases; the actions also work with Back Tap and the Action Button.
struct Shortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: SendClipboardIntent(),
            phrases: ["Send clipboard with \(.applicationName)"],
            shortTitle: "Send Clipboard",
            systemImageName: "doc.on.clipboard"
        )
        AppShortcut(
            intent: GetLatestClipIntent(),
            phrases: ["Get the latest clip from \(.applicationName)"],
            shortTitle: "Get Latest Clip",
            systemImageName: "arrow.down.doc"
        )
    }
}
