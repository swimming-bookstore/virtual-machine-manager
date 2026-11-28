import SwiftUI
import AppKit

extension Notification.Name {
    static let libraryDidChange = Notification.Name("LibraryDidChange")
}

struct Machine: Identifiable, Hashable {
    let name: String
    let disk: String
    /// Kept across a rename so the open guest is not created again.
    var key: String
    var id: String { key }
}

final class LibraryModel: ObservableObject {
    @Published var machines: [Machine] = []
    @Published var selected: String?
    @Published var error: String?
    @Published var cloning = false
    @Published var naming = false
    @Published var renaming = false
    @Published var deleting = false
    @Published var draftName = "My Debian"
    /// Disk being renamed. The folder under machines/ moves with the name.
    private var renamingDisk: String?
    /// Row identity for each disk. Rename moves the entry so the open guest stays mounted.
    private var keys: [String: String] = [:]
    @Published var snapshotReady = SupportDir.goldenReady
    /// The wizard asked to open the main window. That window may not exist yet, so the notification can be missed.
    private static var pendingFirstName = false
    /// Running clones. The detail view is one machine; this keeps the others alive.
    private var sessions: [String: GuestSession] = [:]

    func appear() {
        refresh()
    }

    /// The main window does not force a virtual machine. Opening an empty machine window is not that, and becoming active does not ask again.
    /// The wizard can ask before this window exists. That ask is shown here, once.
    func offerFirstName() {
        appear()
        guard Self.pendingFirstName else { return }
        askFromWizard()
    }

    /// A machine window with no guest. Opening it is not a request to create a virtual machine.
    func noteBlankMachine(_: Bool) {}

    /// The wizard opened the main window. That is the prompt for the first machine. An empty library on its own is not.
    func askFromWizard() {
        Self.pendingFirstName = false
        guard machines.isEmpty else { return }
        askName()
    }

    /// showLibrary posts before the main window exists. Remember the ask until this window opens.
    static func noteWizardAsk() {
        pendingFirstName = true
    }

    /// A machine is not created until the name is chosen. The + button uses this same popup.
    func askName() {
        guard !cloning, !naming, !renaming, !deleting, snapshotReady else { return }
        naming = true
        draftName = "My Debian"
        error = nil
        DispatchQueue.global(qos: .userInitiated).async {
            let suggested = Worker.suggestName()
            DispatchQueue.main.async {
                guard self.naming, !self.cloning, self.draftName == "My Debian" else { return }
                self.draftName = suggested
            }
        }
    }

    /// The + button asks for a name, the same popup as the first machine.
    func cloneNew() {
        guard !cloning, !renaming, !deleting, snapshotReady else { return }
        askName()
    }

    func cancelName() {
        naming = false
        renaming = false
        renamingDisk = nil
        error = nil
    }

    func cancelDelete() {
        deleting = false
        error = nil
    }

    /// Delete uses a confirm popup. The guest is stopped, then the folder under machines/ is removed.
    func askDelete(_ disk: String) {
        guard !cloning, !naming, !renaming, !deleting else { return }
        guard machines.contains(where: { $0.disk == disk }) else { return }
        deleting = true
        error = nil
    }

    /// Rename uses the create popup. The typed name becomes the folder.
    func askRename(_ disk: String) {
        guard !cloning, !naming, !renaming, !deleting else { return }
        guard let machine = machines.first(where: { $0.disk == disk }) else { return }
        renamingDisk = disk
        renaming = true
        draftName = machine.name
        error = nil
    }

    /// Rust trims the name, keeps spaces, and refuses a folder that is already there.
    func createNamed() {
        let typed = draftName.trimmingCharacters(in: .whitespacesAndNewlines)
        if typed.isEmpty { return }
        if renaming {
            rename(to: typed)
        } else {
            create(name: typed)
        }
    }

