import SwiftUI
import Virtualization

/// The screen after the download. The installer writes `disk.img`; that file is the installed system.
/// A finished preseed powers the guest off. SSH login, not the installer screen, is the finish check.
/// This view then snapshots that disk so later machines are clones.
/// Reopening Quick start boots that disk alone. It does not attach the installer again.
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
        let installed = fromSnapshot || GuestSSH.loggedIn(beside: URL(fileURLWithPath: disk)) || InstalledDisk.looksInstalled(disk)
        _guest = StateObject(wrappedValue: GuestSession(fromSnapshot: installed))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                Text(fromSnapshot || guest.installed ? "Debian" : "Installing Debian").font(.title2.weight(.semibold))
                Text(guest.headline)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            GuestView(session: guest, capture: fromSnapshot || guest.installed)
                .frame(width: 800, height: 500)
                .background(.black, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            Text(disk)
                .font(.caption)
                .foregroundStyle(.tertiary)
                .textSelection(.enabled)
                .lineLimit(2)
            if let error = guest.error {
                Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
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
        .padding(24)
        .frame(width: 848, alignment: .topLeading)
        .onAppear {
            // A clone, a login, or a disk the installer already partitioned boots alone.
            // Attaching boot.iso here is a new install: the preseed writes a new label on /dev/vda.
            if fromSnapshot || guest.installed {
                guest.bootDisk(disk)
            } else {
                guest.start(iso: iso, disk: disk)
            }
        }
        .onDisappear { guest.stop() }
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
    @Published var stopped = false
    @Published var snapshotReady = false
    @Published var cloning = false
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

    init(fromSnapshot: Bool = false) {
        installed = fromSnapshot
        headline = fromSnapshot ? "Starting Debian." : "Starting the installer."
        super.init()
    }

    func start(iso: String, disk: String) {
        guard !started else { return }
        started = true
        error = nil
        stopped = false
        diskURL = URL(fileURLWithPath: disk)
        // Reopening Quick start must not boot the installer over a disk that already has a system.
        if GuestSSH.loggedIn(beside: diskURL!) || InstalledDisk.looksInstalled(disk) {
            installed = true
            installing = false
            checking = true
            headline = "Debian is already installed. Checking SSH."
            stopped = false
            boot { try Guest.boot(disk: diskURL!) }
            watchSSH()
            return
        }
        installing = true
        headline = "Starting the installer."
        let folder = URL(fileURLWithPath: iso).deletingLastPathComponent()
        let installer = folder.appendingPathComponent("boot.iso")
        let preseed = folder.appendingPathComponent("preseed.img")
        watchSSH()
        boot {
            try Guest.boot(iso: installer, preseed: preseed, disk: diskURL!)
        }
    }

    func bootDisk(_ path: String) {
        guard !started else { return }
        started = true
        installed = true
        installing = false
        diskURL = URL(fileURLWithPath: path)
        stopped = false
        error = nil
        headline = "Starting Debian."
        boot { try Guest.boot(disk: diskURL!) }
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
        watch?.invalidate()
        machine = nil
        boot { try Guest.boot(disk: disk) }
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
        headline = "SSH login succeeded. Saving a snapshot."
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
            headline = "Installer stopped. Starting Debian to check SSH."
            startInstalled()
            return
        }
        if checking, installed, let disk = diskURL, GuestSSH.loggedIn(beside: disk) {
            headline = "SSH login succeeded. Saving a snapshot."
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
                    self.headline = "Debian shut down, but the snapshot was not saved."
                } else {
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
                        self?.headline = installed
                            ? "Debian is running from the guest disk."
                            : "The installer starts on its own and writes the guest disk."
                    case .failure(let err):
                        self?.stopped = true
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
