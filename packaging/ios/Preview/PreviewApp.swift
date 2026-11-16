import SwiftUI
import UIKit

#if ORIENTATION_FIXTURE
@main
struct OrientationFixtureEntry {
    static func main() {
        UIApplicationMain(CommandLine.argc, CommandLine.unsafeArgv, nil,
                          NSStringFromClass(OrientationFixtureAppDelegate.self))
    }
}
#endif

/// The preview mirrors owned startup decisions rather than letting a generic
/// success reply conceal a broken launcher-start/commit/cancel command.
private enum PreviewEngine {
    static var phase: String?
    static var owner: String?
    static var committed = false
    static var activeVersion = "26.3"
    static var olderServers = true
    static var mods: [[String: Any]] = [
        [
            "id": "copper-lamps",
            "name": "Copper Lamps",
            "version": "1.0.2",
            "description": "Decorative oxidized copper lighting blocks.",
            "authors": ["Horizon Team"],
            "enabled": true,
            "format": "hmod",
            "origin": "horizon",
            "loader": "horizon",
            "verdict": "verified",
            "summary": "Official signed Horizon content mod.",
            "ports": ["4 blocks", "4 items", "4 recipes"],
            "skipped": [],
            "bytes": 48_120,
        ],
    ]

    static func reset() {
        phase = nil; owner = nil; committed = false
        activeVersion = "26.3"; olderServers = true
    }
    static var currentPhase: String {
        phase ?? (ProcessInfo.processInfo.environment["PREVIEW_MISSING_FILES"] == "1" ? "waiting" : "title")
    }
    static func reply(_ line: String) -> [String: Any]? {
        let words = line.split(separator: " ", maxSplits: 1)
        guard let command = words.first else { return nil }
        let id = words.count == 2 ? String(words[1]) : nil
        switch command {
        case "launcher-state": return ["phase": currentPhase, "version": activeVersion]
        case "launcher-versions":
            let payload = (id?.data(using: .utf8)).flatMap {
                try? JSONSerialization.jsonObject(with: $0) as? [String: Any]
            } ?? [:]
            let action = payload["action"] as? String ?? "list"
            switch action {
            case "select":
                if let selected = payload["id"] as? String, !selected.isEmpty { activeVersion = selected }
            case "older-servers":
                olderServers = payload["enabled"] as? Bool ?? true
            case "catalog":
                return [
                    "mirrors": [
                        [
                            "id": "community-26-3",
                            "label": "Prepared 26.3 (Community Mirror)",
                            "version": "26.3",
                            "url": "https://mirror.example/horizon/26.3.zip",
                            "notes": "Full prepared 26.3 runtime & assets.",
                            "installed": true,
                        ],
                    ],
                ]
            case "mojang":
                return [
                    "latest_release": "26.3",
                    "latest_snapshot": "26.4-snapshot-8",
                    "has_simulation_base": true,
                    "total": 3,
                    "versions": [
                        ["id": "26.3", "type": "release", "release_time": "2026-09-15T10:00:00Z", "protocol": 777, "support_tier": "native-26.3", "installed": true, "ready": true, "active": activeVersion == "26.3"],
                        ["id": "1.21.4", "type": "release", "release_time": "2024-12-03T10:00:00Z", "protocol": 769, "support_tier": "protocol-adapter", "installed": true, "ready": true, "active": activeVersion == "1.21.4"],
                        ["id": "1.12.2", "type": "release", "release_time": "2017-09-18T08:39:46Z", "support_tier": "world-datafix-assets", "installed": false, "ready": false, "active": false],
                    ],
                ]
            case "job":
                return ["active": false, "stage": "idle", "done": false]
            default: break
            }
            return [
                "active": activeVersion,
                "older_servers": olderServers,
                "has_simulation_base": true,
                "versions": [
                    ["id": "26.3", "release_type": "release", "source": "horizon-prepared", "protocol": 777, "support_tier": "native-26.3", "full_simulation": true, "ready": true, "active": activeVersion == "26.3", "files": 4200, "bytes": 620_000_000, "notes": "Full prepared Horizon runtime"],
                    ["id": "1.21.4", "release_type": "release", "source": "mojang-official", "protocol": 769, "support_tier": "protocol-adapter", "full_simulation": true, "ready": true, "active": activeVersion == "1.21.4", "files": 3950, "bytes": 540_000_000, "notes": "Mojang 1.21.4 + Wire Adapter"],
                ],
                "job": ["active": false, "stage": "idle", "done": false],
            ]
        case "launcher-mods":
            let payload = (id?.data(using: .utf8)).flatMap {
                try? JSONSerialization.jsonObject(with: $0) as? [String: Any]
            } ?? [:]
            let action = payload["action"] as? String ?? "list"
            switch action {
            case "toggle":
                let target = payload["id"] as? String ?? ""
                let enabled = payload["enabled"] as? Bool ?? false
                mods = mods.map { item in
                    guard item["id"] as? String == target else { return item }
                    var copy = item; copy["enabled"] = enabled; return copy
                }
                return ["ok": true, "mods": mods]
            case "delete":
                let target = payload["id"] as? String ?? ""
                mods.removeAll { $0["id"] as? String == target }
                return ["ok": true, "mods": mods]
            case "horizon-browse":
                return [
                    "hits": [
                        ["id": "waystones-lite", "title": "Waystones Lite", "summary": "Craftable teleportation obelisks.", "authors": ["Horizon"], "categories": ["utility"], "version": "0.4.1", "downloads": 1840, "update_available": false],
                    ],
                ]
            case "modrinth-search":
                return [
                    "hits": [
                        ["project_id": "terralith", "slug": "terralith", "title": "Terralith", "description": "Overworld worldgen overhaul with 95+ biomes.", "author": "Stardust Labs", "downloads": 42_000_000, "categories": ["datapack", "worldgen"], "versions": ["1.21.4", "26.3"], "compatibility_hint": "high"],
                    ],
                ]
            default:
                return ["mods": mods]
            }
        case "launcher-start":
            owner = id
            committed = committed || currentPhase == "world"
            if currentPhase == "waiting" { phase = "loading" }
            return ["ok": true, "phase": currentPhase]
        case "launcher-commit-start":
            guard let id, owner == id, currentPhase != "waiting" else {
                return ["ok": false, "error": "Start no longer active"]
            }
            committed = true
            return ["ok": true, "status": "committed"]
        case "launcher-cancel-start":
            guard let id else { return ["ok": false, "error": "Missing start owner"] }
            guard owner == id else { return ["ok": true, "status": "stale"] }
            guard !committed else { return ["ok": true, "status": "committed"] }
            phase = "waiting"
            owner = nil
            committed = false
            return ["ok": true, "status": "stopped"]
        case "launcher-visible":
            if id == "false", currentPhase != "waiting" { committed = true }
            return ["ok": true]
        default: return nil
        }
    }
}

