import Foundation
import Virtualization

/// Boots the patched netinst, then the disk the installer writes.
/// Virtualization.framework has no CD drive, so the installer is a USB disk.
/// The guest disk is virtio (`/dev/vda`), which is the disk the preseed selects.
enum Guest {
    static let user = "debian"
    /// sshd in the guest listens here. The NAT attachment cannot map it to a host port.
    static let sshPort: UInt16 = 22
    /// The guest framebuffer in pixels. Virtio wants Int. AppKit uses the same numbers as CGFloat.
    static let scanout = (width: 1280, height: 800)
    static let scanoutSize = CGSize(width: CGFloat(scanout.width), height: CGFloat(scanout.height))
    /// Width over height. A missing proposal follows the scanout, not a second ratio.
    static let aspect = CGFloat(scanout.width) / CGFloat(scanout.height)
    /// Half the scanout. The installer panel cannot shrink below this.
    static let installerMin = CGSize(width: CGFloat(scanout.width) / 2, height: CGFloat(scanout.height) / 2)
    /// An eighth of the scanout. The step-list thumbnail.
    static let thumbnail = CGSize(width: CGFloat(scanout.width) / 8, height: CGFloat(scanout.height) / 8)
    /// Clone page guest. Height follows the scanout aspect.
    static let cloneGuest: CGSize = {
        let width: CGFloat = 800
        return CGSize(width: width, height: width / aspect)
    }()

    static func boot(iso: URL, preseed: URL, disk: URL) throws -> VZVirtualMachine {
        return try machine(disk: disk, extra: [iso, preseed], pointer: false)
    }

    static func boot(disk: URL, pointer: Bool = false) throws -> VZVirtualMachine {
        return try machine(disk: disk, extra: [], pointer: pointer, keyboard: true)
    }

    /// Locally administered MAC, stable for this disk, so the DHCP lease can be found again.
    static func macString(for disk: URL) -> String {
        var sum: UInt16 = 0x564d
        for byte in disk.path.utf8 {
            sum = sum &* 33 &+ UInt16(byte)
        }
        return String(format: "02:56:4d:4d:%02x:%02x", UInt8(sum >> 8), UInt8(truncatingIfNeeded: sum))
    }

    /// A HID boot keyboard makes Keyboard configuration a critical question, so the installer does not get one.
    private static func machine(disk: URL, extra: [URL], pointer: Bool, keyboard: Bool = false) throws -> VZVirtualMachine {
        let config = VZVirtualMachineConfiguration()
        // The installer keeps the defaults. A clone reads the file the settings pane wrote.
        let hardware = extra.isEmpty ? GuestHardware.load(beside: disk) : (cpu: GuestHardware.defaultCPU, memoryGB: GuestHardware.defaultMemoryGB)
        config.cpuCount = hardware.cpu
        config.memorySize = UInt64(hardware.memoryGB) * 1024 * 1024 * 1024
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
        graphics.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: scanout.width, heightInPixels: scanout.height)]
        config.graphicsDevices = [graphics]
        config.keyboards = extra.isEmpty && keyboard ? [VZUSBKeyboardConfiguration()] : []
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

/// CPU and memory beside the disk. Rust stores the file. Swift only clamps to what this Mac will boot.
enum GuestHardware {
    static let defaultCPU = 2
    static let defaultMemoryGB = 2

    static func load(beside disk: URL) -> (cpu: Int, memoryGB: Int) {
        let pair = Worker.hardware(disk: disk) ?? (defaultCPU, defaultMemoryGB)
        return clamp(cpuCount: pair.0, memoryGB: pair.1)
    }

    /// What this Mac will boot. The settings pane uses the same bounds.
    static func clamp(cpuCount: Int, memoryGB: Int) -> (cpu: Int, memoryGB: Int) {
        let minCPU = max(1, VZVirtualMachineConfiguration.minimumAllowedCPUCount)
        let maxCPU = max(minCPU, VZVirtualMachineConfiguration.maximumAllowedCPUCount)
        let minBytes = VZVirtualMachineConfiguration.minimumAllowedMemorySize
        let maxBytes = max(minBytes, VZVirtualMachineConfiguration.maximumAllowedMemorySize)
        let minGB = max(1, Int((minBytes + (1024 * 1024 * 1024) - 1) / (1024 * 1024 * 1024)))
        let maxGB = max(minGB, Int(maxBytes / (1024 * 1024 * 1024)))
        return (
            min(max(cpuCount, minCPU), maxCPU),
            min(max(memoryGB, minGB), maxGB)
        )
    }

    static func save(beside disk: URL, cpu: Int, memoryGB: Int) -> String? {
        let hardware = clamp(cpuCount: cpu, memoryGB: memoryGB)
        return Worker.hardware(disk: disk, cpu: hardware.cpu, memoryGB: hardware.memoryGB) == nil
            ? "Could not save CPU and memory."
            : nil
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
