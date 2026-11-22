import Foundation
import UIKit

/// The Rust symbol above, from the engine the app links: `horizon-ios-launcher`'s
/// `horizon_thermal_state`.
///
/// `thermalState` is the one heat signal iOS gives an app, and it arrives as a notification, which
/// only UIKit code can hear. Nothing ever reported it before: the governor in `game/thermal.rs` read
/// an environment variable that nothing sets on a phone, so it was told "nominal" always — the
/// device log carried "thermal nominal" 131 times on a phone that was hot, and heat never once took
/// quality away. This is the missing half.
private let horizonThermalState: @convention(c) (Int32) -> Void = {
    guard let symbol = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "horizon_thermal_state") else {
        return { _ in }
    }
    return unsafeBitCast(symbol, to: (@convention(c) (Int32) -> Void).self)
}()

/// Reports `ProcessInfo.thermalState` now and on every change, for the life of the app.
enum Thermal {
    private static var started = false

    static func watch() {
        guard !started else { return }
        started = true
        report()
        NotificationCenter.default.addObserver(
            forName: ProcessInfo.thermalStateDidChangeNotification,
            object: nil,
            queue: nil
        ) { _ in report() }
    }

    /// Apple's own scale (0 nominal, 1 fair, 2 serious, 3 critical) handed straight to the engine.
    static func report() {
        horizonThermalState(Int32(ProcessInfo.processInfo.thermalState.rawValue))
    }
}
import Foundation

/// How the game answers: the context the launcher passed, and the answer as NUL-terminated UTF-8 JSON that
/// lives only for the call.
public typealias HorizonDone = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void
/// How the launcher asks: one line of the game's control language (docs/horizon-ctl-commands.md), a context,
/// and where the answer goes. `horizon-ios-launcher` hands this over with `horizon_launcher_present`.
public typealias HorizonSubmit = @convention(c) (UnsafePointer<CChar>?, UnsafeMutableRawPointer?, HorizonDone) -> Void

/// The launcher's line to the game under it (crates/horizon-ios-launcher, crates/horizon-client/src/game/launcher.rs).
/// The game reads the line on its next frame, on this same main thread, so nothing here blocks: the answer
/// arrives through the callback, which resumes the waiting task.
enum Bridge {
    /// Set by `horizon_launcher_present`; nil in a preview, where nothing is under the launcher.
    static var submit: HorizonSubmit?

    /// Send `line` to the game and wait for its JSON answer. A game that cannot answer gives `["error": ...]`.
    @MainActor
    static func send(_ line: String) async -> [String: Any] {
        let verb = String(line.prefix(while: { !$0.isWhitespace }))
        let noisyPoll = verb == "launcher-state"
            || verb == "download-state"
            || (verb == "launcher-versions" && (line.contains("\"job\"") || line.contains("\"list\"")))
        if !noisyPoll { Diagnostics.record("Launcher request: \(verb)") }
        guard let submit else {
            return ["error": "no game is running under the launcher"]
        }
        return await withCheckedContinuation { continuation in
            let context = Unmanaged.passRetained(Reply(continuation)).toOpaque()
            line.withCString { submit($0, context, replied) }
        }
    }

    /// `launcher-world` with its arguments as JSON, so a folder with spaces in its name arrives whole.
    @MainActor
    static func world(_ action: String, folder: String, name: String? = nil) async -> [String: Any] {
        var args: [String: String] = ["action": action, "folder": folder]
        args["name"] = name
        guard let data = try? JSONSerialization.data(withJSONObject: args),
              let json = String(data: data, encoding: .utf8)
        else {
            return ["error": "unreadable world name"]
        }
        return await send("launcher-world \(json)")
    }
}

private final class Reply {
    let continuation: CheckedContinuation<[String: Any], Never>

    init(_ continuation: CheckedContinuation<[String: Any], Never>) {
        self.continuation = continuation
    }
}

/// The game calls this once per line it was sent; the context is the retained `Reply`, released here.
private let replied: HorizonDone = { context, text in
    guard let context else { return }
    let reply = Unmanaged<Reply>.fromOpaque(context).takeRetainedValue()
    let answer = text
        .flatMap { String(cString: $0).data(using: .utf8) }
        .flatMap { (try? JSONSerialization.jsonObject(with: $0)) as? [String: Any] }
    if let error = answer?["error"] as? String { Diagnostics.record("Launcher error: \(error)") }
    reply.continuation.resume(returning: answer ?? ["error": "the game's answer was not JSON"])
}

extension Dictionary where Key == String, Value == Any {
    /// The answer's error, when it has one.
    var error: String? { self["error"] as? String }
}
