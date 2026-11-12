import SwiftUI

@main
struct VirtualMachinesApp: App {
    var body: some Scene {
        WindowGroup("Virtual Machine Manager") { Welcome().sized() }
            .windowResizability(.contentSize)
        Window("Quick start", id: "quick-start") { QuickStart().sized() }
            .windowResizability(.contentSize)
    }
}

private extension View {
    func sized() -> some View {
        padding(28).frame(width: 440, height: 480).background(.background)
    }
}

struct QuickStart: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Building Debian").font(.title.weight(.semibold))
            ProgressView()
            Spacer(minLength: 0)
        }
    }
}
