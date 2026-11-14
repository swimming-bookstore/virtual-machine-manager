import Foundation
import SwiftUI

struct WizardStep: Identifiable {
    var id: String
    var title: String
    var detail: String
    var state: String

    static let initial = [
        WizardStep(id: "key", title: "Download OpenPGP keyring", detail: "key-DA87E80D6294BE9B.txt", state: "wait"),
        WizardStep(id: "sums", title: "Download checksums file", detail: "SHA512SUMS", state: "wait"),
        WizardStep(id: "sign", title: "Download signature file", detail: "SHA512SUMS.sign", state: "wait"),
        WizardStep(id: "check", title: "Verify signature", detail: "SHA512SUMS.sign", state: "wait"),
        WizardStep(id: "download", title: "Download ISO", detail: isoName, state: "wait"),
        WizardStep(id: "verify", title: "Verify checksum", detail: "SHA512SUMS", state: "wait"),
    ]

    /// The second page keeps the download list and adds the preseed and patch rows.
    static let preseed = [
        WizardStep(id: "preseed", title: "Prepare preseed disk", detail: "preseed.img", state: "wait"),
        WizardStep(id: "patch", title: "Patch the installer", detail: "boot.iso", state: "wait"),
    ]

    /// Debian setup is the next page of this wizard, not a different window.
    static let setup = [
        WizardStep(id: "installer", title: "Start the installer", detail: "boot.iso", state: "wait"),
        WizardStep(id: "partition", title: "Write the guest disk", detail: "/dev/vda", state: "wait"),
        WizardStep(id: "ssh", title: "Log in with the host key", detail: "guest port 22", state: "wait"),
        WizardStep(id: "snapshot", title: "Save a snapshot", detail: "snapshots/debian", state: "wait"),
    ]

    static var isoName: String {
        #if arch(arm64)
        "debian-*-arm64-netinst.iso"
        #elseif arch(x86_64)
        "debian-*-amd64-netinst.iso"
        #else
        "debian-*-netinst.iso"
        #endif
    }
}

struct WorkerEvent: Decodable {
    var event: String
    var step: String?
    var state: String?
    var detail: String?
    var received: Double?
    var total: Double?
    var path: String?
    var disk: String?
    var key: String?
}

final class Wizard: ObservableObject {
    @Published var steps = WizardStep.initial
    @Published var page = 0
    @Published var received = 0.0
    @Published var total = 0.0
    @Published var headline: String?
    @Published var error: String?
    @Published var readyPath: String?
    @Published var diskPath: String?
    @Published var cancelled = false
    @Published var downloading = false

    private var started = false
    private var process: Process?
    private var output: FileHandle?
    private var buffer = Data()
    private var source: DispatchSourceFileSystemObject?
    private var diskSource: DispatchSourceFileSystemObject?

    var fraction: Double { total > 0 ? min(received / total, 1) : 0 }
    var percent: Int { Int((fraction * 100).rounded()) }
    var bytesLabel: String {
        if total > 0 { return "\(byteCount(received)) of \(byteCount(total))" }
        if downloading { return received > 0 ? byteCount(received) : "Connecting..." }
        return ""
    }

