// Supvan Printer — macOS menu-bar front end for supvan-printer-app.
//
// The printing itself is the Rust service (bundled as
// Contents/MacOS/supvan-printer-app). This shell starts it as a child, talks
// to it over stdin/stdout (`SUPVAN_EVENTS=1`: JSON events out, `connect` /
// `disconnect` / `quit` in), and gives it what a daemon lacks: a status icon,
// notifications when a job is held or fails, a Disconnect button for a
// printer other hosts also use, and a proper app bundle for the Bluetooth
// privacy prompt.
//
// Built by macos/build-app.sh with plain swiftc — no Xcode project.

import AppKit
import CoreBluetooth
import CoreImage
import ServiceManagement
import UserNotifications

/// Chinese for Chinese-language systems, English otherwise.
func L(_ zh: String, _ en: String) -> String {
    (Locale.preferredLanguages.first ?? "en").hasPrefix("zh") ? zh : en
}

struct PrinterEntry {
    let name: String
    let info: String
    let uri: String
}

let logURL = FileManager.default.homeDirectoryForCurrentUser
    .appendingPathComponent("Library/Logs/SupvanPrinter.log")

/// Where the service drops what it printed (`SUPVAN_DUMP_DIR`), so the last
/// job can be looked at. Emptied on every launch.
let jobsURL = FileManager.default.homeDirectoryForCurrentUser
    .appendingPathComponent("Library/Caches/SupvanPrinter/jobs")

/// The densities the calibration prints, one label each: the two ends and the
/// middle of the vendor app's 1–9. One label per density because the firmware
/// appears to take the density once per label — bands at different densities
/// on a single label all came out alike.
let testDensities = [1, 5, 9]

/// Draw a density calibration label for one density: its number, sample text
/// in two sizes, a fine-line grating and a small QR code (which reads `D<n>`),
/// repeated down the label. These are what darkness actually changes — thin
/// strokes fill in, small code modules bleed — where a solid block just
/// saturates. Returns a binary PGM, `widthMM × heightMM` at 8 dots per mm.
func densityTestPGM(widthMM: Int, heightMM: Int, density: Int) -> Data? {
    let testDensities = Array(repeating: density, count: max(1, min(4, heightMM * 8 / 120)))
    let W = widthMM * 8, H = heightMM * 8
    guard W >= 160, H >= testDensities.count * 24,
          let rep = NSBitmapImageRep(
              bitmapDataPlanes: nil, pixelsWide: W, pixelsHigh: H, bitsPerSample: 8, samplesPerPixel: 1,
              hasAlpha: false, isPlanar: false, colorSpaceName: .deviceWhite, bytesPerRow: W, bitsPerPixel: 8),
          let base = NSGraphicsContext(bitmapImageRep: rep)
    else { return nil }
    let cg = base.cgContext
    cg.translateBy(x: 0, y: CGFloat(H))
    cg.scaleBy(x: 1, y: -1)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: cg, flipped: true)
    defer { NSGraphicsContext.restoreGraphicsState() }

    NSColor.white.setFill()
    NSRect(x: 0, y: 0, width: W, height: H).fill()
    NSColor.black.setFill()

    let band = H / testDensities.count
    // The head covers 48 of a 50 mm label, centred: keep clear of the edges.
    let left = 14, right = W - 14
    let ciContext = CIContext()
    let ink: [NSAttributedString.Key: Any] = [.foregroundColor: NSColor.black]

    for (i, density) in testDensities.enumerated() {
        let top = i * band
        if i > 0 { NSRect(x: left, y: top, width: right - left, height: 1).fill() }

        // QR on the right: as many whole dots per module as the band allows.
        var qrSide = 0
        if let filter = CIFilter(name: "CIQRCodeGenerator") {
            filter.setValue("D\(density)".data(using: .ascii), forKey: "inputMessage")
            filter.setValue("M", forKey: "inputCorrectionLevel")
            if let code = filter.outputImage, let image = ciContext.createCGImage(code, from: code.extent) {
                let modules = image.width
                let scale = min(3, (band - 8) / modules)
                if scale >= 2 {
                    qrSide = modules * scale
                    let pixels = NSBitmapImageRep(cgImage: image)
                    let x0 = right - qrSide, y0 = top + (band - qrSide) / 2
                    for my in 0..<modules {
                        for mx in 0..<modules {
                            if let c = pixels.colorAt(x: mx, y: my), c.brightnessComponent < 0.5 {
                                NSRect(x: x0 + mx * scale, y: y0 + my * scale, width: scale, height: scale).fill()
                            }
                        }
                    }
                }
            }
        }

        // The density, large, on the left.
        let big = NSFont.boldSystemFont(ofSize: CGFloat(min(band - 12, 44)))
        let number = NSAttributedString(string: "\(density)", attributes: ink.merging([.font: big]) { $1 })
        number.draw(at: NSPoint(x: CGFloat(left), y: CGFloat(top) + (CGFloat(band) - number.size().height) / 2))

        // Sample text in the middle: a normal size and a small one.
        let textX = left + 62
        let textW = right - qrSide - 34 - textX
        if textW > 60 {
            let lines: [(String, CGFloat)] = [("标签 Label 打印", 20), ("浓度测试小字 abc123 ABC", 12)]
            var y = CGFloat(top) + 6
            for (text, size) in lines where y + size < CGFloat(top + band) {
                let str = NSAttributedString(
                    string: text, attributes: ink.merging([.font: NSFont.systemFont(ofSize: size)]) { $1 })
                str.draw(with: NSRect(x: CGFloat(textX), y: y, width: CGFloat(textW), height: size * 1.4),
                         options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine])
                y += size * 1.45
            }
            // One-dot lines two dots apart: the first thing to fill in when too dark.
            let gy = Int(y) + 4
            if gy + 10 < top + band {
                var x = textX
                while x < textX + 40 {
                    NSRect(x: x, y: gy, width: 1, height: 10).fill()
                    x += 3
                }
                var yy = gy
                while yy < gy + 10 {
                    NSRect(x: textX + 52, y: yy, width: 40, height: 1).fill()
                    yy += 3
                }
            }
        }
    }
    cg.flush()

    guard let pixels = rep.bitmapData else { return nil }
    var out = "P5\n\(W) \(H)\n255\n".data(using: .ascii)!
    for y in 0..<H {
        out.append(UnsafeBufferPointer(start: pixels + y * rep.bytesPerRow, count: W))
    }
    return out
}

