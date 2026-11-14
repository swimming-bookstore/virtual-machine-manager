import Foundation
import Virtualization

/// Boots the patched netinst, then the disk the installer writes.
/// Virtualization.framework has no CD drive, so the installer is a USB disk.
/// The guest disk is virtio (`/dev/vda`), which is the disk the preseed selects.
enum Guest {
    static func boot(iso: URL, preseed: URL, disk: URL) throws -> VZVirtualMachine {
        let marker = InstallMarker.url(beside: disk)
        return try machine(disk: disk, extra: [iso, preseed], pointer: false, marker: marker)
    }

    static func boot(disk: URL) throws -> VZVirtualMachine {
        return try machine(disk: disk, extra: [], pointer: true, marker: nil)
    }

    private static func machine(disk: URL, extra: [URL], pointer: Bool, marker: URL?) throws -> VZVirtualMachine {
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
        if let marker {
            devices.append(try markerDisk(marker))
        }
        config.storageDevices = devices
        let graphics = VZVirtioGraphicsDeviceConfiguration()
        graphics.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: 1280, heightInPixels: 800)]
        config.graphicsDevices = [graphics]
        config.keyboards = [VZUSBKeyboardConfiguration()]
        // The preseed install needs no pointer. Attaching one makes the view capture the host cursor.
        config.pointingDevices = pointer ? [VZUSBScreenCoordinatePointingDeviceConfiguration()] : []
        let network = VZVirtioNetworkDeviceConfiguration()
        network.attachment = VZNATNetworkDeviceAttachment()
        config.networkDevices = [network]
        try config.validate()
        return VZVirtualMachine(configuration: config)
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

    /// The preseed writes `vmm-install-ok` to whichever virtio disk is exactly 1 MiB.
    private static func markerDisk(_ url: URL) throws -> VZVirtioBlockDeviceConfiguration {
        if !FileManager.default.fileExists(atPath: url.path) {
            FileManager.default.createFile(atPath: url.path, contents: nil)
            let handle = try FileHandle(forWritingTo: url)
            try handle.truncate(atOffset: InstallMarker.bytes)
            try handle.close()
        }
        let attachment = try VZDiskImageStorageDeviceAttachment(url: url, readOnly: false)
        return VZVirtioBlockDeviceConfiguration(attachment: attachment)
    }
}

/// The finish token lives beside the guest disk. The preseed selects that disk by size.
enum InstallMarker {
    static let name = "marker.img"
    static let bytes: UInt64 = 1024 * 1024
    static let magic = "vmm-install-ok"

    static func url(beside disk: URL) -> URL {
        disk.deletingLastPathComponent().appendingPathComponent(name)
    }

    static func done(beside disk: URL) -> Bool {
        guard let data = try? Data(contentsOf: url(beside: disk), options: [.uncached]) else { return false }
        return data.starts(with: Data(magic.utf8))
    }
}
