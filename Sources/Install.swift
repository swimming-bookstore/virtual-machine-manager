import SwiftUI
import AppKit
import CoreGraphics
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
    /// The library window fills with the guest. A clone window keeps the fixed page.
    var fill = false
    /// This copy is the dedicated window. The library copy must not embed the same guest view.
    var windowed = false
    /// Library delete. Nil on the installer, so Quick start has no Delete button.
    var onDelete: (() -> Void)? = nil
    var deleteDisabled = false
    /// Opens the dedicated window. Nil on the installer and on that window.
    var onOpen: (() -> Void)? = nil
    /// CPU or memory changed. A pause save from the old counts cannot be restored.
    var onHardware: (() -> Void)? = nil
    @StateObject private var guest: GuestSession
    @State private var pointerInGuest = false
    @Environment(\.openWindow) private var openWindow

    init(iso: String, disk: String, fromSnapshot: Bool = false, embedded: Bool = false, freshInstall: Bool = false, fill: Bool = false, windowed: Bool = false, session: GuestSession? = nil, onDelete: (() -> Void)? = nil, deleteDisabled: Bool = false, onOpen: (() -> Void)? = nil, onHardware: (() -> Void)? = nil, steps: Binding<[WizardStep]>? = nil, slot: InstallSlotModel? = nil) {
        self.iso = iso
        self.disk = disk
        self.fromSnapshot = fromSnapshot
        self.embedded = embedded
        self.freshInstall = freshInstall
        self.fill = fill
        self.windowed = windowed
        self.onDelete = onDelete
        self.deleteDisabled = deleteDisabled
        self.onOpen = onOpen
        self.onHardware = onHardware
        self.steps = steps
        self.slot = slot
        let loggedIn = fromSnapshot || GuestSSH.loggedIn(beside: URL(fileURLWithPath: disk))
        // A partition table is not a finished guest. Neither is a hash of disk.img, or a leftover login file.
        // Running the wizard again starts the installer. Only a snapshot saved after SSH skips Install guest.
        // Reset does not skip it. That run is a new image, the way Packer rebuilds.
        // loggedIn is not a clone. Passing it as fromSnapshot skipped Create snapshot after the files were removed.
        let snapshotSaved = !fromSnapshot && !freshInstall && SupportDir.snapshotReady
        let paused = fromSnapshot && GuestPause.isPaused(beside: URL(fileURLWithPath: disk))
        _guest = StateObject(wrappedValue: session ?? GuestSession(fromSnapshot: fromSnapshot, diskWritten: false, snapshotSaved: snapshotSaved, loggedIn: loggedIn, paused: paused))
    }

    /// Quick start stays the download-sized wizard. A clone is the machine window.
    private var setup: Bool { !fromSnapshot }

    /// A clone captures the pointer while it is running. Quick start does not, including the installer window.
    private var capturePointer: Bool { fromSnapshot && !guest.stopped }

    /// The library window is a normal key window, and the running clone has a USB keyboard.
    /// Keys have to reach that keyboard. Pointer capture is separate, so typing does not grab the cursor.
    private var captureKeyboard: Bool { fromSnapshot && !guest.stopped }

    var body: some View {
        Group {
            if embedded {
                // Quick start draws the screen and the steps. This copy only keeps the guest running.
                Color.clear
            } else if fill {
                filled
            } else {
                page
            }
        }
        .background {
            // The window stays with the mounted guest. Quick start draws the thumbnail, not this page.
            // Quick start opens the panel itself. This page only opens it when the install is visible.
            if setup && !embedded {
                InstallerWindow(guest: guest, presented: $guest.showWindow) {
                    GuestMirror(session: guest)
                        .installerFloor()
                        .background(.black)
                }
            }
        }
        .onDisappear {
            // Closing the wizard mid-install must not stop the guest. The scene still owns this session.
            // A library clone is owned by LibraryModel, so adding a machine does not stop this one.
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
            if fromSnapshot { return }
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
                guest.holdClone(disk)
            } else {
                guest.start(iso: iso, disk: disk)
            }
        }
        .onChange(of: guest.snapshotReady) { _, ready in
            // The library is the machine window. Do not open a second guest page.
            if ready, !fromSnapshot { showLibrary(select: "name") }
        }
    }

    private var filled: some View {
        Group {
            if windowed {
                guestStage
                    .toolbar { machineToolbar(nil) }
            } else {
                // The guest fills the window. Settings sit below it, so the gear jumps down.
                // Scrolling stays on. The guest view only takes the pointer while the cursor is over it.
                ScrollViewReader { proxy in
                    ScrollView {
                        VStack(spacing: 0) {
                            guestStage
                                .id("vm")
                                .frame(minHeight: max(WindowMetrics.libraryMin.height - 52, Guest.cloneGuest.height))
                            if fromSnapshot {
                                MachineSettings(disk: disk, running: !guest.stopped, onChange: onHardware)
                                    .id("settings")
                            }
                        }
                    }
                    .scrollDisabled(capturePointer && pointerInGuest)
                    .toolbar { machineToolbar(proxy) }
                }
            }
        }
    }

    private var guestStage: some View {
        ZStack {
            screenBody
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(.black)
            if let error = guest.error {
                Text(error).font(.callout).foregroundStyle(.red).padding()
            }
        }
    }

    /// The guest view can live in one window. While the dedicated window has it, the library shows a way back.
    private var showsGuest: Bool { windowed || !guest.inDedicatedWindow }

    @ViewBuilder private var screenBody: some View {
        if showsGuest {
            guestScreen.allowsHitTesting(!guest.stopped)
        } else {
            dedicatedNote
        }
    }

    private var dedicatedNote: some View {
        ZStack {
            Color.black
            if let preview = guest.pausePreview {
                Image(nsImage: preview)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
            }
            if let onOpen {
                Button("Show window", action: onOpen)
                    .buttonStyle(.borderedProminent)
            }
        }
    }

    private var page: some View {
        VStack(alignment: .leading, spacing: 18) {
            header
            if !setup {
                guestScreen
                    .frame(width: Guest.cloneGuest.width, height: Guest.cloneGuest.height)
            }
            if setup { stepList }
            WrappingPath(text: disk)
            if let error = guest.error {
                Text(error).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            actions
        }
        .padding(WindowMetrics.pagePad)
        .frame(width: setup ? WindowMetrics.quickStartWidth : Guest.cloneGuest.width + WindowMetrics.pagePad * 2, alignment: .topLeading)
        .toolbar {
            if fromSnapshot { machineToolbar(nil) }
        }
    }

    /// Top and Settings sit left of Open. Then Start, Pause, and Stop, then Delete at the far right. Icons only; the help text names the action.
    /// Open uses rectangle.on.rectangle, the same glyph UTM uses for Windows.
    /// Top scrolls back to the guest; Settings is the gear.
    /// Delete stays red because it removes the machine.
    /// primaryAction stops before the centered title. confirmationAction is the trailing edge.
    @ToolbarContentBuilder
    private func machineToolbar(_ scroll: ScrollViewProxy?) -> some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            if let scroll {
                Button {
                    withAnimation { scroll.scrollTo("vm", anchor: .top) }
                } label: {
                    Image(systemName: "arrow.up.to.line")
                }
                .help("Scroll to top")
                Button {
                    withAnimation { scroll.scrollTo("settings", anchor: .top) }
                } label: {
                    Image(systemName: "gearshape")
                }
                .help("Settings")
            }
            if let onOpen {
                Button(action: onOpen) {
                    Image(systemName: "rectangle.on.rectangle")
                }
                .help("Open in a window")
            }
            Button { guest.bootDisk(disk, pointer: true) } label: {
                Image(systemName: "play.fill")
            }
            .disabled(!guest.stopped)
            .help("Start")
            Button { guest.pauseGuest() } label: {
                Image(systemName: "pause.fill")
            }
            .disabled(guest.stopped)
            .help("Pause")
            Button { guest.halt() } label: {
                Image(systemName: "stop.fill")
            }
            .disabled(guest.stopped && !guest.paused)
            .help("Stop")
        }
        if let onDelete {
            ToolbarItem(placement: .confirmationAction) {
                Button(action: onDelete) {
                    Image(systemName: "trash")
                        .foregroundStyle(.red)
                }
                .tint(.red)
                .disabled(deleteDisabled)
                .help("Delete this virtual machine")
            }
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

    /// The guest screen. A clone window shows Guest.cloneGuest. Quick start uses a thumbnail on the step instead.
    /// A pause keeps the last frame, the way UTM caches the view before the machine is stopped.
    /// GuestView stays mounted under that frame. Replacing it unhooks the virtual machine view, and the next pause has nothing to snapshot.
    /// The still is an AppKit sibling on top of the Metal guest. A SwiftUI Image in the ZStack does not cover it.
    private var guestScreen: some View {
        GuestView(
            session: guest,
            capture: capturePointer,
            keyboard: captureKeyboard,
            preview: guest.pausePreview,
            onPointerInGuest: { inside in
                DispatchQueue.main.async {
                    if pointerInGuest != inside {
                        pointerInGuest = inside
                    }
                }
            }
        )
        .id(guest.screenEpoch)
        .background(.black)
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
                if !fromSnapshot && !guest.snapshotReady {
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

    /// Copy the snapshot into the library. That window is the machine; a second guest window is not.
    private func newMachine() {
        guest.cloning = true
        let dir = SupportDir.url
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Worker.cloneMachine(dir: dir)
            DispatchQueue.main.async {
                guest.cloning = false
                switch result {
                case .success(let path):
                    showLibrary(select: path)
                case .failure(let message):
                    guest.error = message
                }
            }
        }
    }

    private func showLibrary(select disk: String? = nil) {
        // "name" is the wizard finishing. The main window may not exist yet, so remember the ask.
        if disk == "name" {
            LibraryModel.noteWizardAsk()
        }
        NotificationCenter.default.post(name: .libraryDidChange, object: disk)
        if let window = NSApp.windows.first(where: { AppWindow.isMain($0) }) {
            window.makeKeyAndOrderFront(nil)
        } else {
            openWindow(id: "main")
        }
        AppWindow.closeQuickStart()
    }
}

/// The main window scrolls down to CPU and memory. The gear jumps here.
/// A running guest keeps the values it booted with. The next Start reads the file beside the disk.
private struct MachineSettings: View {
    let disk: String
    var running: Bool
    var onChange: (() -> Void)? = nil
    @State private var cpu = GuestHardware.defaultCPU
    @State private var memory = GuestHardware.defaultMemoryGB
    @State private var error: String?
    @State private var savedCPU = GuestHardware.defaultCPU
    @State private var savedMemory = GuestHardware.defaultMemoryGB

    private var maxCPU: Int { VZVirtualMachineConfiguration.maximumAllowedCPUCount }
    private var maxMemory: Int { Int(VZVirtualMachineConfiguration.maximumAllowedMemorySize / (1024 * 1024 * 1024)) }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Settings").font(.title3.weight(.semibold))
            Text(running ? "CPU and memory apply the next time this virtual machine starts." : "CPU and memory for this virtual machine.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            row("CPU", cpu, 1...max(1, maxCPU), cpu == 1 ? "core" : "cores") { cpu = $0; store() }
            row("Memory", memory, 1...max(1, maxMemory), "GB") { memory = $0; store() }
            if let error { Text(error).font(.callout).foregroundStyle(.red) }
        }
        .padding(WindowMetrics.pagePad)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.background)
        .onAppear(perform: load)
        .onChange(of: disk) { _, _ in load() }
    }

    private func row(_ title: String, _ value: Int, _ range: ClosedRange<Int>, _ unit: String, set: @escaping (Int) -> Void) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(title)
                Spacer()
                Text("\(value) \(unit)").foregroundStyle(.secondary).monospacedDigit()
            }
            Slider(value: Binding(get: { Double(value) }, set: { set(Int($0.rounded())) }), in: Double(range.lowerBound)...Double(range.upperBound), step: 1)
                .disabled(running)
        }
    }

    private func load() {
        let hardware = GuestHardware.load(beside: URL(fileURLWithPath: disk))
        cpu = hardware.cpu
        memory = hardware.memoryGB
        savedCPU = cpu
        savedMemory = memory
        error = nil
    }

    private func store() {
        let hardware = GuestHardware.clamp(cpuCount: cpu, memoryGB: memory)
        cpu = hardware.cpu
        memory = hardware.memoryGB
        guard cpu != savedCPU || memory != savedMemory else { return }
        error = GuestHardware.save(beside: URL(fileURLWithPath: disk), cpu: cpu, memoryGB: memory)
        if error == nil {
            savedCPU = cpu
            savedMemory = memory
            onChange?()
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
    }
}

