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

    /// Page two. Debian setup is the next page.
    static let preseed = [
        WizardStep(id: "sshkey", title: "Prepare the host key", detail: "id_ed25519", state: "wait"),
        WizardStep(id: "preseed", title: "Prepare preseed disk", detail: "preseed.img", state: "wait"),
        WizardStep(id: "disk", title: "Prepare the guest disk", detail: "disk.img", state: "wait"),
        WizardStep(id: "patch", title: "Prepare installer", detail: "boot.iso", state: "wait"),
    ]

    /// The installer rows. They are the third page, not this one. A clone window is not this wizard.
    static let setup = [
        WizardStep(id: "installer", title: "Start the installer", detail: "boot.iso", state: "wait"),
        WizardStep(id: "partition", title: "Install guest", detail: "disk.img", state: "wait"),
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
    /// Set when the user leaves the page the worker is on. Events do not pull them back.
    private var pinned = false
    @Published var received = 0.0
    @Published var total = 0.0
    @Published var headline: String?
    @Published var error: String?
    @Published var readyPath: String?
    @Published var diskPath: String?
    @Published var cancelled = false
    @Published var downloading = false

    private var started = false
    /// ISO path from the last ready event. Rewinding does not forget it, so a deleted ISO still maps to the download step.
    private var isoPath: String?
    private var process: Process?
    private var output: FileHandle?
    private var buffer = Data()
    private var source: DispatchSourceFileSystemObject?
    private var diskSource: DispatchSourceFileSystemObject?
    private var supportSource: DispatchSourceFileSystemObject?

    var fraction: Double { total > 0 ? min(received / total, 1) : 0 }
    var percent: Int { Int((fraction * 100).rounded()) }
    var bytesLabel: String {
        if total > 0 { return "\(byteCount(received)) of \(byteCount(total))" }
        if downloading { return received > 0 ? byteCount(received) : "Connecting..." }
        return ""
    }

    func start() { start(from: nil) }

    /// `resume` is the first step whose file is gone. Earlier steps are left as they are.
    func start(from resume: String?) {
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
        var args = ["download", "--dir", CacheFolder.url.deletingLastPathComponent().path]
        if let resume, resume != "key" {
            args += ["--from", resume]
        }
        process.arguments = args
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
            watchSupport()
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
        supportSource?.cancel()
        output?.readabilityHandler = nil
    }

    /// A finished step's file was removed. Run again from that step. Earlier steps stay done.
    func cacheWentAway() {
        guard !cancelled else { return }
        if process?.isRunning == true { return }
        guard let missing = firstMissing() else { return }
        resume(from: missing)
    }

    private func resume(from id: String) {
        process?.terminate()
        reap(process)
        rewind(to: id)
        forgetDiskIfNeeded()
        started = false
        start(from: id)
    }

    /// The first finished step whose file is gone. Later files are rebuilt by that step.
    private func firstMissing() -> String? {
        let order = ["key", "sums", "sign", "check", "download", "verify", "sshkey", "preseed", "disk", "patch"]
        for id in order {
            guard let step = steps.first(where: { $0.id == id }), step.state == "done" else { continue }
            if !artifactOK(id) { return id }
        }
        return nil
    }

    private func artifactOK(_ id: String) -> Bool {
        let fm = FileManager.default
        let cache = CacheFolder.url
        let support = cache.deletingLastPathComponent()
        func has(_ url: URL) -> Bool { fm.fileExists(atPath: url.path) }
        switch id {
        case "key":
            return has(cache.appendingPathComponent("key-DA87E80D6294BE9B.txt"))
        case "sums":
            return has(cache.appendingPathComponent("SHA512SUMS"))
        case "sign":
            return has(cache.appendingPathComponent("SHA512SUMS.sign"))
        case "check":
            return true
        case "download":
            guard let isoPath else { return false }
            return has(URL(fileURLWithPath: isoPath))
        case "verify":
            return has(cache.appendingPathComponent("manifest.json"))
        case "sshkey":
            return has(support.appendingPathComponent("ssh/id_ed25519"))
                && has(support.appendingPathComponent("ssh/id_ed25519.pub"))
                && has(support.appendingPathComponent("ssh/authorized_keys"))
        case "preseed":
            return has(support.appendingPathComponent("preseed.img"))
        case "disk":
            let disk = diskPath.map { URL(fileURLWithPath: $0) }
                ?? support.appendingPathComponent("machines/debian/disk.img")
            return has(disk)
        case "patch":
            return has(support.appendingPathComponent("boot.iso"))
        default:
            return true
        }
    }

    /// Drop this step and everything after it. The page follows the step that has to run.
    private func rewind(to id: String) {
        if !Self.pageTwo.contains(id) {
            steps.removeAll { Self.pageTwo.contains($0.id) }
        } else if !steps.contains(where: { $0.id == "sshkey" }) {
            steps.append(contentsOf: WizardStep.preseed)
        }
        guard let index = steps.firstIndex(where: { $0.id == id }) else { return }
        for i in index..<steps.count {
            steps[i].state = i == index ? "now" : "wait"
            steps[i].detail = Self.detail(for: steps[i].id)
        }
        readyPath = nil
        diskPath = nil
        downloading = false
        received = 0
        total = 0
        error = nil
        headline = nil
        cancelled = false
        page = Self.pageTwo.contains(id) ? 1 : 0
        pinned = false
    }

    private static func detail(for id: String) -> String {
        switch id {
        case "key": return "key-DA87E80D6294BE9B.txt"
        case "sums": return "SHA512SUMS"
        case "sign", "check": return "SHA512SUMS.sign"
        case "download": return WizardStep.isoName
        case "verify": return "SHA512SUMS"
        case "sshkey": return "id_ed25519"
        case "preseed": return "preseed.img"
        case "disk": return "disk.img"
        case "patch": return "boot.iso"
        default: return ""
        }
    }

    /// Preparing the disk writes this file. A stale login must not survive it.
    private func forgetDiskIfNeeded() {
        let disk = CacheFolder.url.deletingLastPathComponent().appendingPathComponent("machines/debian/disk.img")
        if !FileManager.default.fileExists(atPath: disk.path) {
            GuestSSH.forget(beside: disk)
        }
    }

    private func reset() {
        source?.cancel()
        source = nil
        diskSource?.cancel()
        diskSource = nil
        supportSource?.cancel()
        supportSource = nil
        buffer.removeAll()
        steps = WizardStep.initial
        page = 0
        pinned = false
        received = 0
        total = 0
        downloading = false
        error = nil
        readyPath = nil
        diskPath = nil
        isoPath = nil
        cancelled = false
        headline = nil
        started = false
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

    /// preseed.img and boot.iso sit beside the cache. Deleting them does not write the cache folder.
    private func watchSupport() {
        supportSource?.cancel()
        supportSource = nil
        let support = CacheFolder.url.deletingLastPathComponent()
        try? FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
        let fd = open(support.path, O_EVTONLY)
        guard fd >= 0 else { return }
        let src = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: [.write, .delete, .rename], queue: .main)
        src.setEventHandler { [weak self] in self?.cacheWentAway() }
        src.setCancelHandler { close(fd) }
        src.resume()
        supportSource = src
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
        follow()
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
            if let step = event.step, Self.pageTwo.contains(step) { showPreseed() }
            if let step = event.step, let state = event.state { applyState(step, state: state, detail: event.detail) }
            if event.event == "ready" {
                readyPath = event.path
                diskPath = event.disk
                if let path = event.path { isoPath = path }
                if downloading {
                    total = max(total, 1)
                    received = total
                }
                if let path = event.path { watch((path as NSString).deletingLastPathComponent) }
                if let disk = event.disk { watchDisk(disk) }
                watchSupport()
            }
            follow()
        case "error":
            error = event.detail ?? "Download failed."
            headline = "Download failed"
            if let step = event.step, steps.contains(where: { $0.id == step }) {
                applyState(step, state: "fail", detail: event.detail)
            } else {
                failCurrent()
            }
            follow()
        default:
            break
        }
    }

    /// Remember the host key, preseed, the guest disk, and the installer copy. They stay off the download page.
    private func showPreseed() {
        guard !steps.contains(where: { $0.id == "sshkey" }) else { return }
        for earlier in steps.indices where steps[earlier].state != "fail" {
            steps[earlier].state = "done"
        }
        steps.append(contentsOf: WizardStep.preseed)
    }

    /// The page the worker has reached. Prepare is the second page. Install is the third, once the files are ready.
    private var latest: Int {
        if readyPath != nil && diskPath != nil && error == nil { return 2 }
        return preseedReached ? 1 : 0
    }

    private static let pageTwo = ["sshkey", "preseed", "disk", "patch"]

    private var preseedReached: Bool {
        steps.contains { Self.pageTwo.contains($0.id) && $0.state != "wait" }
    }

    /// Move to the worker's page unless the user is looking at another one.
    private func follow() {
        if page > latest {
            page = latest
            pinned = false
        }
        if !pinned { page = latest }
    }

    func back() {
        guard page > 0 else { return }
        page -= 1
        pinned = true
    }

    func forward() {
        guard page < latest else { return }
        page += 1
        pinned = page != latest
    }

    var canGoBack: Bool { page > 0 }
    var canGoForward: Bool { page < latest }

    /// Rows for the page on screen. Page two is the host key, preseed, the disk, and the installer copy.
    var visibleSteps: [WizardStep] {
        let ids = page == 1 ? Self.pageTwo : ["key", "sums", "sign", "check", "download", "verify"]
        return ids.compactMap { id in steps.first { $0.id == id } }
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
        openFolder(url, name: "cache folder")
    }
}

