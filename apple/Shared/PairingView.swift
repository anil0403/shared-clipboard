import CoreImage.CIFilterBuiltins
import SwiftUI

/// Pair with another device: show this device's QR code, enter or scan the other's link, or pick a nearby device
/// and compare codes.
struct PairingView: View {
    @ObservedObject private var model = ClipModel.shared
    @State private var qr: CGImage?
    @State private var link = ""
    @State private var scanning = false

    var body: some View {
        Form {
            Section("Scan this with the other device") {
                if let qr {
                    Image(decorative: qr, scale: 1)
                        .interpolation(.none)
                        .resizable()
                        .scaledToFit()
                        .frame(width: 220, height: 220)
                }
                Button("New Code", action: refresh)
            }
            Section("Or use the other device's code") {
                #if os(iOS)
                Button("Scan Its Code") { scanning = true }
                #endif
                TextField("Paste its link", text: $link)
                Button("Pair") {
                    Task {
                        await model.pair(link: link)
                        link = ""
                    }
                }
                .disabled(link.isEmpty)
            }
            Section("Nearby") {
                let ids = model.nearby.keys.filter { id in !model.devices.contains { $0.id == id } }.sorted()
                if ids.isEmpty { Text("Looking for devices…") }
                ForEach(ids, id: \.self) { id in
                    Button(model.nearby[id] ?? id) { Task { await model.pair(id: id) } }
                }
            }
            if let message = model.message {
                Text(message).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        // Make a code when shown, and once the node has started.
        .onChange(of: model.node == nil, initial: true) { _, _ in refresh() }
        #if os(macOS)
        .frame(minWidth: 360, minHeight: 540)
        #else
        .navigationTitle("Pair a Device")
        .sheet(isPresented: $scanning) {
            QRScanner { code in
                scanning = false
                Task { await model.pair(link: code) }
            }
        }
        #endif
    }

    /// A new one-time link (valid 10 minutes), drawn as a QR code.
    private func refresh() {
        qr = model.node.flatMap { qrCode($0.pairingLink()) }
    }
}

/// `text` as a QR code, one pixel per module: scale it up with `.interpolation(.none)`.
func qrCode(_ text: String) -> CGImage? {
    let filter = CIFilter.qrCodeGenerator()
    filter.message = Data(text.utf8)
    guard let image = filter.outputImage else { return nil }
    return CIContext().createCGImage(image, from: image.extent)
}