    /// The folder is the name. A running guest keeps its session under the new disk path.
    private func rename(to name: String) {
        guard let disk = renamingDisk, !cloning else { return }
        cloning = true
        error = nil
        let dir = SupportDir.url
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Worker.renameMachine(dir: dir, disk: disk, name: name)
            DispatchQueue.main.async {
                self.cloning = false
                switch result {
                case .success(let next):
                    self.adopt(disk, as: next)
                    self.renaming = false
                    self.renamingDisk = nil
                    self.refresh()
                    self.selected = next
                case .failure(let message):
                    self.error = message
                }
            }
        }
    }

    /// Stop the guest first. A running disk cannot be removed out from under Virtualization.
    func confirmDelete() {
        guard let disk = selected, !cloning, deleting else { return }
        guard machines.contains(where: { $0.disk == disk }) else {
            deleting = false
            return
        }
        cloning = true
        error = nil
        release(disk)
        let dir = SupportDir.url
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Worker.deleteMachine(dir: dir, disk: disk)
            DispatchQueue.main.async {
                self.cloning = false
                switch result {
                case .success:
                    self.deleting = false
                    self.refresh()
                case .failure(let message):
                    self.error = message
                }
            }
        }
    }

    /// Drop the session before the folder goes. refresh also stops a guest whose disk is already gone.
    private func release(_ disk: String) {
        keys.removeValue(forKey: disk)
        guard let session = sessions.removeValue(forKey: disk) else { return }
        session.stop()
    }

    /// The dedicated window sets this. Closing that window clears it. Do not create a session just to clear the flag.
    func setDedicatedWindow(_ disk: String, open: Bool) {
        sessions[disk]?.inDedicatedWindow = open
    }

    /// Settings changed CPU or memory. A pause save was made with the old counts, so it cannot be restored.
    func dropPause(for disk: String) {
        sessions[disk]?.forgetPause()
    }

    private func create(name: String?) {
        if let name, name.isEmpty { return }
        guard !cloning, snapshotReady else { return }
        cloning = true
        error = nil
        let dir = SupportDir.url
        DispatchQueue.global(qos: .userInitiated).async {
            self.create(name: name, dir: dir)
        }
    }

    private func create(name: String?, dir: URL) {
        let result = Worker.cloneMachine(dir: dir, name: name)
        DispatchQueue.main.async {
            self.cloning = false
            switch result {
            case .success(let disk):
                self.naming = false
                self.refresh()
                self.selected = disk
            case .failure(let message):
                self.naming = name != nil
                self.error = message
            }
        }
    }

    func refresh() {
        snapshotReady = SupportDir.goldenReady
        let root = SupportDir.url.appendingPathComponent("machines", isDirectory: true)
        let fm = FileManager.default
        var found: [Machine] = []
        if let names = try? fm.contentsOfDirectory(atPath: root.path) {
            for name in names.sorted() where !name.hasPrefix(".") {
                let disk = root.appendingPathComponent(name).appendingPathComponent("disk.img")
                if fm.fileExists(atPath: disk.path) {
                    found.append(Machine(name: name, disk: disk.path, key: disk.path))
                }
            }
        }
        let live = Set(found.map(\.disk))
        keys = keys.filter { live.contains($0.key) }
        for machine in found.indices {
            let disk = found[machine].disk
            if keys[disk] == nil { keys[disk] = disk }
            found[machine].key = keys[disk] ?? disk
        }
        let next = found
        let same = machines.map(\.disk) == next.map(\.disk) && machines.map(\.name) == next.map(\.name) && machines.map(\.key) == next.map(\.key)
        if !same {
            machines = next
        }
        for disk in sessions.keys.filter({ !live.contains($0) }) {
            sessions[disk]?.stop()
            sessions.removeValue(forKey: disk)
        }
        if selected == nil || !next.contains(where: { $0.disk == selected }) {
            selected = next.first?.disk
        }
        // The sidebar binding can keep the deleted path after the last row is gone. That path is not a machine.
        if let selected, !next.contains(where: { $0.disk == selected }) {
            self.selected = nil
        }
    }

    /// Point the running guest at the new disk before refresh drops the old path.
    private func adopt(_ disk: String, as next: String) {
        guard next != disk else { return }
        keys[next] = keys.removeValue(forKey: disk) ?? disk
        guard let session = sessions.removeValue(forKey: disk) else { return }
        session.retarget(next)
        sessions[next] = session
    }

    /// Same guest after the sidebar selects another row. Creating a clone must not stop this one.
    func session(for disk: String) -> GuestSession {
        if sessions[disk] == nil {
            let paused = GuestPause.isPaused(beside: URL(fileURLWithPath: disk))
            sessions[disk] = GuestSession(fromSnapshot: true, diskWritten: false, snapshotSaved: false, loggedIn: true, paused: paused)
        }
        return sessions[disk]!
    }
}