private func failureText(_ error: Error?) -> String? {
    error?.localizedDescription
}

private func failureText(_ result: Result<Void, Error>) -> String? {
    if case .failure(let error) = result { return error.localizedDescription }
    return nil
}

private func pngData(_ image: NSImage) -> Data? {
    var rect = NSRect(origin: .zero, size: image.size)
    if let cg = image.cgImage(forProposedRect: &rect, context: nil, hints: nil) {
        if let data = NSBitmapImageRep(cgImage: cg).representation(using: .png, properties: [:]) {
            return data
        }
    }
    guard let tiff = image.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff) else { return nil }
    return rep.representation(using: .png, properties: [:])
}

/// Copy cacheDisplay into our own bitmap so the pause still is not tied to the view.
private func ownedImage(_ image: CGImage, size: CGSize) -> NSImage {
    let width = image.width
    let height = image.height
    guard width > 0, height > 0, let ctx = CGContext(
        data: nil,
        width: width,
        height: height,
        bitsPerComponent: 8,
        bytesPerRow: 0,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else {
        return NSImage(cgImage: image, size: size)
    }
    ctx.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
    guard let copy = ctx.makeImage() else {
        return NSImage(cgImage: image, size: size)
    }
    return NSImage(cgImage: copy, size: size)
}

/// cacheDisplay of a Metal guest is often fully transparent. That is not a frame.
private func visiblePixels(_ image: NSImage) -> Bool {
    var rect = NSRect(origin: .zero, size: image.size)
    guard let cg = image.cgImage(forProposedRect: &rect, context: nil, hints: nil), cg.width > 1, cg.height > 1 else {
        return false
    }
    switch cg.alphaInfo {
    case .none, .noneSkipLast, .noneSkipFirst:
        return true
    default:
        break
    }
    let pixel = UnsafeMutablePointer<UInt8>.allocate(capacity: 4)
    defer { pixel.deallocate() }
    guard let ctx = CGContext(
        data: pixel,
        width: 1,
        height: 1,
        bitsPerComponent: 8,
        bytesPerRow: 4,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else {
        return true
    }
    let stepX = max(cg.width / 8, 1)
    let stepY = max(cg.height / 8, 1)
    var y = 0
    while y < cg.height {
        var x = 0
        while x < cg.width {
            pixel.update(repeating: 0, count: 4)
            ctx.clear(CGRect(x: 0, y: 0, width: 1, height: 1))
            ctx.draw(cg, in: CGRect(x: -x, y: -y, width: cg.width, height: cg.height))
            if pixel[3] > 8 { return true }
            x += stepX
        }
        y += stepY
    }
    return false
}

/// Forwards Virtualization callbacks. GuestSession is not an NSObject, so a completion cannot erase it to NSObject.
final class MachineDelegate: NSObject, VZVirtualMachineDelegate {
    weak var session: GuestSession?
    var token = 0

    func guestDidStop(_: VZVirtualMachine) {
        deliver(token, nil)
    }

    func virtualMachine(_: VZVirtualMachine, didStopWithError error: Error) {
        deliver(token, error.localizedDescription)
    }

    private func deliver(_ token: Int, _ message: String?) {
        let session = session
        DispatchQueue.main.async {
            session?.stoppedFromDelegate(token, message: message)
        }
    }
}

final class GuestSession: ObservableObject {
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
    /// The guest view is mounted in the dedicated window. The library must not embed it too.
    @Published var inDedicatedWindow = false
    /// Bumped so a closed window's guest view is created again. The old one stays blank.
    @Published private(set) var screenEpoch = 0
    /// A pause save beside this disk. Start restores it instead of a cold boot.
    @Published var paused = false
    /// Last guest frame at pause. Shown while the machine is stopped.
    @Published var pausePreview: NSImage?
    /// Quick start passes its own list. A clone keeps the rows on this session.
    var steps: Binding<[WizardStep]>?
    /// Replaced when a window opens again. The view that closed stays blank if it is shown a second time.
    var view = InstallScreen()
    private let callbacks = MachineDelegate()
    /// Bumped when the running machine changes, so a late callback cannot act on the next boot.
    private var generation = 0
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

    init(fromSnapshot: Bool = false, diskWritten: Bool = false, snapshotSaved: Bool = false, loggedIn: Bool = false, paused: Bool = false) {
        installed = fromSnapshot || diskWritten || snapshotSaved
        self.loggedIn = fromSnapshot || snapshotSaved || loggedIn
        fromSnapshotBoot = fromSnapshot
        snapshotReady = snapshotSaved
        saved = snapshotSaved
        self.paused = paused
        headline = if snapshotSaved && !fromSnapshot {
            "Debian is installed and shut down. A snapshot is saved for new machines."
        } else if fromSnapshot {
            paused ? "Debian is paused." : "Debian is stopped. The guest disk is the installed system."
        } else if diskWritten {
            "Debian is already installed. Checking SSH."
        } else {
            "Starting the installer."
        }
        callbacks.session = self
        if snapshotSaved && !fromSnapshot {
            stopped = true
            markSetup("partition", state: "done")
            markSetup("ssh", state: "done")
            markSetup("snapshot", state: "done")
        } else if fromSnapshot {
            stopped = true
        }
    }

    /// Rename moved the folder. The running guest keeps this session and reads the new disk path.
    func retarget(_ path: String) {
        diskURL = URL(fileURLWithPath: path)
    }

    /// The dedicated window is closing. Leave the guest view out of that window so the next open can embed it.
    func releaseScreen() {
        view.removeFromSuperview()
    }

    /// The dedicated window is opening. The view left behind by the closed window is blank, so mount a new one.
    func claimScreen() {
        view.virtualMachine = nil
        view.removeFromSuperview()
        view = InstallScreen()
        screenEpoch += 1
    }

    /// The new guest view is in the window. Assigning the machine before that leaves the window black.
    func showScreen() {
        guard view.window != nil, let machine else { return }
        if view.virtualMachine !== machine {
            view.virtualMachine = machine
        }
    }

    /// A clone stays off until Start. A pause save is restored then, not on open.
    func holdClone(_ path: String) {
        if started && !stopped { return }
        diskURL = URL(fileURLWithPath: path)
        installing = false
        checking = false
        stopped = true
        installed = true
        started = true
        error = nil
        paused = GuestPause.isPaused(beside: diskURL!)
        pausePreview = paused ? NSImage(contentsOf: GuestPause.preview(beside: diskURL!)) : nil
        headline = paused ? "Debian is paused." : "Debian is stopped. The guest disk is the installed system."
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
        guard !started || stopped else { return }
        started = true
        leaving = false
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
        if fromSnapshotBoot, paused, let vm = machine {
            resumePaused(vm, disk: diskURL!)
            return
        }
        if fromSnapshotBoot && (paused || GuestPause.isPaused(beside: diskURL!)) {
            restorePaused(pointer: pointer)
            return
        }
        // A clone is already installed. Do not probe SSH or save a snapshot; that path powers the guest off.
        checking = !fromSnapshotBoot
        paused = false
        pausePreview = nil
        headline = fromSnapshotBoot ? "Starting Debian." : "Debian is already installed. Checking SSH."
        markSetup("partition", state: "done")
        markSetup("ssh", state: checking ? "now" : "done")
        boot { try Guest.boot(disk: diskURL!, pointer: pointer) }
    }

    /// Resume a clone that is still paused in memory. Restoring the save would start a second machine on the same disk.
    private func resumePaused(_ vm: VZVirtualMachine, disk: URL) {
        paused = true
        headline = "Resuming Debian."
        let token = generation
        let resumed: (String?) -> Void = { [weak self] message in
            DispatchQueue.main.async { self?.resumeFinished(token, disk: disk, message: message) }
        }
        vm.resume { result in
            resumed(failureText(result))
        }
    }

    /// Restore a pause save, then resume. A save with no machine identifier cannot match and is dropped.
    private func restorePaused(pointer: Bool) {
        guard let disk = diskURL else { return }
        if !GuestPause.hasMachineID(beside: disk) {
            dropUnrestorablePause(disk, pointer: pointer)
            return
        }
        paused = true
        headline = "Resuming Debian."
        let vm: VZVirtualMachine
        do {
            vm = try Guest.boot(disk: disk, pointer: pointer)
        } catch {
            paused = true
            stopped = true
            fail(error.localizedDescription)
            headline = "Debian is paused."
            return
        }
        bind(vm)
        let token = generation
        let failed: (String) -> Void = { [weak self] message in
            DispatchQueue.main.async { self?.restoreFailed(token, message) }
        }
        let resumed: (String?) -> Void = { [weak self] message in
            DispatchQueue.main.async { self?.resumeFinished(token, disk: disk, message: message) }
        }
        vm.restoreMachineStateFrom(url: GuestPause.state(beside: disk)) { (error: Error?) in
            if let message = failureText(error) {
                failed(message)
                return
            }
            vm.resume { result in
                resumed(failureText(result))
            }
        }
    }

    /// A pause from before the identifier was stored cannot be restored. Start the disk instead.
    private func dropUnrestorablePause(_ disk: URL, pointer: Bool) {
        paused = false
        pausePreview = nil
        GuestPause.forget(beside: disk)
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            _ = Worker.clearPaused(disk: disk)
            DispatchQueue.main.async { self?.publishLibrary() }
        }
        checking = false
        headline = "Starting Debian."
        boot { try Guest.boot(disk: disk, pointer: pointer) }
    }

    private func restoreFailed(_ token: Int, _ message: String) {
        guard token == generation else { return }
        paused = true
        stopped = true
        dropMachine()
        error = message
        headline = "Debian is paused."
    }

    private func resumeFinished(_ token: Int, disk: URL, message: String?) {
        guard token == generation, machine != nil else { return }
        if let message {
            paused = true
            stopped = true
            error = message
            headline = "Debian is paused."
            return
        }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let err = Worker.clearPaused(disk: disk)
            DispatchQueue.main.async {
                guard let self, token == self.generation else { return }
                if let err { self.error = err }
                self.paused = false
                self.pausePreview = nil
                self.stopped = false
                self.headline = "Debian is running from the guest disk."
                self.publishLibrary()
            }
        }
    }

    private func dropMachine() {
        generation += 1
        callbacks.token = generation
        let running = machine
        machine = nil
        running?.delegate = nil
        // Leaving the Metal view attached draws black over the pause still after stop.
        if view.virtualMachine === running {
            view.virtualMachine = nil
        }
    }

    private func bind(_ vm: VZVirtualMachine) {
        generation += 1
        callbacks.token = generation
        vm.delegate = callbacks
        view.virtualMachine = vm
        view.capture = false
        view.keyboard = false
        machine = vm
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
        dropMachine()
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
        dropMachine()
        stopped = true
        running?.stop { _ in }
    }

    func stop() {
        leaving = true
        probing = false
        watch?.invalidate()
        watch = nil
        // The host-key login is still running. Kill it, or the next open stays on Verify with host key.
        GuestSSH.stop()
        stopped = true
        let running = machine
        dropMachine()
        running?.stop { _ in }
    }

    /// Stop a running clone. Start boots the same disk again. A pause save is dropped.
    func halt() {
        guard fromSnapshotBoot, (!stopped || paused) else { return }
        probing = false
        watch?.invalidate()
        watch = nil
        stopped = true
        headline = "Debian stopped. The guest disk is the installed system."
        let running = machine
        dropMachine()
        running?.stop { _ in }
        guard let disk = diskURL else {
            paused = false
            pausePreview = nil
            return
        }
        let drop = paused || GuestPause.isPaused(beside: disk)
        paused = false
        pausePreview = nil
        if drop {
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                _ = Worker.clearPaused(disk: disk)
                DispatchQueue.main.async { self?.publishLibrary() }
            }
        }
    }

    /// Settings changed the CPU or memory. The pause save was made with the old counts, so Start boots fresh.
    func forgetPause() {
        guard fromSnapshotBoot, let disk = diskURL else { return }
        let drop = paused || GuestPause.isPaused(beside: disk)
        paused = false
        pausePreview = nil
        headline = stopped ? "Debian is stopped. The guest disk is the installed system." : headline
        guard drop else { return }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            _ = Worker.clearPaused(disk: disk)
            DispatchQueue.main.async { self?.publishLibrary() }
        }
    }

    /// Pause a running clone and keep the save beside the disk. Quit can resume it.
    func pauseGuest() {
        guard fromSnapshotBoot, !stopped, !paused, machine != nil, let disk = diskURL else { return }
        headline = "Pausing Debian."
        let token = generation
        let failed: (String) -> Void = { [weak self] message in
            DispatchQueue.main.async { self?.pauseFailed(token, message) }
        }
        // Last guest pixels before pause. cacheDisplay is often empty for Metal; then the paused machine keeps the frame.
        let earlier = view.snapshot()
        // Capture self here. A nested [weak self] in the pause block disagrees with that block's capture.
        let pausedVM: () -> Void = { [weak self] in
            DispatchQueue.main.async {
                guard let self, token == self.generation else { return }
                let shot = [earlier, self.view.cachedPixels()].compactMap { $0 }.first(where: visiblePixels)
                // Pin the frame now. Saving state takes a moment, and the Metal guest goes black after that.
                if let shot {
                    self.pausePreview = shot
                    if let pane = self.view.superview as? GuestPane {
                        pane.show(shot)
                    }
                    self.view.virtualMachine = nil
                }
                self.writePause(token, disk: disk, frame: shot)
            }
        }
        guard let vm = machine else { return }
        vm.pause { result in
            if let message = failureText(result) {
                failed(message)
                return
            }
            pausedVM()
        }
    }

    private func pauseFailed(_ token: Int, _ message: String) {
        guard token == generation, machine != nil else { return }
        error = message
        headline = "Debian is running from the guest disk."
    }

    private func writePause(_ token: Int, disk: URL, frame: NSImage?) {
        guard token == generation, let vm = machine else { return }
        let url = GuestPause.state(beside: disk)
        _ = try? FileManager.default.removeItem(at: url)
        let saved: (String?) -> Void = { [weak self] message in
            DispatchQueue.main.async { self?.pauseSaved(token, disk: disk, url: url, frame: frame, message: message) }
        }
        vm.saveMachineStateTo(url: url) { (error: Error?) in
            saved(failureText(error))
        }
    }

    private func pauseSaved(_ token: Int, disk: URL, url: URL, frame: NSImage?, message: String?) {
        guard token == generation, let vm = machine else { return }
        if let message {
            error = message
            headline = "Debian is running from the guest disk."
            pausePreview = nil
            view.virtualMachine = vm
            (view.superview as? GuestPane)?.show(nil)
            vm.resume { _ in }
            return
        }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let err = Worker.markPaused(disk: disk)
            DispatchQueue.main.async {
                guard let self, token == self.generation, self.machine != nil else { return }
                if let err {
                    self.error = err
                    self.headline = "Debian is running from the guest disk."
                    self.pausePreview = nil
                    self.view.virtualMachine = self.machine
                    (self.view.superview as? GuestPane)?.show(nil)
                    self.machine?.resume { _ in }
                    _ = try? FileManager.default.removeItem(at: url)
                    return
                }
                self.paused = true
                self.stopped = true
                let shot = [frame, self.pausePreview, self.view.cachedPixels()].compactMap { $0 }.first(where: visiblePixels)
                self.pausePreview = shot
                // pausePreview = frame is the still GuestPane shows over the Metal guest.
                if let shot, let png = pngData(shot) {
                    let preview = GuestPause.preview(beside: disk)
                    try? png.write(to: preview, options: .atomic)
                    if let diskImage = NSImage(contentsOf: preview) {
                        self.pausePreview = diskImage
                    }
                }
                self.view.virtualMachine = nil
                if let pane = self.view.superview as? GuestPane {
                    pane.show(self.pausePreview)
                }
                self.headline = "Debian is paused."
                self.publishLibrary()
                // A visible still can cover the guest. Without one, keep the paused machine so Metal still shows the last frame.
                if self.pausePreview != nil {
                    let running = self.machine
                    self.dropMachine()
                    running?.stop { _ in }
                }
            }
        }
    }

    /// The guest pane reads pause files. The sidebar does not, so a pause must not rebuild that list.
    fileprivate func publishLibrary() {
        NotificationCenter.default.post(name: .libraryDidChange, object: nil)
    }

    deinit {
        leaving = true
        watch?.invalidate()
        GuestSSH.stop()
        let running = machine
        dropMachine()
        running?.stop { _ in }
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
        dropMachine()
        started = false
        let installer = disk.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("installer/boot.iso")
        // start() finds installer/ from the ISO's parent. The file name is not used.
        start(iso: installer.path, disk: disk.path)
    }

    fileprivate func stoppedFromDelegate(_ token: Int, message: String?) {
        guard token == generation else { return }
        noteStopped()
        if let message, !snapshotReady, error == nil, !checking {
            fail(message)
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
            dropMachine()
            return
        }
        guard !stopped else { return }
        stopped = true
        watch?.invalidate()
        dropMachine()
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
            let stoppedCopy: () -> Void = { [weak self] in
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.stopped = true
                    self.watch?.invalidate()
                    self.dropMachine()
                    self.copySnapshot(disk)
                }
            }
            machine.stop { _ in stoppedCopy() }
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
                    self.publishLibrary()
                }
            }
        }
    }

    func attach(_ screen: InstallScreen) {
        screen.capture = false
        screen.keyboard = false
        if screen.virtualMachine !== machine {
            screen.virtualMachine = machine
        }
    }

    private func boot(_ make: () throws -> VZVirtualMachine) {
        do {
            let vm = try make()
            bind(vm)
            let token = generation
            let installed = installed
            let started: (String?) -> Void = { [weak self] message in
                DispatchQueue.main.async {
                    self?.bootFinished(token, installed: installed, message: message)
                }
            }
            vm.start { result in
                started(failureText(result))
            }
        } catch {
            stopped = true
            fail(error.localizedDescription)
        }
    }

    private func bootFinished(_ token: Int, installed: Bool, message: String?) {
        guard token == generation, machine != nil else { return }
        if let message {
            stopped = true
            if checking, !leaving, !snapshotReady {
                retrySSH()
            } else {
                if !installed { failSetup() }
                fail(message)
            }
            return
        }
        stopped = false
        if installed {
            // The already-installed boot is waiting on SSH. Do not replace that with a boot headline.
            // A clone is a machine the user is running, so it keeps the running headline.
            if checking && !fromSnapshotBoot {
                headline = "Debian is already installed. Checking SSH."
                markSetup("ssh", state: "now")
            } else {
                headline = "Debian is running from the guest disk."
            }
            // start() can report success before state becomes .running. Probe from here.
            // A saved snapshot is the artifact. Do not probe it again.
            if checking && !snapshotReady {
                watchSSH()
            }
        } else {
            markSetup("partition", state: "now")
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

/// Opens a preview of the guest at its original scanout. The wizard stays the download size.
/// The panel has a normal title bar. The guest fills the body, with no padding.
/// Quick start mounts this. The embedded install is invisible, so it cannot be the only owner.
/// The thumbnail keeps its own view. This window uses a second one, so opening does not blank it.
struct InstallerWindow<Content: View>: NSViewRepresentable {
    @ObservedObject var guest: GuestSession
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
                panel.orderFrontRegardless()
                return
            }
            // Opens at the scanout. The guest fills the body under the title bar. Resize still works.
            let guestSize = Guest.scanoutSize
            let host = NSHostingView(rootView: content())
            host.frame = NSRect(origin: .zero, size: guestSize)
            let panel = NSPanel(
                contentRect: host.frame,
                styleMask: [.titled, .closable, .miniaturizable, .resizable],
                backing: .buffered,
                defer: false
            )
            panel.title = "Installer"
            panel.backgroundColor = .black
            panel.isMovableByWindowBackground = true
            panel.isReleasedWhenClosed = false
            panel.hidesOnDeactivate = false
            panel.contentView = host
            panel.contentMinSize = Guest.installerMin
            // A non-activating panel does not become key, so it cannot take the keyboard.
            // The guest view also ignores the pointer. Quick start never attaches one.
            panel.styleMask.insert(.nonactivatingPanel)
            panel.isFloatingPanel = true
            panel.becomesKeyOnlyIfNeeded = true
            panel.level = .floating
            let anchor = parent ?? NSApp.windows.first { $0.title == "Quick start" && $0.isVisible }
            if let anchor {
                var origin = anchor.frame.origin
                origin.x += anchor.frame.width + 16
                if let visible = (anchor.screen ?? NSScreen.main)?.visibleFrame {
                    if origin.x + panel.frame.width > visible.maxX {
                        origin.x = max(visible.minX, visible.maxX - panel.frame.width)
                    }
                    if origin.y + panel.frame.height > visible.maxY {
                        origin.y = visible.maxY - panel.frame.height
                    }
                    if origin.y < visible.minY { origin.y = visible.minY }
                }
                panel.setFrameOrigin(origin)
            }
            panel.makeFirstResponder(nil)
            let delegate = PanelClose(guest: guest)
            panel.delegate = delegate
            objc_setAssociatedObject(panel, &panelCloseKey, delegate, .OBJC_ASSOCIATION_RETAIN)
            panel.orderFrontRegardless()
            self.panel = panel
        }

        func close() {
            panel?.close()
            panel = nil
        }
    }
}

