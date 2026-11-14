import SwiftUI

struct Welcome: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(spacing: 0) {
            Spacer(minLength: 28)
            Image(systemName: "desktopcomputer")
                .font(.system(size: 28, weight: .medium))
                .foregroundStyle(.white)
                .frame(width: 64, height: 64)
                .background(RoundedRectangle(cornerRadius: 16, style: .continuous).fill(.blue.gradient))
            Text("Welcome")
                .font(.title2.weight(.semibold))
                .padding(.top, 16)
            Button(action: { openWindow(id: "quick-start") }) {
                VStack(spacing: 2) {
                    Text("Quick start")
                        .font(.body.weight(.semibold))
                    Text("Download Debian 13 for this Mac.")
                        .font(.caption)
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 6)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .padding(.top, 22)
            Spacer(minLength: 28)
        }
        .padding(.horizontal, 36)
        .frame(width: 380)
    }
}
