import SwiftUI
import AppKit

/// Quick start's download and the installer it starts. The window can close in the middle of Install guest.
/// The scene keeps this, so the virtual machine and the step list are still there when the window opens again.
/// Closing the app during Verify with host key stops that login. The next open starts the installer again instead of leaving that row stuck.
final class QuickStartModel: ObservableObject {
    let wizard = Wizard()
    let installSlot = InstallSlotModel()

    init() {
        NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification,
            object: nil,
            queue: .main
        ) { _ in
            GuestSSH.stop()
        }
    }
}

/// Shared window sizes. The scene default and the view that fills it must not each keep a copy.
enum WindowMetrics {
    static let pagePad: CGFloat = 24
    static let quickStartWidth: CGFloat = 440
    static let quickStartMinHeight: CGFloat = 360
    /// Scene default and the page's ideal height.
    static let quickStart = CGSize(width: quickStartWidth, height: 640)
    static let splashWidth: CGFloat = 380
    static let library = CGSize(width: 1100, height: 720)
    static let libraryMin = CGSize(width: 900, height: 560)
}

@main
struct VirtualMachinesApp: App {
    @StateObject private var quickStart = QuickStartModel()
    /// The library and a machine window share one session. A session owned by the library view would die when that window closes.
    @StateObject private var library = LibraryModel()

    var body: some Scene {
        WindowGroup("Virtual Machine Manager", id: "main") { Welcome(library: library) }
            .windowResizability(.automatic)
            .defaultSize(width: WindowMetrics.library.width, height: WindowMetrics.library.height)
        Window("Quick start", id: "quick-start") {
            QuickStart(wizard: quickStart.wizard, installSlot: quickStart.installSlot)
        }
            // The list scrolls inside the window. Do not size the window to the full list.
            .windowResizability(.automatic)
            .defaultSize(width: WindowMetrics.quickStart.width, height: WindowMetrics.quickStart.height)
        // One window per machine. The library stays the list. This window is the guest, at the scanout size.
        // A nil value is an empty window, not a request to create a machine. The wizard asks when it opens the main window.
        WindowGroup(id: "machine", for: String.self) { key in
            if let key = key.wrappedValue, !key.isEmpty {
                MachineWindow(library: library, machineKey: key)
            } else {
                EmptyMachineWindow(library: library)
            }
        }
            .windowResizability(.automatic)
            .defaultSize(width: Guest.scanoutSize.width, height: Guest.scanoutSize.height)
    }
}