    func start() {
        guard !started else { return }
        started = true
        cancelled = false
        error = nil
        readyPath = nil
        diskPath = nil
        guard let worker = WorkerPath.url() else {
            error = "The worker is not built. Run cargo build --release, then try again."
            failCurrent()
            headline = "Could not start"
            return
        }
        let process = Process()
        process.executableURL = worker
        process.arguments = ["download", "--dir", CacheFolder.url.deletingLastPathComponent().path]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        output = pipe.fileHandleForReading
        output?.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            if data.isEmpty { handle.readabilityHandler = nil; return }
            DispatchQueue.main.async { self?.consume(data) }
        }
        process.terminationHandler = { [weak self] process in
            DispatchQueue.main.async {
                guard let self else { return }
                let current = self.process === process
                self.reap(process)
                guard current, !self.cancelled, self.readyPath == nil, self.error == nil, process.terminationStatus != 0 else { return }
                self.error = "The download stopped before the image was ready."
                self.headline = "Download failed"
                self.failCurrent()
            }
        }
        do {
            try process.run()
            self.process = process
            try? FileManager.default.createDirectory(at: CacheFolder.url, withIntermediateDirectories: true)
            watch(CacheFolder.url.path)
        } catch {
            self.error = "Could not start the worker. \(error.localizedDescription)"
            headline = "Could not start"
            failCurrent()
        }
    }

    func retry() {
        process?.terminate()
        reap(process)
        reset()
        start()
    }

    deinit {
        source?.cancel()
        diskSource?.cancel()
        output?.readabilityHandler = nil
    }

    /// Clear the ticks when the finished ISO, the guest disk, or the opened cache folder is gone.
    func cacheWentAway() {
        let finished = readyPath != nil || steps.contains { $0.state == "done" }
        guard finished else { return }
        if readyPath == nil && process?.isRunning == true { return }
        guard imageMissing() || diskMissing() else { return }
        if process?.isRunning == true { process?.terminate() }
        reap(process)
        reset()
        start()
    }

    private func reset() {
        source?.cancel()
        source = nil
        diskSource?.cancel()
        diskSource = nil
        buffer.removeAll()
        steps = WizardStep.initial
        page = 0
        received = 0
        total = 0
        downloading = false
        error = nil
        readyPath = nil
        diskPath = nil
        cancelled = false
        headline = nil
        started = false
    }

    private func imageMissing() -> Bool {
        if let readyPath { return !FileManager.default.fileExists(atPath: readyPath) }
        let cache = CacheFolder.url
        let fm = FileManager.default
        if !fm.fileExists(atPath: cache.path) { return true }
        let names = ["SHA512SUMS", "SHA512SUMS.sign", "manifest.json", "key-DA87E80D6294BE9B.txt"]
        if names.contains(where: { fm.fileExists(atPath: cache.appendingPathComponent($0).path) }) { return false }
        let items = (try? fm.contentsOfDirectory(at: cache, includingPropertiesForKeys: nil)) ?? []
        return !items.contains { $0.pathExtension == "iso" }
    }

    /// `disk.img` is the installed system. Removing it is not a finished install.
    /// The login proof is not that disk. Leave it and the next empty disk looks finished.
    private func diskMissing() -> Bool {
        guard let diskPath else { return false }
        let gone = !FileManager.default.fileExists(atPath: diskPath)
        if gone { GuestSSH.forget(beside: URL(fileURLWithPath: diskPath)) }
        return gone
    }

    private func watch(_ path: String) {
        source?.cancel()
        source = nil
        let fd = open(path, O_EVTONLY)
        guard fd >= 0 else { return }
        let src = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: [.write, .delete, .rename], queue: .main)
        src.setEventHandler { [weak self] in self?.cacheWentAway() }
        src.setCancelHandler { close(fd) }
        src.resume()
        source = src
    }

    /// Finder deleting disk.img does not write the cache folder. Watch the machine folder too.
    private func watchDisk(_ path: String) {
        diskSource?.cancel()
        diskSource = nil
        let folder = (path as NSString).deletingLastPathComponent
        let fd = open(folder, O_EVTONLY)
        guard fd >= 0 else { return }
        let src = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: [.write, .delete, .rename], queue: .main)
        src.setEventHandler { [weak self] in self?.cacheWentAway() }
        src.setCancelHandler { close(fd) }
        src.resume()
        diskSource = src
    }

    private func reap(_ done: Process?) {
        guard done == nil || process === done else { return }
        output?.readabilityHandler = nil
        try? output?.close()
        output = nil
        process = nil
    }

    func cancel() {
        cancelled = true
        process?.terminate()
        reap(process)
        error = nil
        headline = "Cancelled. The download was not saved."
        if let index = steps.firstIndex(where: { $0.state == "now" }) {
            steps[index].state = "wait"
            steps[index].detail = "Cancelled"
        }
    }

    private func consume(_ data: Data) {
        buffer.append(data)
        while let newline = buffer.firstIndex(of: 10) {
            let line = buffer.prefix(upTo: newline)
            buffer.removeSubrange(...newline)
            guard !line.isEmpty, let event = try? JSONDecoder().decode(WorkerEvent.self, from: Data(line)) else { continue }
            apply(event)
        }
    }

    private func apply(_ event: WorkerEvent) {
        if cancelled { return }
        // The bar is only for bytes coming off the network. A cached ISO still emits checksum progress.
        if event.event == "progress", event.step == "download" {
            downloading = true
            if let received = event.received { self.received = received }
            if let total = event.total, total > 0 { self.total = total }
        }
        switch event.event {
        case "detail":
            if let step = event.step, let detail = event.detail, !detail.isEmpty, let index = steps.firstIndex(where: { $0.id == step }) {
                steps[index].detail = detail
            }
        case "step", "ready":
            if event.step == "preseed" || event.step == "patch" { showPreseed() }
            if let step = event.step, let state = event.state { applyState(step, state: state, detail: event.detail) }
            if event.event == "ready" {
                readyPath = event.path
                diskPath = event.disk
                if downloading {
                    total = max(total, 1)
                    received = total
                }
                if let path = event.path { watch((path as NSString).deletingLastPathComponent) }
                if let disk = event.disk { watchDisk(disk) }
            }
        case "error":
            error = event.detail ?? "Download failed."
            headline = "Download failed"
            if let step = event.step, steps.contains(where: { $0.id == step }) {
                applyState(step, state: "fail", detail: event.detail)
            } else {
                failCurrent()
            }
        default:
            break
        }
    }

    private func showPreseed() {
        if page == 1 { return }
        page = 1
        // Same list as the first page. The finished download rows stay, then preseed and patch.
        var next = steps.filter { $0.id != "preseed" && $0.id != "patch" }
        for earlier in next.indices where next[earlier].state != "fail" {
            next[earlier].state = "done"
        }
        next.append(contentsOf: WizardStep.preseed)
        steps = next
    }

    private func applyState(_ id: String, state: String, detail: String?) {
        guard let index = steps.firstIndex(where: { $0.id == id }) else { return }
        if let detail, !detail.isEmpty { steps[index].detail = detail }
        steps[index].state = state
    }

    private func failCurrent() {
        if let index = steps.firstIndex(where: { $0.state == "now" }) ?? steps.firstIndex(where: { $0.state == "wait" }) {
            steps[index].state = "fail"
        }
    }

    private func byteCount(_ n: Double) -> String {
        let mb = 1024.0 * 1024.0
        let gb = mb * 1024.0
        if n >= gb { return String(format: "%.1f GB", n / gb) }
        if n >= mb { return String(format: "%.0f MB", n / mb) }
        return String(format: "%.0f KB", n / 1024.0)
    }
}

