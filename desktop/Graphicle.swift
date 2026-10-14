import AppKit
import Darwin
import Foundation

// The macOS shell owns the Rust service's lifetime. All application data,
// retrieval, workflows, HTTP and MCP remain in the standalone Rust executable.
@MainActor
final class GraphicleDelegate: NSObject, NSApplicationDelegate {
    private var service: Process?
    private var currentURL: URL?
    private var errors = ""
    private var stopping = false
    private var terminationSignal: DispatchSourceSignal?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        let main = NSMenu()
        let item = NSMenuItem()
        main.addItem(item)
        let menu = NSMenu()
        menu.addItem(withTitle: "Open Graphicle", action: #selector(openApp), keyEquivalent: "o").target = self
        menu.addItem(withTitle: "Add or Sync Project Folder…", action: #selector(syncFolder), keyEquivalent: "n").target = self
        menu.addItem(withTitle: "Install Command Line Tool…", action: #selector(installCLI), keyEquivalent: "").target = self
        menu.addItem(withTitle: "Show Data Folder", action: #selector(showData), keyEquivalent: "") .target = self
        menu.addItem(.separator())
        menu.addItem(withTitle: "Quit Graphicle", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        item.submenu = menu
        NSApp.mainMenu = main
        // A normal process termination must follow the same cleanup path as
        // Quit, so an updater or terminal stop cannot orphan the Rust worker.
        signal(SIGTERM, SIG_IGN)
        let termination = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        termination.setEventHandler { [weak self] in
            self?.trace("Termination signal received")
            // AppKit's deferred quit can run a nested event loop. Schedule it
            // through that loop, outside the main dispatch queue's signal block.
            self?.perform(#selector(GraphicleDelegate.requestQuit), with: nil, afterDelay: 0)
        }
        termination.resume()
        terminationSignal = termination
        startService()
        trace("Application started")
    }

    @objc private func requestQuit() { NSApp.terminate(nil) }

    private func trace(_ message: String) {
        if ProcessInfo.processInfo.environment["GRAPHICLE_DIAGNOSTICS"] == "1" {
            FileHandle.standardError.write(Data(("Graphicle: " + message + "\n").utf8))
        }
    }

    private func dataURL() -> URL {
        if let override = ProcessInfo.processInfo.environment["GRAPHICLE_DATA_DIR"] {
            return URL(fileURLWithPath: override, isDirectory: true)
        }
        return FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Graphicle", isDirectory: true)
    }

    private func startService() {
        guard let binary = Bundle.main.url(forResource: "graphicle", withExtension: nil) else {
            fail("The app's memory service is missing. Reinstall this build.")
            return
        }
        let process = Process()
        process.executableURL = binary
        process.arguments = ["serve", "--port", "0"]
        if ProcessInfo.processInfo.environment["GRAPHICLE_NO_BROWSER"] != "1" {
            process.arguments?.append("--open")
        }
        let pipe = Pipe()
        process.standardError = pipe
        process.standardOutput = FileHandle.nullDevice
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let bytes = handle.availableData
            guard !bytes.isEmpty, let text = String(data: bytes, encoding: .utf8) else { return }
            Task { @MainActor in self?.receive(text) }
        }
        process.terminationHandler = { [weak self] child in
            Task { @MainActor in
                guard let self, !self.stopping else { return }
                self.fail(self.errors.isEmpty ? "The memory service stopped (\(child.terminationStatus))." : self.errors)
            }
        }
        service = process
        do { try process.run() } catch { fail(error.localizedDescription) }
    }

    private func receive(_ text: String) {
        errors = String((errors + text).suffix(4000))
        for line in errors.split(separator: "\n") {
            if let range = line.range(of: "Graphicle is ready at "),
               let url = URL(string: String(line[range.upperBound...])) {
                currentURL = url
            }
        }
    }

    @objc private func openApp() {
        guard let url = currentURL else { return }
        // Each browser opening receives a fresh short-lived ticket. Reusing the
        // startup ticket would fail after its first use or after three minutes.
        Task {
            do {
                let file = dataURL().appendingPathComponent("local-session.json")
                let bytes = try Data(contentsOf: file)
                guard bytes.count <= 8192,
                      let session = try JSONSerialization.jsonObject(with: bytes) as? [String: Any],
                      let workspace = session["workspace"] as? String,
                      let capability = session["capability"] as? String else { throw CocoaError(.fileReadCorruptFile) }
                var request = URLRequest(url: url.appendingPathComponent("api/workspaces/\(workspace)/browser-session"))
                request.httpMethod = "POST"
                request.timeoutInterval = 10
                request.setValue("application/json", forHTTPHeaderField: "Content-Type")
                request.setValue(capability, forHTTPHeaderField: "X-Workspace-Capability")
                request.httpBody = try JSONSerialization.data(withJSONObject: ["project": session["project"] ?? NSNull()])
                let (data, response) = try await URLSession.shared.data(for: request)
                guard (response as? HTTPURLResponse)?.statusCode == 200,
                      let result = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let ticket = result["ticket"] as? String,
                      var destination = URLComponents(url: url, resolvingAgainstBaseURL: false) else {
                    throw CocoaError(.fileReadNoPermission)
                }
                destination.fragment = "session=\(ticket)"
                if let target = destination.url { NSWorkspace.shared.open(target) }
            } catch {
                NSWorkspace.shared.open(url) // Manual login remains available.
            }
        }
    }

    @objc private func showData() { NSWorkspace.shared.open(dataURL()) }

    @objc private func installCLI() {
        guard let binary = Bundle.main.url(forResource: "graphicle", withExtension: nil) else { return }
        let manager = FileManager.default
        let directory = manager.homeDirectoryForCurrentUser.appendingPathComponent(".local/bin", isDirectory: true)
        let destination = directory.appendingPathComponent("graphicle")
        let alert = NSAlert()
        do {
            try manager.createDirectory(at: directory, withIntermediateDirectories: true)
            let existing = try? manager.destinationOfSymbolicLink(atPath: destination.path)
            if existing != binary.path {
                guard existing == nil && !manager.fileExists(atPath: destination.path) else {
                    alert.messageText = "Another graphicle command is already installed"
                    alert.informativeText = "Your existing command was kept. You can run the native tool directly at \(binary.path)."
                    alert.runModal()
                    return
                }
                try manager.createSymbolicLink(at: destination, withDestinationURL: binary)
            }
            alert.messageText = "The Graphicle command is installed"
            alert.informativeText = "Installed at \(destination.path). If your shell does not find it, add $HOME/.local/bin to your PATH. Run graphicle --help to get started."
        } catch {
            alert.messageText = "The command could not be installed"
            alert.informativeText = error.localizedDescription
        }
        alert.runModal()
    }

    @objc private func syncFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Sync project"
        panel.message = "Graphicle respects .gitignore and skips common secret files and symlinks."
        guard panel.runModal() == .OK, let folder = panel.url,
              let binary = Bundle.main.url(forResource: "graphicle", withExtension: nil) else { return }
        let child = Process()
        child.executableURL = binary
        child.arguments = ["sync", folder.path]
        let pipe = Pipe()
        child.standardError = pipe
        child.standardOutput = FileHandle.nullDevice
        child.terminationHandler = { [weak self] process in
            let message = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? "Sync failed."
            Task { @MainActor in
                if process.terminationStatus == 0 { self?.openApp() }
                else {
                    let alert = NSAlert()
                    alert.messageText = "The project was not synced"
                    alert.informativeText = message
                    alert.runModal()
                }
            }
        }
        do { try child.run() } catch { fail(error.localizedDescription) }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        openApp()
        return true
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        trace("Stopping the memory service")
        stopping = true
        guard let process = service, process.isRunning else { return .terminateNow }
        process.interrupt()
        DispatchQueue.global().async { [weak self] in
            let deadline = Date().addingTimeInterval(4)
            while process.isRunning && Date() < deadline { Thread.sleep(forTimeInterval: 0.05) }
            if process.isRunning { process.terminate() }
            DispatchQueue.main.async { [weak self] in
                self?.trace("Memory service stopped; finishing quit")
                NSApp.reply(toApplicationShouldTerminate: true)
            }
        }
        return .terminateLater
    }

    private func fail(_ message: String) {
        let alert = NSAlert()
        alert.messageText = "Graphicle could not start"
        alert.informativeText = message
        alert.addButton(withTitle: "Quit")
        NSApp.activate(ignoringOtherApps: true)
        alert.runModal()
        NSApp.terminate(nil)
    }
}

@main
struct GraphicleMain {
    @MainActor
    static func main() {
        let app = NSApplication.shared
        let delegate = GraphicleDelegate()
        app.delegate = delegate
        // NSApplication.delegate is weak. Keep the owner of the worker and
        // menu actions alive throughout the event loop in optimized builds.
        withExtendedLifetime(delegate) { app.run() }
    }
}
