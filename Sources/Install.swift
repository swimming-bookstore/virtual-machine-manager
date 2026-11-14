import SwiftUI
import Virtualization

/// The screen after the download. The installer writes `disk.img`; that file is the installed system.
/// A finished preseed powers the guest off. SSH login, not the installer screen, is the finish check.
/// This view then snapshots that disk so later machines are clones.
/// Setup is the next wizard page: the same steps as the download page, with a small guest popup instead of the ISO bar.
/// Reopening Quick start boots a written disk alone, then checks SSH. It does not attach the installer or a pointer.
/// A partition table is not a finished login. Quick start never attaches a pointer, so the popup cannot capture the cursor.
struct Install: View {
    let iso: String
    let disk: String
    var fromSnapshot = false
    @StateObject private var guest: GuestSession
    @Environment(\.openWindow) private var openWindow

    init(iso: String, disk: String, fromSnapshot: Bool = false) {
        self.iso = iso
        self.disk = disk
        self.fromSnapshot = fromSnapshot
        let loggedIn = fromSnapshot || GuestSSH.loggedIn(beside: URL(fileURLWithPath: disk))
        let written = loggedIn || InstalledDisk.looksInstalled(disk)
        _guest = StateObject(wrappedValue: GuestSession(fromSnapshot: loggedIn, diskWritten: written))
    }

    /// Quick start stays the download-sized wizard. A clone is the machine window.
    private var setup: Bool { !fromSnapshot }

    /// A clone captures. Quick start does not, even after SSH, because that view is only a popup.
    private var capturePointer: Bool { fromSnapshot }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            header
            if setup {
                popupButton
            } else {
                guestScreen
                    .frame(width: 800, height: 500)
            }
            if setup { stepList }
            Text(disk)
                .font(.caption)
                .foregroundStyle(.tertiary)
                .textSelection(.enabled)
                .lineLimit(2)
            if let error = guest.error {
                Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            actions
        }
        .padding(24)
        .frame(width: setup ? 440 : 848, alignment: .topLeading)
        .onAppear {
            // A clone, a login, or a disk the installer already partitioned boots alone.
            // Quick start still does not attach a pointer, so the popup cannot capture the host cursor.
            if fromSnapshot || guest.installed {
                guest.bootDisk(disk, pointer: fromSnapshot)
            } else {
                guest.start(iso: iso, disk: disk)
            }
        }
        .onDisappear {
            guest.showPopup = false
            guest.stop()
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(setup ? "Debian setup" : "Debian").font(.title2.weight(.semibold))
            Text(guest.headline)
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The guest, where the download page draws the ISO bar. Setup opens it in a popup instead of growing this window.
    private var guestScreen: some View {
        GuestView(session: guest, capture: capturePointer)
            .background(.black, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }

    /// Same slot as the ISO progress. The installer stays in a small popup so this page keeps the download width.
    private var popupButton: some View {
        Button(guest.showPopup ? "Hide installer" : "Show installer") {
            guest.showPopup.toggle()
        }
        .buttonStyle(.bordered)
        .popover(isPresented: $guest.showPopup, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 8) {
                Text("Installer").font(.headline)
                Text(guest.headline)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                guestScreen
                    .frame(width: 640, height: 400)
            }
            .padding(16)
        }
    }

    private var stepList: some View {
        VStack(spacing: 0) {
            ForEach(guest.setupSteps) { step in
                setupRow(step).id(step.id + step.state + step.detail)
                if step.id != guest.setupSteps.last?.id { Divider().padding(.leading, 42) }
            }
        }
        .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    private var actions: some View {
        HStack {
            Spacer(minLength: 12)
            if guest.stopped {
                Button(guest.installed ? "Start Debian" : "Start from the guest disk") {
                    guest.startInstalled()
                }
                .buttonStyle(.borderedProminent)
                if guest.snapshotReady {
                    Button("New virtual machine") { newMachine() }
                        .buttonStyle(.bordered)
                        .disabled(guest.cloning)
                }
                if guest.error != nil && guest.installed && !guest.snapshotReady {
                    Button("Save snapshot") { guest.saveAgain() }
                        .buttonStyle(.bordered)
                }
                if guest.error != nil && !guest.installed && !fromSnapshot {
                    Button("Try again") { guest.start(iso: iso, disk: disk) }.buttonStyle(.bordered)
                }
            }
        }
    }

    private func setupRow(_ step: WizardStep) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: setupIcon(step.state)).foregroundStyle(setupColor(step.state)).frame(width: 18)
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

    private func setupIcon(_ state: String) -> String {
        switch state {
        case "done": return "checkmark.circle.fill"
        case "now": return "circle.inset.filled"
        case "fail": return "xmark.circle.fill"
        default: return "circle"
        }
    }

    private func setupColor(_ state: String) -> Color {
        switch state {
        case "done": return .green
        case "now": return .accentColor
        case "fail": return .red
        default: return Color.secondary.opacity(0.45)
        }
    }

    private func newMachine() {
        guest.cloning = true
        let dir = SupportDir.url
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Worker.cloneMachine(dir: dir)
            DispatchQueue.main.async {
                guest.cloning = false
                switch result {
                case .success(let path):
                    openWindow(id: "debian", value: path)
                case .failure(let message):
                    guest.error = message
                }
            }
        }
    }
}

