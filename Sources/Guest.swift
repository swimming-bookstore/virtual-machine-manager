import Foundation
import Darwin
import Virtualization

/// Boots the patched netinst, then the disk the installer writes.
/// Virtualization.framework has no CD drive, so the installer is a USB disk.
/// The guest disk is virtio (`/dev/vda`), which is the disk the preseed selects.
enum Guest {
    static let user = "debian"
    /// sshd in the guest listens here. The NAT attachment cannot map it to a host port.
    static let sshPort: UInt16 = 22

    static func boot(iso: URL, preseed: URL, disk: URL) throws -> VZVirtualMachine {
        return try machine(disk: disk, extra: [iso, preseed], pointer: false)
    }

    static func boot(disk: URL, pointer: Bool = false) throws -> VZVirtualMachine {
        return try machine(disk: disk, extra: [], pointer: pointer)
    }

    /// Locally administered MAC, stable for this disk, so the DHCP lease can be found again.
    static func macString(for disk: URL) -> String {
        var sum: UInt16 = 0x564d
        for byte in disk.path.utf8 {
            sum = sum &* 33 &+ UInt16(byte)
        }
        return String(format: "02:56:4d:4d:%02x:%02x", UInt8(sum >> 8), UInt8(truncatingIfNeeded: sum))
    }

    /// Address on this disk's NIC. Nil when the guest is off or still booting.
    /// UTM asks the guest agent and matches hardwareAddress. This matches the MAC the attachment wrote.
    /// ARP stays empty until the host asks, and the subnet is not always 192.168.64.
    static func natAddress(disk: URL) -> String? {
        let want = normalizeMAC(assignedMAC(disk: disk) ?? macString(for: disk))
        guard !want.isEmpty else { return nil }
        return lookup(mac: want)
    }

    /// Lease file first. It is written when the guest gets an address, and it does not need an ARP probe.
    /// A MAC miss still shows the latest guest lease, then the only address on the vmnet bridge.
    private static func lookup(mac want: String) -> String? {
        if let host = matched(mac: want) { return host }
        if let host = onlyBridgeGuest() { return host }
        if let host = latestLease() { return host }
        primeArp()
        return matched(mac: want) ?? onlyBridgeGuest() ?? latestLease()
    }

    private static func matched(mac want: String) -> String? {
        guard !want.isEmpty else { return nil }
        if let host = leaseAddress(mac: want) { return host }
        if let text = arpText(), let host = address(in: text, mac: want) { return host }
        return nil
    }

    private static func address(in text: String, mac want: String) -> String? {
        for line in text.split(separator: "\n") {
            guard let open = line.firstIndex(of: "("),
                  let close = line[open...].firstIndex(of: ")"),
                  open < close else { continue }
            let host = String(line[line.index(after: open)..<close])
            guard isGuestHost(host) else { continue }
            guard let mac = line.split(whereSeparator: \.isWhitespace).first(where: { $0.split(separator: ":").count == 6 }),
                  normalizeMAC(String(mac)) == want else { continue }
            return host
        }
        return nil
    }

    private static func arpText() -> String? {
        for path in ["/usr/sbin/arp", "/usr/bin/arp"] {
            if let text = commandText(path, ["-an"]) {
                return text
            }
        }
        return nil
    }

    /// The only guest address on a vmnet bridge. A LAN neighbor on en0 is not this.
    private static func onlyBridgeGuest() -> String? {
        guard let text = arpText() else { return nil }
        var found: [String] = []
        for line in text.split(separator: "\n") {
            guard let open = line.firstIndex(of: "("),
                  let close = line[open...].firstIndex(of: ")"),
                  open < close else { continue }
            let host = String(line[line.index(after: open)..<close])
            guard isGuestHost(host) else { continue }
            let words = line.split(whereSeparator: \.isWhitespace).map(String.init)
            guard let on = words.firstIndex(of: "on"), words.count > on + 1 else { continue }
            let iface = words[on + 1]
            guard iface.hasPrefix("bridge") || iface.hasPrefix("vmnet") || iface.hasPrefix("vmenet") else { continue }
            if !found.contains(host) { found.append(host) }
        }
        return found.count == 1 ? found[0] : nil
    }

    /// Newest guest lease. bootpd appends, so the last block is the current grant.
    private static func latestLease() -> String? {
        for path in leasePaths {
            if let host = latestLease(at: path) { return host }
        }
        return nil
    }

