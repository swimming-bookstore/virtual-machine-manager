import SwiftUI
import Virtualization

/// The screen after the download. The installer writes the snapshot disk.
/// A finished preseed powers the guest off. SSH login, not the installer screen, is the finish check.
/// This view then marks that disk ready so later machines are clones.
/// Quick start is one page. This view stays mounted so the guest keeps running, and its rows are not drawn there.
/// Quick start draws a small thumbnail on Install guest, and only while that step is running. Open sits beside it.
/// A partition table, a leftover login file, or a hash of the disk does not mean the guest finished.
/// Running the wizard again starts the installer from Install guest. A saved snapshot is the finished install.
/// Reopening that disk does not start the installer virtual machine.
/// Nothing is installing then, so there is no download bar and no thumbnail.
/// Quick start never attaches a pointer, so neither view can capture the cursor.
struct Install: View {
    let iso: String
    let disk: String
    var fromSnapshot = false
    /// Quick start keeps this view mounted and draws the steps itself. A clone window draws them here.
    var embedded = false
    var steps: Binding<[WizardStep]>? = nil
    /// Quick start's bar slot. The mounted install publishes its session so that slot does not start another guest.
    var slot: InstallSlotModel? = nil
    /// Reset is building a new image. A snapshot already on disk must not skip the installer.
    var freshInstall = false
    @StateObject private var guest: GuestSession
    @Environment(\.openWindow) private var openWindow

    init(iso: String, disk: String, fromSnapshot: Bool = false, embedded: Bool = false, freshInstall: Bool = false, steps: Binding<[WizardStep]>? = nil, slot: InstallSlotModel? = nil) {
        self.iso = iso
        self.disk = disk
        self.fromSnapshot = fromSnapshot
        self.embedded = embedded
        self.freshInstall = freshInstall
        self.steps = steps
        self.slot = slot
        let loggedIn = fromSnapshot || GuestSSH.loggedIn(beside: URL(fileURLWithPath: disk))
        // A partition table is not a finished guest. Neither is a hash of disk.img, or a leftover login file.
        // Running the wizard again starts the installer. Only a snapshot saved after SSH skips Install guest.
        // Reset does not skip it. That run is a new image, the way Packer rebuilds.
        // loggedIn is not a clone. Passing it as fromSnapshot skipped Create snapshot after the files were removed.
        let snapshotSaved = !fromSnapshot && !freshInstall && SupportDir.snapshotReady
        _guest = StateObject(wrappedValue: GuestSession(fromSnapshot: fromSnapshot, diskWritten: false, snapshotSaved: snapshotSaved, loggedIn: loggedIn))
    }

    /// Quick start stays the download-sized wizard. A clone is the machine window.
    private var setup: Bool { !fromSnapshot }

    /// A clone captures. Quick start does not, including the installer window opened from the thumbnail.
    private var capturePointer: Bool { fromSnapshot }