final class GuestSession: NSObject, ObservableObject, VZVirtualMachineDelegate {
    @Published var headline: String
    @Published var error: String?
    @Published var installed: Bool
    /// SSH has logged in. Quick start uses that to boot the disk alone. It still does not capture.
    @Published var loggedIn: Bool
    @Published var stopped = false
    @Published var snapshotReady = false
    @Published var cloning = false
    @Published var showPopup = false
    @Published var setupSteps = WizardStep.setup
    let view = InstallScreen()
    private var machine: VZVirtualMachine?
    private var diskURL: URL?
    private var started = false
    private var leaving = false
    private var installing = false
    private var checking = false
    private var saved = false
    private var watch: Timer?
    private var probing = false
    private let fromSnapshotBoot: Bool

    init(fromSnapshot: Bool = false, diskWritten: Bool = false) {
        installed = fromSnapshot || diskWritten
        loggedIn = fromSnapshot
        fromSnapshotBoot = fromSnapshot
        headline = fromSnapshot ? "Starting Debian." : (diskWritten ? "Debian is already installed. Checking SSH." : "Starting the installer.")
        super.init()
    }

    func start(iso: String, disk: String) {
        guard !started else { return }
        started = true
        error = nil
        stopped = false
        diskURL = URL(fileURLWithPath: disk)
        // Reopening Quick start must not boot the installer over a disk that already has a partition table.
        // That table is not a finished login. The disk boots alone, with no pointer, and SSH is still required.
        if GuestSSH.loggedIn(beside: diskURL!) || InstalledDisk.looksInstalled(disk) {
            installed = true
            loggedIn = GuestSSH.loggedIn(beside: diskURL!)
            installing = false
            checking = true
            headline = "Debian is already installed. Checking SSH."
            stopped = false
            boot { try Guest.boot(disk: diskURL!, pointer: false) }
            watchSSH()
            return
        }
        installing = true
        setupSteps = WizardStep.setup
        markSetup("installer", state: "now", detail: "boot.iso")
        headline = "Starting the installer."
        let folder = URL(fileURLWithPath: iso).deletingLastPathComponent()
        let installer = folder.appendingPathComponent("boot.iso")
        let preseed = folder.appendingPathComponent("preseed.img")
        watchSSH()
        boot {
            try Guest.boot(iso: installer, preseed: preseed, disk: diskURL!)
        }
    }

    func bootDisk(_ path: String, pointer: Bool) {
        guard !started else { return }
        started = true
        installed = true
        loggedIn = GuestSSH.loggedIn(beside: URL(fileURLWithPath: path)) || fromSnapshotBoot
        installing = false
        // A partition table is not a finished install. SSH is. The probe only runs while this is set.
        checking = true
        diskURL = URL(fileURLWithPath: path)
        stopped = false
        error = nil
        headline = fromSnapshotBoot ? "Starting Debian." : "Debian is already installed. Checking SSH."
        markSetup("installer", state: "done", detail: nil)
        markSetup("partition", state: "done", detail: "Guest disk written")
        markSetup("ssh", state: "now", detail: "guest port 22")
        boot { try Guest.boot(disk: diskURL!, pointer: pointer) }
        watchSSH()
    }