private var panelCloseKey = 0

private func guestProposal(_ proposal: ProposedViewSize) -> CGSize {
    let width = proposal.width.flatMap { $0.isFinite && $0 > 1 ? $0 : nil } ?? Guest.cloneGuest.width
    let height = proposal.height.flatMap { $0.isFinite && $0 > 1 ? $0 : nil } ?? width / Guest.aspect
    return CGSize(width: width, height: height)
}

extension View {
    /// The installer panel cannot shrink below half the scanout. The guest still fills whatever is larger.
    func installerFloor() -> some View {
        frame(
            minWidth: Guest.installerMin.width,
            maxWidth: .infinity,
            minHeight: Guest.installerMin.height,
            maxHeight: .infinity
        )
    }
}

private final class PanelClose: NSObject, NSWindowDelegate {
    let guest: GuestSession
    init(guest: GuestSession) { self.guest = guest }
    func windowWillClose(_ notification: Notification) {
        DispatchQueue.main.async { self.guest.showWindow = false }
    }
    func windowDidBecomeKey(_ notification: Notification) {
        guard let window = notification.object as? NSWindow else { return }
        window.makeFirstResponder(nil)
    }
}

struct GuestView: NSViewRepresentable {
    let session: GuestSession
    var capture: Bool
    /// Send key events to the guest. The installer thumbnail leaves this off.
    var keyboard: Bool = false
    var preview: NSImage? = nil
    var onPointerInGuest: ((Bool) -> Void)? = nil

