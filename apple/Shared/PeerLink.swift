import Foundation
import Network

/// A backup path between an iPhone and a Mac that aren't on the same network: Apple's peer-to-peer Wi-Fi (AWDL, what
/// AirDrop uses). It only carries the core's own QUIC packets, so iroh's encryption, ids and pairing rules stay as they
/// are: the core just sees the other device at a local address (127.0.0.1:port).
///
/// The Mac listens: DNS-SD service `_sharedclipp2p._tcp`, with its id in TXT. The iPhone, while the app is open, looks
/// for it, and connects if the Mac doesn't come online over the normal network within a few seconds. On the TCP link,
/// each QUIC packet is one frame: a 2-byte length (big-endian), then the packet. The iPhone's first frame is its id.
///
/// On each side, a local UDP socket stands in for the other device: what the core sends to it goes over the link, and
/// what comes over the link is sent from it to the core. Its port stays the same for a device, so the core remembers
/// one local address for that device, not a new one each time.
final class PeerLink {
    private static let service = "_sharedclipp2p._tcp"
    /// How long the normal network gets to connect a device before the iPhone uses this link.
    private static let grace: TimeInterval = 3

    private let node: Node
    private let queue = DispatchQueue(label: "peerlink")
    private var listener: NWListener?
    private var browser: NWBrowser?
    /// Open links, by device id.
    private var relays: [String: Relay] = [:]
    /// Devices the iPhone is waiting on or connecting to.
    private var dialing: Set<String> = []

    init(node: Node) {
        self.node = node
        queue.async { [weak self] in
            #if os(macOS)
            self?.listen()
            #else
            self?.browse()
            #endif
        }
    }

    deinit {
        let (listener, browser, relays) = (listener, browser, relays)
        queue.async {
            listener?.cancel()
            browser?.cancel()
            relays.values.forEach { $0.close() }
        }
    }

    private func isPaired(_ id: String) -> Bool { node.devices().contains { $0.id == id } }
    private func isOnline(_ id: String) -> Bool { node.devices().contains { $0.id == id && $0.online } }

    // MARK: Mac: wait for iPhones

    private func listen() {
        let parameters = NWParameters.tcp
        parameters.includePeerToPeer = true
        guard let listener = try? NWListener(using: parameters) else { return }
        listener.service = NWListener.Service(type: Self.service, txtRecord: NWTXTRecord(["id": node.id()]))
        listener.newConnectionHandler = { [weak self] in self?.accept($0) }
        listener.stateUpdateHandler = { state in
            if case .failed(let error) = state { print("PeerLink: can't listen (\(error))") }
        }
        listener.start(queue: queue)
        self.listener = listener
    }

    private func accept(_ connection: NWConnection) {
        connection.start(queue: queue)
        Relay.readFrame(connection) { [weak self] hello in
            guard let self, let hello, let id = String(data: hello, encoding: .utf8), self.isPaired(id) else {
                return connection.cancel()
            }
            self.attach(id, connection)
        }
        // A connection that doesn't say who it is in time goes away.
        queue.asyncAfter(deadline: .now() + 10) { [weak self] in
            guard let self, !self.relays.values.contains(where: { $0.connection === connection }) else { return }
            connection.cancel()
        }
    }

    // MARK: iPhone: find the Mac

    private func browse() {
        let parameters = NWParameters()
        parameters.includePeerToPeer = true
        let browser = NWBrowser(for: .bonjourWithTXTRecord(type: Self.service, domain: nil), using: parameters)
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            results.forEach { self?.consider($0) }
        }
        browser.start(queue: queue)
        self.browser = browser
    }

    private func consider(_ result: NWBrowser.Result) {
        guard case .bonjour(let txt) = result.metadata, let id = txt["id"], relays[id] == nil, !dialing.contains(id),
              isPaired(id) else { return }
        dialing.insert(id)
        // On the same Wi-Fi the device connects the normal way, and this link isn't needed.
        queue.asyncAfter(deadline: .now() + Self.grace) { [weak self] in
            guard let self else { return }
            if self.isOnline(id) {
                self.dialing.remove(id)
            } else {
                self.dial(id, result.endpoint)
            }
        }
    }

    private func dial(_ id: String, _ endpoint: NWEndpoint) {
        let parameters = NWParameters.tcp
        parameters.includePeerToPeer = true
        let connection = NWConnection(to: endpoint, using: parameters)
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return connection.cancel() }
            switch state {
            case .ready:
                // From here the relay notices a broken link (its reads fail).
                connection.stateUpdateHandler = nil
                self.dialing.remove(id)
                Relay.sendFrame(connection, Data(self.node.id().utf8))
                self.attach(id, connection)
                if let port = self.relays[id]?.localPort {
                    self.node.found(id: id, addr: "127.0.0.1:\(port)")
                }
            case .waiting, .failed:
                self.dialing.remove(id)
                connection.cancel()
            default:
                break
            }
        }
        connection.start(queue: queue)
    }

    // MARK: Both

    private func attach(_ id: String, _ connection: NWConnection) {
        relays[id]?.close()
        guard let relay = Relay(id: id, connection: connection, corePort: node.port(), queue: queue) else {
            return connection.cancel()
        }
        relay.onClose = { [weak self, weak relay] in
            if let self, let relay, self.relays[id] === relay { self.relays[id] = nil }
        }
        relays[id] = relay
    }
}

