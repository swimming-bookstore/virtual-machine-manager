import SwiftUI

@main
struct VirtualMachinesApp: App {
    var body: some Scene {
        WindowGroup("Virtual Machine Manager") { Welcome() }
            .windowResizability(.contentSize)
        Window("Quick start", id: "quick-start") { QuickStart() }
            .windowResizability(.contentSize)
        WindowGroup(id: "debian", for: String.self) { $disk in
            if let disk {
                Install(iso: "", disk: disk, fromSnapshot: true)
            }
        }
        .windowResizability(.contentSize)
    }
}