    func makeNSView(context: Context) -> GuestPane {
        let pane = GuestPane()
        pane.embed(session.view)
        session.view.onPointerInGuest = onPointerInGuest
        session.view.capture = capture
        session.view.keyboard = keyboard
        // The view's own size guess is a tiny blank. The slot proposes the real size.
        session.view.setContentHuggingPriority(.defaultLow, for: .horizontal)
        session.view.setContentHuggingPriority(.defaultLow, for: .vertical)
        session.view.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        session.view.setContentCompressionResistancePriority(.defaultLow, for: .vertical)
        pane.show(preview)
        pane.onWindow = { [weak session] in
            DispatchQueue.main.async { session?.showScreen() }
        }
        return pane
    }

    func updateNSView(_ nsView: GuestPane, context: Context) {
        nsView.embed(session.view)
        session.view.onPointerInGuest = onPointerInGuest
        session.view.capture = capture
        session.view.keyboard = keyboard
        nsView.onWindow = { [weak session] in
            DispatchQueue.main.async { session?.showScreen() }
        }
        // A paused still must not be cleared by a SwiftUI pass that still has a nil preview.
        if let preview {
            nsView.show(preview)
        } else if !session.paused {
            nsView.show(nil)
        }
        if nsView.window != nil {
            DispatchQueue.main.async { [weak session] in session?.showScreen() }
        }
    }