/// One link to one device: the TCP connection, and the local UDP socket that stands in for the device. Runs on the
/// PeerLink's queue.
private final class Relay {
    /// Local port for each device id, kept between runs.
    private static let portsKey = "peerLinkPorts"

    let connection: NWConnection
    let localPort: UInt16
    var onClose: (() -> Void)?
    private let fd: Int32
    private let source: DispatchSourceRead
    private let corePort: UInt16
    private var closed = false

    init?(id: String, connection: NWConnection, corePort: UInt16, queue: DispatchQueue) {
        guard let (fd, port) = Self.socket(for: id) else { return nil }
        self.connection = connection
        self.fd = fd
        self.localPort = port
        self.corePort = corePort
        source = DispatchSource.makeReadSource(fileDescriptor: fd, queue: queue)
        source.setEventHandler { [weak self] in self?.drain() }
        source.setCancelHandler { Darwin.close(fd) }
        source.resume()
        receive()
    }

    func close() {
        guard !closed else { return }
        closed = true
        source.cancel()
        connection.cancel()
        onClose?()
    }

    /// Packets from the core: over the link.
    private func drain() {
        var buffer = [UInt8](repeating: 0, count: 65_536)
        while !closed {
            let count = recv(fd, &buffer, buffer.count, 0)
            guard count > 0 else { return }
            Self.sendFrame(connection, Data(buffer[0..<count])) { [weak self] in self?.close() }
        }
    }

    /// Packets from the link: to the core, from the local socket.
    private func receive() {
        Self.readFrame(connection) { [weak self] packet in
            guard let self, !self.closed else { return }
            guard let packet else { return self.close() }
            var address = Self.loopback(self.corePort)
            packet.withUnsafeBytes { bytes in
                withUnsafePointer(to: &address) {
                    $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                        _ = sendto(self.fd, bytes.baseAddress, bytes.count, 0, $0, Self.addressSize)
                    }
                }
            }
            self.receive()
        }
    }

    // MARK: Frames

    static func sendFrame(_ connection: NWConnection, _ payload: Data, onError: (() -> Void)? = nil) {
        var frame = Data([UInt8(payload.count >> 8), UInt8(payload.count & 0xff)])
        frame.append(payload)
        connection.send(content: frame, completion: .contentProcessed { error in
            if error != nil { onError?() }
        })
    }

    /// Reads one frame; `nil` when the link is closed or broken.
    static func readFrame(_ connection: NWConnection, _ done: @escaping (Data?) -> Void) {
        connection.receive(minimumIncompleteLength: 2, maximumLength: 2) { head, _, _, error in
            guard let head, head.count == 2, error == nil else { return done(nil) }
            let length = Int(head[head.startIndex]) << 8 | Int(head[head.startIndex + 1])
            guard length > 0 else { return done(Data()) }
            connection.receive(minimumIncompleteLength: length, maximumLength: length) { body, _, _, error in
                guard let body, body.count == length, error == nil else { return done(nil) }
                done(body)
            }
        }
    }

    // MARK: Local socket

    private static let addressSize = socklen_t(MemoryLayout<sockaddr_in>.size)

    private static func loopback(_ port: UInt16) -> sockaddr_in {
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        address.sin_addr.s_addr = UInt32(0x7f00_0001).bigEndian // 127.0.0.1
        return address
    }

    /// A non-blocking UDP socket on 127.0.0.1, on the device's port from last time if it's free.
    private static func socket(for id: String) -> (Int32, UInt16)? {
        var ports = UserDefaults.standard.dictionary(forKey: portsKey) as? [String: Int] ?? [:]
        let fd = Darwin.socket(AF_INET, SOCK_DGRAM, 0)
        guard fd >= 0 else { return nil }
        let wanted = UInt16(clamping: ports[id] ?? 0)
        guard bind(fd, wanted) || (wanted != 0 && bind(fd, 0)) else {
            Darwin.close(fd)
            return nil
        }
        _ = fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK)
        var bound = sockaddr_in()
        var size = addressSize
        _ = withUnsafeMutablePointer(to: &bound) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &size) }
        }
        let port = UInt16(bigEndian: bound.sin_port)
        ports[id] = Int(port)
        UserDefaults.standard.set(ports, forKey: portsKey)
        return (fd, port)
    }

    private static func bind(_ fd: Int32, _ port: UInt16) -> Bool {
        var address = loopback(port)
        return withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(fd, $0, addressSize) }
        } == 0
    }
}