/// Read a binary PBM (`P4`) — the service's record of the dots it sent, which
/// is mirrored for the print head — and return it the right way round as PNG.
func pngFromMirroredPBM(_ url: URL) -> Data? {
    guard let d = try? Data(contentsOf: url) else { return nil }
    var i = d.startIndex
    var fields: [String] = []
    while fields.count < 3 {
        while i < d.endIndex, d[i] == 0x20 || d[i] == 0x0A || d[i] == 0x0D || d[i] == 0x09 { i += 1 }
        var j = i
        while j < d.endIndex, !(d[j] == 0x20 || d[j] == 0x0A || d[j] == 0x0D || d[j] == 0x09) { j += 1 }
        guard j > i, let f = String(data: d[i..<j], encoding: .ascii) else { return nil }
        fields.append(f)
        i = j
    }
    i += 1
    guard fields[0] == "P4", let w = Int(fields[1]), let h = Int(fields[2]), w > 0, h > 0 else { return nil }
    let stride = (w + 7) / 8
    guard d.endIndex - i >= stride * h,
          let rep = NSBitmapImageRep(
              bitmapDataPlanes: nil, pixelsWide: w, pixelsHigh: h, bitsPerSample: 8, samplesPerPixel: 1,
              hasAlpha: false, isPlanar: false, colorSpaceName: .deviceWhite, bytesPerRow: w, bitsPerPixel: 8),
          let out = rep.bitmapData
    else { return nil }
    for y in 0..<h {
        for x in 0..<w {
            let byte = d[i + y * stride + x / 8]
            let ink = (byte >> (7 - UInt8(x % 8))) & 1 == 1
            out[y * w + (w - 1 - x)] = ink ? 0 : 255
        }
    }
    return rep.representation(using: .png, properties: [:])
}

final class Log {
    static let shared = Log()
    let handle: FileHandle