    /// Take the size SwiftUI proposes. A missing width is not zero, so closing the wizard does not blank the guest.
    func sizeThatFits(_ proposal: ProposedViewSize, nsView: GuestPane, context: Context) -> CGSize? {
        guestProposal(proposal)
    }
}

/// GuestView stays mounted under that frame. The still is an NSImageView above the Metal guest.
/// guest.pausePreview is this overlay. A SwiftUI Image in a ZStack does not cover VZVirtualMachineView.
final class GuestPane: NSView {
    let still = NSImageView()
    var onWindow: () -> Void = {}

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = NSColor.black.cgColor
        still.imageScaling = .scaleProportionallyUpOrDown
        still.wantsLayer = true
        still.layer?.backgroundColor = NSColor.black.cgColor
        still.layer?.zPosition = 10
        addSubview(still)
    }

    required init?(coder: NSCoder) { nil }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard window != nil else { return }
        onWindow()
    }

    func embed(_ screen: InstallScreen) {
        if screen.superview !== self {
            screen.removeFromSuperview()
            addSubview(screen, positioned: .below, relativeTo: still)
        }
        still.layer?.zPosition = 10
    }

    func show(_ image: NSImage?) {
        still.image = image
        still.isHidden = image == nil
        still.alphaValue = 1
        still.layer?.zPosition = 10
        var rect = NSRect(origin: .zero, size: image?.size ?? .zero)
        still.layer?.contents = image?.cgImage(forProposedRect: &rect, context: nil, hints: nil)
        if let screen = subviews.first(where: { $0 is InstallScreen }) as? InstallScreen {
            // Hide the Metal guest while a still is up. Otherwise it draws on top of the image.
            screen.isHidden = image != nil
            screen.alphaValue = image == nil ? 1 : 0
            addSubview(still, positioned: .above, relativeTo: screen)
        }
        needsLayout = true
        layoutSubtreeIfNeeded()
    }

    override func layout() {
        super.layout()
        let frozen = still.image != nil && !still.isHidden
        for child in subviews where child !== still {
            child.frame = bounds
            // Stop can unhide the Metal guest. Keep it covered while the still is up.
            if frozen {
                child.isHidden = true
                child.alphaValue = 0
            }
        }
        still.frame = bounds
        still.layer?.zPosition = 10
    }

    override var isOpaque: Bool { true }

    /// The thumbnail's click is a SwiftUI control above this pane. A Metal guest must not keep that click.
    /// A miss also has to miss this pane. Otherwise the pane keeps the wheel and the window cannot scroll.
    override func hitTest(_ point: NSPoint) -> NSView? {
        guard let screen = subviews.first(where: { $0 is InstallScreen }) as? InstallScreen,
              screen.capture || screen.keyboard else {
            return nil
        }
        guard let hit = super.hitTest(point), hit === screen || hit.isDescendant(of: screen) else {
            return nil
        }
        return hit
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
        screen.keyboard = false
        session.attach(screen)
        return screen
    }

    func updateNSView(_ nsView: InstallScreen, context: Context) {
        nsView.capture = false
        nsView.keyboard = false
        session.attach(nsView)
    }

    /// Same fallback as the thumbnail. A closed wizard proposes no size, and a nil answer blanks the guest.
    func sizeThatFits(_ proposal: ProposedViewSize, nsView: InstallScreen, context: Context) -> CGSize? {
        guestProposal(proposal)
    }
}

