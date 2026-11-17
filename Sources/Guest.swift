import Foundation
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

    /// The MAC the attachment wrote, or the stable address for this disk when that file is missing.
    static func lookupMAC(disk: URL) -> String {
        normalizeMAC(assignedMAC(disk: disk) ?? macString(for: disk))
    }

    /// How `arp -an` prints it. `02:56:4d:4d:09:00` is `2:56:4d:4d:9:0` in that table.
    static func arpMAC(disk: URL) -> String {
        let padded = lookupMAC(disk: disk)
        guard !padded.isEmpty else { return "" }
        return padded.split(separator: ":").map { octet in
            let trimmed = octet.drop { $0 == "0" }
            return trimmed.isEmpty ? "0" : String(trimmed)
        }.joined(separator: ":")
    }

    /// Address on this disk's NIC. Nil when ARP has no row for this MAC yet.
    /// Each machine has its own MAC. The IP is that ARP row, on whichever interface `arp -an` printed.
    static func natAddress(disk: URL) -> String? {
        let want = lookupMAC(disk: disk)
        guard !want.isEmpty, let text = arpText() else { return nil }
        return macInARP(text, mac: want, gateways: hostGateways())
    }

    /// Any ARP row for this MAC, except the host gateway. `bridge100` is common; the name is not required.
    private static func macInARP(_ text: String, mac want: String, gateways: Set<String>) -> String? {
        var found: String?
        for line in text.split(whereSeparator: \.isNewline) {
            guard let row = arpRow(String(line)), normalizeMAC(row.mac) == want else { continue }
            guard !gateways.contains(row.host), !hostSide(row.host), usableIPv4(row.host) else { continue }
            found = row.host
        }
        return found
    }

    private static func arpRow(_ line: String) -> (host: String, mac: String, iface: String)? {
        guard let open = line.firstIndex(of: "("),
              let close = line[open...].firstIndex(of: ")"),
              open < close else { return nil }
        let host = String(line[line.index(after: open)..<close])
        let words = line.split(whereSeparator: \.isWhitespace).map(String.init)
        let mac = words.first { token in
            let parts = token.split { $0 == ":" || $0 == "-" }
            return parts.count == 6 || parts.count == 7
        } ?? ""
        let iface = words.firstIndex(of: "on").flatMap { index -> String? in
            guard words.count > index + 1 else { return nil }
            return String(words[index + 1].prefix { $0.isLetter || $0.isNumber })
        } ?? ""
        return (host, mac, iface)
    }

    private static func arpText() -> String? {
        for path in ["/usr/sbin/arp", "/usr/bin/arp"] {
            if let text = commandText(path, ["-an"]) {
                return text
            }
        }
        return nil
    }

    /// Not this Mac, loopback, or a broadcast. `.1` is always the gateway.
    /// `.2` on a vmnet shared network is this Mac too.
    private static func hostSide(_ host: String) -> Bool {
        let parts = host.split(separator: ".")
        guard parts.count == 4,
              let c = Int(parts[2]), let d = Int(parts[3]) else { return false }
        return parts[0] == "192" && parts[1] == "168" && c >= 64 && (d == 1 || d == 2)
    }

    private static func usableIPv4(_ host: String) -> Bool {
        let parts = host.split(separator: ".")
        guard parts.count == 4,
              let a = Int(parts[0]), let d = Int(parts[3]),
              parts[1].allSatisfy(\.isNumber), parts[2].allSatisfy(\.isNumber) else { return false }
        return a != 0 && a != 127 && a < 224 && d != 0 && d != 255
    }

    /// Addresses configured on vmnet bridges, plus the other host address on that subnet.
    /// A bridge at `192.168.64.1` also owns `.2`. A bridge at `192.168.65.2` also owns `.1`.
    private static func hostGateways() -> Set<String> {
        var found: Set<String> = []
        for host in bridgeAddresses() {
            found.insert(host)
            found.formUnion(pair(of: host))
        }
        return found
    }

    /// `192.168.64.1` and `192.168.64.2` are the same Mac. Whichever ifconfig shows, skip both.
    private static func pair(of host: String) -> Set<String> {
        let parts = host.split(separator: ".")
        guard parts.count == 4, parts[0] == "192", parts[1] == "168",
              let c = Int(parts[2]), c >= 64, parts[3] == "1" || parts[3] == "2" else { return [] }
        let prefix = parts.prefix(3).joined(separator: ".")
        return ["\(prefix).1", "\(prefix).2"]
    }

    private static func bridgeAddresses() -> [String] {
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
            let host = words[inet + 1]
            guard usableIPv4(host) || host.split(separator: ".").count == 4 else { continue }
            if !found.contains(host) { found.append(host) }
        }
        return found
    }

    private static func commandText(_ path: String, _ args: [String]) -> String? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: path)
        process.arguments = args
        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err
        guard (try? process.run()) != nil else { return nil }
        process.waitUntilExit()
        let data = out.fileHandleForReading.readDataToEndOfFile()
        guard !data.isEmpty else { return nil }
        return String(data: data, encoding: .utf8) ?? String(data: data, encoding: .isoLatin1)
    }

    private static func normalizeMAC(_ text: String) -> String {
        let raw = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let body = raw.split(separator: ",").last.map { $0.trimmingCharacters(in: .whitespaces) } ?? raw
        let token = body.split(whereSeparator: \.isWhitespace).first.map(String.init) ?? body
        let parts = token.split { $0 == ":" || $0 == "-" }.map(String.init)
        var octets = parts
        if octets.count == 7, let kind = UInt8(octets[0], radix: 16), kind <= 1 {
            octets.removeFirst()
        }
        guard octets.count == 6 else { return "" }
        var bytes: [String] = []
        for part in octets {
            guard let value = UInt8(hexOctet(part), radix: 16) else { return "" }
            bytes.append(String(format: "%02x", value))
        }
        return bytes.joined(separator: ":")
    }

    /// `arp` may leave a comma or bracket on the last octet. Only the hex counts.
    private static func hexOctet(_ part: String) -> String {
        let body = part.drop { !$0.isHexDigit }
        return String(body.prefix { $0.isHexDigit })
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
    /// The host is `.1` or `.2` on the vmnet bridge. sshd is on the guest address, port 22. The subnet is not always 192.168.64.
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
    /// A clone used to copy the snapshot id. A second VM with that id does not get its own DHCP lease.
    private static func machineIdentifier(beside disk: URL) throws -> VZGenericMachineIdentifier {
        let url = disk.deletingLastPathComponent().appendingPathComponent("machine-id")
        if let data = try? Data(contentsOf: url),
           let id = VZGenericMachineIdentifier(dataRepresentation: data),
           GuestPause.isPaused(beside: disk) || !sharedIdentity(data, disk: disk, file: url) {
            return id
        }
        let id = VZGenericMachineIdentifier()
        try id.dataRepresentation.write(to: url, options: .atomic)
        return id
    }

    /// True when this file is the snapshot id, or another machine already has these bytes.
    private static func sharedIdentity(_ data: Data, disk: URL, file: URL) -> Bool {
        let support = disk.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let snapshot = support.appendingPathComponent("snapshots").appendingPathComponent("debian").appendingPathComponent("machine-id")
        if file.path != snapshot.path, let other = try? Data(contentsOf: snapshot), other == data {
            return true
        }
        let machines = support.appendingPathComponent("machines")
        guard let names = try? FileManager.default.contentsOfDirectory(atPath: machines.path) else { return false }
        for name in names {
            let other = machines.appendingPathComponent(name).appendingPathComponent("machine-id")
            if other.path == file.path { continue }
            if let bytes = try? Data(contentsOf: other), bytes == data { return true }
        }
        return false
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
/// The worker walks ARP and a short run of the vmnet subnet. Swift only starts that process and can kill it.
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
