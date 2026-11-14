import SwiftUI
import Virtualization

/// The screen after the download. The installer installs Debian onto `disk.img`.
/// A finished preseed powers the guest off. SSH login, not the installer screen, is the finish check.
/// This view then snapshots that disk so later machines are clones.
/// Quick start is one page. This view stays mounted so the guest keeps running, and its rows are not drawn there.
/// The top slot is the download bar, then this guest once install starts. The same switch is on both screens.
/// Reopening Quick start boots a written disk alone, then checks SSH. It does not attach the installer or a pointer.
/// A saved snapshot is the finished install. Reopening that disk does not start the installer virtual machine.
/// That page shows the download bar slot empty. Nothing is installing.
/// A partition table is not a finished login. Quick start never attaches a pointer, so neither view can capture the cursor.
struct Install: View {
    let iso: String
    let disk: String
    var fromSnapshot = false
    /// Quick start keeps this view mounted and draws the steps itself. A clone window draws them here.
    var embedded = false
    var steps: Binding<[WizardStep]>? = nil
    var missingFile: Binding<String?>? = nil
    /// Quick start's bar slot. The mounted install publishes its session so that slot does not start another guest.
    var slot: InstallSlotModel? = nil
    @StateObject private var guest: GuestSession
    @State private var folderError: String?
    @Environment(\.openWindow) private var openWindow

    init(iso: String, disk: String, fromSnapshot: Bool = false, embedded: Bool = false, steps: Binding<[WizardStep]>? = nil, missingFile: Binding<String?>? = nil, slot: InstallSlotModel? = nil) {
        self.iso = iso
        self.disk = disk
        self.fromSnapshot = fromSnapshot
        self.embedded = embedded
        self.steps = steps
        self.missingFile = missingFile
        self.slot = slot
        let loggedIn = fromSnapshot || GuestSSH.loggedIn(beside: URL(fileURLWithPath: disk))
        let written = loggedIn || InstalledDisk.looksInstalled(disk)
        // The installer disk is machines/debian. A snapshot finishes that VM only while this disk is still the installed one.
        // A new empty disk, after the old one was removed, is not that snapshot. Quick start installs again.
        // loggedIn is not a clone. Passing it as fromSnapshot skipped Create snapshot after the files were removed.
        let snapshotSaved = !fromSnapshot && SupportDir.snapshotReady && written
        _guest = StateObject(wrappedValue: GuestSession(fromSnapshot: fromSnapshot, diskWritten: written, snapshotSaved: snapshotSaved, loggedIn: loggedIn))
    }

    /// Quick start stays the download-sized wizard. A clone is the machine window.
    private var setup: Bool { !fromSnapshot }