/// The guest view hides the host cursor as soon as the pointer is over it.
/// Preseed does not use a pointer, so those events stay with the host until Debian is running.
final class InstallScreen: VZVirtualMachineView {
    /// Closing the wizard mid-install proposes a zero size. That guess is a blank screen, so keep the last real one.
    /// The virtual machine stays running. The window is not the guest.
    private var drawn = Guest.cloneGuest
    /// Set once this view has been in a window and then removed. The next window has to assign the machine again.
    private var leftWindow = false
    /// Sees the cursor leave the guest picture. Enter and exit are not enough once capture hides the pointer.
    private var pointerWatch: Any?

    var capture = false {
        didSet { applyCapture() }
    }

    /// Forward keys without capturing the pointer. The library window can type. The installer thumbnail cannot.
    var keyboard = false {
        didSet { applyCapture() }
    }

    var onPointerInGuest: ((Bool) -> Void)?

    /// Last pixels of the guest. Pause writes this before the machine is stopped.
    func snapshot() -> NSImage? {
        cachedPixels()
    }

    /// cacheDisplay of a Metal guest is often fully transparent. That is not a frame.
    func cachedPixels() -> NSImage? {
        let rect = bounds
        guard rect.width > 1, rect.height > 1, let rep = bitmapImageRepForCachingDisplay(in: rect) else { return nil }
        cacheDisplay(in: rect, to: rep)
        guard let cg = rep.cgImage else { return nil }
        let cached = ownedImage(cg, size: rect.size)
        return visiblePixels(cached) ? cached : nil
    }

