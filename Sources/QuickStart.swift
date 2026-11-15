import Foundation
import SwiftUI
import AppKit
import Darwin

struct WizardStep: Identifiable {
    var id: String
    var title: String
    var state: String

    /// The only rows the wizard draws. The other steps still run.
    static let shownIDs = ["download", "partition", "snapshot"]

    /// Signature, checksum, host key, and the installer files belong to one of those three rows.
    static func face(of id: String) -> String {
        switch id {
        case "sshkey", "preseed", "disk", "patch", "partition", "ssh": return "partition"
        case "snapshot": return "snapshot"
        default: return "download"
        }
    }

    /// One word under the title. Waiting, Downloading, Downloaded. Not a file path.
    static func status(_ id: String, state: String) -> String {
        switch (id, state) {
        case ("download", "now"): return "Downloading"
        case ("download", "done"): return "Downloaded"
        case ("download", "fail"): return "Failed"
        case ("partition", "now"): return "Installing"
        case ("partition", "done"): return "Installed"
        case ("partition", "fail"): return "Failed"
        case ("snapshot", "now"): return "Saving"
        case ("snapshot", "done"): return "Saved"
        case ("snapshot", "fail"): return "Failed"
        default: return "Waiting"
        }
    }

    /// Download ISO, Install guest, and Create snapshot, with the state of the work behind them.
    static func visible(_ steps: [WizardStep], cancelled: Bool = false) -> [WizardStep] {
        shownIDs.compactMap { id in
            guard var step = steps.first(where: { $0.id == id }) else { return nil }
            step.state = displayState(id, in: steps, cancelled: cancelled)
            return step
        }
    }

    static func displayState(_ id: String, in steps: [WizardStep], cancelled: Bool = false) -> String {
        let group = steps.filter { face(of: $0.id) == id }
        if group.contains(where: { $0.state == "fail" }) { return "fail" }
        if group.contains(where: { $0.state == "now" }) { return "now" }
        if !group.isEmpty, group.allSatisfy({ $0.state == "done" }) { return "done" }
        if !cancelled, group.contains(where: { $0.state == "done" }) { return "now" }
        return "wait"
    }

    static let initial = [
        WizardStep(id: "key", title: "Download OpenPGP keyring", state: "wait"),
        WizardStep(id: "sums", title: "Download checksums file", state: "wait"),
        WizardStep(id: "sign", title: "Download signature file", state: "wait"),
        WizardStep(id: "check", title: "Verify signature", state: "wait"),
        WizardStep(id: "download", title: "Download ISO", state: "wait"),
        WizardStep(id: "verify", title: "Verify checksum", state: "wait"),
    ]

    /// Host key, preseed, guest disk, and the installer copy. Same list as the download steps.
    static let preseed = [
        WizardStep(id: "sshkey", title: "Prepare the host key", state: "wait"),
        WizardStep(id: "preseed", title: "Prepare preseed disk", state: "wait"),
        WizardStep(id: "disk", title: "Prepare the guest disk", state: "wait"),
        WizardStep(id: "patch", title: "Prepare installer", state: "wait"),
    ]

    /// Install guest covers starting the installer. A clone window is not this wizard.
    static let setup = [
        WizardStep(id: "partition", title: "Install guest", state: "wait"),
        WizardStep(id: "ssh", title: "Verify with host key", state: "wait"),
        WizardStep(id: "snapshot", title: "Create snapshot", state: "wait"),
    ]

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
    @Published var steps = WizardStep.initial + WizardStep.preseed + WizardStep.setup
    @Published var received = 0.0
    @Published var total = 0.0
    @Published var headline: String?
    @Published var error: String?
    @Published var readyPath: String?
    @Published var diskPath: String?
    @Published var cancelled = false
    @Published var downloading = false
    /// True only for the run Reset just started. Opening the window again does not set this.
    private var rebuild = false
    /// Changes on Reset so the mounted install is a new guest, not the one that already finished.
    @Published var runID = 0

    private var started = false
    private var process: Process?
    private var output: FileHandle?
    private var buffer = Data()

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
        var arguments = ["download", "--dir", CacheFolder.url.deletingLastPathComponent().path]
        // Reset rebuilds the image. Opening the window again still reuses a disk that is already there.
        if rebuild {
            arguments.append("--rebuild")
        }
        process.arguments = arguments
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