enum AppWindow {
    static func closeMain() {
        for window in NSApp.windows where isMain(window) { window.close() }
    }

    static func closeQuickStart() {
        for window in NSApp.windows where isQuickStart(window) { window.close() }
    }

    static func isMain(_ window: NSWindow) -> Bool {
        window.identifier?.rawValue == "main"
            || window.title == "Virtual Machine Manager"
            || window.title == "Welcome"
    }

    static func isQuickStart(_ window: NSWindow) -> Bool {
        window.identifier?.rawValue == "quick-start" || window.title == "Quick start"
    }
}

struct Welcome: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @ObservedObject var library: LibraryModel

    private var showsLibrary: Bool { library.snapshotReady || !library.machines.isEmpty }

    var body: some View {
        Group {
            if showsLibrary {
                librarySplit
            } else {
                splash
            }
        }
        .background(MainWindowChrome(splash: !showsLibrary))
        .sheet(isPresented: Binding(get: { library.naming || library.renaming }, set: { shown in
            if !shown { library.cancelName() }
        })) {
            NameMachine(library: library)
        }
        .sheet(isPresented: Binding(get: { library.deleting }, set: { shown in
            if !shown { library.cancelDelete() }
        })) {
            DeleteMachine(library: library)
        }
        .onAppear {
            library.offerFirstName()
            AppWindow.closeQuickStart()
        }
        .onChange(of: showsLibrary) { _, shown in
            if shown { AppWindow.closeQuickStart() }
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            library.appear()
        }
        .onReceive(NotificationCenter.default.publisher(for: .libraryDidChange)) { note in
            // Pause, stop, and start post this with no disk. Do not rebuild the left list for those.
            guard let disk = note.object as? String else { return }
            library.refresh()
            // "name" is the wizard finishing. Opening the main window from there prompts for the first machine.
            if disk == "name" {
                library.askFromWizard()
            } else {
                library.selected = disk
            }
            if library.snapshotReady || !library.machines.isEmpty {
                AppWindow.closeQuickStart()
            }
        }
    }

    /// Same splash as master. Quick start is how a first snapshot is made.
    private var splash: some View {
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
            Button(action: openQuickStart) {
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
        .frame(width: WindowMetrics.splashWidth)
    }

    private var librarySplit: some View {
        NavigationSplitView {
            List(library.machines, selection: $library.selected) { machine in
                Text(machine.name)
                    .lineLimit(1)
                    .contextMenu {
                        Button("Open") { openMachine(machine.key) }
                        Button("Rename") { library.askRename(machine.disk) }
                    }
                    .simultaneousGesture(TapGesture(count: 2).onEnded { openMachine(machine.key) })
            }
            .listStyle(.sidebar)
            .navigationSplitViewColumnWidth(min: 200, ideal: 240, max: 300)
            .toolbar {
                // Keep sidebar rows below the traffic lights.
                ToolbarItem(placement: .navigation) { EmptyView() }
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        library.cloneNew()
                    } label: {
                        Label("New virtual machine", systemImage: "plus")
                    }
                    .help("New virtual machine")
                    .disabled(library.cloning || library.naming || library.renaming || library.deleting || !library.snapshotReady)
                }
            }
        } detail: {
            Group {
                if let disk = library.selected {
                    Install(
                        iso: "",
                        disk: disk,
                        fromSnapshot: true,
                        fill: true,
                        session: library.session(for: disk),
                        onDelete: { library.askDelete(disk) },
                        deleteDisabled: library.cloning || library.naming || library.renaming || library.deleting,
                        onOpen: {
                            if let key = library.machines.first(where: { $0.disk == disk })?.key {
                                openMachine(key)
                            }
                        },
                        onHardware: { library.dropPause(for: disk) }
                    )
                        .id(library.machines.first { $0.disk == disk }?.key ?? disk)
                } else if library.cloning {
                    // Creating the next machine. An empty library is not this; the last delete already finished.
                    ProgressView()
                } else {
                    Color.clear
                }
            }
            .navigationTitle("Virtual Machine Manager")
        }
        .frame(minWidth: WindowMetrics.libraryMin.width, idealWidth: WindowMetrics.library.width, maxWidth: .infinity, minHeight: WindowMetrics.libraryMin.height, idealHeight: WindowMetrics.library.height, maxHeight: .infinity)
    }

    /// The guest window is keyed by the stable machine id, not the folder. Rename moves the folder and must not open a second window.
    private func openMachine(_ key: String) {
        guard let machine = library.machines.first(where: { $0.key == key }) else { return }
        // Let go of the guest view before the other window embeds it. Two panes cannot share that view.
        library.session(for: machine.disk).inDedicatedWindow = true
        openWindow(id: "machine", value: key)
    }

    private func openQuickStart() {
        openWindow(id: "quick-start")
        DispatchQueue.main.async {
            dismissWindow(id: "main")
            AppWindow.closeMain()
        }
    }
}