    deinit {
        if let pointerWatch {
            NSEvent.removeMonitor(pointerWatch)
        }
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
        // The scroll view in the main window installs its own tracking areas after this view moves in.
        // Capture has to win that pass, or the pointer stays with the host.
        DispatchQueue.main.async { [weak self] in
            self?.applyCapture()
        }
        // The first window works. A view that already left a window stays black until the machine is assigned again.
        if window == nil {
            leftWindow = true
            releaseScroll()
            stopWatchingPointer()
            return
        }
        guard leftWindow, let running = virtualMachine else { return }
        leftWindow = false
        DispatchQueue.main.async { [weak self] in
            guard let self, self.window != nil else { return }
            self.virtualMachine = nil
            self.virtualMachine = running
        }
    }

    override func layout() {
        super.layout()
        if capture {
            window?.invalidateCursorRects(for: self)
            followPointer(nil)
        } else {
            disarm()
            releaseScroll()
        }
    }

    override func mouseEntered(with event: NSEvent) {
        guard capture else { return }
        followPointer(event)
        super.mouseEntered(with: event)
    }

    override func mouseExited(with event: NSEvent) {
        releaseScroll()
        if capture { super.mouseExited(with: event) }
    }

    override func mouseMoved(with event: NSEvent) {
        guard capture else { return }
        followPointer(event)
        if eventInGuest(event) { super.mouseMoved(with: event) }
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
        // point is in the superview. The picture can be smaller than this view. Black bars must scroll.
        let local = convert(point, from: superview)
        guard (capture || keyboard), guestRect.contains(local) else { return nil }
        return super.hitTest(point)
    }