    private static func latestLease(at path: String) -> String? {
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDir) else { return nil }
        if isDir.boolValue {
            guard let names = try? FileManager.default.contentsOfDirectory(atPath: path) else { return nil }
            var last: String?
            for name in names.sorted() where name.hasSuffix(".lease") || name.hasSuffix(".plist") || name.contains("dhcp") {
                if let host = latestLease(at: (path as NSString).appendingPathComponent(name)) {
                    last = host
                }
            }
            return last
        }
        guard let data = try? Data(contentsOf: URL(fileURLWithPath: path)), !data.isEmpty else { return nil }
        let text = String(data: data, encoding: .utf8) ?? String(data: data, encoding: .isoLatin1) ?? ""
        return latestLeaseHost(text)
    }

    private static func latestLeaseHost(_ text: String) -> String? {
        if text.contains("<key>") { return latestLeaseXML(text) }
        var found: String?
        var ip: String?
        for raw in text.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty || line == "{" { ip = nil; continue }
            if line == "}" {
                if let ip, isGuestHost(ip) { found = ip }
                ip = nil
                continue
            }
            guard let (key, value) = leaseField(line) else { continue }
            if key == "ip_address" || key == "ip-address" { ip = value }
        }
        if let ip, isGuestHost(ip) { found = ip }
        return found
    }

    private static func latestLeaseXML(_ text: String) -> String? {
        var found: String?
        var pending: String?
        var ip: String?
        for raw in text.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line == "<dict>" { ip = nil; continue }
            if line == "</dict>" {
                if let ip, isGuestHost(ip) { found = ip }
                ip = nil
                continue
            }
            if line.hasPrefix("<key>"), line.hasSuffix("</key>") {
                pending = String(line.dropFirst(5).dropLast(6))
                continue
            }
            if line.hasPrefix("<string>"), line.hasSuffix("</string>"), let key = pending {
                if key == "ip_address" || key == "ip-address" {
                    ip = String(line.dropFirst(8).dropLast(9))
                }
                pending = nil
            }
        }
        if let ip, isGuestHost(ip) { found = ip }
        return found
    }

    /// vmnet writes one lease file, not a directory. Each guest is `key=value` inside `{ }`.
    /// `hw_address` is `1,aa:bb:cc:dd:ee:ff`. ARP stays empty until this Mac talks to the guest.
    private static let leasePaths = [
        "/var/db/dhcpd_leases",
        "/private/var/db/dhcpd_leases",
        "/var/db/vmnet/dhcpd_leases",
        "/private/var/db/vmnet/dhcpd_leases",
    ]

    private static func leaseAddress(mac want: String) -> String? {
        for path in leasePaths {
            if let host = leaseAddress(at: path, mac: want) { return host }
        }
        return nil
    }

    private static func leaseAddress(at path: String, mac want: String) -> String? {
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDir) else { return nil }
        if isDir.boolValue {
            guard let names = try? FileManager.default.contentsOfDirectory(atPath: path) else { return nil }
            for name in names.sorted() where name.hasSuffix(".lease") || name.hasSuffix(".plist") || name.contains("dhcp") {
                if let host = leaseAddress(at: (path as NSString).appendingPathComponent(name), mac: want) {
                    return host
                }
            }
            return nil
        }
        guard let data = try? Data(contentsOf: URL(fileURLWithPath: path)), !data.isEmpty else { return nil }
        let text = String(data: data, encoding: .utf8) ?? String(data: data, encoding: .isoLatin1) ?? ""
        return leaseHost(text, mac: want)
    }

    /// Last matching block wins. The file keeps the current lease last.
    private static func leaseHost(_ text: String, mac want: String) -> String? {
        if text.contains("<key>") { return leaseHostXML(text, mac: want) }
        var found: String?
        var ip: String?
        var mac: String?
        func consider() {
            if let ip, let mac, normalizeMAC(mac) == want, isGuestHost(ip) {
                found = ip
            }
        }
        for raw in text.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty || line == "{" {
                ip = nil
                mac = nil
                continue
            }
            if line == "}" {
                consider()
                ip = nil
                mac = nil
                continue
            }
            guard let (key, value) = leaseField(line) else { continue }
            if key == "ip_address" || key == "ip-address" { ip = value }
            if key == "hw_address" || key == "hw-address" || key == "mac_address" || key == "hardware_address" {
                mac = value
            } else if key == "identifier", mac == nil {
                mac = value
            }
        }
        consider()
        return found
    }

    private static func leaseHostXML(_ text: String, mac want: String) -> String? {
        var found: String?
        var pending: String?
        var ip: String?
        var mac: String?
        func consider() {
            if let ip, let mac, normalizeMAC(mac) == want, isGuestHost(ip) {
                found = ip
            }
        }
        for raw in text.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line == "<dict>" { ip = nil; mac = nil; continue }
            if line == "</dict>" { consider(); ip = nil; mac = nil; continue }
            if line.hasPrefix("<key>"), line.hasSuffix("</key>") {
                pending = String(line.dropFirst(5).dropLast(6))
                continue
            }
            if line.hasPrefix("<string>"), line.hasSuffix("</string>"), let key = pending {
                let value = String(line.dropFirst(8).dropLast(9))
                if key == "ip_address" || key == "ip-address" { ip = value }
                if key == "hw_address" || key == "hw-address" || key == "mac_address" { mac = value }
                pending = nil
            }
        }
        consider()
        return found
    }

    private static func leaseField(_ line: String) -> (String, String)? {
        let trimmed = line.trimmingCharacters(in: CharacterSet(charactersIn: "\";"))
        if let eq = trimmed.firstIndex(of: "=") {
            let key = String(trimmed[..<eq]).trimmingCharacters(in: .whitespaces).trimmingCharacters(in: CharacterSet(charactersIn: "\""))
            let value = String(trimmed[trimmed.index(after: eq)...]).trimmingCharacters(in: .whitespaces).trimmingCharacters(in: CharacterSet(charactersIn: "\";"))
            guard !key.isEmpty, !value.isEmpty else { return nil }
            return (key, value)
        }
        let parts = trimmed.split(whereSeparator: \.isWhitespace).map(String.init)
        guard parts.count >= 2 else { return nil }
        let key = parts[0].trimmingCharacters(in: CharacterSet(charactersIn: "\""))
        let value = parts.dropFirst().joined(separator: " ").trimmingCharacters(in: CharacterSet(charactersIn: "\";"))
        return (key, value)
    }

    private static func isGuestHost(_ host: String) -> Bool {
        let parts = host.split(separator: ".")
        guard parts.count == 4,
              let a = Int(parts[0]), let d = Int(parts[3]),
              parts[1].allSatisfy(\.isNumber), parts[2].allSatisfy(\.isNumber) else { return false }
        return a != 0 && a != 127 && a < 224 && d != 0 && d != 1 && d != 255
    }

    /// vmnet's bridge is `.1`. Ask that subnet so the guest answers ARP. 192.168.64 is Apple's first subnet.
    private static func natPrefixes() -> [String] {
        var found: [String] = []
        let text = commandText("/sbin/ifconfig", ["-a"]) ?? commandText("/usr/sbin/ifconfig", ["-a"]) ?? ""
        var name = ""
        for raw in text.split(whereSeparator: \.isNewline) {
            let line = String(raw)
            if !line.hasPrefix("\t") && !line.hasPrefix(" ") {
                name = String(line.split(separator: ":").first ?? "")
            }
            let bridge = name.hasPrefix("bridge") || name.hasPrefix("vmnet") || name.hasPrefix("vmenet")
            guard bridge else { continue }
            let words = line.split(whereSeparator: \.isWhitespace).map(String.init)
            guard let inet = words.firstIndex(of: "inet"), words.count > inet + 1 else { continue }
            let parts = words[inet + 1].split(separator: ".")
            guard parts.count == 4, parts[3] == "1",
                  let third = Int(parts[2]), third >= 64,
                  parts[0] == "192", parts[1] == "168" else { continue }
            let prefix = parts.prefix(3).joined(separator: ".")
            if !found.contains(prefix) { found.append(prefix) }
        }
        if found.isEmpty { found.append("192.168.64") }
        return found
    }

    /// ARP stays empty until the host asks. One short connect per address is enough for the cache.
    private static func primeArp() {
        let prefixes = natPrefixes()
        let group = DispatchGroup()
        let queue = DispatchQueue(label: "guest-arp", attributes: .concurrent)
        for prefix in prefixes {
            for n in 2...254 {
                group.enter()
                queue.async {
                    poke("\(prefix).\(n)")
                    group.leave()
                }
            }
        }
        _ = group.wait(timeout: .now() + 1.5)
    }

    private static func poke(_ host: String) {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        if fd < 0 { return }
        defer { close(fd) }
        let flags = fcntl(fd, F_GETFL, 0)
        if flags >= 0 { _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK) }
        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = in_port_t(22).bigEndian
        let parsed: Int32 = host.withCString { cstr in
            withUnsafeMutableBytes(of: &addr.sin_addr) { raw in
                inet_pton(AF_INET, cstr, raw.baseAddress)
            }
        }
        if parsed != 1 { return }
        let started = withUnsafePointer(to: &addr) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                Darwin.connect(fd, sa, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        if started == 0 { return }
        var pfd = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
        _ = Darwin.poll(&pfd, nfds_t(1), 150)
    }

    private static func commandText(_ path: String, _ args: [String]) -> String? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: path)
        process.arguments = args
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        guard (try? process.run()) != nil else { return nil }
        process.waitUntilExit()
        return String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)
    }

    private static func normalizeMAC(_ text: String) -> String {
        let raw = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let body = raw.split(separator: ",").last.map { $0.trimmingCharacters(in: .whitespaces) } ?? raw
        let token = body.split(whereSeparator: \.isWhitespace).first.map(String.init) ?? body
        let parts = token.split { $0 == ":" || $0 == "-" }
        guard parts.count == 6 else { return "" }
        var bytes: [String] = []
        for part in parts {
            guard let value = UInt8(part, radix: 16) else { return "" }
            bytes.append(String(format: "%02x", value))
        }
        return bytes.joined(separator: ":")
    }

    private static var assigned: [String: String] = [:]
    private static let macLock = NSLock()
    /// The MAC the attachment actually uses. Written beside the disk so the next lookup matches the lease.
    static func noteMAC(_ mac: VZMACAddress, disk: URL) {
        let text = mac.string.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        macLock.lock()
        assigned[disk.path] = text
        macLock.unlock()
        let url = disk.deletingLastPathComponent().appendingPathComponent("mac")
        try? (text + "\n").write(to: url, atomically: true, encoding: .utf8)
    }

    private static func assignedMAC(disk: URL) -> String? {
        macLock.lock()
        let cached = assigned[disk.path]
        macLock.unlock()
        if let cached, !cached.isEmpty { return cached }
        let url = disk.deletingLastPathComponent().appendingPathComponent("mac")
        guard let text = try? String(contentsOf: url, encoding: .utf8) else { return nil }
        let mac = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return mac.contains(":") || mac.contains("-") ? mac : nil
    }

    private static func machine(disk: URL, extra: [URL], pointer: Bool) throws -> VZVirtualMachine {
        let config = VZVirtualMachineConfiguration()
        config.cpuCount = 2
        config.memorySize = 2 * 1024 * 1024 * 1024
        // A pause save is tied to this identifier. A fresh one makes restore fail with an invalid argument.
        let platform = VZGenericPlatformConfiguration()
        platform.machineIdentifier = try machineIdentifier(beside: disk)
        config.platform = platform
        let loader = VZEFIBootLoader()
        loader.variableStore = try efiStore(beside: disk)
        config.bootLoader = loader
        var devices: [VZStorageDeviceConfiguration] = []
        for url in extra {
            devices.append(try usb(url, readOnly: true))
        }
        devices.append(try guestDisk(disk))
        config.storageDevices = devices
        let graphics = VZVirtioGraphicsDeviceConfiguration()
        graphics.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: 1280, heightInPixels: 800)]
        config.graphicsDevices = [graphics]
        config.keyboards = [VZUSBKeyboardConfiguration()]
        // Attaching a pointer makes the guest view capture the host cursor.
        // Quick start never does that. Only a clone the user is running gets one.
        config.pointingDevices = pointer ? [VZUSBScreenCoordinatePointingDeviceConfiguration()] : []
        config.networkDevices = [network(disk: disk)]
        try config.validate()
        return VZVirtualMachine(configuration: config)
    }

    /// `VZNATNetworkDeviceAttachment()` takes no arguments. There is no forwarding-rules type.
    /// The host is `.1` on the vmnet bridge. sshd is on the guest address, port 22. The subnet is not always 192.168.64.
    private static func network(disk: URL) -> VZVirtioNetworkDeviceConfiguration {
        let network = VZVirtioNetworkDeviceConfiguration()
        network.attachment = VZNATNetworkDeviceAttachment()
        if let mac = VZMACAddress(string: macString(for: disk)) {
            network.macAddress = mac
        }
        noteMAC(network.macAddress, disk: disk)
        return network
    }

    private static func efiStore(beside disk: URL) throws -> VZEFIVariableStore {
        let url = disk.deletingLastPathComponent().appendingPathComponent("nvram.bin")
        if FileManager.default.fileExists(atPath: url.path) {
            return VZEFIVariableStore(url: url)
        }
        return try VZEFIVariableStore(creatingVariableStoreAt: url, options: [])
    }

    /// The identifier the pause save was written with. A new one is stored beside the disk.
    private static func machineIdentifier(beside disk: URL) throws -> VZGenericMachineIdentifier {
        let url = disk.deletingLastPathComponent().appendingPathComponent("machine-id")
        if let data = try? Data(contentsOf: url), let id = VZGenericMachineIdentifier(dataRepresentation: data) {
            return id
        }
        let id = VZGenericMachineIdentifier()
        try id.dataRepresentation.write(to: url, options: .atomic)
        return id
    }

    private static func usb(_ url: URL, readOnly: Bool) throws -> VZUSBMassStorageDeviceConfiguration {
        let attachment = try VZDiskImageStorageDeviceAttachment(url: url, readOnly: readOnly)
        return VZUSBMassStorageDeviceConfiguration(attachment: attachment)
    }

    private static func guestDisk(_ url: URL) throws -> VZVirtioBlockDeviceConfiguration {
        let attachment = try VZDiskImageStorageDeviceAttachment(url: url, readOnly: false)
        return VZVirtioBlockDeviceConfiguration(attachment: attachment)
    }
}