    /// The installer powers off when the preseed check passes. The next start is the disk it wrote.
    /// A black installer screen is not finished. SSH after this boot is.
    func startInstalled() {
        guard let disk = diskURL else { return }
        error = nil
        stopped = false
        installed = true
        installing = false
        checking = true
        loggedIn = GuestSSH.loggedIn(beside: disk)
        watch?.invalidate()
        machine = nil
        // This boot only checks SSH. A pointer here is what captures the cursor in the popup.
        boot { try Guest.boot(disk: disk, pointer: false) }
        watchSSH()
    }

    func stop() {
        leaving = true
        watch?.invalidate()
        machine?.stop { _ in }
    }

    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        DispatchQueue.main.async { self.noteStopped() }
    }

    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError err: Error) {
        DispatchQueue.main.async {
            let message = err.localizedDescription
            self.noteStopped()
            if !self.snapshotReady && self.error == nil && !self.checking {
                self.fail(message)
            }
        }
    }

    /// A black installer screen is not finished. SSH after the installed disk boots is.
    private func watchSSH() {
        watch?.invalidate()
        watch = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            self?.checkSSH()
        }
    }

    private func checkSSH() {
        guard installed, checking, !snapshotReady, !probing, let disk = diskURL, machine?.state == .running else { return }
        probing = true
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let ok = GuestSSH.probe(beside: disk)
            DispatchQueue.main.async {
                guard let self else { return }
                self.probing = false
                if ok { self.loginSucceeded() }
            }
        }
    }

    private func loginSucceeded() {
        guard installed, checking, !snapshotReady else { return }
        loggedIn = true
        showPopup = false
        headline = "SSH login succeeded. Saving a snapshot."
        markSetup("ssh", state: "done", detail: "debian")
        markSetup("snapshot", state: "now", detail: "snapshots/debian")
        watch?.invalidate()
        saveSnapshot()
    }

    private func noteStopped() {
        guard !leaving, !stopped else { return }
        stopped = true
        watch?.invalidate()
        machine = nil
        if installing {
            installing = false
            installed = true
            checking = true
            markSetup("installer", state: "done", detail: nil)
            markSetup("partition", state: "done", detail: "Guest disk written")
            markSetup("ssh", state: "now", detail: "guest port 22")
            headline = "Installer stopped. Starting Debian to check SSH."
            startInstalled()
            return
        }
        if checking, installed, let disk = diskURL, GuestSSH.loggedIn(beside: disk) {
            loggedIn = true
            showPopup = false
            headline = "SSH login succeeded. Saving a snapshot."
            markSetup("ssh", state: "done", detail: "debian")
            markSetup("snapshot", state: "now", detail: "snapshots/debian")
            saveSnapshot()
            return
        }
        headline = checking
            ? "Debian stopped. Waiting to check SSH."
            : "Debian stopped. The guest disk is the installed system."
    }

    func saveAgain() {
        guard let disk = diskURL, GuestSSH.loggedIn(beside: disk) else {
            error = "SSH has not logged in."
            return
        }
        saved = false
        error = nil
        headline = "Saving a snapshot."
        saveSnapshot()
    }

    private func saveSnapshot() {
        guard !saved, let disk = diskURL, GuestSSH.loggedIn(beside: disk) else { return }
        saved = true
        let support = disk.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let err = Worker.snapshot(dir: support)
            DispatchQueue.main.async {
                guard let self else { return }
                if let err {
                    self.error = err
                    self.failSetup()
                    self.headline = "Debian shut down, but the snapshot was not saved."
                } else {
                    self.markSetup("snapshot", state: "done", detail: "Saved")
                    self.snapshotReady = true
                    self.headline = "Debian is installed and shut down. A snapshot is saved for new machines."
                }
            }
        }
    }

    private func boot(_ make: () throws -> VZVirtualMachine) {
        do {
            let machine = try make()
            machine.delegate = self
            view.virtualMachine = machine
            self.machine = machine
            let installed = installed
            machine.start { [weak self] result in
                DispatchQueue.main.async {
                    switch result {
                    case .success:
                        self?.stopped = false
                        if installed {
                            // The already-installed boot is waiting on SSH. Do not replace that with a boot headline.
                            // A clone is a machine the user is running, so it keeps the running headline.
                            if self?.checking == true && self?.fromSnapshotBoot == false {
                                self?.headline = "Debian is already installed. Checking SSH."
                                self?.markSetup("ssh", state: "now", detail: "guest port 22")
                            } else {
                                self?.headline = "Debian is running from the guest disk."
                            }
                        } else {
                            self?.markSetup("installer", state: "done", detail: nil)
                            self?.markSetup("partition", state: "now", detail: "Writing /dev/vda")
                            self?.headline = "The installer starts on its own and writes the guest disk."
                        }
                    case .failure(let err):
                        self?.stopped = true
                        if !installed { self?.failSetup() }
                        self?.fail(err.localizedDescription)
                    }
                }
            }
        } catch {
            stopped = true
            fail(error.localizedDescription)
        }
    }

    private func fail(_ message: String) {
        error = message
        headline = "The virtual machine did not start."
        started = false
    }

    private func markSetup(_ id: String, state: String, detail: String?) {
        guard let index = setupSteps.firstIndex(where: { $0.id == id }) else { return }
        if let detail, !detail.isEmpty { setupSteps[index].detail = detail }
        setupSteps[index].state = state
        if state == "done" || state == "now" {
            for earlier in setupSteps.indices where earlier < index && setupSteps[earlier].state != "done" && setupSteps[earlier].state != "fail" {
                setupSteps[earlier].state = "done"
            }
        }
    }

    private func failSetup() {
        if let index = setupSteps.firstIndex(where: { $0.state == "now" }) ?? setupSteps.firstIndex(where: { $0.state == "wait" }) {
            setupSteps[index].state = "fail"
        }
    }
}