    /// The guest replaces the download bar while this install is still running.
    /// A saved snapshot does not install again, so the bar slot stays empty.
    private var showsGuest: Bool { setup && !guest.snapshotReady }

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
        .task {
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 400_000_000)
                guest.noteMissingFiles()
            }
        }
        .onAppear {
            slot?.session = guest
            if let steps {
                guest.steps = steps
            }
            guest.missingFile = missingFile
            // A saved snapshot means the installer already shut down. Leave that VM stopped.
            // A clone, a login, or a disk the installer already partitioned boots alone.
            // Quick start still does not attach a pointer, so the installer window cannot capture the host cursor.
            if guest.snapshotReady && !fromSnapshot {
                guest.holdFinished(disk)
            } else if fromSnapshot || guest.installed {
                guest.bootDisk(disk, pointer: fromSnapshot)
            } else {
                guest.start(iso: iso, disk: disk)
            }
        }
        .onDisappear {
            if slot?.session === guest { slot?.session = nil }
            guest.stop()
        }
    }

    private var page: some View {
        VStack(alignment: .leading, spacing: 18) {
            header
            if showsGuest {
                guestTop
            } else if !setup {
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
            if let folderError {
                Text(folderError).font(.caption).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
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

    /// The guest in the slot the download page uses for the ISO bar.
    private var guestScreen: some View {
        GuestView(session: guest, capture: capturePointer)
            .background(.black, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }

    /// Same slot as the ISO progress. The guest replaces the bar while install is running.
    /// A saved snapshot does not install again, so this slot is empty. Quick start never attaches a pointer.
    private var guestTop: some View {
        guestScreen
            .frame(maxWidth: .infinity)
            .frame(height: 220)
    }

    /// Installer rows. A clone window is not this wizard.
    private var stepList: some View {
        let shown = guest.setupSteps
        return VStack(spacing: 0) {
            ForEach(shown) { step in
                setupRow(step).id(step.id + step.state + step.detail)
                if step.id != shown.last?.id { Divider().padding(.leading, 42) }
            }
        }
        .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    private var actions: some View {
        HStack {
            if setup && !embedded {
                Button("Open folder") { folderError = SupportDir.open() }.buttonStyle(.bordered)
            }
            Spacer(minLength: 12)
            if guest.stopped {
                // The installer disk is the snapshot source. Starting it again is not a machine the user runs.
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
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: setupIcon(step.state)).foregroundStyle(setupColor(step.state)).frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text(step.title).font(.body.weight(step.state == "now" ? .semibold : .regular))
                Text(step.detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
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
            .frame(maxWidth: .infinity, maxHeight: .infinity)
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
    /// A removed installer file. Quick start restarts the download step that writes it.
    var missingFile: Binding<String?>?
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
    private var savingSnapshot = false
    private let fromSnapshotBoot: Bool

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
            markSetup("installer", state: "done", detail: nil)
            markSetup("partition", state: "done", detail: "Installed")
            markSetup("ssh", state: "done", detail: "debian")
            markSetup("snapshot", state: "done", detail: SupportDir.snapshotLocation)
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
        markSetup("installer", state: "done", detail: nil)
        markSetup("partition", state: "done", detail: "Installed")
        markSetup("ssh", state: "done", detail: "debian")
        markSetup("snapshot", state: "done", detail: SupportDir.snapshotLocation)
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
            return
        }
        installing = true
        setupSteps = WizardStep.setup
        markSetup("installer", state: "now", detail: "boot.iso")
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
        loggedIn = GuestSSH.loggedIn(beside: URL(fileURLWithPath: path)) || fromSnapshotBoot
        installing = false
        // A partition table is not a finished install. SSH is. The probe only runs while this is set.
        checking = true
        diskURL = URL(fileURLWithPath: path)
        stopped = false
        error = nil
        // The snapshot is already saved. Reopening must not start the installer virtual machine.
        if !fromSnapshotBoot && SupportDir.snapshotReady {
            holdFinished(path)
            return
        }
        headline = fromSnapshotBoot ? "Starting Debian." : "Debian is already installed. Checking SSH."
        markSetup("installer", state: "done", detail: nil)
        markSetup("partition", state: "done", detail: "Installed")
        markSetup("ssh", state: "now", detail: "guest port 22")
        boot { try Guest.boot(disk: diskURL!, pointer: pointer) }
    }

    /// The installer powers off when the preseed check passes. The next start is the disk it wrote.
    /// A black installer screen is not finished. SSH after this boot is.
    func startInstalled() {
        guard let disk = diskURL else { return }
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
        // The first probe must not wait for VZVirtualMachine.state == .running.
        // start() reports success before that state changes, and a stopped guest never reaches it.
        checkSSH()
        watch = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            self?.checkSSH()
        }
    }

    private func checkSSH() {
        // A saved snapshot still needs a probe when the login file was removed.
        guard installed, checking, !probing, let disk = diskURL else { return }
        if snapshotReady && GuestSSH.loggedIn(beside: disk) { return }
        // A stopped machine cannot answer. Wait, but do not require .running: that state lags start().
        if stopped && machine == nil { return }
        probing = true
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let ok = GuestSSH.probe(beside: disk) { self?.leaving == true }
            DispatchQueue.main.async {
                guard let self else { return }
                self.probing = false
                if ok { self.loginSucceeded() }
            }
        }
    }

    private func loginSucceeded() {
        guard installed, checking else { return }
        checking = false
        loggedIn = true
        showWindow = false
        markSetup("ssh", state: "done", detail: "debian")
        watch?.invalidate()
        // The proof file was removed, but the snapshot is still there. Restore the proof and leave the VM stopped.
        if snapshotReady {
            headline = "Debian is installed and shut down. A snapshot is saved for new machines."
            stopped = true
            machine?.stop { _ in }
            return
        }
        headline = "SSH login succeeded. Saving a snapshot."
        markSetup("snapshot", state: "now", detail: SupportDir.snapshotLocation)
        saveSnapshot()
    }

    /// A step's file disappeared. Run that step again, including while the installer is still going.
    func noteMissingFiles() {
        guard !leaving, let disk = diskURL else { return }
        if !FileManager.default.fileExists(atPath: disk.path) {
            if fromSnapshotBoot && error == nil {
                error = "The guest disk was removed."
                headline = "The virtual machine did not start."
                stopped = true
                machine?.stop { _ in }
            } else if !fromSnapshotBoot, missingFile?.wrappedValue == nil, setupSteps.first(where: { $0.id == "installer" })?.state == "done" {
                missingFile?.wrappedValue = "disk"
            }
            return
        }
        if fromSnapshotBoot { return }
        let installer = disk.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("installer", isDirectory: true)
        if !FileManager.default.fileExists(atPath: installer.appendingPathComponent("boot.iso").path) {
            if missingFile?.wrappedValue == nil { missingFile?.wrappedValue = "patch" }
            return
        }
        if !FileManager.default.fileExists(atPath: installer.appendingPathComponent("preseed.img").path) {
            if missingFile?.wrappedValue == nil { missingFile?.wrappedValue = "preseed" }
            return
        }
        let proof = GuestSSH.loggedIn(beside: disk)
        let snap = SupportDir.snapshotReady
        let snapshotDone = setupSteps.first { $0.id == "snapshot" }?.state == "done"
        // The row can still say done after the files are gone. Copy the installed disk again.
        if !savingSnapshot && (snapshotReady || snapshotDone) && !snap {
            snapshotReady = false
            saved = false
            showWindow = false
            if proof {
                markSetup("snapshot", state: "now", detail: SupportDir.snapshotLocation)
                headline = "SSH login succeeded. Saving a snapshot."
                saveSnapshot()
            } else {
                markSetup("snapshot", state: "wait", detail: SupportDir.snapshotLocation)
                headline = "The snapshot was removed."
            }
        }
        let sshDone = setupSteps.first { $0.id == "ssh" }?.state == "done"
        if sshDone && !proof && !checking && !installing {
            loggedIn = false
            installed = true
            checking = true
            stopped = false
            error = nil
            markSetup("ssh", state: "now", detail: "guest port 22")
            headline = "Debian is already installed. Checking SSH."
            if machine == nil {
                boot { try Guest.boot(disk: disk, pointer: false) }
            } else {
                watchSSH()
            }
        }
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
            markSetup("installer", state: "done", detail: nil)
            markSetup("partition", state: "done", detail: "Installed")
            markSetup("ssh", state: "now", detail: "guest port 22")
            headline = "Installer stopped. Starting Debian to check SSH."
            startInstalled()
            return
        }
        if checking, installed, let disk = diskURL, GuestSSH.loggedIn(beside: disk) {
            checking = false
            loggedIn = true
            showWindow = false
            markSetup("ssh", state: "done", detail: "debian")
            if snapshotReady {
                headline = "Debian is installed and shut down. A snapshot is saved for new machines."
                return
            }
            headline = "SSH login succeeded. Saving a snapshot."
            markSetup("snapshot", state: "now", detail: SupportDir.snapshotLocation)
            saveSnapshot()
            return
        }
        headline = checking
            ? "Debian stopped. Waiting to check SSH."
            : "Debian stopped. The guest disk is the installed system."
    }

    func saveAgain() {
        guard !savingSnapshot, let disk = diskURL, GuestSSH.loggedIn(beside: disk) else {
            if !savingSnapshot { error = "SSH has not logged in." }
            return
        }
        saved = false
        error = nil
        headline = "Saving a snapshot."
        markSetup("snapshot", state: "now", detail: SupportDir.snapshotLocation)
        saveSnapshot()
    }

    /// Copy machines/debian after the guest has stopped. A removed snapshot is copied again.
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
                    self.markSetup("snapshot", state: "done", detail: SupportDir.snapshotLocation)
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
                            // start() can report success before state becomes .running. Probe from here.
                            self?.watchSSH()
                        } else {
                            self?.markSetup("installer", state: "done", detail: nil)
                            self?.markSetup("partition", state: "now", detail: "Installing")
                            self?.headline = "The installer starts on its own and installs Debian."
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
        if let detail, !detail.isEmpty {
            // The short list shows the file name. A full path belongs in the log.
            if id == "snapshot", detail.hasPrefix("/") || detail == SupportDir.snapshotLocation {
                setupSteps[index].path = detail
                setupSteps[index].detail = "snapshots/debian"
            } else {
                setupSteps[index].detail = detail
            }
        }
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
                // Keep a path the log already stored. The installer row only knows the short label.
                let path = row.path ?? list[at].path
                list[at] = row
                list[at].path = path
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
        return session.view
    }

    func updateNSView(_ nsView: InstallScreen, context: Context) {
        nsView.capture = capture
    }
}

/// Second view of the same guest, used only by the installer window.
/// The thumbnail keeps the first view, so opening the window does not blank it.
/// Second view of the same guest. The thumbnail keeps the first view, so opening the window does not blank it.
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
