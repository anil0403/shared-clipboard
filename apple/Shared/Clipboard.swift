import Foundation
import UniformTypeIdentifiers
#if os(macOS)
import AppKit
#else
import UIKit
#endif

/// The clipboard formats we send (plan.md section 6): MIME type on the wire, UTI on Apple devices.
enum Clipboard {
    static let formats = [
        (mime: "text/plain;charset=utf-8", uti: "public.utf8-plain-text"),
        (mime: "text/html", uti: "public.html"),
        (mime: "image/png", uti: "public.png"),
        (mime: "image/jpeg", uti: "public.jpeg"),
    ]

    /// The UTI to write a received format as. Any `text/plain` is UTF-8 text.
    static func uti(_ mime: String) -> String? {
        mime.hasPrefix("text/plain") ? "public.utf8-plain-text" : formats.first { $0.mime == mime }?.uti
    }

    #if os(macOS)
    /// Never sent: password managers' items, and Universal Clipboard's (Apple syncs those itself).
    private static let skipped: Set<String> = [
        "org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType", "com.apple.is-remote-clipboard",
    ]
    /// The change count of our own last write, which mustn't be sent back.
    @MainActor private static var ownChange = -1

    /// What's on the clipboard to send, or nil.
    @MainActor static func read() -> (formats: [Format], files: [File])? {
        let pb = NSPasteboard.general
        guard pb.changeCount != ownChange,
              Set((pb.types ?? []).map(\.rawValue)).isDisjoint(with: skipped) else { return nil }
        // Copied files: send the files, not Finder's file name and icon.
        let urls = pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL] ?? []
        if !urls.isEmpty {
            let files = urls.compactMap(file)
            return files.isEmpty ? nil : ([], files)
        }
        var found = formats.compactMap { f in pb.data(forType: .init(f.uti)).map { Format(mime: f.mime, data: $0) } }
        // Many apps copy images only as TIFF; we send PNG.
        if !found.contains(where: { $0.mime.hasPrefix("image/") }),
           let tiff = pb.data(forType: .tiff),
           let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:]) {
            found.append(Format(mime: "image/png", data: png))
        }
        return found.isEmpty ? nil : (found, [])
    }

    /// Puts a received item on the clipboard. `.currentHostOnly` keeps Universal Clipboard from forwarding it.
    @MainActor static func write(_ clip: Clip) {
        let pb = NSPasteboard.general
        _ = pb.prepareForNewContents(with: .currentHostOnly)
        if clip.files.isEmpty {
            let item = NSPasteboardItem()
            for f in clip.formats {
                if let uti = uti(f.mime) { _ = item.setData(f.data, forType: .init(uti)) }
            }
            _ = pb.writeObjects([item])
        } else {
            // The core saved the files in its data folder; point at them.
            _ = pb.writeObjects(clip.files.map { NSURL(fileURLWithPath: $0.path) })
        }
        ownChange = pb.changeCount
    }

    // ponytail: regular files only; folders are skipped until the core can send them.
    private static func file(_ url: URL) -> File? {
        guard let values = try? url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey]),
              values.isRegularFile == true else { return nil }
        return File(name: url.lastPathComponent, path: url.path, size: UInt64(values.fileSize ?? 0))
    }

    #else
    /// Puts a received item on the clipboard. `.localOnly` keeps Universal Clipboard from forwarding it.
    @MainActor static func write(_ clip: Clip) {
        var items: [[String: Any]] = []
        var item: [String: Any] = [:]
        for f in clip.formats {
            if let uti = uti(f.mime) { item[uti] = f.data }
        }
        if !item.isEmpty { items.append(item) }
        // Files go on the clipboard as their contents, so other apps can paste them.
        for file in clip.files {
            let type = UTType(filenameExtension: URL(fileURLWithPath: file.name).pathExtension) ?? .data
            if let data = FileManager.default.contents(atPath: file.path) { items.append([type.identifier: data]) }
        }
        UIPasteboard.general.setItems(items, options: [.localOnly: true])
    }

    /// What we can send from pasted or shared items: our formats, text and links, other images as PNG, and files.
    static func load(_ providers: [NSItemProvider]) async -> (formats: [Format], files: [File]) {
        var found: [Format] = []
        var files: [File] = []
        for p in providers {
            let known = formats.filter { p.hasItemConformingToTypeIdentifier($0.uti) }
            if !known.isEmpty {
                for f in known {
                    if let data = await loadData(p, f.uti) { found.append(Format(mime: f.mime, data: data)) }
                }
            } else if let text = await loadText(p) {
                found.append(Format(mime: "text/plain;charset=utf-8", data: Data(text.utf8)))
            } else if p.hasItemConformingToTypeIdentifier(UTType.image.identifier),
                      let data = await loadData(p, UTType.image.identifier),
                      let png = UIImage(data: data)?.pngData() {
                found.append(Format(mime: "image/png", data: png))
            } else if let file = await loadFile(p) {
                files.append(file)
            }
        }
        return (found, files)
    }

    private static func loadData(_ p: NSItemProvider, _ uti: String) async -> Data? {
        await withCheckedContinuation { (c: CheckedContinuation<Data?, Never>) in
            _ = p.loadDataRepresentation(forTypeIdentifier: uti) { data, _ in c.resume(returning: data) }
        }
    }

    /// Plain text, or a web link as text.
    private static func loadText(_ p: NSItemProvider) async -> String? {
        if p.canLoadObject(ofClass: String.self) {
            return await withCheckedContinuation { (c: CheckedContinuation<String?, Never>) in
                _ = p.loadObject(ofClass: String.self) { text, _ in c.resume(returning: text) }
            }
        }
        if p.canLoadObject(ofClass: URL.self) {
            let url = await withCheckedContinuation { (c: CheckedContinuation<URL?, Never>) in
                _ = p.loadObject(ofClass: URL.self) { url, _ in c.resume(returning: url) }
            }
            if let url, !url.isFileURL { return url.absoluteString }
        }
        return nil
    }

    /// Copies a shared file out of the provider's temporary file, which is deleted when the callback returns.
    // ponytail: providers that offer only a file URL are skipped; read them with security-scoped access if needed.
    private static func loadFile(_ p: NSItemProvider) async -> File? {
        guard let type = p.registeredTypeIdentifiers.first(where: { $0 != UTType.fileURL.identifier }) else { return nil }
        return await withCheckedContinuation { (c: CheckedContinuation<File?, Never>) in
            _ = p.loadFileRepresentation(forTypeIdentifier: type) { url, _ in
                guard let url else { return c.resume(returning: nil) }
                let copy = FileManager.default.temporaryDirectory
                    .appendingPathComponent(UUID().uuidString)
                    .appendingPathComponent(url.lastPathComponent)
                do {
                    try FileManager.default.createDirectory(
                        at: copy.deletingLastPathComponent(), withIntermediateDirectories: true)
                    try FileManager.default.copyItem(at: url, to: copy)
                    let size = try copy.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
                    c.resume(returning: File(name: copy.lastPathComponent, path: copy.path, size: UInt64(size)))
                } catch {
                    c.resume(returning: nil)
                }
            }
        }
    }
    #endif
}
