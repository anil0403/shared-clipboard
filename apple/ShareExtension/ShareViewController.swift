import UIKit

/// "Shared Clipboard" in the share sheet: sends the shared text, links, images or files to the paired devices, then
/// closes. It runs a short-lived node of its own, with the app's keys and paired devices from the App Group folder.
///
/// Risks, untested until it runs on a device:
/// - Share extensions get about 120 MB of memory; the node and large images must fit.
/// - If the app is running too, two nodes use the same identity and data folder at once.
/// - Large files may need longer than the 3 s it waits after `copy`.
final class ShareViewController: UIViewController {
    private let label = UILabel()

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        label.text = "Sending…"
        label.textAlignment = .center
        label.numberOfLines = 0
        label.frame = view.bounds.insetBy(dx: 20, dy: 20)
        label.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(label)
        let items = extensionContext?.inputItems as? [NSExtensionItem] ?? []
        let providers = items.flatMap { $0.attachments ?? [] }
        Task { await send(providers) }
    }

    private func send(_ providers: [NSItemProvider]) async {
        let model = ClipModel.shared
        model.writesClipboard = false // the extension mustn't replace what the user copied
        await model.start()
        await model.waitForDevices()
        await model.send(providers)
        if let message = model.message { label.text = message }
        // `copy` only starts sending; stop() would cut it off.
        // ponytail: a fixed 3 s wait; switch to a core call that waits for delivery if one is added.
        try? await Task.sleep(nanoseconds: 3_000_000_000)
        await model.stop()
        extensionContext?.completeRequest(returningItems: nil, completionHandler: nil)
    }
}