/// Create copies the snapshot under this name. The + button uses this same popup.
private struct NameMachine: View {
    @ObservedObject var library: LibraryModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(library.renaming ? "Rename this virtual machine" : "Name this virtual machine")
                .font(.headline)
            TextField("Name", text: $library.draftName)
                .textFieldStyle(.roundedBorder)
                .onSubmit { library.createNamed() }
            if let error = library.error {
                Text(error).font(.callout).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Cancel") {
                    library.cancelName()
                }
                Button(library.renaming ? "Rename" : "Create") { library.createNamed() }
                    .buttonStyle(.borderedProminent)
                    .disabled(library.draftName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || library.cloning)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 320)
    }
}

/// Delete removes the machine folder. The confirm is required; Cancel leaves the guest running.
private struct DeleteMachine: View {
    @ObservedObject var library: LibraryModel

    private var name: String {
        library.machines.first { $0.disk == library.selected }?.name ?? "this virtual machine"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Delete this virtual machine?")
                .font(.headline)
            Text("\(name) will be removed. This cannot be undone.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let error = library.error {
                Text(error).font(.callout).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Cancel") {
                    library.cancelDelete()
                }
                .keyboardShortcut(.cancelAction)
                Button("Delete") { library.confirmDelete() }
                    .buttonStyle(.borderedProminent)
                    .tint(.red)
                    .disabled(library.cloning)
            }
        }
        .padding(20)
        .frame(width: 320)
    }
}

/// A machine window with no guest. Opening it is not the wizard, so it does not ask to create a virtual machine.
struct EmptyMachineWindow: View {
    @ObservedObject var library: LibraryModel

    var body: some View {
        Color.black
            .frame(minWidth: Guest.installerMin.width, minHeight: Guest.installerMin.height)
            .onAppear { library.noteBlankMachine(true) }
            .onDisappear { library.noteBlankMachine(false) }
    }
}