/// The host key. The install is finished only after this key logs in.
/// The proof sits beside the disk it reached. One app-wide file would still say "ok" after that disk was deleted.
/// The worker walks ARP and a short run of 192.168.64.2–254. Swift only starts that process and can kill it.
enum GuestSSH {
    static func dir(beside disk: URL) -> URL {
        disk.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("ssh", isDirectory: true)
    }

    static func proof(beside disk: URL) -> URL {
        disk.deletingLastPathComponent().appendingPathComponent("ssh-ok")
    }

    static func loggedIn(beside disk: URL) -> Bool {
        for url in [proof(beside: disk), dir(beside: disk).appendingPathComponent("ssh-ok")] {
            if let text = try? String(contentsOf: url, encoding: .utf8),
               text.hasPrefix(Guest.user) && text.contains("ok") {
                return true
            }
        }
        return false
    }

    /// The disk is gone, so the old login is not a finished install.
    static func forget(beside disk: URL) {
        for url in [proof(beside: disk), dir(beside: disk).appendingPathComponent("ssh-ok")] {
            _ = try? FileManager.default.removeItem(at: url)
        }
    }

    /// One attempt. A closed port is not a finished install.
    static func probe(beside disk: URL, cancel: @escaping () -> Bool = { false }) -> Bool {
        if cancel() { return false }
        if loggedIn(beside: disk) { return true }
        return Worker.probe(disk: disk, cancel: cancel)
    }

