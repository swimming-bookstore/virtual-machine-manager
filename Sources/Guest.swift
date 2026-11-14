import Foundation
import Virtualization

/// Boots the patched netinst, then the disk the installer writes.
/// Virtualization.framework has no CD drive, so the installer is a USB disk.
/// The guest disk is virtio (`/dev/vda`), which is the disk the preseed selects.
enum Guest {
    static let user = "debian"
    static let sshPort: UInt16 = 2222

    static func boot(iso: URL, preseed: URL, disk: URL) throws -> VZVirtualMachine {
        return try machine(disk: disk, extra: [iso, preseed], pointer: false, forward: true)
    }

    static func boot(disk: URL) throws -> VZVirtualMachine {
        return try machine(disk: disk, extra: [], pointer: true, forward: true)
    }

    private static func machine(disk: URL, extra: [URL], pointer: Bool, forward: Bool) throws -> VZVirtualMachine {
        let config = VZVirtualMachineConfiguration()
        config.cpuCount = 2
        config.memorySize = 2 * 1024 * 1024 * 1024
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
        // The preseed install needs no pointer. Attaching one makes the view capture the host cursor.
        config.pointingDevices = pointer ? [VZUSBScreenCoordinatePointingDeviceConfiguration()] : []
        config.networkDevices = [network(forward: forward)]
        try config.validate()
        return VZVirtualMachine(configuration: config)
    }

    /// Host port 2222 is the guest's SSH. A login there is the finish check.
    /// `VZNATNetworkDeviceAttachment` only has `init()`. A forwarding-rule argument is a call it does not take.
    private static func network(forward: Bool) -> VZVirtioNetworkDeviceConfiguration {
        let network = VZVirtioNetworkDeviceConfiguration()
        if forward {
            network.attachment = VZNATNetworkDeviceAttachment()
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
enum GuestSSH {
    static func dir(beside disk: URL) -> URL {
        disk.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("ssh", isDirectory: true)
    }

    static func loggedIn(beside disk: URL) -> Bool {
        let proof = dir(beside: disk).appendingPathComponent("logged-in")
        guard let text = try? String(contentsOf: proof, encoding: .utf8) else { return false }
        return text.hasPrefix(Guest.user) && text.contains("ok")
    }

    /// One attempt. A closed port is not a finished install.
    static func probe(beside disk: URL) -> Bool {
        if loggedIn(beside: disk) { return true }
        let folder = dir(beside: disk)
        let key = folder.appendingPathComponent("id_ed25519")
        guard FileManager.default.isReadableFile(atPath: key.path) else { return false }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/ssh")
        process.arguments = [
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "ConnectTimeout=2",
            "-p", String(Guest.sshPort),
            "-i", key.path,
            "-l", Guest.user,
            "127.0.0.1",
            "true",
        ]
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        guard (try? process.run()) != nil else { return false }
        process.waitUntilExit()
        guard process.terminationStatus == 0 else { return false }
        let proof = folder.appendingPathComponent("logged-in")
        try? "\(Guest.user) ok\n".write(to: proof, atomically: true, encoding: .utf8)
        return true
    }
}