    /// Start this wizard over from the first step. Deleting a file does not do this.
    /// Only the download cache is kept. Preseed, the guest disk, and the snapshot are done again, the way Packer rebuilds an image.
    /// The guest from the previous run is stopped first, so Reset does not leave Verify with host key scanning.
    func resetWizard(session: GuestSession?) {
        session?.stop()
        process?.terminate()
        reap(process)
        reset()
        rebuild = true
        runID += 1
        start()
    }

    deinit {
        output?.readabilityHandler = nil
    }

    private func reset() {
        buffer.removeAll()
        steps = WizardStep.initial + WizardStep.preseed + WizardStep.setup
        received = 0
        total = 0
        downloading = false
        error = nil
        readyPath = nil
        diskPath = nil
        cancelled = false
        headline = nil
        started = false
        rebuild = false
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
        downloading = false
        if let index = steps.firstIndex(where: { $0.state == "now" }) {
            steps[index].state = "wait"
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
        case "step", "ready":
            if let step = event.step, let state = event.state {
                applyState(step, state: state)
                // The bar belongs to Download ISO while that step is running. The next step takes it away.
                if step == "download", state != "now" { downloading = false }
            }
            if event.event == "ready" {
                readyPath = event.path
                diskPath = event.disk
                if downloading {
                    total = max(total, 1)
                    received = total
                }
            }
        case "error":
            error = event.detail ?? "Download failed."
            headline = "Download failed"
            if let step = event.step, steps.contains(where: { $0.id == step }) {
                applyState(step, state: "fail")
            } else {
                failCurrent()
            }
        default:
            break
        }
    }

    /// Download ISO, Install guest, and Create snapshot. The other steps still run.
    var visibleSteps: [WizardStep] { WizardStep.visible(steps, cancelled: cancelled) }

    private func applyState(_ id: String, state: String) {
        guard let index = steps.firstIndex(where: { $0.id == id }) else { return }
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
}

/// The guest is as wide as the window. Height follows 16:10 from that width, so a short window does not shrink it.
/// A closed wizard proposes nothing. That is not a zero-size guest, which is a blank screen.
/// A clone window names the guest disk. The path wraps instead of truncating to cache/….
struct WrappingPath: NSViewRepresentable {
    let text: String

    func makeNSView(context: Context) -> NSTextField {
        let field = NSTextField(wrappingLabelWithString: text)
        field.isEditable = false
        field.isSelectable = true
        field.isBezeled = false
        field.drawsBackground = false
        field.font = NSFont.preferredFont(forTextStyle: .caption1)
        field.textColor = .secondaryLabelColor
        field.lineBreakMode = .byCharWrapping
        field.maximumNumberOfLines = 0
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        field.setContentHuggingPriority(.required, for: .vertical)
        return field
    }

    func updateNSView(_ nsView: NSTextField, context: Context) {
        if nsView.stringValue != text { nsView.stringValue = text }
    }

    func sizeThatFits(_ proposal: ProposedViewSize, nsView: NSTextField, context: Context) -> CGSize? {
        let width = proposal.width ?? 360
        guard width.isFinite, width > 1 else { return nil }
        nsView.preferredMaxLayoutWidth = width
        let height = nsView.cell?.cellSize(forBounds: NSRect(x: 0, y: 0, width: width, height: 10_000)).height ?? 14
        return CGSize(width: width, height: max(14, ceil(height)))
    }
}

enum SupportDir {
    static var url: URL { CacheFolder.url.deletingLastPathComponent() }

    /// The worker calls this a snapshot. The directory is snapshots/debian beside the cache.
    static var snapshotLocation: String {
        url.appendingPathComponent("snapshots", isDirectory: true)
            .appendingPathComponent("debian", isDirectory: true).path
    }