    /// Quit in the middle of Verify with host key. The login still running would block the next open on that row.
    static func stop() {
        Worker.stopProbe()
    }
}

/// Pause files beside a clone. The save is Virtualization state; the marker is written after it.
enum GuestPause {
    static func state(beside disk: URL) -> URL {
        disk.deletingLastPathComponent().appendingPathComponent("paused.vzvmsave")
    }

    /// Last guest frame, written before the paused machine is stopped.
    static func preview(beside disk: URL) -> URL {
        disk.deletingLastPathComponent().appendingPathComponent("paused.png")
    }

    static func isPaused(beside disk: URL) -> Bool {
        let dir = disk.deletingLastPathComponent()
        return FileManager.default.fileExists(atPath: dir.appendingPathComponent("paused").path)
            && FileManager.default.fileExists(atPath: state(beside: disk).path)
    }

    /// The save is only valid for the identifier written beside the disk. An older save has none.
    static func hasMachineID(beside disk: URL) -> Bool {
        FileManager.default.fileExists(atPath: disk.deletingLastPathComponent().appendingPathComponent("machine-id").path)
    }

    static func forget(beside disk: URL) {
        let dir = disk.deletingLastPathComponent()
        _ = try? FileManager.default.removeItem(at: dir.appendingPathComponent("paused"))
        _ = try? FileManager.default.removeItem(at: state(beside: disk))
        _ = try? FileManager.default.removeItem(at: preview(beside: disk))
    }
}

/// A disk the installer already wrote. An empty sparse file is not that disk.
/// A partition table is not an SSH login. It only means the next boot is the disk alone.
enum InstalledDisk {
    static func looksInstalled(_ path: String) -> Bool {
        let url = URL(fileURLWithPath: path)
        guard let handle = try? FileHandle(forReadingFrom: url) else { return false }
        defer { _ = try? handle.close() }
        guard let sector = try? handle.read(upToCount: 512), sector.count == 512 else { return false }
        return sector[510] == 0x55 && sector[511] == 0xaa
    }
}