enum CacheFolder {
    static var url: URL {
        if let dir = ProcessInfo.processInfo.environment["VMM_DIR"], !dir.isEmpty {
            return URL(fileURLWithPath: dir, isDirectory: true).appendingPathComponent("cache", isDirectory: true)
        }
        return FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/virtual-machine-manager", isDirectory: true)
            .appendingPathComponent("cache", isDirectory: true)
    }

    static func open() -> String? {
        let folder = url
        do { try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true) }
        catch { return "Could not create the cache folder. \(error.localizedDescription)" }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/open")
        process.arguments = [folder.path]
        do { try process.run() } catch { return "Could not open the cache folder. \(error.localizedDescription)" }
        return nil
    }
}

enum SupportDir {
    static var url: URL { CacheFolder.url.deletingLastPathComponent() }

    static var snapshotReady: Bool {
        FileManager.default.fileExists(atPath: url.appendingPathComponent("snapshots/debian/ready").path)
    }
}

enum ClonedDisk {
    case success(String)
    case failure(String)
}

enum Worker {
    static func snapshot(dir: URL) -> String? {
        let (code, text) = run(["snapshot", "--dir", dir.path])
        return code == 0 ? nil : (message(text) ?? "Could not save the snapshot.")
    }

    static func cloneMachine(dir: URL) -> ClonedDisk {
        let (code, text) = run(["clone", "--dir", dir.path])
        if code != 0 {
            return .failure(message(text) ?? "Could not create a virtual machine.")
        }
        guard let disk = diskPath(text) else {
            return .failure("The clone did not name a disk.")
        }
        return .success(disk)
    }

    private static func run(_ args: [String]) -> (Int32, String) {
        guard let url = WorkerPath.url() else {
            return (1, "The worker is not built. Run cargo build --release, then try again.")
        }
        let process = Process()
        process.executableURL = url
        process.arguments = args
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        do {
            try process.run()
        } catch {
            return (1, error.localizedDescription)
        }
        process.waitUntilExit()
        let text = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        return (process.terminationStatus, text)
    }

    private static func message(_ text: String) -> String? {
        for line in text.split(separator: "\n") {
            if let data = line.data(using: .utf8),
               let event = try? JSONDecoder().decode(WorkerEvent.self, from: data),
               let detail = event.detail, !detail.isEmpty {
                return detail
            }
        }
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }

