// Hauksbee.app launcher.
//
// A minimal Cocoa process (compiled with swiftc, so Finder never opens a
// Terminal window) that starts the bundled `hauksbee serve` as a child and
// ties the server's lifetime to the app's:
//
//   - launch: spawn Contents/MacOS/hauksbee serve. The child's stdout is a
//     pipe, not a TTY, so serve's headless-launch detection auto-opens the
//     system browser at the real bound URL. The launcher itself opens no
//     windows; the browser tab IS the UI.
//   - quit (Cmd-Q, Dock right-click > Quit): applicationWillTerminate sends
//     SIGTERM to the child and waits for it, so the server never outlives the
//     app. No orphaned processes.
//   - server dies on its own (port disaster, crash): the termination handler
//     quits the app, so a dead server never leaves a zombie Dock icon.
//
// Why not `exec` the server directly: an exec'd plain binary shows in the Dock
// but has no Cocoa run loop, so Quit is ignored and the Dock offers only Force
// Quit. A real NSApplication delegate makes quitting work the way a Mac user
// expects.

import AppKit

final class LauncherDelegate: NSObject, NSApplicationDelegate {
    private var server: Process?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Resources/bin, not MacOS/: the default macOS filesystem is
        // case-insensitive, so `hauksbee` beside the `Hauksbee` launcher would
        // be the SAME path and clobber it.
        let bin = Bundle.main.bundleURL
            .appendingPathComponent("Contents/Resources/bin/hauksbee")
        guard FileManager.default.isExecutableFile(atPath: bin.path) else {
            presentFailure("The hauksbee binary is missing from the app bundle.")
            return
        }

        let proc = Process()
        proc.executableURL = bin
        proc.arguments = ["serve"]
        // Belt and braces for the quit path: applicationWillTerminate SIGTERMs
        // the child on a normal Quit, but a raw SIGTERM/SIGKILL to the
        // launcher itself never reaches the delegate, and the child then
        // serves on as an orphan. This env var tells serve to watch its parent
        // and exit when the launcher is gone. Opt-in per spawn, so a terminal
        // user's backgrounded `hauksbee serve` is never affected.
        var env = ProcessInfo.processInfo.environment
        env["HAUKSBEE_EXIT_WITH_PARENT"] = String(ProcessInfo.processInfo.processIdentifier)
        proc.environment = env
        // Pipes, not the launcher's (nonexistent) TTY: serve sees a non-TTY
        // stdout and auto-opens the browser at whatever port it really bound.
        proc.standardInput = FileHandle.nullDevice
        proc.standardOutput = Pipe()
        proc.standardError = Pipe()
        proc.terminationHandler = { _ in
            DispatchQueue.main.async { NSApp.terminate(nil) }
        }
        do {
            try proc.run()
            server = proc
        } catch {
            presentFailure("Could not start the hauksbee server: \(error.localizedDescription)")
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        guard let proc = server, proc.isRunning else { return }
        // Detach the handler first so the child's exit does not re-enter
        // terminate() while we are already terminating.
        proc.terminationHandler = nil
        proc.terminate() // SIGTERM
        proc.waitUntilExit()
    }

    private func presentFailure(_ message: String) {
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Hauksbee could not start"
        alert.informativeText = message
        alert.runModal()
        NSApp.terminate(nil)
    }
}

let app = NSApplication.shared
let delegate = LauncherDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)

// A minimal main menu so Cmd-Q works from the keyboard, not just the Dock.
let menubar = NSMenu()
let appMenuItem = NSMenuItem()
menubar.addItem(appMenuItem)
let appMenu = NSMenu()
appMenu.addItem(
    NSMenuItem(
        title: "Quit Hauksbee",
        action: #selector(NSApplication.terminate(_:)),
        keyEquivalent: "q"
    )
)
appMenuItem.submenu = appMenu
app.mainMenu = menubar

app.run()