/// The machine, outside the sidebar. Closing it does not stop the guest. The library still owns the session.
struct MachineWindow: View {
    @ObservedObject var library: LibraryModel
    let machineKey: String
    @Environment(\.dismiss) private var dismiss
    @State private var looked = false

    private var machine: Machine? {
        library.machines.first { $0.key == machineKey }
    }

    var body: some View {
        Group {
            if let machine {
                Install(
                    iso: "",
                    disk: machine.disk,
                    fromSnapshot: true,
                    fill: true,
                    windowed: true,
                    session: library.session(for: machine.disk)
                )
                .id(machine.key)
                .navigationTitle(machine.name)
            } else if looked {
                Color.black.onAppear { dismiss() }
            } else {
                Color.black
            }
        }
        .frame(minWidth: Guest.installerMin.width, minHeight: Guest.installerMin.height)
        .background(GuestWindowChrome(title: machine?.name ?? "Debian", onShow: restoreScreen))
        .onAppear {
            library.appear()
            looked = true
            if let disk = machine?.disk {
                library.session(for: disk).claimScreen()
                library.setDedicatedWindow(disk, open: true)
            }
        }
        .onDisappear {
            // Closing the window gives the guest view back to the library. Do not clear this later, or a new open loses the flag.
            // Take the guest view out before the window is destroyed. A view left there draws blank the next time it is shown.
            if let disk = library.machines.first(where: { $0.key == machineKey })?.disk {
                library.session(for: disk).releaseScreen()
                library.setDedicatedWindow(disk, open: false)
            }
        }
        .onChange(of: library.machines.map(\.key)) { _, keys in
            if looked, !keys.contains(machineKey) { dismiss() }
        }
    }

    /// The window came back without a new Open. The guest view from the closed window is blank, so mount another.
    private func restoreScreen() {
        guard let disk = machine?.disk else { return }
        let session = library.session(for: disk)
        session.claimScreen()
        library.setDedicatedWindow(disk, open: true)
    }
}

/// Splash stays the compact card. The library is a normal resizable window.
/// The title is applied before the window is shown, so it does not pop in after the first frame.
private struct MainWindowChrome: NSViewRepresentable {
    var splash: Bool

    func makeNSView(context: Context) -> ChromeView {
        let view = ChromeView()
        view.splash = splash
        return view
    }

    func updateNSView(_ view: ChromeView, context: Context) {
        view.splash = splash
        view.apply()
    }
}

/// viewDidMoveToWindow runs before the window is shown. The old async apply hid the title after launch.
private final class ChromeView: NSView {
    var splash = false

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        apply()
    }

    func apply() {
        guard let window else { return }
        window.titlebarAppearsTransparent = false
        if window.toolbarStyle != .unified {
            window.toolbarStyle = .unified
        }
        if splash {
            window.title = "Welcome"
            window.titleVisibility = .visible
            window.styleMask.remove(.resizable)
            let height = max(window.contentView?.fittingSize.height ?? 0, 248)
            let size = NSSize(width: WindowMetrics.splashWidth, height: height)
            if window.contentView?.frame.size != size {
                window.setContentSize(size)
            }
        } else {
            window.title = "Virtual Machine Manager"
            // Keep the title up. Hiding it left the bar empty until a later layout, which is the jump.
            window.titleVisibility = .visible
            window.styleMask.insert(.resizable)
            guard window.frame.width < WindowMetrics.libraryMin.width || window.frame.height < WindowMetrics.libraryMin.height else { return }
            let screen = window.screen?.visibleFrame ?? NSScreen.main?.visibleFrame ?? NSRect(x: 0, y: 0, width: WindowMetrics.library.width, height: WindowMetrics.library.height)
            var frame = window.frame
            frame.size.width = min(WindowMetrics.library.width, screen.width)
            frame.size.height = min(WindowMetrics.library.height, screen.height)
            if frame.maxY > screen.maxY { frame.origin.y = screen.maxY - frame.height }
            if frame.minY < screen.minY { frame.origin.y = screen.minY }
            if frame.maxX > screen.maxX { frame.origin.x = screen.maxX - frame.width }
            if frame.minX < screen.minX { frame.origin.x = screen.minX }
            if frame != window.frame {
                window.setFrame(frame, display: true)
            }
        }
    }
}

