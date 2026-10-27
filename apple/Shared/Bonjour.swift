import Foundation
import Network
import dnssd

/// Finds other devices and announces this one: DNS-SD service `_sharedclip._udp`, with the id, name and platform in
/// TXT (docs/protocol.md, "Finding devices"). Runs on the main queue; deinit stops both.
///
/// Announcing uses the dns_sd C API (`DNSServiceRegister`), not `NWListener`: the core already owns the UDP port, and
/// NWListener would bind it. `NetService` would also work but needs a run loop and a delegate; DNSServiceRegister
/// with no callback only registers the name and needs neither.
final class Bonjour {
    private let node: Node
    private let browser: NWBrowser
    private var registration: DNSServiceRef?

    init(node: Node, name: String, platform: String, onNearby: @escaping ([String: String]) -> Void) {
        self.node = node
        let ownId = node.id()
        browser = NWBrowser(for: .bonjourWithTXTRecord(type: "_sharedclip._udp", domain: nil), using: NWParameters())
        browser.browseResultsChangedHandler = { [weak self] results, changes in
            var nearby: [String: String] = [:]
            for result in results {
                if case .bonjour(let txt) = result.metadata, let id = txt["id"], id != ownId {
                    nearby[id] = txt["name"] ?? id
                }
            }
            onNearby(nearby)
            for change in changes {
                if case .added(let result) = change { self?.resolve(result) }
                if case .changed(_, let result, _) = change { self?.resolve(result) }
            }
        }
        browser.start(queue: .main)
        register(id: ownId, name: name, platform: platform)
    }

    deinit {
        browser.cancel()
        if let registration { DNSServiceRefDeallocate(registration) }
    }

    /// Turns a found service into "ip:port" for the core. A UDP NWConnection resolves it without sending anything.
    private func resolve(_ result: NWBrowser.Result) {
        guard case .bonjour(let txt) = result.metadata, let id = txt["id"], id != node.id() else { return }
        let parameters = NWParameters.udp
        // IPv4 only: a link-local IPv6 address needs a scope id, which "ip:port" can't carry.
        (parameters.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options)?.version = .v4
        let connection = NWConnection(to: result.endpoint, using: parameters)
        let node = self.node
        connection.stateUpdateHandler = { state in
            switch state {
            case .ready:
                if case .hostPort(.ipv4(let ip), let port)? = connection.currentPath?.remoteEndpoint {
                    let host = ip.rawValue.map { String($0) }.joined(separator: ".")
                    node.found(id: id, addr: "\(host):\(port.rawValue)")
                }
                connection.cancel()
            case .waiting, .failed:
                connection.cancel()
            default:
                break
            }
        }
        connection.start(queue: .main)
        DispatchQueue.main.asyncAfter(deadline: .now() + 10) { connection.cancel() }
    }

    private func register(id: String, name: String, platform: String) {
        var txt: [UInt8] = []
        for entry in ["id=\(id)", "name=\(name)", "platform=\(platform)"] {
            let bytes = Array(entry.utf8.prefix(255))
            txt.append(UInt8(bytes.count))
            txt += bytes
        }
        // No service name: mDNSResponder uses the computer's name and renames on conflicts. The port goes in network
        // byte order. With no callback, nothing needs to process replies.
        let status = DNSServiceRegister(
            &registration, 0, 0, nil, "_sharedclip._udp", nil, nil,
            node.port().bigEndian, UInt16(txt.count), txt, nil, nil
        )
        if status != 0 { // kDNSServiceErr_NoError
            print("Bonjour: can't announce this device (\(status))")
        }
    }
}
