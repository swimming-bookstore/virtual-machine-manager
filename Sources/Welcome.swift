import SwiftUI

struct Welcome: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(spacing: 0) {
            Spacer(minLength: 0)
            Image(systemName: "desktopcomputer")
                .font(.system(size: 28, weight: .medium))
                .foregroundStyle(.white)
                .frame(width: 64, height: 64)
                .background(RoundedRectangle(cornerRadius: 16, style: .continuous).fill(.blue.gradient))
            Text("Welcome")
                .font(.title.weight(.semibold))
                .padding(.top, 18)
            Button("Quick start") { openWindow(id: "quick-start") }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .padding(.top, 22)
            Spacer(minLength: 0)
        }
    }
}