    var body: some View {
        Group {
            if embedded {
                // Quick start draws the screen and the steps. This copy only keeps the guest running.
                Color.clear
            } else {
                page
            }
        }
        .background {
            // The window stays with the mounted guest. Quick start draws the thumbnail, not this page.
            // The embedded copy is invisible, so it must not also open the installer panel.
            if setup && !embedded {
                InstallerWindow(guest: guest, presented: $guest.showWindow) {
                    GuestMirror(session: guest)
                        .frame(width: 800, height: 500)
                        .background(.black, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
                }
            }
        }
        .onDisappear {
            // Closing the wizard mid-install must not stop the guest. The scene still owns this session.
            // A clone window is the only one that stops its machine when it goes away.
            // Verify with host key is different: its login scan outlives the window. Stop that scan.
            // The next wizard run starts the installer again. It does not resume the disk alone.
            if embedded {
                DispatchQueue.main.async {
                    let open = NSApp.windows.contains { $0.title == "Quick start" && $0.isVisible }
                    if !open {
                        guest.pauseHostKey()
                    }
                }
                return
            }
            if slot?.session === guest { slot?.session = nil }
            guest.stop()
        }
        .onAppear {
            slot?.session = guest
            if let steps {
                guest.steps = steps
            }
            // A saved snapshot means the installer already shut down. Leave that VM stopped.
            // A clone boots its own disk. Running the wizard again starts the installer, even if this disk has a partition table.
            // Quick start still does not attach a pointer, so the installer window cannot capture the host cursor.
            if guest.snapshotReady && !fromSnapshot {
                guest.holdFinished(disk)
            } else if guest.leaving && !fromSnapshot {
                guest.resumeHostKey()
            } else if fromSnapshot {
                guest.bootDisk(disk, pointer: true)
            } else {
                guest.start(iso: iso, disk: disk)
            }
        }
    }

    private var page: some View {
        VStack(alignment: .leading, spacing: 18) {
            header
            if !setup {
                guestScreen
                    .frame(width: 800, height: 500)
            }
            if setup { stepList }
            WrappingPath(text: disk)
            if let error = guest.error {
                Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            actions
        }
        .padding(24)
        .frame(width: setup ? 440 : 848, alignment: .topLeading)
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

    /// The guest screen. A clone window shows this at 800 by 500. Quick start uses a thumbnail on the step instead.
    private var guestScreen: some View {
        GuestView(session: guest, capture: capturePointer)
            .background(.black, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }

    /// Installer rows. A clone window is not this wizard.
    private var stepList: some View {
        let shown = WizardStep.visible(guest.setupSteps)
        return VStack(spacing: 0) {
            ForEach(shown) { step in
                setupRow(step).id(step.id + step.state)
                if step.id != shown.last?.id { Divider().padding(.leading, 42) }
            }
        }
        .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    private var actions: some View {
        HStack {
            Spacer(minLength: 12)
            if guest.stopped {
                // The installer disk is the snapshot being built. Starting it again is not a machine the user runs.
                // New virtual machine clones that snapshot. Start Debian stays for a disk that still needs the SSH check.
                if !guest.snapshotReady {
                    Button(guest.installed ? "Start Debian" : "Start from the guest disk") {
                        guest.startInstalled()
                    }
                    .buttonStyle(.borderedProminent)
                }
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
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Image(systemName: setupIcon(step.state)).foregroundStyle(setupColor(step.state)).frame(width: 18)
                VStack(alignment: .leading, spacing: 2) {
                    Text(step.title).font(.body.weight(step.state == "now" ? .semibold : .regular))
                    Text(WizardStep.status(step.id, state: step.state))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 8)
            }
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

/// Holds the guest the mounted install started, so the bar slot can draw that screen.
final class InstallSlotModel: ObservableObject {
    @Published var session: GuestSession?
}

/// The guest screen in the slot the download page uses for the ISO bar.
/// This is the session Quick start already started. It is not a second installer, and it does not draw the step list.
struct InstallGuest: View {
    @ObservedObject var session: GuestSession

    var body: some View {
        GuestView(session: session, capture: false)
            .background(.black, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
            .allowsHitTesting(false)
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
    @Published var showWindow = false
    @Published var cloning = false
    @Published var setupSteps = WizardStep.setup
    /// Quick start passes its own list. A clone keeps the rows on this session.
    var steps: Binding<[WizardStep]>?
    let view = InstallScreen()
    private var machine: VZVirtualMachine?
    private var diskURL: URL?
    private var started = false
    /// Set when the window closes during Verify with host key. The next wizard run starts the installer again.
    private(set) var leaving = false
    private var installing = false
    private var checking = false
    private var saved = false
    private var watch: Timer?
    private var probing = false
    private var savingSnapshot = false
    /// Boots of the installed disk while Verify with host key has not logged in. A quit used to leave that row waiting.
    private var sshBoots = 0
    private let fromSnapshotBoot: Bool

    /// True while Install guest still has to run. A saved snapshot does not. A removed disk does.
    var needsInstallGuest: Bool {
        !fromSnapshotBoot && !snapshotReady
    }

    /// The installer image is attached. Verify with host key is not this.
    var installingGuest: Bool { installing }

    init(fromSnapshot: Bool = false, diskWritten: Bool = false, snapshotSaved: Bool = false, loggedIn: Bool = false) {
        installed = fromSnapshot || diskWritten || snapshotSaved
        self.loggedIn = fromSnapshot || snapshotSaved || loggedIn
        fromSnapshotBoot = fromSnapshot
        snapshotReady = snapshotSaved
        saved = snapshotSaved
        headline = if snapshotSaved && !fromSnapshot {
            "Debian is installed and shut down. A snapshot is saved for new machines."
        } else if fromSnapshot {
            "Starting Debian."
        } else if diskWritten {
            "Debian is already installed. Checking SSH."
        } else {
            "Starting the installer."
        }
        super.init()
        if snapshotSaved && !fromSnapshot {
            stopped = true
            markSetup("partition", state: "done")
            markSetup("ssh", state: "done")
            markSetup("snapshot", state: "done")
        }
    }

    /// Quick start came back after the snapshot. Do not start the installer virtual machine.
    func holdFinished(_ path: String) {
        diskURL = URL(fileURLWithPath: path)
        installing = false
        checking = false
        stopped = true
        installed = true
        loggedIn = true
        snapshotReady = true
        saved = true
        started = true
        showWindow = false
        error = nil
        headline = "Debian is installed and shut down. A snapshot is saved for new machines."
        markSetup("partition", state: "done")
        markSetup("ssh", state: "done")
        markSetup("snapshot", state: "done")
    }

    func start(iso: String, disk: String) {
        guard !started else { return }
        started = true
        error = nil
        stopped = false
        diskURL = URL(fileURLWithPath: disk)
        // The snapshot is already saved. Reopening must not start the installer virtual machine.
        if snapshotReady {
            holdFinished(disk)
            return
        }
        // A partition table or a leftover login file is not a finished guest. The wizard runs the installer again.
        installing = true
        setupSteps = WizardStep.setup
        markSetup("partition", state: "now")
        headline = "Starting the installer."
        let installer = URL(fileURLWithPath: iso).deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("installer", isDirectory: true)
        boot {
            try Guest.boot(
                iso: installer.appendingPathComponent("boot.iso"),
                preseed: installer.appendingPathComponent("preseed.img"),
                disk: diskURL!
            )
        }
    }

    func bootDisk(_ path: String, pointer: Bool) {
        guard !started else { return }
        started = true
        installed = true
        loggedIn = fromSnapshotBoot || GuestSSH.loggedIn(beside: URL(fileURLWithPath: path))
        installing = false
        diskURL = URL(fileURLWithPath: path)
        stopped = false
        error = nil
        // The snapshot is the artifact. Do not boot it just to check SSH again.
        if !fromSnapshotBoot && SupportDir.snapshotReady {
            holdFinished(path)
            return
        }
        // A partition table is not a finished install. SSH is. The probe only runs while this is set.
        checking = true
        headline = fromSnapshotBoot ? "Starting Debian." : "Debian is already installed. Checking SSH."
        markSetup("partition", state: "done")
        markSetup("ssh", state: "now")
        boot { try Guest.boot(disk: diskURL!, pointer: pointer) }
    }

    /// The installer powers off when the preseed check passes. The next start is the disk it wrote.
    /// A black installer screen is not finished. SSH after this boot is.
    func startInstalled() {
        sshBoots = 0
        bootInstalled()
    }

    /// Same boot as Start Debian. Retries do not reset the count, so a guest that exits immediately does not loop.
    private func bootInstalled() {
        guard !leaving, let disk = diskURL else { return }
        // The installer already finished and the snapshot is saved. Do not start it again.
        if !fromSnapshotBoot && SupportDir.snapshotReady {
            holdFinished(disk.path)
            return
        }
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
    }

    /// Closing Quick start during Verify with host key. Stop the login so it cannot block the next open.
    /// The next wizard run starts the installer again. It does not boot this disk alone.
    func pauseHostKey() {
        guard checking, !fromSnapshotBoot, !snapshotReady else { return }
        leaving = true
        probing = false
        watch?.invalidate()
        watch = nil
        GuestSSH.stop()
        let running = machine
        machine = nil
        stopped = true
        running?.delegate = nil
        running?.stop { _ in }
    }

    func stop() {
        leaving = true
        probing = false
        watch?.invalidate()
        watch = nil
        // The host-key login is still running. Kill it, or the next open stays on Verify with host key.
        GuestSSH.stop()
        machine?.stop { _ in }
    }

    deinit {
        leaving = true
        watch?.invalidate()
        GuestSSH.stop()
        machine?.delegate = nil
        machine?.stop { _ in }
    }

    /// The app was closed on Verify with host key. The next wizard run starts the installer again.
    /// A partition table is not proof the guest finished, so this does not boot the disk alone.
    func resumeHostKey() {
        leaving = false
        guard !fromSnapshotBoot, let disk = diskURL else { return }
        if snapshotReady || SupportDir.snapshotReady {
            holdFinished(disk.path)
            return
        }
        loggedIn = false
        checking = false
        stopped = false
        saved = false
        sshBoots = 0
        error = nil
        showWindow = false
        probing = false
        installed = false
        machine = nil
        started = false
        let installer = disk.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("installer/boot.iso")
        // start() finds installer/ from the ISO's parent. The file name is not used.
        start(iso: installer.path, disk: disk.path)
    }

    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        DispatchQueue.main.async {
            // A host-key pause stops the old machine. Its callback must not boot over the one the next open started.
            guard virtualMachine === self.machine else { return }
            self.noteStopped()
        }
    }

    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError err: Error) {
        DispatchQueue.main.async {
            guard virtualMachine === self.machine else { return }
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
        // The first probe must not wait for VZVirtualMachine.state == .running.
        // start() reports success before that state changes, and a stopped guest never reaches it.
        checkSSH()
        watch = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            self?.checkSSH()
        }
    }

    private func checkSSH() {
        // SSH is how this build finishes. A saved snapshot is the artifact, so it is not probed again.
        guard installed, checking, !snapshotReady, !probing, let disk = diskURL else { return }
        // A stopped machine cannot answer. Wait, but do not require .running: that state lags start().
        if stopped && machine == nil { return }
        probing = true
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let ok = GuestSSH.probe(beside: disk) { self == nil || self?.leaving == true }
            DispatchQueue.main.async {
                guard let self else { return }
                self.probing = false
                if ok { self.loginSucceeded() }
            }
        }
    }

    private func loginSucceeded() {
        guard installed, checking, !snapshotReady else { return }
        checking = false
        loggedIn = true
        showWindow = false
        markSetup("ssh", state: "done")
        watch?.invalidate()
        // The snapshot is already the artifact. Packer does not log in again to recreate it.
        headline = "SSH login succeeded. Saving a snapshot."
        markSetup("snapshot", state: "now")
        saveSnapshot()
    }

    private func noteStopped() {
        guard !leaving else { return }
        // Stopping the guest so the snapshot can be copied is not a failed boot.
        if savingSnapshot {
            stopped = true
            watch?.invalidate()
            machine = nil
            return
        }
        guard !stopped else { return }
        stopped = true
        watch?.invalidate()
        machine = nil
        if installing {
            installing = false
            installed = true
            checking = true
            markSetup("partition", state: "done")
            markSetup("ssh", state: "now")
            headline = "Installer stopped. Starting Debian to check SSH."
            startInstalled()
            return
        }
        if checking, installed, !snapshotReady, let disk = diskURL, GuestSSH.loggedIn(beside: disk) {
            checking = false
            loggedIn = true
            showWindow = false
            markSetup("ssh", state: "done")
            headline = "SSH login succeeded. Saving a snapshot."
            markSetup("snapshot", state: "now")
            saveSnapshot()
            return
        }
        // The guest stopped before the host key logged in. Waiting here is what leaves Verify with host key stuck.
        if checking, installed, !snapshotReady {
            retrySSH()
            return
        }
        headline = "Debian stopped. The guest disk is the installed system."
    }

    /// Boot the installed disk again. A quit in the middle of Verify with host key used to leave this row waiting.
    private func retrySSH() {
        guard !leaving, checking, installed, !snapshotReady else { return }
        sshBoots += 1
        if sshBoots <= 3 {
            headline = "Debian stopped. Checking SSH again."
            markSetup("ssh", state: "now")
            DispatchQueue.main.async { [weak self] in
                self?.bootInstalled()
            }
            return
        }
        headline = "Debian stopped before the host key logged in."
        error = "SSH has not logged in."
    }

    func saveAgain() {
        guard !savingSnapshot, let disk = diskURL, GuestSSH.loggedIn(beside: disk) else {
            if !savingSnapshot { error = "SSH has not logged in." }
            return
        }
        saved = false
        error = nil
        headline = "Saving a snapshot."
        markSetup("snapshot", state: "now")
        saveSnapshot()
    }

    /// Copy snapshots/debian after the guest has stopped. A removed snapshot is copied again.
    private func saveSnapshot() {
        guard !saved, let disk = diskURL, GuestSSH.loggedIn(beside: disk) else { return }
        saved = true
        savingSnapshot = true
        error = nil
        if let machine, !stopped {
            machine.stop { [weak self] _ in
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.stopped = true
                    self.watch?.invalidate()
                    self.machine = nil
                    self.copySnapshot(disk)
                }
            }
            return
        }
        copySnapshot(disk)
    }

    private func copySnapshot(_ disk: URL) {
        let support = disk.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let err = Worker.snapshot(dir: support)
            DispatchQueue.main.async {
                guard let self else { return }
                self.savingSnapshot = false
                if let err {
                    self.saved = false
                    self.snapshotReady = false
                    self.stopped = true
                    self.error = err
                    self.failSetup()
                    self.headline = "Debian shut down, but the snapshot was not saved."
                } else {
                    self.markSetup("snapshot", state: "done")
                    self.showWindow = false
                    self.snapshotReady = true
                    self.stopped = true
                    self.headline = "Debian is installed and shut down. A snapshot is saved for new machines."
                }
            }
        }
    }

    func attach(_ screen: InstallScreen) {
        screen.capture = false
        if screen.virtualMachine !== machine {
            screen.virtualMachine = machine
        }
    }

    private func boot(_ make: () throws -> VZVirtualMachine) {
        do {
            let machine = try make()
            machine.delegate = self
            view.virtualMachine = machine
            view.capture = false
            self.machine = machine
            let installed = installed
            machine.start { [weak self] result in
                DispatchQueue.main.async {
                    // A close during Verify with host key stops this machine. Ignore a start that lost the race.
                    guard let self, self.machine === machine else { return }
                    switch result {
                    case .success:
                        self.stopped = false
                        if installed {
                            // The already-installed boot is waiting on SSH. Do not replace that with a boot headline.
                            // A clone is a machine the user is running, so it keeps the running headline.
                            if self.checking && !self.fromSnapshotBoot {
                                self.headline = "Debian is already installed. Checking SSH."
                                self.markSetup("ssh", state: "now")
                            } else {
                                self.headline = "Debian is running from the guest disk."
                            }
                            // start() can report success before state becomes .running. Probe from here.
                            // A saved snapshot is the artifact. Do not probe it again.
                            if self.checking && !self.snapshotReady {
                                self.watchSSH()
                            }
                        } else {
                            self.markSetup("partition", state: "now")
                            self.headline = "The installer starts on its own and installs Debian."
                        }
                    case .failure(let err):
                        self.stopped = true
                        if self.checking, !self.leaving, !self.snapshotReady {
                            self.retrySSH()
                        } else {
                            if !installed { self.failSetup() }
                            self.fail(err.localizedDescription)
                        }
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

    private func markSetup(_ id: String, state: String) {
        guard let index = setupSteps.firstIndex(where: { $0.id == id }) else { return }
        setupSteps[index].state = state
        if state == "done" || state == "now" {
            for earlier in setupSteps.indices where earlier < index && setupSteps[earlier].state != "done" && setupSteps[earlier].state != "fail" {
                setupSteps[earlier].state = "done"
            }
        }
        if let steps {
            var list = steps.wrappedValue
            for row in setupSteps {
                guard let at = list.firstIndex(where: { $0.id == row.id }) else { continue }
                list[at].state = row.state
            }
            steps.wrappedValue = list
        }
    }

    private func failSetup() {
        if let index = setupSteps.firstIndex(where: { $0.state == "now" }) ?? setupSteps.firstIndex(where: { $0.state == "wait" }) {
            setupSteps[index].state = "fail"
        }
    }
}

/// Opens the guest in its own window. The wizard stays the download size.
/// The view is not in the wizard while this window is open, so the thumbnail mirrors it.
private struct InstallerWindow<Content: View>: NSViewRepresentable {
    let guest: GuestSession
    @Binding var presented: Bool
    @ViewBuilder var content: () -> Content

    func makeNSView(context: Context) -> NSView {
        let view = NSView(frame: .zero)
        context.coordinator.sync(presented, host: view)
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        context.coordinator.sync(presented, host: nsView)
    }

    static func dismantleNSView(_ nsView: NSView, coordinator: Coordinator) {
        coordinator.close()
    }

    func makeCoordinator() -> Coordinator { Coordinator(guest: guest, content: content) }

    final class Coordinator {
        let guest: GuestSession
        let content: () -> Content
        var panel: NSPanel?

        init(guest: GuestSession, content: @escaping () -> Content) {
            self.guest = guest
            self.content = content
        }

        func sync(_ presented: Bool, host: NSView) {
            if presented {
                open(near: host.window)
            } else {
                close()
            }
        }

        func open(near parent: NSWindow?) {
            if let panel {
                panel.orderFront(nil)
                return
            }
            let host = NSHostingView(rootView: content().padding(16))
            host.frame = NSRect(x: 0, y: 0, width: 832, height: 532)
            let panel = NSPanel(
                contentRect: host.frame,
                styleMask: [.titled, .closable, .miniaturizable],
                backing: .buffered,
                defer: false
            )
            panel.title = "Installer"
            panel.isReleasedWhenClosed = false
            panel.hidesOnDeactivate = false
            panel.contentView = host
            // A non-activating panel does not become key, so it cannot take the keyboard.
            // The guest view also ignores the pointer. Quick start never attaches one.
            panel.styleMask.insert(.nonactivatingPanel)
            panel.becomesKeyOnlyIfNeeded = true
            panel.level = .floating
            if let parent {
                var origin = parent.frame.origin
                origin.x += parent.frame.width + 16
                panel.setFrameOrigin(origin)
            }
            panel.makeFirstResponder(nil)
            let delegate = PanelClose(guest: guest)
            panel.delegate = delegate
            objc_setAssociatedObject(panel, &panelCloseKey, delegate, .OBJC_ASSOCIATION_RETAIN)
            panel.orderFront(nil)
            self.panel = panel
        }

        func close() {
            panel?.close()
            panel = nil
        }
    }
}

private var panelCloseKey = 0

private final class PanelClose: NSObject, NSWindowDelegate {
    let guest: GuestSession
    init(guest: GuestSession) { self.guest = guest }
    func windowWillClose(_ notification: Notification) {
        DispatchQueue.main.async { self.guest.showWindow = false }
    }
    func windowDidBecomeKey(_ notification: Notification) {
        (notification.object as? NSWindow)?.makeFirstResponder(nil)
    }
}

struct GuestView: NSViewRepresentable {
    let session: GuestSession
    var capture: Bool

    func makeNSView(context: Context) -> InstallScreen {
        session.view.capture = capture
        // The view's own size guess is a tiny blank. The slot proposes the real size.
        session.view.setContentHuggingPriority(.defaultLow, for: .horizontal)
        session.view.setContentHuggingPriority(.defaultLow, for: .vertical)
        session.view.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        session.view.setContentCompressionResistancePriority(.defaultLow, for: .vertical)
        return session.view
    }

    func updateNSView(_ nsView: InstallScreen, context: Context) {
        nsView.capture = capture
    }

    /// Take the size SwiftUI proposes. A missing width is not zero, so closing the wizard does not blank the guest.
    func sizeThatFits(_ proposal: ProposedViewSize, nsView: InstallScreen, context: Context) -> CGSize? {
        let width = proposal.width.flatMap { $0.isFinite && $0 > 1 ? $0 : nil } ?? 800
        let height = proposal.height.flatMap { $0.isFinite && $0 > 1 ? $0 : nil } ?? width * 10.0 / 16.0
        return CGSize(width: width, height: height)
    }
}

/// Second view of the same guest, used only by the installer window.
/// The thumbnail keeps the first view, so opening the window does not blank it.
/// Neither view captures. The window is a non-activating panel, so it also does not take the keyboard.
struct GuestMirror: NSViewRepresentable {
    let session: GuestSession

    func makeNSView(context: Context) -> InstallScreen {
        let screen = InstallScreen()
        screen.capture = false
        session.attach(screen)
        return screen
    }

    func updateNSView(_ nsView: InstallScreen, context: Context) {
        nsView.capture = false
        session.attach(nsView)
    }

    /// Same fallback as the thumbnail. A closed wizard proposes no size, and a nil answer blanks the guest.
    func sizeThatFits(_ proposal: ProposedViewSize, nsView: InstallScreen, context: Context) -> CGSize? {
        let width = proposal.width.flatMap { $0.isFinite && $0 > 1 ? $0 : nil } ?? 800
        let height = proposal.height.flatMap { $0.isFinite && $0 > 1 ? $0 : nil } ?? width * 10.0 / 16.0
        return CGSize(width: width, height: height)
    }
}

/// The guest view hides the host cursor as soon as the pointer is over it.
/// Preseed does not use a pointer, so those events stay with the host until Debian is running.
final class InstallScreen: VZVirtualMachineView {
    /// Closing the wizard mid-install proposes a zero size. That guess is a blank screen, so keep the last real one.
    /// The virtual machine stays running. The window is not the guest.
    private var drawn = CGSize(width: 800, height: 500)

    var capture = false {
        didSet { applyCapture() }
    }

    override var intrinsicContentSize: NSSize { drawn }

    override func setFrameSize(_ newSize: NSSize) {
        if newSize.width > 1, newSize.height > 1 {
            drawn = newSize
            super.setFrameSize(newSize)
        } else if drawn.width > 1 {
            super.setFrameSize(drawn)
        } else {
            super.setFrameSize(newSize)
        }
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

    override func keyDown(with event: NSEvent) { if capture { super.keyDown(with: event) } }
    override func keyUp(with event: NSEvent) { if capture { super.keyUp(with: event) } }
    override func flagsChanged(with event: NSEvent) { if capture { super.flagsChanged(with: event) } }

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