    private init() {
        let fm = FileManager.default
        try? fm.createDirectory(at: logURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        // Start over once the file gets big; this is a diagnostic log, not a record.
        if let size = (try? fm.attributesOfItem(atPath: logURL.path))?[.size] as? Int, size > 1_000_000 {
            try? fm.removeItem(at: logURL)
        }
        if !fm.fileExists(atPath: logURL.path) {
            fm.createFile(atPath: logURL.path, contents: nil)
        }
        handle = (try? FileHandle(forWritingTo: logURL)) ?? FileHandle.standardError
        handle.seekToEndOfFile()
    }

    func write(_ message: String) {
        let stamp = ISO8601DateFormatter().string(from: Date())
        handle.write("[\(stamp) app] \(message)\n".data(using: .utf8)!)
    }
}

func log(_ message: String) { Log.shared.write(message) }

/// Run a tool and return (exit status, stdout). C locale so output parses.
func run(_ path: String, _ args: [String]) -> (Int32, String) {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: path)
    p.arguments = args
    var env = ProcessInfo.processInfo.environment
    env["LANG"] = "C"
    env["LC_ALL"] = "C"
    p.environment = env
    let out = Pipe()
    p.standardOutput = out
    p.standardError = out
    do { try p.run() } catch { return (-1, "\(error)") }
    let data = out.fileHandleForReading.readDataToEndOfFile()
    p.waitUntilExit()
    return (p.terminationStatus, String(data: data, encoding: .utf8) ?? "")
}

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate, UNUserNotificationCenterDelegate,
    CBCentralManagerDelegate
{
    private let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let menu = NSMenu()

    // The service.
    private var helper: Process?
    private var helperInput: FileHandle?
    private var stdoutBuffer = Data()
    private var quitting = false
    private var restarts = 0
    private var helperError: String?

    // What the service last told us.
    private var printers: [PrinterEntry] = []
    private var connected = false
    private var connecting = false
    private var reasons: [String] = []
    private var media: (Int, Int)?
    private var gap: Int?
    /// The loaded roll carries a genuine tag: its size and gap are read from
    /// the tag and cannot be set from here. `nil` until the printer was read.
    private var rollLocked: Bool?
    private var remaining: Int?
    private var activeJobs = Set<Int>()
    private var heldJobs: [Int: [String]] = [:]

    private var queueName: String?
    private var notificationsAllowed = false

    /// Exists only to raise the Bluetooth privacy prompt on the app's behalf.
    private var bluetoothProbe: CBCentralManager?
    private var bluetoothDenied = false

    /// What the page mostly is, which decides how grey becomes dots.
    /// "threshold": plain black and white — crisp text and scannable codes.
    /// "bayer": halftone — photographs keep their tones.
    private var ditherMode: String {
        get { UserDefaults.standard.string(forKey: "ditherMode") ?? "threshold" }
        set { UserDefaults.standard.set(newValue, forKey: "ditherMode") }
    }

    /// Print density on the vendor app's own scale, 1 (lightest) to 9; 4 is
    /// the vendor default. The printer takes it once per label.
    private var density: Int {
        get {
            let v = UserDefaults.standard.object(forKey: "density") as? Int ?? 4
            return min(max(v, 1), 9)
        }
        set { UserDefaults.standard.set(newValue, forKey: "density") }
    }

    /// The service's baseline darkness (0–100) that lands on `density`: it
    /// maps darkness onto the protocol's 0–15 as `(d * 15 + 50) / 100`.
    private var darknessForDensity: Int { (density * 100 + 7) / 15 }

    private let port = 8631
    private var idleSeconds: Int {
        let v = UserDefaults.standard.integer(forKey: "idleDisconnectSeconds")
        return v > 0 ? v : 90
    }

    // MARK: Lifecycle

    func applicationDidFinishLaunching(_ notification: Notification) {
        let me = Bundle.main.bundleIdentifier ?? ""
        if NSRunningApplication.runningApplications(withBundleIdentifier: me).count > 1 {
            log("another instance is already running; exiting")
            NSApp.terminate(nil)
            return
        }
        log("launch: \(Bundle.main.bundlePath)")
        try? FileManager.default.removeItem(at: jobsURL)
        try? FileManager.default.createDirectory(at: jobsURL, withIntermediateDirectories: true)

        menu.delegate = self
        statusItem.menu = menu
        updateIcon()

        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { ok, error in
            log("notification request granted=\(ok) error=\(error?.localizedDescription ?? "none")")
            self.refreshNotificationStatus()
        }

        NSAppleEventManager.shared().setEventHandler(
            self, andSelector: #selector(handleURL(_:reply:)),
            forEventClass: AEEventClass(kInternetEventClass), andEventID: AEEventID(kAEGetURL))

        NSWorkspace.shared.notificationCenter.addObserver(
            self, selector: #selector(willSleep), name: NSWorkspace.willSleepNotification, object: nil)

        ensureBluetoothThenStart()
    }

    // MARK: Bluetooth permission

    /// The service must not start before macOS has an answer about Bluetooth:
    /// its first IOBluetooth call would block on the pending prompt. So the
    /// app asks first — a CBCentralManager is what raises the prompt — and
    /// starts the service once access is granted. The service is a child of
    /// this app, so the grant covers it.
    private func ensureBluetoothThenStart() {
        let auth = CBCentralManager.authorization
        log("bluetooth authorization=\(auth.rawValue) (0 undetermined, 1 restricted, 2 denied, 3 allowed)")
        switch auth {
        case .allowedAlways:
            bluetoothDenied = false
            bluetoothProbe = nil
            if helper == nil { startHelper() }
        case .notDetermined:
            if bluetoothProbe == nil {
                bluetoothProbe = CBCentralManager(
                    delegate: self, queue: nil, options: [CBCentralManagerOptionShowPowerAlertKey: false])
            }
        default:
            bluetoothDenied = true
            bluetoothProbe = nil
        }
        updateIcon()
    }

    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        log("bluetooth state=\(central.state.rawValue)")
        if CBCentralManager.authorization != .notDetermined {
            ensureBluetoothThenStart()
        }
    }

    @objc private func openBluetoothPrivacy() {
        NSWorkspace.shared.open(
            URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Bluetooth")!)
    }

    func applicationWillTerminate(_ notification: Notification) {
        quitting = true
        send("quit")
        if let helper, helper.isRunning {
            // Give it a moment to hang up the Bluetooth link cleanly.
            let deadline = Date().addingTimeInterval(1.5)
            while helper.isRunning && Date() < deadline { usleep(50_000) }
            if helper.isRunning { helper.terminate() }
        }
    }

    /// Free the printer before the Mac sleeps: the link would die anyway and
    /// the printer would stay "connected" to a host that is gone.
    @objc private func willSleep() {
        send("disconnect")
    }

    /// `supvanprinter://connect|disconnect` — for Shortcuts and scripts.
    @objc private func handleURL(_ event: NSAppleEventDescriptor, reply: NSAppleEventDescriptor) {
        guard let s = event.paramDescriptor(forKeyword: keyDirectObject)?.stringValue,
              let host = URL(string: s)?.host else { return }
        log("url command: \(host)")
        switch host {
        case "connect": connect()
        case "disconnect": disconnect()
        case "test-density": testDensity()
        default: break
        }
    }

    // MARK: Service process

    private func startHelper() {
        let exe = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/supvan-printer-app")
        let p = Process()
        p.executableURL = exe
        var env = ProcessInfo.processInfo.environment
        env["SUPVAN_EVENTS"] = "1"
        env["SUPVAN_ON_DEMAND"] = "1"
        env["SUPVAN_IDLE_DISCONNECT_SECS"] = String(idleSeconds)
        env["SUPVAN_HOST"] = "127.0.0.1"
        env["SUPVAN_PORT"] = String(port)
        env["SUPVAN_MDNS"] = "0"
        env["IPP_PRINTER_APP_POLL_SECS"] = "10"
        env["SUPVAN_DITHER"] = ditherMode
        env["SUPVAN_DARKNESS"] = String(darknessForDensity)
        env["SUPVAN_DUMP_DIR"] = jobsURL.path
        if env["RUST_LOG"] == nil { env["RUST_LOG"] = "info" }
        p.environment = env

        let input = Pipe()
        let output = Pipe()
        p.standardInput = input
        p.standardOutput = output
        p.standardError = Log.shared.handle
        output.fileHandleForReading.readabilityHandler = { [weak self] h in
            let data = h.availableData
            if data.isEmpty {
                h.readabilityHandler = nil
                return
            }
            DispatchQueue.main.async { self?.consume(data) }
        }
        p.terminationHandler = { [weak self] proc in
            DispatchQueue.main.async { self?.helperExited(proc.terminationStatus) }
        }
        do {
            try p.run()
            helper = p
            helperInput = input.fileHandleForWriting
            helperError = nil
            log("service started pid=\(p.processIdentifier)")
        } catch {
            helperError = "\(error.localizedDescription)"
            log("service failed to start: \(error)")
        }
        updateIcon()
    }

    private func helperExited(_ status: Int32) {
        log("service exited status=\(status)")
        helper = nil
        helperInput = nil
        connected = false
        connecting = false
        activeJobs.removeAll()
        if quitting { return }
        restarts += 1
        if restarts > 5 {
            helperError = L("打印服务反复退出，请查看日志", "The print service keeps exiting — see the log")
            notify(L("打印服务已停止", "Print service stopped"), helperError!)
            updateIcon()
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
            guard let self, !self.quitting, self.helper == nil else { return }
            self.startHelper()
        }
        updateIcon()
    }

    private func send(_ command: String) {
        guard let helperInput, let helper, helper.isRunning else { return }
        try? helperInput.write(contentsOf: (command + "\n").data(using: .utf8)!)
    }

    private func consume(_ data: Data) {
        stdoutBuffer.append(data)
        while let nl = stdoutBuffer.firstIndex(of: 0x0A) {
            let line = stdoutBuffer.subdata(in: stdoutBuffer.startIndex..<nl)
            stdoutBuffer.removeSubrange(stdoutBuffer.startIndex...nl)
            guard !line.isEmpty,
                  let obj = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                  let event = obj["event"] as? String else { continue }
            handle(event, obj)
        }
    }

    // MARK: Events

    private func handle(_ event: String, _ e: [String: Any]) {
        let job = e["job"] as? Int ?? -1
        let why = e["reasons"] as? [String] ?? []
        switch event {
        case "printers":
            printers = (e["printers"] as? [[String: Any]] ?? []).compactMap {
                guard let name = $0["name"] as? String, let uri = $0["uri"] as? String else { return nil }
                return PrinterEntry(name: name, info: $0["info"] as? String ?? name, uri: uri)
            }
            restarts = 0
            log("printers: \(printers.map(\.name))")
            refreshQueue()
            autoAddQueueOnce()
        case "link":
            connected = e["connected"] as? Bool ?? false
            connecting = false
            if !connected { reasons = [] }
            log("link connected=\(connected)")
        case "status":
            let was = (connected, reasons)
            connected = e["connected"] as? Bool ?? false
            connecting = false
            reasons = connected ? why : []
            if let mm = e["media_mm"] as? [Int], mm.count == 2 {
                media = (mm[0], mm[1])
                if densityTestPending { testDensity() }
            }
            if let left = e["remaining"] as? Int { remaining = left }
            if let g = e["gap_mm"] as? Int { gap = g }
            if let locked = e["roll_locked"] as? Bool {
                if rollLocked != locked {
                    log("roll: tagged=\(locked) media=\(String(describing: media)) gap=\(String(describing: gap))")
                }
                rollLocked = locked
            }
            if was != (connected, reasons) {
                log("status connected=\(connected) reasons=\(reasons) media=\(String(describing: media)) remaining=\(String(describing: remaining))")
            }
        case "test_print":
            connecting = false
            let ok = e["ok"] as? Bool ?? false
            let message = e["message"] as? String ?? ""
            log("test_print ok=\(ok) \(message)")
            if !ok {
                notify(L("测试条没有打出来", "The test label didn't print"),
                       message.contains("cannot reach") ? explain(["offline"], nil) : message, important: true)
            }
        case "label_set":
            let ok = e["ok"] as? Bool ?? false
            let message = e["message"] as? String ?? ""
            log("label_set ok=\(ok) \(message)")
            if ok {
                notify(L("标签规格已写入打印机", "Label settings written"),
                       L("系统打印机的默认纸张会随之更新。", "The system printer's default paper size follows."))
            } else if message.contains("genuine RFID") {
                rollLocked = true
                notify(L("这卷标签不能改规格", "This roll can't be changed"), lockedExplanation, important: true)
            } else if message.contains("cannot reach") {
                notify(L("没有写入：连接不上打印机", "Not written: can't reach the printer"),
                       explain(["offline"], nil), important: true)
            } else {
                notify(L("没有写入标签规格", "Label settings not written"), message, important: true)
            }
        case "connect_failed":
            connecting = false
            densityTestPending = false
            log("connect failed")
            notify(L("连接不上打印机", "Can't reach the printer"), explain(["offline"], nil), important: true)
        case "job_started":
            activeJobs.insert(job)
        case "job_done":
            activeJobs.remove(job)
            if heldJobs.removeValue(forKey: job) != nil {
                notify(L("已继续打印", "Printing resumed"), L("暂停的任务已经打印完成。", "The held job has printed."))
            }
            log("job \(job) done")
        case "job_held":
            activeJobs.remove(job)
            if heldJobs[job] != why {
                heldJobs[job] = why
                log("job \(job) held: \(why) \(e["message"] as? String ?? "")")
                notify(L("打印已暂停", "Printing paused"),
                       explain(why, e["message"] as? String) + L(" 处理好后会自动继续。", " It will resume once fixed."),
                       important: true)
            }
        case "job_failed":
            activeJobs.remove(job)
            heldJobs.removeValue(forKey: job)
            log("job \(job) failed: \(why) \(e["message"] as? String ?? "")")
            notify(L("打印失败", "Print failed"), explain(why, e["message"] as? String), important: true)
        default:
            break
        }
        updateIcon()
    }

    /// A sentence a person can act on, for a set of IPP reason keywords.
    private func explain(_ why: [String], _ message: String?) -> String {
        if why.contains("offline") {
            return L("请确认打印机已开机，并且没有被手机或其他电脑占用。",
                     "Check that the printer is on and not connected to a phone or another computer.")
        }
        if why.contains("cover-open") {
            return L("打印机上盖没有合上。", "The printer cover is open.")
        }
        if why.contains("media-jam") {
            return L("标签没装到位或卡纸：请把标签头拉出切刀外，再合上盖子。",
                     "Label not seated or jammed: pull the label's edge out past the cutter and close the cover.")
        }
        if why.contains("media-empty") {
            return L("标签用完了，或者没有安装标签。", "Out of labels, or no label roll installed.")
        }
        if why.contains("media-needed") {
            return L("色带用完了。", "The ribbon has run out.")
        }
        return message ?? L("未知错误，请查看日志。", "Unknown error — see the log.")
    }

    private var lockedExplanation: String {
        let size = media.map { "\($0.0)×\($0.1) mm" } ?? ""
        return L(
            "装的是带芯片的原装标签\(size.isEmpty ? "" : "（\(size)）")。打印机直接从芯片读取尺寸和间距，并用传感器定位每一张，所以这里改不了，也不需要改。换上不带芯片的标签后，就可以在这里设置。",
            "A genuine tagged roll\(size.isEmpty ? "" : " (\(size))") is loaded. The printer reads size and gap from the tag and finds each label with its sensor, so there is nothing to set here. Untagged stock can be set.")
    }

    private var problem: String? {
        if bluetoothDenied {
            return L("没有蓝牙权限，无法连接打印机", "No Bluetooth permission — can't reach the printer")
        }
        if let helperError { return helperError }
        if let held = heldJobs.values.first { return explain(held, nil) }
        let blocking = reasons.filter { $0 != "media-low" }
        return blocking.isEmpty ? nil : explain(blocking, nil)
    }

    // MARK: Notifications

    private func refreshNotificationStatus() {
        UNUserNotificationCenter.current().getNotificationSettings { settings in
            let ok = settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional
            let status = settings.authorizationStatus.rawValue
            DispatchQueue.main.async {
                if status != self.notificationStatusLogged {
                    self.notificationStatusLogged = status
                    log("notifications allowed=\(ok) (status \(status): 0 undetermined, 1 denied, 2 authorized)")
                }
                self.notificationsAllowed = ok
            }
        }
    }

    @objc private func openNotificationSettings() {
        NSWorkspace.shared.open(
            URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension")!)
    }

    private var alertShowing = false
    /// A density test asked for before the label size was known.
    private var densityTestPending = false
    private var notificationStatusLogged = -1

    /// The fallback when notifications are switched off for the app: a print
    /// that silently fails is the one thing this app exists to prevent, so
    /// say it in a window instead.
    private func alert(_ title: String, _ body: String) {
        guard !alertShowing else { return }
        alertShowing = true
        let a = NSAlert()
        a.messageText = title
        a.informativeText = body
        a.alertStyle = .warning
        a.addButton(withTitle: L("好", "OK"))
        a.addButton(withTitle: L("改用系统通知…", "Use notifications instead…"))
        NSApp.activate(ignoringOtherApps: true)
        if a.runModal() == .alertSecondButtonReturn {
            openNotificationSettings()
        }
        alertShowing = false
    }

    private func notify(_ title: String, _ body: String, important: Bool = false) {
        log("notify: \(title) — \(body)")
        guard notificationsAllowed else {
            if important { alert(title, body) }
            return
        }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        UNUserNotificationCenter.current().add(
            UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        ) { error in
            if let error { log("notification not delivered: \(error)") }
        }
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler done: @escaping (UNNotificationPresentationOptions) -> Void) {
        done([.banner, .sound])
    }

    // MARK: Status item

    private func updateIcon() {
        let symbol: String
        if problem != nil {
            symbol = "exclamationmark.triangle.fill"
        } else if connected {
            symbol = "printer.fill"
        } else {
            symbol = "printer"
        }
        let image = NSImage(systemSymbolName: symbol, accessibilityDescription: L("硕方打印机", "Supvan Printer"))
        image?.isTemplate = true
        statusItem.button?.image = image
        statusItem.button?.toolTip = stateLine
    }

    private var stateLine: String {
        if let problem { return problem }
        if !activeJobs.isEmpty { return L("正在打印…", "Printing…") }
        if connecting { return L("正在连接…", "Connecting…") }
        if connected { return L("已连接", "Connected") }
        if bluetoothProbe != nil {
            return L("等待允许使用蓝牙…", "Waiting for Bluetooth permission…")
        }
        if printers.isEmpty && helper != nil {
            return L("没有找到已配对的硕方打印机", "No paired Supvan printer found")
        }
        return L("未连接（打印时自动连接）", "Not connected (connects when you print)")
    }

    private func disabled(_ title: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.isEnabled = false
        return item
    }

    private func action(_ title: String, _ selector: Selector, checked: Bool = false) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: selector, keyEquivalent: "")
        item.target = self
        item.state = checked ? .on : .off
        return item
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        refreshQueue()
        refreshNotificationStatus()
        menu.removeAllItems()

        menu.addItem(disabled(printers.first.map { displayName($0) } ?? L("硕方标签打印机", "Supvan label printer")))
        menu.addItem(disabled(stateLine))
        if let (w, h) = media {
            var line = L("标签 \(w)×\(h) mm", "Label \(w)×\(h) mm")
            if rollLocked == false, let gap { line += L(" · 间距 \(gap) mm", " · gap \(gap) mm") }
            if let remaining { line += L(" · 剩余 \(remaining) 张", " · \(remaining) left") }
            menu.addItem(disabled(line))
        }
        menu.addItem(.separator())

        if bluetoothDenied {
            menu.addItem(action(L("打开隐私设置允许蓝牙…", "Open Privacy Settings to allow Bluetooth…"),
                                #selector(openBluetoothPrivacy)))
            menu.addItem(action(L("我已允许，重试", "I've allowed it — retry"), #selector(retryBluetooth)))
        } else if bluetoothProbe != nil {
            // Nothing to do until the prompt is answered.
        } else if printers.isEmpty {
            menu.addItem(action(L("打开蓝牙设置配对…", "Open Bluetooth Settings to pair…"), #selector(openBluetoothSettings)))
            menu.addItem(action(L("重新查找打印机", "Look for printers again"), #selector(restartService)))
        } else if connected {
            menu.addItem(action(L("断开连接", "Disconnect"), #selector(disconnect)))
        } else {
            let item = action(L("连接并刷新状态", "Connect and refresh"), #selector(connect))
            item.isEnabled = !connecting
            menu.addItem(item)
        }
        if !printers.isEmpty && !bluetoothDenied && bluetoothProbe == nil {
            menu.addItem(action(L("标签设置…", "Label settings…"), #selector(editLabel)))
        }

        let content = NSMenuItem(title: L("打印内容", "Content"), action: nil, keyEquivalent: "")
        let contentMenu = NSMenu()
        for (mode, title) in [
            ("threshold", L("文字和二维码（纯黑白，边缘清晰）", "Text and codes (plain black and white)")),
            ("bayer", L("照片（灰度抖动）", "Photos (halftone)")),
        ] {
            let item = action(title, #selector(chooseDither(_:)), checked: ditherMode == mode)
            item.representedObject = mode
            contentMenu.addItem(item)
        }
        content.submenu = contentMenu
        menu.addItem(content)

        let dark = NSMenuItem(title: L("打印浓度", "Density"), action: nil, keyEquivalent: "")
        let darkMenu = NSMenu()
        for value in 1...9 {
            var title = String(value)
            switch value {
            case 1: title += L("（最浅）", " (lightest)")
            case 4: title += L("（硕方默认）", " (vendor default)")
            case 9: title += L("（最深）", " (darkest)")
            default: break
            }
            let item = action(title, #selector(chooseDarkness(_:)), checked: density == value)
            item.representedObject = value
            darkMenu.addItem(item)
        }
        darkMenu.addItem(.separator())
        darkMenu.addItem(action(L("打印浓度对比（用 \(testDensities.count) 张标签）", "Print a density comparison (\(testDensities.count) labels)"),
                                #selector(testDensity)))
        dark.submenu = darkMenu
        menu.addItem(dark)
        menu.addItem(.separator())

        if let queueName {
            menu.addItem(disabled(L("系统打印机：\(queueName)", "System printer: \(queueName)")))
        } else if !printers.isEmpty {
            menu.addItem(action(L("添加到系统打印机", "Add to system printers"), #selector(addQueue)))
        }
        if !notificationsAllowed {
            menu.addItem(action(L("允许通知（出错时提醒）…", "Allow notifications (for print errors)…"),
                                #selector(openNotificationSettings)))
        }
        menu.addItem(action(L("登录时启动", "Open at login"), #selector(toggleLoginItem),
                            checked: SMAppService.mainApp.status == .enabled))
        menu.addItem(action(L("查看上次打印的点阵图", "Show the last print as sent"), #selector(showLastPrint)))
        menu.addItem(action(L("查看日志", "Show log"), #selector(showLog)))
        menu.addItem(.separator())
        menu.addItem(action(L("退出", "Quit"), #selector(quit)))
    }

    /// "Supvan T50 Pro T0200A2501010001" → "Supvan T50 Pro".
    private func displayName(_ p: PrinterEntry) -> String {
        var words = p.info.split(separator: " ").map(String.init)
        if let last = words.last, last.count >= 12, last.allSatisfy({ $0.isLetter || $0.isNumber }) {
            words.removeLast()
        }
        return words.joined(separator: " ")
    }

    // MARK: Actions

    @objc private func connect() {
        connecting = true
        updateIcon()
        send("connect")
    }

    @objc private func disconnect() {
        send("disconnect")
    }

    /// Width, length and gap of the loaded label, for stock without a tag —
    /// what the vendor app's custom-label screen does.
    @objc private func editLabel() {
        NSApp.activate(ignoringOtherApps: true)
        if rollLocked == true {
            let a = NSAlert()
            a.messageText = L("这卷标签不需要设置", "Nothing to set for this roll")
            a.informativeText = lockedExplanation
            a.addButton(withTitle: L("好", "OK"))
            a.runModal()
            return
        }

        func field(_ value: Int) -> NSTextField {
            let f = NSTextField(string: String(value))
            f.alignment = .right
            f.widthAnchor.constraint(equalToConstant: 64).isActive = true
            return f
        }
        let width = field(media?.0 ?? 40)
        let length = field(media?.1 ?? 30)
        let gapField = field(gap ?? 3)
        let grid = NSGridView(views: [
            [NSTextField(labelWithString: L("宽度（最大 50）", "Width (max 50)")), width, NSTextField(labelWithString: "mm")],
            [NSTextField(labelWithString: L("长度（最大 120）", "Length (max 120)")), length, NSTextField(labelWithString: "mm")],
            [NSTextField(labelWithString: L("间距", "Gap")), gapField, NSTextField(labelWithString: "mm")],
        ])
        grid.column(at: 0).xPlacement = .trailing
        grid.rowSpacing = 8
        grid.columnSpacing = 8
        grid.frame = NSRect(origin: .zero, size: grid.fittingSize)

        let a = NSAlert()
        a.messageText = L("标签设置", "Label settings")
        a.informativeText = L(
            "按实际装的标签填写。写入后保存在打印机里，系统打印机的默认纸张也会跟着变。带芯片的原装标签由打印机自动识别，不能在这里改。",
            "Enter the stock actually loaded. The values are stored in the printer and become the system printer's default paper size. A genuine tagged roll identifies itself and can't be changed here.")
        a.accessoryView = grid
        a.addButton(withTitle: L("写入打印机", "Write to printer"))
        a.addButton(withTitle: L("取消", "Cancel"))
        a.window.initialFirstResponder = width
        guard a.runModal() == .alertFirstButtonReturn else { return }

        guard let w = Int(width.stringValue.trimmingCharacters(in: .whitespaces)), (1...50).contains(w),
              let h = Int(length.stringValue.trimmingCharacters(in: .whitespaces)), (1...120).contains(h),
              let g = Int(gapField.stringValue.trimmingCharacters(in: .whitespaces)), (0...30).contains(g)
        else {
            let bad = NSAlert()
            bad.messageText = L("数值不对", "Those values won't work")
            bad.informativeText = L("宽度 1–50，长度 1–120，间距 0–30，都是整数毫米。",
                                    "Width 1–50, length 1–120, gap 0–30, whole millimetres.")
            bad.runModal()
            return
        }
        log("set_label \(w) \(h) \(g)")
        connecting = !connected
        updateIcon()
        send("set_label \(w) \(h) \(g)")
    }

    /// One label per entry in `testDensities`, each printed wholly at that
    /// density and saying which it is.
    @objc private func testDensity() {
        connecting = !connected
        updateIcon()
        // The pages are drawn to the loaded label, so that has to be known
        // first: if the printer has not been read yet, read it and come back.
        guard let (w, h) = media else {
            log("density test: label size unknown, connecting first")
            densityTestPending = true
            send("connect")
            return
        }
        densityTestPending = false
        for density in testDensities {
            let page = jobsURL.appendingPathComponent("density-test-\(density).pgm")
            guard let pgm = densityTestPGM(widthMM: w, heightMM: h, density: density),
                  (try? pgm.write(to: page)) != nil
            else {
                log("density test: could not draw \(w)x\(h)mm at \(density)")
                continue
            }
            log("print_pgm \(density) \(w)x\(h)mm")
            send("print_pgm \(density) \(page.path)")
        }
    }

    @objc private func chooseDither(_ sender: NSMenuItem) {
        guard let mode = sender.representedObject as? String else { return }
        ditherMode = mode
        log("dither mode -> \(mode)")
        send("set_dither \(mode)")
    }

    @objc private func chooseDarkness(_ sender: NSMenuItem) {
        guard let value = sender.representedObject as? Int else { return }
        density = value
        log("density -> \(value) (darkness \(darknessForDensity))")
        send("set_darkness \(darknessForDensity)")
    }

    /// Open the dots of the most recent job, exactly as they went to the
    /// printer — the quickest way to tell a layout problem from a print one.
    @objc private func showLastPrint() {
        let fm = FileManager.default
        let pages = ((try? fm.contentsOfDirectory(at: jobsURL, includingPropertiesForKeys: [.contentModificationDateKey])) ?? [])
            .filter { $0.pathExtension == "pbm" && !$0.lastPathComponent.contains("printhead") }
            .sorted {
                let a = (try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
                let b = (try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
                return a < b
            }
        guard let last = pages.last, let png = pngFromMirroredPBM(last) else {
            NSApp.activate(ignoringOtherApps: true)
            let a = NSAlert()
            a.messageText = L("还没有可看的打印", "Nothing printed yet")
            a.informativeText = L("这次启动以来还没有打印过。打印一张之后再来看。",
                                  "Nothing has been printed since the app started.")
            a.runModal()
            return
        }
        let out = jobsURL.appendingPathComponent("last-print.png")
        do {
            try png.write(to: out)
            NSWorkspace.shared.open(out)
        } catch {
            log("could not write \(out.path): \(error)")
        }
    }

    @objc private func restartService() {
        restarts = 0
        if let helper, helper.isRunning {
            send("quit") // helperExited restarts it
        } else {
            startHelper()
        }
    }

    @objc private func retryBluetooth() {
        ensureBluetoothThenStart()
    }

    @objc private func openBluetoothSettings() {
        NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.BluetoothSettings")!)
    }

    @objc private func showLog() {
        NSWorkspace.shared.open(logURL)
    }

    @objc private func toggleLoginItem() {
        do {
            if SMAppService.mainApp.status == .enabled {
                try SMAppService.mainApp.unregister()
            } else {
                try SMAppService.mainApp.register()
            }
            log("login item status=\(SMAppService.mainApp.status.rawValue)")
        } catch {
            log("login item change failed: \(error)")
            notify(L("无法更改登录启动", "Couldn't change the login item"), error.localizedDescription)
        }
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }

    // MARK: CUPS queue

    /// The CUPS queue that points at our service, if there is one.
    private func refreshQueue() {
        guard let printer = printers.first else {
            queueName = nil
            return
        }
        // `lpstat -e` (names only) and `lpoptions` (key=value) are the two
        // CUPS listings that are not translated; `lpstat -v` is, whatever
        // LANG says, so it cannot be parsed.
        let suffix = ":\(port)/ipp/print/\(printer.name)"
        queueName = nil
        let (_, names) = run("/usr/bin/lpstat", ["-e"])
        for name in names.split(separator: "\n").map(String.init) {
            let (_, options) = run("/usr/bin/lpoptions", ["-p", name])
            guard let uri = options.split(separator: " ").first(where: { $0.hasPrefix("device-uri=") }) else {
                continue
            }
            if uri.hasSuffix(suffix) && (uri.contains("//localhost:") || uri.contains("//127.0.0.1:")) {
                queueName = name
                return
            }
        }
    }

    /// First launch with a printer and no queue: create it, so "print from any
    /// app" works without a trip to System Settings. Only ever once — if the
    /// user removes the queue later, that is their call.
    private func autoAddQueueOnce() {
        let key = "didAutoAddQueue"
        guard queueName == nil, !printers.isEmpty, !UserDefaults.standard.bool(forKey: key) else { return }
        UserDefaults.standard.set(true, forKey: key)
        addQueue()
    }

    @objc private func addQueue() {
        guard let printer = printers.first else { return }
        let title = displayName(printer)
        DispatchQueue.global().async {
            // `-m everywhere` asks our own IPP service for its capabilities; the
            // printer itself need not be on.
            let (status, out) = run("/usr/sbin/lpadmin", [
                "-p", printer.name, "-E", "-v", printer.uri, "-m", "everywhere",
                "-D", title, "-o", "printer-is-shared=false",
            ])
            DispatchQueue.main.async {
                log("lpadmin status=\(status) \(out.trimmingCharacters(in: .whitespacesAndNewlines))")
                self.refreshQueue()
                if status == 0 {
                    self.notify(L("已添加系统打印机", "Printer added"),
                                L("现在可以在任何应用的打印对话框里选择“\(title)”。",
                                  "Choose “\(title)” in any app's Print dialog."))
                } else {
                    self.notify(L("添加系统打印机失败", "Couldn't add the printer"), out)
                }
            }
        }
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