    override var acceptsFirstResponder: Bool { capture || keyboard }

    override func becomeFirstResponder() -> Bool {
        (capture || keyboard) && super.becomeFirstResponder()
    }

    override func mouseDown(with event: NSEvent) {
        if capture || keyboard {
            window?.makeFirstResponder(self)
        }
        if capture { super.mouseDown(with: event) }
    }

    override func mouseUp(with event: NSEvent) { if capture { super.mouseUp(with: event) } }
    override func mouseDragged(with event: NSEvent) { if capture { super.mouseDragged(with: event) } }
    override func rightMouseDown(with event: NSEvent) { if capture { super.rightMouseDown(with: event) } }
    override func scrollWheel(with event: NSEvent) {
        // This view fills the window. A wheel outside the picture has to reach the scroll view.
        guard capture, eventInGuest(event) else {
            nextResponder?.scrollWheel(with: event)
            return
        }
        super.scrollWheel(with: event)
    }

    override func keyDown(with event: NSEvent) {
        if capture || keyboard { super.keyDown(with: event) }
    }
    override func keyUp(with event: NSEvent) {
        if capture || keyboard { super.keyUp(with: event) }
    }
    override func flagsChanged(with event: NSEvent) {
        if capture || keyboard { super.flagsChanged(with: event) }
    }

    private func applyCapture() {
        capturesSystemKeys = capture || keyboard
        if capture {
            updateTrackingAreas()
            window?.acceptsMouseMovedEvents = true
            watchPointer()
            window?.makeFirstResponder(self)
            followPointer(nil)
        } else {
            disarm()
            releaseScroll()
            stopWatchingPointer()
            window?.makeFirstResponder(nil)
            NSCursor.unhide()
            _ = CGAssociateMouseAndMouseCursorPosition(1)
        }
        window?.invalidateCursorRects(for: self)
    }

    /// The guest picture, aspect-fit in this view. Bars around it are not the virtual machine.
    private var guestRect: NSRect {
        let bounds = self.bounds
        guard bounds.width > 1, bounds.height > 1 else { return bounds }
        let aspect = Guest.aspect
        if bounds.width / bounds.height > aspect {
            let width = bounds.height * aspect
            return NSRect(x: bounds.midX - width / 2, y: bounds.minY, width: width, height: bounds.height)
        }
        let height = bounds.width / aspect
        return NSRect(x: bounds.minX, y: bounds.midY - height / 2, width: bounds.width, height: height)
    }

    /// Match the scroll lock to where the cursor is. A captured cursor hides the host pointer, so enter and exit are not enough.
    private func followPointer(_ event: NSEvent?) {
        guard capture else {
            releaseScroll()
            return
        }
        holdScroll(event.map { eventInGuest($0) } ?? pointerInside)
    }

    /// The host cursor, in this view. Capture hides it, so the window location is the one that still moves.
    private var pointerInside: Bool {
        guard let window else { return false }
        let point = convert(window.mouseLocationOutsideOfEventStream, from: nil)
        return guestRect.contains(point)
    }

    private func eventInGuest(_ event: NSEvent) -> Bool {
        guestRect.contains(convert(event.locationInWindow, from: nil))
    }

    /// Scroll and move events still arrive after the cursor leaves the picture. Release the scroll view before it handles them.
    private func watchPointer() {
        guard pointerWatch == nil else { return }
        pointerWatch = NSEvent.addLocalMonitorForEvents(matching: [.mouseMoved, .leftMouseDown, .rightMouseDown, .leftMouseDragged, .rightMouseDragged, .scrollWheel]) { [weak self] event in
            guard let self, event.window === self.window else { return event }
            self.followPointer(event)
            return event
        }
    }

    private func stopWatchingPointer() {
        if let pointerWatch {
            NSEvent.removeMonitor(pointerWatch)
            self.pointerWatch = nil
        }
    }

    /// The main window's scroll view keeps a drag that starts on the guest, so the cursor never leaves the host.
    /// Hold that gesture only while the pointer is over the guest picture. Settings and the bars around it keep scrolling.
    /// The dedicated window has no scroll view, so this is a no-op there.
    private var holdingScroll = false

    private func holdScroll(_ hold: Bool) {
        guard let scroll = enclosingScrollView else {
            if holdingScroll {
                holdingScroll = false
                onPointerInGuest?(false)
            }
            return
        }
        guard hold != holdingScroll else { return }
        holdingScroll = hold
        onPointerInGuest?(hold)
        scroll.verticalScrollElasticity = hold ? .none : .automatic
        scroll.horizontalScrollElasticity = hold ? .none : .automatic
        var views: [NSView] = [scroll, scroll.contentView]
        if let content = scroll.documentView { views.append(content) }
        for view in views {
            for recognizer in view.gestureRecognizers {
                recognizer.isEnabled = !hold
            }
        }
    }

    private func releaseScroll() {
        holdScroll(false)
    }

    private func disarm() {
        trackingAreas.forEach(removeTrackingArea)
    }
}
