import SwiftUI

struct Welcome: View {
    @Environment(\.openWindow) private var openWindow
    @State private var canClone = false
    @State private var cloning = false
    @State private var cloneError: String?

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
            if canClone {
                Button(action: newDebian) {
                    VStack(spacing: 2) {
                        Text(cloning ? "Creating…" : "New Debian")
                            .font(.body.weight(.semibold))
                        Text("Clone the installed snapshot.")
                            .font(.caption)
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 6)
                }
                .buttonStyle(.bordered)
                .controlSize(.large)
                .disabled(cloning)
                .padding(.top, 10)
            }
            if let cloneError {
                Text(cloneError)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 8)
            }
            Spacer(minLength: 28)
        }
        .padding(.horizontal, 36)
        .frame(width: 380)
        .onAppear { canClone = SupportDir.snapshotReady }
    }

    private func newDebian() {
        guard !cloning else { return }
        cloning = true
        cloneError = nil
        let dir = SupportDir.url
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Worker.cloneMachine(dir: dir)
            DispatchQueue.main.async {
                cloning = false
                switch result {
                case .success(let disk):
                    canClone = true
                    openWindow(id: "debian", value: disk)
                case .failure(let message):
                    cloneError = message
                }
            }
        }
    }
}