struct GuestView: NSViewRepresentable {
    let session: GuestSession
    var capture: Bool

    func makeNSView(context: Context) -> InstallScreen {
        session.view.capture = capture
        return session.view
    }

    func updateNSView(_ nsView: InstallScreen, context: Context) {
        nsView.capture = capture
    }
}

/// The guest view hides the host cursor as soon as the pointer is over it.
/// Preseed does not use a pointer, so those events stay with the host until Debian is running.
final class InstallScreen: VZVirtualMachineView {
    var capture = false {
        didSet { applyCapture() }
    }

    override var virtualMachine: VZVirtualMachine? {
        didSet { if !capture { disarm() } }
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        applyCapture()
    }

    override func layout() {
        super.layout()
        if !capture { disarm() }
    }

    override func addTrackingArea(_ trackingArea: NSTrackingArea) {
        if capture { super.addTrackingArea(trackingArea) }
    }

    override func updateTrackingAreas() {
        if capture {
            super.updateTrackingAreas()
        } else {
            disarm()
        }
    }

    override func resetCursorRects() {
        if capture {
            super.resetCursorRects()
        } else {
            discardCursorRects()
            addCursorRect(bounds, cursor: .arrow)
        }
    }

    override func addCursorRect(_ rect: NSRect, cursor: NSCursor) {
        super.addCursorRect(rect, cursor: capture ? cursor : .arrow)
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        capture ? super.hitTest(point) : nil
    }

    override var acceptsFirstResponder: Bool { capture }

    override func becomeFirstResponder() -> Bool {
        capture && super.becomeFirstResponder()
    }

    override func mouseEntered(with event: NSEvent) { if capture { super.mouseEntered(with: event) } }
    override func mouseMoved(with event: NSEvent) { if capture { super.mouseMoved(with: event) } }
    override func mouseDown(with event: NSEvent) { if capture { super.mouseDown(with: event) } }
    override func mouseUp(with event: NSEvent) { if capture { super.mouseUp(with: event) } }
    override func mouseDragged(with event: NSEvent) { if capture { super.mouseDragged(with: event) } }
    override func rightMouseDown(with event: NSEvent) { if capture { super.rightMouseDown(with: event) } }
    override func scrollWheel(with event: NSEvent) { if capture { super.scrollWheel(with: event) } }

    private func applyCapture() {
        capturesSystemKeys = capture
        if capture {
            updateTrackingAreas()
        } else {
            disarm()
            window?.makeFirstResponder(nil)
            NSCursor.unhide()
            CGAssociateMouseAndMouseCursorPosition(1)
        }
        window?.invalidateCursorRects(for: self)
    }

    private func disarm() {
        trackingAreas.forEach(removeTrackingArea)
    }
}
