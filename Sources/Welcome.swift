import SwiftUI
import AppKit

extension Notification.Name {
    static let libraryDidChange = Notification.Name("LibraryDidChange")
}

struct Machine: Identifiable, Hashable {
    let name: String
    let disk: String
    var id: String { disk }
}

final class LibraryModel: ObservableObject {
    @Published var machines: [Machine] = []
    @Published var selected: String?
    @Published var error: String?
    @Published var cloning = false
    @Published var snapshotReady = SupportDir.snapshotReady
    /// Running clones. The detail view is one machine; this keeps the others alive.
    private var sessions: [String: GuestSession] = [:]

    func appear() {
        refresh()
        guard machines.isEmpty, !cloning, error == nil, snapshotReady else { return }
        cloneNew()
    }

    /// Copy the snapshot into machines/. The library is the machine window.
    func cloneNew() {
        guard !cloning, snapshotReady else { return }
        cloning = true
        error = nil
        let dir = SupportDir.url
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Worker.cloneMachine(dir: dir)
            DispatchQueue.main.async {
                self.cloning = false
                switch result {
                case .success(let disk):
                    self.refresh()
                    self.selected = disk
                case .failure(let message):
                    self.error = message
                }
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
                    found.append(Machine(name: name, disk: disk.path))
                }
            }
        }
        let next = found
        let same = machines.map(\.disk) == next.map(\.disk) && machines.map(\.name) == next.map(\.name)
        if !same {
            machines = next
        }
        let live = Set(next.map(\.disk))
        for disk in sessions.keys.filter({ !live.contains($0) }) {
            sessions[disk]?.stop()
            sessions.removeValue(forKey: disk)
        }
        if selected == nil || !next.contains(where: { $0.disk == selected }) {
            selected = next.first?.disk
        }
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
            library.appear()
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
            library.appear()
            library.selected = disk
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
                    .disabled(library.cloning || !library.snapshotReady)
                }
            }
        } detail: {
            Group {
                if let disk = library.selected {
                    Install(iso: "", disk: disk, fromSnapshot: true, fill: true, session: library.session(for: disk))
                        .id(disk)
                } else if library.cloning || (library.machines.isEmpty && library.error == nil && library.snapshotReady) {
                    ProgressView()
                } else {
                    VStack(spacing: 12) {
                        if let error = library.error {
                            Text(error).font(.callout).foregroundStyle(.red)
                        }
                        Button("Quick start") { openWindow(id: "quick-start") }
                            .buttonStyle(.borderedProminent)
                    }
                }
            }
            .navigationTitle("Virtual Machine Manager")
        }
        .frame(minWidth: WindowMetrics.libraryMin.width, idealWidth: WindowMetrics.library.width, maxWidth: .infinity, minHeight: WindowMetrics.libraryMin.height, idealHeight: WindowMetrics.library.height, maxHeight: .infinity)
    }

    private func openQuickStart() {
        openWindow(id: "quick-start")
        DispatchQueue.main.async {
            dismissWindow(id: "main")
            AppWindow.closeMain()
        }
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
