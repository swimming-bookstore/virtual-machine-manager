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
    @Published var draftName = "My Debian"
    /// Disk being renamed. The folder under machines/ moves with the name.
    private var renamingDisk: String?
    /// Row identity for each disk. Rename moves the entry so the open guest stays mounted.
    private var keys: [String: String] = [:]
    @Published var snapshotReady = SupportDir.snapshotReady
    /// Running clones. The detail view is one machine; this keeps the others alive.
    private var sessions: [String: GuestSession] = [:]

    func appear() {
        refresh()
    }

    /// Opening the library with no machines asks for the first name. Becoming active does not ask again.
    func offerFirstName() {
        appear()
        guard machines.isEmpty, !cloning, !naming, !renaming, error == nil, snapshotReady else { return }
        askName()
    }

    /// A machine is not created until the name is chosen. The + button uses this same popup.
    func askName() {
        guard !cloning, !naming, !renaming, snapshotReady else { return }
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
        guard !cloning, !renaming, snapshotReady else { return }
        askName()
    }

    func cancelName() {
        naming = false
        renaming = false
        renamingDisk = nil
        error = nil
    }

    /// Rename uses the create popup. The typed name becomes the folder.
    func askRename(_ disk: String) {
        guard !cloning, !naming, !renaming else { return }
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
        snapshotReady = SupportDir.snapshotReady
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
    @StateObject private var library = LibraryModel()

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
            if disk == "name" {
                if library.machines.isEmpty { library.askName() }
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
                        Button("Rename") { library.askRename(machine.disk) }
                    }
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
                    .disabled(library.cloning || library.naming || library.renaming || !library.snapshotReady)
                }
            }
        } detail: {
            Group {
                if let disk = library.selected {
                    Install(iso: "", disk: disk, fromSnapshot: true, fill: true, session: library.session(for: disk))
                        .id(library.machines.first { $0.disk == disk }?.key ?? disk)
                } else if library.machines.isEmpty || library.cloning {
                    ProgressView()
                } else {
                    Color.clear
                }
            }
            .navigationTitle("Virtual Machine Manager")
        }
        .frame(minWidth: WindowMetrics.libraryMin.width, idealWidth: WindowMetrics.library.width, maxWidth: .infinity, minHeight: WindowMetrics.libraryMin.height, idealHeight: WindowMetrics.library.height, maxHeight: .infinity)
        .sheet(isPresented: Binding(get: { library.naming || library.renaming }, set: { shown in
            if !shown { library.cancelName() }
        })) {
            NameMachine(library: library)
        }
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