    private static func diskPath(_ text: String) -> String? {
        for line in text.split(separator: "\n") {
            if let data = line.data(using: .utf8),
               let event = try? JSONDecoder().decode(WorkerEvent.self, from: data),
               let disk = event.disk, !disk.isEmpty {
                return disk
            }
        }
        return nil
    }
}

enum WorkerPath {
    static func url() -> URL? {
        if let env = ProcessInfo.processInfo.environment["VMM_WORKER"], !env.isEmpty {
            let url = URL(fileURLWithPath: env)
            if FileManager.default.isExecutableFile(atPath: url.path) { return url }
        }
        let exe = URL(fileURLWithPath: CommandLine.arguments[0]).resolvingSymlinksInPath()
        let beside = exe.deletingLastPathComponent()
        let repo = beside.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let candidates = [
            beside.appendingPathComponent("virtual-machine-worker"),
            repo.appendingPathComponent("target/release/virtual-machine-worker"),
            repo.appendingPathComponent("target/debug/virtual-machine-worker"),
        ]
        return candidates.first { FileManager.default.isExecutableFile(atPath: $0.path) }
    }
}

struct QuickStart: View {
    @StateObject private var wizard = Wizard()
    @State private var folderError: String?

    var body: some View {
        Group {
            if let iso = wizard.readyPath, let disk = wizard.diskPath, wizard.error == nil {
                Install(iso: iso, disk: disk)
            } else {
                download
            }
        }
        .onAppear(perform: wizard.start)
        .task {
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 400_000_000)
                wizard.cacheWentAway()
            }
        }
    }

    private var download: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 4) {
                Text(wizard.page == 1 ? "Preseed" : "Debian 13").font(.title2.weight(.semibold))
                if let headline = wizard.headline {
                    Text(headline).font(.subheadline).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            progress
            VStack(spacing: 0) {
                ForEach(wizard.steps) { step in
                    row(step).id(step.id + step.state + step.detail)
                    if step.id != wizard.steps.last?.id { Divider().padding(.leading, 42) }
                }
            }
            .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            if let path = wizard.readyPath, wizard.error == nil {
                Text(path).font(.caption).foregroundStyle(.tertiary).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
            if let error = wizard.error {
                Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Button("Open cache folder") { folderError = CacheFolder.open() }.buttonStyle(.bordered)
                Spacer(minLength: 12)
                if wizard.error != nil || wizard.cancelled {
                    Button("Try again", action: wizard.retry).buttonStyle(.borderedProminent)
                } else if wizard.readyPath == nil {
                    Button("Cancel", role: .cancel, action: wizard.cancel).buttonStyle(.bordered)
                }
            }
            if let folderError {
                Text(folderError).font(.caption).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(24)
        .frame(width: 440, alignment: .topLeading)
    }

    @ViewBuilder private var progress: some View {
        // The bar is up as soon as the ISO fetch starts, including while the size is unknown.
        // ProgressView eases toward each value, so the fill is drawn at the byte count instead.
        if wizard.page == 0 && wizard.downloading {
            VStack(alignment: .leading, spacing: 6) {
                byteBar
                HStack {
                    Text(wizard.bytesLabel)
                    Spacer()
                    if wizard.total > 0 {
                        Text("\(wizard.percent)%").monospacedDigit()
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
        }
    }

    private var byteBar: some View {
        Capsule()
            .fill(Color.primary.opacity(0.12))
            .overlay(alignment: .leading) {
                Capsule()
                    .fill(Color.accentColor)
                    .scaleEffect(x: wizard.fraction, y: 1, anchor: .leading)
            }
            .frame(height: 6)
            .animation(nil, value: wizard.fraction)
            .accessibilityLabel("Download progress")
            .accessibilityValue(wizard.total > 0 ? "\(wizard.percent) percent" : "Connecting")
    }

    private func row(_ step: WizardStep) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: icon(step.state)).foregroundStyle(color(step.state)).frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text(step.title).font(.body.weight(step.state == "now" ? .semibold : .regular))
                Text(step.detail).font(.caption).foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .background(step.state == "now" ? Color.accentColor.opacity(0.08) : Color.clear)
    }

    private func icon(_ state: String) -> String {
        switch state {
        case "done": return "checkmark.circle.fill"
        case "now": return "circle.inset.filled"
        case "fail": return "xmark.circle.fill"
        default: return "circle"
        }
    }

    private func color(_ state: String) -> Color {
        switch state {
        case "done": return .green
        case "now": return .accentColor
        case "fail": return .red
        default: return Color.secondary.opacity(0.45)
        }
    }
}