private var finishPreviewImport: (() -> Void)?

private let previewSubmit: HorizonSubmit = { pointer, context, done in
    let line = pointer.map { String(cString: $0) } ?? ""
#if ORIENTATION_FIXTURE
    if ProcessInfo.processInfo.environment["PREVIEW_GAME_PRESENTATION"] == "1",
       line.hasPrefix("launcher-visible ") {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            orientationFixtureRoot?.launcherVisible = line.hasSuffix(" true")
            orientationFixtureRoot?.setNeedsUpdateOfSupportedInterfaceOrientations()
            "{\"ok\":true}".withCString { done(context, $0) }
        }
        return
    }
#endif
    let response: [String: Any]
    if let engine = PreviewEngine.reply(line) { response = engine }
    else if let options = PreviewOptions.reply(line) { response = options }
    else { switch line {
    case "launcher-state": response = ["phase": ProcessInfo.processInfo.environment["PREVIEW_MISSING_FILES"] == "1" ? "waiting" : "title", "version": "26.3"]
    case "game-files-source":
        if let url = ProcessInfo.processInfo.environment["PREVIEW_GAME_FILES_URL"] {
            response = ["url": url, "version": "26.3"]
        } else {
            response = ["error": "Automatic downloads are disabled in the preview."]
        }
    case _ where line.hasPrefix("import-game-files "):
        if ProcessInfo.processInfo.environment["PREVIEW_MISSING_FILES"] == "1",
           let bytes = try? Data(contentsOf: URL(fileURLWithPath: String(line.dropFirst("import-game-files ".count)))),
           bytes == Data("horizon-download-fixture".utf8) {
            try? FileManager.default.createDirectory(at: GameFolder.resources, withIntermediateDirectories: true)
            try? Data("{}".utf8).write(to: GameFolder.resources.appendingPathComponent("horizon-resources.json"))
            if ProcessInfo.processInfo.environment["PREVIEW_HOLD_IMPORT"] == "1" {
                finishPreviewImport = {
                    finishPreviewImport = nil
                    #"{"ok":true,"kind":"resources"}"#.withCString { done(context, $0) }
                }
                return
            }
            response = ["ok": true, "kind": "resources"]
        } else { response = ["error": "Unexpected preview fixture."] }
    case "worlds":
        response = ["worlds": [
            ["folder": "meadow", "name": "Meadow Valley", "mode": "survival", "version": "26.3", "last_played": Date().timeIntervalSince1970 * 1000 - 3600000],
            ["folder": "creative", "name": "Creative Workshop", "mode": "creative", "version": "26.3", "last_played": Date().timeIntervalSince1970 * 1000 - 86400000],
            ["folder": "islands", "name": "The Islands", "mode": "survival", "version": "26.3", "last_played": Date().timeIntervalSince1970 * 1000 - 172800000]
        ]]
    case "servers": response = ["servers": [["index": 0, "name": "Hypixel", "address": "mc.hypixel.net"]]]
    case "server-ping mc.hypixel.net": response = ["motd": "§6§lHypixel Network §8[§c1.8 / 26.3§8]\n§f§lSKYBLOCK §r§dHalloween Event", "online": 9813, "max": 200000, "version": "Java Edition", "ms": 79]
    default: response = ["ok": true]
    }
    }
    let data = try! JSONSerialization.data(withJSONObject: response)
    String(data: data, encoding: .utf8)!.withCString { done(context, $0) }
}