    /// The snapshot saved after SSH. A partition table, a hash, or a leftover login file is not that.
    /// Deleting the snapshot disk is the same as deleting the ready marker.
    static var snapshotReady: Bool {
        let snap = url.appendingPathComponent("snapshots/debian", isDirectory: true)
        let fm = FileManager.default
        guard fm.fileExists(atPath: snap.appendingPathComponent("disk.img").path),
              let text = try? String(contentsOf: snap.appendingPathComponent("ready"), encoding: .utf8) else { return false }
        return text.hasPrefix("ssh-ok")
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

    /// One SSH attempt in the worker. A closed port is not a finished install.
    /// `cancel` is set when the app closes so a scan cannot keep the next open on "Checking SSH".
    static func probe(disk: URL, cancel: @escaping () -> Bool) -> Bool {
        guard let url = WorkerPath.url() else { return false }
        let process = Process()
        process.executableURL = url
        process.arguments = ["ssh", "--disk", disk.path]
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        let scan = ProbeScan.shared
        let token = scan.begin(process)
        defer { scan.end(process) }
        guard (try? process.run()) != nil else { return false }
        if cancel() || scan.cancelled(token) {
            scan.kill(process)
            return false
        }
        let finished = DispatchSemaphore(value: 0)
        process.terminationHandler = { _ in finished.signal() }
        while finished.wait(timeout: .now() + 0.2) == .timedOut {
            if cancel() || scan.cancelled(token) {
                scan.kill(process)
                _ = finished.wait(timeout: .now() + 1)
                return false
            }
        }
        return !scan.cancelled(token) && process.terminationStatus == 0
    }

    static func stopProbe() {
        ProbeScan.shared.stop()
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

/// The worker Verify with host key is waiting on. Closing the app must kill it, or the next open stays on that row.
final class ProbeScan: @unchecked Sendable {
    static let shared = ProbeScan()
    private let lock = NSLock()
    private var running: [Process] = []
    private var epoch = 0

    func begin(_ process: Process) -> Int {
        lock.lock()
        let token = epoch
        running.append(process)
        lock.unlock()
        return token
    }

    func end(_ process: Process) {
        lock.lock()
        running.removeAll { $0 === process }
        lock.unlock()
    }

    func cancelled(_ token: Int) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return token != epoch
    }

    func kill(_ process: Process) {
        let pid = process.processIdentifier
        if pid > 0 { kill(-pid, SIGTERM) }
        if process.isRunning { process.terminate() }
    }

    /// Kill the login that is still running. A login that starts a moment later sees the new epoch and exits.
    /// The next open starts a new check.
    func stop() {
        lock.lock()
        epoch += 1
        let pending = running
        running = []
        lock.unlock()
        for process in pending {
            kill(process)
        }
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

/// Keeps the Quick start window inside the visible screen, above the Dock and below the menu bar.
/// The step list scrolls. A tall list must not grow the window past the screen.
private struct FitWindowHeight: NSViewRepresentable {
    func makeNSView(context: Context) -> FitView { FitView() }

    func updateNSView(_ nsView: FitView, context: Context) {
        nsView.fitIfNeeded()
    }

    final class FitView: NSView {
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            fitIfNeeded()
        }

        func fitIfNeeded() {
            guard window != nil else { return }
            DispatchQueue.main.async { [weak self] in
                self?.apply()
            }
        }

        private func apply() {
            guard let window, let visible = (window.screen ?? NSScreen.main)?.visibleFrame else { return }
            let contentRect = window.contentRect(forFrameRect: window.frame)
            let chrome = window.frame.height - contentRect.height
            let maxContent = max(360, visible.height - chrome)
            let minSize = NSSize(width: 440, height: 360)
            let maxSize = NSSize(width: max(440, visible.width), height: maxContent)
            if window.contentMinSize != minSize { window.contentMinSize = minSize }
            if window.contentMaxSize != maxSize { window.contentMaxSize = maxSize }
            var frame = window.frame
            if frame.height > visible.height { frame.size.height = visible.height }
            if frame.width > visible.width { frame.size.width = visible.width }
            if frame.maxY > visible.maxY { frame.origin.y -= frame.maxY - visible.maxY }
            if frame.minY < visible.minY { frame.origin.y = visible.minY }
            if frame.minX < visible.minX { frame.origin.x = visible.minX }
            if frame.maxX > visible.maxX { frame.origin.x -= frame.maxX - visible.maxX }
            if frame != window.frame {
                window.setFrame(frame, display: true)
            }
        }
    }
}

struct QuickStart: View {
    @ObservedObject var wizard: Wizard
    @ObservedObject var installSlot: InstallSlotModel

    var body: some View {
        // One page. Install stays mounted so the guest is not stopped to show a step.
        // A zero-size mount never appears, so the install guest and the screen never start.
        // The thumbnail is drawn on Install guest while that step is running. This copy only keeps the guest running.
        page
            .background {
                if let iso = wizard.readyPath, let disk = wizard.diskPath, wizard.error == nil {
                    // Reset drops the previous guest. The same disk path must not keep a session that already finished.
                    Install(iso: iso, disk: disk, embedded: true, freshInstall: wizard.runID > 0, steps: $wizard.steps, slot: installSlot)
                        .id(wizard.runID)
                        .opacity(0)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
            }
            .background { FitWindowHeight() }
        .onAppear { wizard.start() }
    }

    /// Title and the buttons stay on screen. Only the steps scroll, so a tall list cannot hide a button or run under the Dock.
    /// The running step is brought to the top, so a later row is not left below the fold.
    private var page: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Debian 13").font(.title2.weight(.semibold))
                if let headline = wizard.headline {
                    Text(headline).font(.subheadline).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            ScrollViewReader { proxy in
                ScrollView {
                    stepList
                }
                .onChange(of: currentStepID) { _, id in
                    scrollToCurrent(proxy, id)
                }
                .onAppear { scrollToCurrent(proxy, currentStepID) }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            if let error = wizard.error ?? installSlot.session?.error {
                Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer(minLength: 12)
                // Reset keeps the download cache and rebuilds the image. Deleting a file does not.
                Button("Reset") { wizard.resetWizard(session: installSlot.session) }.buttonStyle(.bordered)
                if wizard.error != nil || wizard.cancelled {
                    Button("Try again", action: wizard.retry).buttonStyle(.borderedProminent)
                } else if let session = installSlot.session, session.stopped, session.installed, !session.loggedIn, !session.snapshotReady {
                    Button("Start Debian") { session.startInstalled() }.buttonStyle(.borderedProminent)
                } else if let session = installSlot.session, session.error != nil, session.installed, !session.snapshotReady, let disk = wizard.diskPath, GuestSSH.loggedIn(beside: URL(fileURLWithPath: disk)) {
                    Button("Save snapshot") { session.saveAgain() }.buttonStyle(.bordered)
                } else if wizard.readyPath == nil {
                    Button("Cancel", role: .cancel, action: wizard.cancel).buttonStyle(.bordered)
                }
            }
        }
        .padding(24)
        .frame(minWidth: 440, idealWidth: 440, maxWidth: .infinity, minHeight: 360, idealHeight: 640, maxHeight: .infinity, alignment: .topLeading)
    }

    /// The running row, or the failed one. A hidden step scrolls to the row it belongs to.
    private var currentStepID: String? {
        let id = wizard.steps.first { $0.state == "now" }?.id ?? wizard.steps.first { $0.state == "fail" }?.id
        return id.map { WizardStep.face(of: $0) }
    }

    /// Bring that row to the top of the list. A short delay lets the row finish laying out first.
    private func scrollToCurrent(_ proxy: ScrollViewProxy, _ id: String?) {
        guard let id else { return }
        DispatchQueue.main.async {
            withAnimation { proxy.scrollTo(id, anchor: .top) }
        }
    }

    private var stepList: some View {
        VStack(spacing: 0) {
            let shown = wizard.visibleSteps
            ForEach(shown) { step in
                // The id is the step, not its state. State changes redraw the row without losing the scroll target.
                row(step).id(step.id)
                if step.id != shown.last?.id { Divider().padding(.leading, 42) }
            }
        }
        .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    /// The download bar lives on Download ISO. It is not a slot above the list.
    @ViewBuilder private var progress: some View {
        // The bar is up as soon as the ISO fetch starts, including while the size is unknown.
        // ProgressView eases toward each value, so the fill is drawn at the byte count instead.
        if wizard.downloading {
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
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Image(systemName: icon(step.state)).foregroundStyle(color(step.state)).frame(width: 18)
                VStack(alignment: .leading, spacing: 2) {
                    Text(step.title).font(.body.weight(step.state == "now" ? .semibold : .regular))
                    Text(WizardStep.status(step.id, state: step.state))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 8)
            }
            // The bar is on Download ISO only while that fetch is running. A finished row does not keep it.
            if step.id == "download", step.state == "now" {
                progress
            }
            if step.id == "partition", showsInstaller {
                installerView
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .background(step.state == "now" ? Color.accentColor.opacity(0.08) : Color.clear)
    }

    /// The thumbnail lives on Install guest, and only while that step is running. A saved snapshot has nothing to show.
    private var showsInstaller: Bool {
        guard wizard.error == nil, let session = installSlot.session else { return false }
        guard wizard.steps.first(where: { $0.id == "partition" })?.state == "now" else { return false }
        return session.needsInstallGuest && !session.showWindow
    }

    /// A small 16:10 thumbnail, not a full-width preview.
    private var installerView: some View {
        InstallGuest(session: installSlot.session!)
            .frame(width: 160, height: 100)
            .accessibilityLabel("Installer")
            .accessibilityHint("The installer")
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
