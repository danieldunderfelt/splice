import AppKit
import Foundation

// Phase 0 spike, macOS destination half.
// Question: does a synthesized CGEvent left-button press delivered to a borderless
// accepts-first-mouse panel start an NSDraggingSession (via the promise provider API the
// shelf already uses) that further synthesized events drive and that Finder accepts?
//
// Build: swiftc -O -o macos-drag-attach main.swift
// Run (Accessibility permission required for the binary or its terminal):
//   ./macos-drag-attach --origin 900 300 --target 900 700 /path/a.txt /path/b.bin
// Coordinates are global points, origin at the top-left of the main display.
// Expected log tail on success: "mouseDown received", "beginDraggingSession returned",
// then one "promise: wrote <n> bytes to <dest>" per file and "session ended operation=copy".
// Drop the files onto a Finder window or the Desktop at --target.

let start = Date()
func log(_ s: String) {
    let t = String(format: "%7.3f", Date().timeIntervalSince(start))
    FileHandle.standardError.write("[\(t)] \(s)\n".data(using: .utf8)!)
}

var origin = CGPoint(x: 900, y: 300)
var target = CGPoint(x: 900, y: 700)
var files: [URL] = []
var args = Array(CommandLine.arguments.dropFirst())
while !args.isEmpty {
    let a = args.removeFirst()
    switch a {
    case "--origin": origin = CGPoint(x: Double(args.removeFirst())!, y: Double(args.removeFirst())!)
    case "--target": target = CGPoint(x: Double(args.removeFirst())!, y: Double(args.removeFirst())!)
    default: files.append(URL(fileURLWithPath: a).standardizedFileURL)
    }
}
precondition(!files.isEmpty, "pass at least one file")
log("origin=\(origin) target=\(target) files=\(files.map { $0.path })")

final class Promise: NSObject, NSFilePromiseProviderDelegate {
    let source: URL
    init(source: URL) { self.source = source }
    func filePromiseProvider(_ p: NSFilePromiseProvider, fileNameForType fileType: String) -> String { source.lastPathComponent }
    func filePromiseProvider(_ p: NSFilePromiseProvider, writePromiseTo url: URL, completionHandler: @escaping (Error?) -> Void) {
        do {
            try FileManager.default.copyItem(at: source, to: url)
            let n = (try? FileManager.default.attributesOfItem(atPath: url.path)[.size] as? Int) ?? -1
            log("promise: wrote \(n) bytes to \(url.path)")
            completionHandler(nil)
        } catch {
            log("promise: failed \(error)")
            completionHandler(error)
        }
    }
    func operationQueue(for p: NSFilePromiseProvider) -> OperationQueue { Promise.queue }
    static let queue = OperationQueue()
}

final class OriginView: NSView, NSDraggingSource {
    var promises: [Promise] = []
    var started = false
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
    override func mouseDown(with event: NSEvent) {
        log("mouseDown received at \(event.locationInWindow) windowNumber=\(event.windowNumber)")
        beginDrag(event)
    }
    override func mouseDragged(with event: NSEvent) {
        if !started { log("mouseDragged received before a session; starting from it"); beginDrag(event) }
    }
    func beginDrag(_ event: NSEvent) {
        guard !started else { return }
        started = true
        var items: [NSDraggingItem] = []
        for (i, url) in files.enumerated() {
            let type = UTTypeForPath(url)
            let provider = NSFilePromiseProvider(fileType: type, delegate: Promise(source: url))
            promises.append(provider.delegate as! Promise)
            let item = NSDraggingItem(pasteboardWriter: provider)
            let p = convert(event.locationInWindow, from: nil)
            item.setDraggingFrame(NSRect(x: p.x - 16 + CGFloat(i) * 6, y: p.y - 16 + CGFloat(i) * 6, width: 32, height: 32),
                                  contents: NSWorkspace.shared.icon(forFile: url.path))
            items.append(item)
        }
        let session = beginDraggingSession(with: items, event: event, source: self)
        session.animatesToStartingPositionsOnCancelOrFail = true
        log("beginDraggingSession returned; session=\(session)")
    }
    func draggingSession(_ session: NSDraggingSession, sourceOperationMaskFor context: NSDraggingContext) -> NSDragOperation { .copy }
    func ignoreModifierKeys(for session: NSDraggingSession) -> Bool { true }
    func draggingSession(_ session: NSDraggingSession, endedAt screenPoint: NSPoint, operation: NSDragOperation) {
        log("session ended at \(screenPoint) operation=\(operation.rawValue == 0 ? "none (cancelled)" : operation == .copy ? "copy" : "\(operation.rawValue)")")
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) {
            log(operation == .copy ? "RESULT: OK" : "RESULT: FAILED (no copy operation)")
            exit(operation == .copy ? 0 : 2)
        }
    }
}

func UTTypeForPath(_ url: URL) -> String {
    if let t = try? url.resourceValues(forKeys: [.typeIdentifierKey]).typeIdentifier { return t }
    return "public.data"
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let screenHeight = NSScreen.screens.first!.frame.height
let size: CGFloat = 200
// AppKit origin is bottom-left; convert the top-left global point.
let frame = NSRect(x: origin.x - size / 2, y: screenHeight - origin.y - size / 2, width: size, height: size)
let panel = NSPanel(contentRect: frame, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
panel.level = .screenSaver
panel.isOpaque = false
panel.backgroundColor = NSColor.red.withAlphaComponent(0.35)
panel.ignoresMouseEvents = false
panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
let view = OriginView(frame: NSRect(origin: .zero, size: frame.size))
panel.contentView = view
panel.orderFrontRegardless()
log("origin panel shown at \(frame)")

// Synthesized HID-level events, the same path Splice's injector uses.
let src = CGEventSource(stateID: .hidSystemState)
func post(_ type: CGEventType, _ p: CGPoint) {
    let e = CGEvent(mouseEventSource: src, mouseType: type, mouseCursorPosition: p, mouseButton: .left)!
    e.setIntegerValueField(.eventSourceUserData, value: 0x5350_5349)
    e.post(tap: .cghidEventTap)
}
DispatchQueue.global().async {
    Thread.sleep(forTimeInterval: 1.0)
    post(.mouseMoved, CGPoint(x: origin.x - 3, y: origin.y - 3)); Thread.sleep(forTimeInterval: 0.15)
    post(.mouseMoved, origin); Thread.sleep(forTimeInterval: 0.3)
    log("posting left down at \(origin)")
    post(.leftMouseDown, origin); Thread.sleep(forTimeInterval: 0.4)
    let steps = 40
    for i in 1...steps {
        let t = Double(i) / Double(steps)
        post(.leftMouseDragged, CGPoint(x: origin.x + (target.x - origin.x) * t, y: origin.y + (target.y - origin.y) * t))
        Thread.sleep(forTimeInterval: 0.02)
    }
    Thread.sleep(forTimeInterval: 0.5)
    log("posting left up at \(target)")
    post(.leftMouseUp, target)
}
DispatchQueue.main.asyncAfter(deadline: .now() + 25) { log("RESULT: FAILED (timeout)"); exit(3) }
app.run()
