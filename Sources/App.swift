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

@main
struct VirtualMachinesApp: App {
    @StateObject private var quickStart = QuickStartModel()

    var body: some Scene {
        WindowGroup("Virtual Machine Manager", id: "main") { Welcome() }
            .windowResizability(.automatic)
            .defaultSize(width: 1100, height: 720)
        Window("Quick start", id: "quick-start") {
            QuickStart(wizard: quickStart.wizard, installSlot: quickStart.installSlot)
        }
            // The list scrolls inside the window. Do not size the window to the full list.
            .windowResizability(.automatic)
            .defaultSize(width: 440, height: 640)
    }
}