/// Title is the machine name. The size stays whatever the user made it. A forced frame would fight the resize.
/// A restored window may not run onAppear. This view moves into that window, so it can mount the guest again.
private struct GuestWindowChrome: NSViewRepresentable {
    var title: String
    var onShow: () -> Void

    func makeNSView(context: Context) -> GuestChromeView {
        let view = GuestChromeView()
        view.title = title
        view.onShow = onShow
        return view
    }

    func updateNSView(_ view: GuestChromeView, context: Context) {
        view.title = title
        view.onShow = onShow
        view.apply()
    }
}

private final class GuestChromeView: NSView {
    var title = "Debian"
    var onShow: () -> Void = {}
    private var keyObserver: NSObjectProtocol?
    private var closeObserver: NSObjectProtocol?
    private var fullObserver: NSObjectProtocol?
    private var leaveObserver: NSObjectProtocol?
    private var closed = false

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        apply()
        if let keyObserver { NotificationCenter.default.removeObserver(keyObserver) }
        if let closeObserver { NotificationCenter.default.removeObserver(closeObserver) }
        if let fullObserver { NotificationCenter.default.removeObserver(fullObserver) }
        if let leaveObserver { NotificationCenter.default.removeObserver(leaveObserver) }
        keyObserver = nil
        closeObserver = nil
        fullObserver = nil
        leaveObserver = nil
        guard let window else { return }
        closeObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification,
            object: window,
            queue: .main
        ) { [weak self] _ in
            self?.closed = true
        }
        keyObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeKeyNotification,
            object: window,
            queue: .main
        ) { [weak self] _ in
            guard let self, self.closed else { return }
            self.closed = false
            self.onShow()
        }
        // Full screen keeps the toolbar unless the window itself hides it.
        fullObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.willEnterFullScreenNotification,
            object: window,
            queue: .main
        ) { [weak self] note in
            guard let window = note.object as? NSWindow else { return }
            self?.hideBar(window)
        }
        leaveObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didExitFullScreenNotification,
            object: window,
            queue: .main
        ) { [weak self] note in
            guard let window = note.object as? NSWindow else { return }
            self?.showBar(window)
        }
        window.makeKeyAndOrderFront(nil)
    }

    deinit {
        if let keyObserver { NotificationCenter.default.removeObserver(keyObserver) }
        if let closeObserver { NotificationCenter.default.removeObserver(closeObserver) }
        if let fullObserver { NotificationCenter.default.removeObserver(fullObserver) }
        if let leaveObserver { NotificationCenter.default.removeObserver(leaveObserver) }
    }

    func apply() {
        guard let window else { return }
        window.title = title
        window.styleMask.insert(.resizable)
        window.styleMask.insert(.fullSizeContentView)
        let min = NSSize(width: Guest.installerMin.width, height: Guest.installerMin.height)
        if window.contentMinSize != min {
            window.contentMinSize = min
        }
        if window.styleMask.contains(.fullScreen) {
            hideBar(window)
        } else {
            showBar(window)
        }
    }

    /// The guest fills the screen. The title bar and the Open / Start / Pause / Stop / Delete bar go with it.
    private func hideBar(_ window: NSWindow) {
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.toolbar?.isVisible = false
        window.styleMask.insert(.fullSizeContentView)
    }

    /// Back to the windowed bar. The title stays the machine name.
    private func showBar(_ window: NSWindow) {
        window.toolbar?.isVisible = true
        window.titleVisibility = .visible
        window.titlebarAppearsTransparent = false
        if window.toolbarStyle != .unified {
            window.toolbarStyle = .unified
        }
    }
}