/// Opens a directory in Finder. The download page uses the cache. Prepare and setup use the data folder, which also holds the host key, the guest disk, and the snapshot.
private func openFolder(_ folder: URL, name: String) -> String? {
    do { try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true) }
    catch { return "Could not create the \(name). \(error.localizedDescription)" }
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/open")
    process.arguments = [folder.path]
    do { try process.run() } catch { return "Could not open the \(name). \(error.localizedDescription)" }
    return nil
}

enum SupportDir {
    static var url: URL { CacheFolder.url.deletingLastPathComponent() }

    /// Host key, preseed, boot copy, cache, guest disk, and snapshot. Prepare and setup are not the download cache.
    static func open() -> String? {
        openFolder(url, name: "folder")
    }

    /// Both files. Deleting the snapshot disk is the same as deleting the ready marker.
    static var snapshotReady: Bool {
        let snap = url.appendingPathComponent("snapshots/debian", isDirectory: true)
        let fm = FileManager.default
        return fm.fileExists(atPath: snap.appendingPathComponent("ready").path)
            && fm.fileExists(atPath: snap.appendingPathComponent("disk.img").path)
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
        // Install stays mounted, so Back does not stop the guest. Prepare is the page under it.
        ZStack(alignment: .topLeading) {
            if let iso = wizard.readyPath, let disk = wizard.diskPath, wizard.error == nil {
                Install(
                    iso: iso,
                    disk: disk,
                    onBack: wizard.canGoBack ? { wizard.back() } : nil as (() -> Void)?,
                    onForward: wizard.canGoForward ? { wizard.forward() } : nil as (() -> Void)?
                )
                    .opacity(wizard.page == 2 ? 1 : 0)
                    .allowsHitTesting(wizard.page == 2)
                    .accessibilityHidden(wizard.page != 2)
            }
            if wizard.page != 2 || wizard.readyPath == nil || wizard.error != nil {
                download
                    .background(.background)
            }
        }
        .onAppear { wizard.start() }
        .task {
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 400_000_000)
                wizard.cacheWentAway()
            }
        }
    }

    private var pageTitle: String {
        switch wizard.page {
        case 1: return "Prepare"
        default: return "Debian 13"
        }
    }

    private var download: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 4) {
                Text(pageTitle).font(.title2.weight(.semibold))
                if let headline = wizard.headline {
                    Text(headline).font(.subheadline).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            progress
            VStack(spacing: 0) {
                let shown = wizard.visibleSteps
                ForEach(shown) { step in
                    row(step).id(step.id + step.state + step.detail)
                    if step.id != shown.last?.id { Divider().padding(.leading, 42) }
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
                if wizard.page == 0 {
                    Button("Open cache folder") { folderError = CacheFolder.open() }.buttonStyle(.bordered)
                } else {
                    Button("Open folder") { folderError = SupportDir.open() }.buttonStyle(.bordered)
                }
                Spacer(minLength: 12)
                if wizard.canGoBack {
                    Button("Back", action: wizard.back).buttonStyle(.bordered)
                }
                if wizard.canGoForward {
                    Button("Next", action: wizard.forward).buttonStyle(.bordered)
                }
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
