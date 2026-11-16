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

    /// Address for this disk's MAC. ARP first, then the DHCP lease. Nil when the guest is off or still booting.
    /// The MAC written at boot is the one vmnet recorded. A recomputed one misses the lease if Virtualization kept its own.
    static func natAddress(disk: URL) -> String? {
        let want = normalizeMAC(assignedMAC(disk: disk) ?? macString(for: disk))
        guard !want.isEmpty else { return nil }
        if let host = lookup(mac: want) { return host }
        // The host ARP table stays empty until this Mac talks to the guest. A short ping fills it.
        nudgeARP()
        return lookup(mac: want)
    }

    private static func lookup(mac want: String) -> String? {
        if let text = arpText(), let host = address(in: text, mac: want) {
            return host
        }
        return leaseAddress(mac: want)
    }

    private static func address(in text: String, mac want: String) -> String? {
        for line in text.split(separator: "\n") {
            guard let open = line.firstIndex(of: "("),
                  let close = line[open...].firstIndex(of: ")"),
                  open < close else { continue }
            let host = line[line.index(after: open)..<close]
            guard host.hasPrefix("192.168.64."), host != "192.168.64.1" else { continue }
            guard let mac = line.split(whereSeparator: \.isWhitespace).first(where: { $0.split(separator: ":").count == 6 }),
                  normalizeMAC(String(mac)) == want else { continue }
            return String(host)
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
        host.hasPrefix("192.168.64.") && host != "192.168.64.1" && host.split(separator: ".").count == 4
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
    private static var nudgeCursor = 2
    private static var nudging = false

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

    /// Ask 192.168.64.2–254 who is home, a slice at a time, so ARP learns the guest MAC.
    private static func nudgeARP() {
        macLock.lock()
        if nudging {
            macLock.unlock()
            return
        }
        nudging = true
        let start = nudgeCursor
        nudgeCursor = 2 + (start - 2 + 24) % 253
        macLock.unlock()
        for offset in 0..<24 {
            let n = 2 + (start - 2 + offset) % 253
            spawn("/sbin/ping", ["-c", "1", "-W", "200", "-q", "192.168.64.\(n)"])
        }
        Thread.sleep(forTimeInterval: 0.35)
        macLock.lock()
        nudging = false
        macLock.unlock()
    }

    private static func spawn(_ path: String, _ args: [String]) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: path)
        process.arguments = args
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try? process.run()
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
    /// The host is the gateway for 192.168.64.0/24 and reaches sshd on the guest address, port 22.
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