#if !ORIENTATION_FIXTURE
@main
#endif
struct LauncherPreview: App {
    @State private var previewNotice = false
    init() {
        PreviewEngine.reset()
        for key in ["HORIZON_HOME", "HORIZON_LAUNCHER_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME",
                    "HORIZON_LAUNCHER_TRASH", "HORIZON_LAUNCHER_CLIPBOARD", "HORIZON_LAUNCHER_DESKTOP"] {
            if let value = ProcessInfo.processInfo.environment[key] {
                try? FileManager.default.createDirectory(atPath: value, withIntermediateDirectories: true)
            }
        }
        if ProcessInfo.processInfo.environment["PREVIEW_MISSING_FILES"] == "1" {
            try? FileManager.default.removeItem(at: GameFolder.resources)
            try? FileManager.default.removeItem(at: GameFolder.versions)
        }
        Theme.registerFonts()
        Bridge.submit = previewSubmit
        if let signed = ProcessInfo.processInfo.environment["PREVIEW_SIGNED_IN"] {
            let profiles = signed == "1" ? [
                MicrosoftAccount.Profile(name: "SkyExplorer", uuid: "00000000000000000000000000000000"),
                MicrosoftAccount.Profile(name: "BuilderTwo", uuid: "22222222222222222222222222222222"),
            ] : []
            let catalog = MicrosoftAccount.Catalog(profiles: profiles, selected: profiles.first?.uuid)
            UserDefaults.standard.set(try! JSONEncoder().encode(catalog), forKey: "microsoftAccounts.v2")
            UserDefaults.standard.removeObject(forKey: "microsoftProfile")
        }
        if ProcessInfo.processInfo.environment["PREVIEW_SIGNED_IN"] != nil {
            try? "name:OfflinePlayer\n".write(to: GameFolder.documents.appendingPathComponent("horizon-player.txt"), atomically: true, encoding: .utf8)
        }
    }
    var body: some Scene {
        WindowGroup {
            if ProcessInfo.processInfo.environment["PREVIEW_NATIVE_GESTURES"] != nil {
                NativeGestureFixture().ignoresSafeArea().persistentSystemOverlays(.hidden)
            } else {
            HomeView(onPlay: { previewNotice = true })
                .alert("Launcher UI preview", isPresented: $previewNotice) {
                    Button("OK", role: .cancel) {}
                } message: { Text("The worlds and server entry here are sample data for reviewing the interface. This preview does not start the game.") }
                .frame(width: ProcessInfo.processInfo.environment["PREVIEW_WIDTH"].flatMap(Double.init).map { CGFloat($0) })
                .overlay(alignment: .bottomLeading) {
                    if ProcessInfo.processInfo.environment["PREVIEW_HOLD_IMPORT"] == "1" {
                        Button("Finish fixture import") { finishPreviewImport?() }
                            .accessibilityIdentifier("fixture-finish-import")
                            .padding(.leading, 20).padding(.bottom, 110)
                    }
                }
            }
        }
    }
}

