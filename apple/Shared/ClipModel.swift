import Foundation
#if os(iOS)
import UIKit
#endif

// The core's types (Node, Clip, Device, ...) come from Generated/SharedClipCore.swift, compiled into this module.
// It also declares an `Error` enum, which hides Swift's `Error` here: don't write `Error` in this module.

/// A code to compare with the one the other device shows (`Events.onPairingCode`).
struct PairingCode {
    let device: Device
    let code: String
}

/// The core node and what the UI shows. One per process, on the main thread.
@MainActor
final class ClipModel: ObservableObject {
    static let shared = ClipModel()

    @Published private(set) var node: Node?
    @Published var devices: [Device] = []
    @Published var history: [Summary] = []
    @Published var offers: [Offer] = []
    /// Devices Bonjour sees now, paired or not: id → name.
    @Published var nearby: [String: String] = [:]
    @Published var pairingCode: PairingCode? {
        didSet { askingCode = pairingCode != nil }
    }
    @Published var askingCode = false
    /// The last error, for the user.
    @Published var message: String?
    @Published var paused = false {
        didSet { node?.setPaused(paused: paused) }
    }
    /// The Share Extension turns this off: it shouldn't replace the user's clipboard.
    var writesClipboard = true

    private var bonjour: Bonjour?
    private var starting: Task<Void, Never>?

    /// Starts the node. If it's running, looks for devices again: on iOS the connections may be gone after the app
    /// was in the background.
    func start() async {
        if let node {
            browse(node)
            return
        }
        if starting == nil {
            starting = Task { await boot() }
        }
        await starting?.value
        starting = nil
    }

    func stop() async {
        bonjour = nil
        await node?.stop()
        node = nil
    }

    /// Sends what was copied on this device to the paired devices.
    func send(_ formats: [Format], _ files: [File]) async {
        guard let node, !(formats.isEmpty && files.isEmpty) else { return }
        do {
            try await node.copy(formats: formats, files: files)
        } catch {
            message = error.localizedDescription
        }
        history = node.history()
    }

    #if os(iOS)
    /// Sends pasted or shared items.
    func send(_ providers: [NSItemProvider]) async {
        let item = await Clipboard.load(providers)
        await send(item.formats, item.files)
    }
    #endif

    func pair(link: String) async {
        await attempt { _ = try await $0.pairLink(link: link.trimmingCharacters(in: .whitespacesAndNewlines)) }
    }

    func pair(id: String) async {
        await attempt { _ = try await $0.pairDevice(id: id) }
    }

    func confirm(_ request: PairingCode, _ accept: Bool) {
        node?.confirmPairing(id: request.device.id, accept: accept)
        pairingCode = nil
    }

    func unpair(_ id: String) async {
        await attempt { try await $0.unpair(id: id) }
    }

    /// Gets an item that was only announced, and puts it on the clipboard.
    func fetch(_ offer: Offer) async {
        guard let node else { return }
        do {
            received(try await node.fetch(id: offer.id))
            offers.removeAll { $0.id == offer.id }
        } catch {
            message = error.localizedDescription
        }
    }

    /// Puts a history item back on the clipboard.
    func copy(_ item: Summary) async {
        guard let node else { return }
        do {
            Clipboard.write(try await node.item(id: item.id))
        } catch {
            message = error.localizedDescription
        }
    }

    /// Waits until every paired device is connected, or `seconds` pass.
    func waitForDevices(seconds: Double = 3) async {
        let end = Date().addingTimeInterval(seconds)
        while Date() < end, !devices.allSatisfy(\.online) {
            try? await Task.sleep(nanoseconds: 200_000_000)
        }
    }

    fileprivate func received(_ clip: Clip) {
        if writesClipboard { Clipboard.write(clip) }
        history = node?.history() ?? []
    }

    private func boot() async {
        do {
            try FileManager.default.createDirectory(at: Self.dataDir, withIntermediateDirectories: true)
            let config = Config(dataDir: Self.dataDir.path, name: Self.deviceName, platform: Self.platform, port: 0)
            let node = try await Node.start(config: config, events: CoreEvents())
            node.setPaused(paused: paused)
            self.node = node
            devices = node.devices()
            history = node.history()
            browse(node)
        } catch {
            message = error.localizedDescription
        }
    }

    private func browse(_ node: Node) {
        bonjour = nil // the old one deregisters first
        bonjour = Bonjour(node: node, name: Self.deviceName, platform: Self.platformName) { [weak self] in
            self?.nearby = $0
        }
    }

    /// Runs a core call, showing its error if it fails.
    private func attempt(_ work: (Node) async throws -> Void) async {
        guard let node else { return }
        message = nil
        do {
            try await work(node)
        } catch {
            message = error.localizedDescription
        }
    }

    #if os(macOS)
    private static let platform = Platform.macos
    private static let platformName = "macos"
    private static var deviceName: String { Host.current().localizedName ?? "Mac" }
    private static var dataDir: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("com.noobsbot.sharedclip")
    }
    #else
    private static let platform = Platform.ios
    private static let platformName = "ios"
    private static var deviceName: String { UIDevice.current.name }
    /// The App Group container, so the Share Extension has the app's key and paired devices. Unsigned builds have no
    /// App Group; they use the app's own folder.
    private static var dataDir: URL {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: "group.com.noobsbot.sharedclip")
            ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
    }
    #endif
}

/// The core calls these on its own threads. Each hands its work to the main thread, in order.
final class CoreEvents: Events {
    func onClip(clip: Clip) {
        onMain { $0.received(clip) }
    }

    func onOffer(offer: Offer) {
        onMain { $0.offers.insert(offer, at: 0) }
    }

    func onDevices(devices: [Device]) {
        onMain { $0.devices = devices }
    }

    func onPairingCode(device: Device, code: String) {
        onMain { $0.pairingCode = PairingCode(device: device, code: code) }
    }

    private func onMain(_ work: @escaping @Sendable @MainActor (ClipModel) -> Void) {
        DispatchQueue.main.async {
            MainActor.assumeIsolated { work(ClipModel.shared) }
        }
    }
}

extension Summary {
    /// One line for menus and lists.
    var summary: String {
        if !files.isEmpty { return files.map(\.name).joined(separator: ", ") }
        if let text {
            return String((text.split(whereSeparator: \.isNewline).first ?? "").prefix(60))
        }
        return mimes.contains { $0.hasPrefix("image/") } ? "Image" : "Item"
    }
}

extension Offer {
    /// One line for menus and lists, with the size.
    var summary: String {
        let bytes = ByteCountFormatter.string(fromByteCount: Int64(size), countStyle: .file)
        return "\(fileNames.first ?? mimes.first ?? "Item") (\(bytes))"
    }
}