private struct NativeGestureFixture: UIViewControllerRepresentable {
    func makeUIViewController(context: Context) -> NativeGestureFixtureController { NativeGestureFixtureController() }
    func updateUIViewController(_ uiViewController: NativeGestureFixtureController, context: Context) {}
}
private final class NativeGestureFixtureController: UIViewController {
    private var link: CADisplayLink?
    private let output = UILabel()
    private let target = UILabel()
    private var events: [String] = []
    override var prefersHomeIndicatorAutoHidden: Bool { true }
    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black; view.accessibilityIdentifier = "native-gesture-canvas"
        output.textColor = .white; output.numberOfLines = 0
        output.accessibilityIdentifier = "native-gesture-events"; output.text = "ready"
        target.text = "Hotbar"; target.textAlignment = .center; target.textColor = .yellow
        target.accessibilityIdentifier = "native-gesture-hotbar"
        view.addSubview(output); view.addSubview(target)
        TouchPreferences.shared.save(TouchConfiguration())
    }
    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        turn(view.window?.windowScene,to:.landscape)
        TouchGestureBridge.shared.attach(to: view)
        link = CADisplayLink(target: self, selector: #selector(frame))
        link?.add(to: .main, forMode: .common)
    }
    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated); link?.invalidate(); link = nil
    }
    @objc private func frame() {
        guard view.bounds.width > 0 else { return }
        let displayScale = view.window?.screen.scale ?? UIScreen.main.scale
        let geometry = TouchEditorGeometry(size: view.bounds.size, displayScale: displayScale, limit: 0)
        output.frame = CGRect(x: view.bounds.width * 0.35, y: 50, width: view.bounds.width * 0.55, height: 120)
        target.frame = CGRect(x: view.bounds.width / 2 - 40, y: view.bounds.height - 20 * geometry.pointsPerUnit, width: 80, height: 20 * geometry.pointsPerUnit)
        var buffer = [Double](repeating: -999, count: 6 * 64)
        let flags: UInt32 = ProcessInfo.processInfo.environment["PREVIEW_NATIVE_GESTURES"] == "world" ? 1 : 0
        let count = buffer.withUnsafeMutableBufferPointer { bytes in
            horizonTouchGestures(1, flags, geometry.gui.width, geometry.gui.height,
                                 geometry.pointsPerUnit * displayScale, bytes.baseAddress, UInt32(bytes.count))
        }
        for index in 0..<Int(count) {
            let offset = index * 6
            events.append("\(Int(buffer[offset])):\(Int(buffer[offset + 1])):\(Int(buffer[offset + 4]))")
        }
        if !events.isEmpty {
            output.text = events.suffix(20).joined(separator: " ")
            output.accessibilityLabel = events.joined(separator: " ")
        }
    }
}

#if ORIENTATION_FIXTURE
// UIKit fixture uses the production presenter, unlike the SwiftUI-only preview.
private weak var orientationFixtureRoot: OrientationFixtureGameController?

private final class OrientationFixtureAppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        Theme.registerFonts()
        Bridge.submit = previewSubmit
        UserDefaults.standard.removeObject(forKey: "microsoftProfile")
        return true
    }
    func application(_ application: UIApplication, configurationForConnecting session: UISceneSession,
                     options: UIScene.ConnectionOptions) -> UISceneConfiguration {
        let config = UISceneConfiguration(name: "Orientation fixture", sessionRole: session.role)
        config.delegateClass = OrientationFixtureSceneDelegate.self
        return config
    }
}
private final class OrientationFixtureSceneDelegate: NSObject, UIWindowSceneDelegate {
    var window: UIWindow?
    func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
        guard let scene = scene as? UIWindowScene else { return }
        let game = OrientationFixtureGameController()
        orientationFixtureRoot = game
        let window = UIWindow(windowScene: scene)
        window.rootViewController = game
        self.window = window
        window.makeKeyAndVisible()
    }
}
private final class OrientationFixtureGameController: UIViewController {
    var launcherVisible = false
    private var firstAppearance = true
    override var supportedInterfaceOrientations: UIInterfaceOrientationMask {
        launcherVisible ? .allButUpsideDown : .landscape
    }
    override var preferredInterfaceOrientationForPresentation: UIInterfaceOrientation { .landscapeRight }
    override func present(_ controller: UIViewController, animated: Bool, completion: (() -> Void)? = nil) {
        controller.view.accessibilityIdentifier = launcherVisible
            ? "launcher-presentation-permission-ready" : "launcher-presentation-before-permission"
        super.present(controller, animated: animated, completion: completion)
    }
    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        let label = UILabel()
        label.text = "Game fixture"
        label.accessibilityIdentifier = "orientation-game-root"
        label.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(label)
        NSLayoutConstraint.activate([label.centerXAnchor.constraint(equalTo: view.centerXAnchor),
                                     label.centerYAnchor.constraint(equalTo: view.centerYAnchor)])
    }
    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        guard firstAppearance else { return }
        firstAppearance = false
        turn(view.window?.windowScene, to: .landscape)
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
            _ = horizonLauncherPresent(previewSubmit)
        }
    }
}

#endif
