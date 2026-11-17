import XCTest
import UIKit
@testable import LauncherPreview

final class VersionsAndModsTests: XCTestCase {
    func testHasGameFilesChecksBothLegacyResourcesAndManagedVersions() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }

        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        XCTAssertFalse(GameFolder.hasGameFiles(in: root))

        let verResources = root.appendingPathComponent("versions/1.21.4/resources")
        try FileManager.default.createDirectory(at: verResources, withIntermediateDirectories: true)
        try Data("{}".utf8).write(to: verResources.appendingPathComponent("horizon-resources.json"))
        XCTAssertTrue(GameFolder.hasGameFiles(in: root))

        try FileManager.default.removeItem(at: root.appendingPathComponent("versions"))
        XCTAssertFalse(GameFolder.hasGameFiles(in: root))

        let legacy = root.appendingPathComponent("resources")
        try FileManager.default.createDirectory(at: legacy, withIntermediateDirectories: true)
        try Data("{}".utf8).write(to: legacy.appendingPathComponent("horizon-resources.json"))
        XCTAssertTrue(GameFolder.hasGameFiles(in: root))

        try FileManager.default.removeItem(at: legacy)
        let hiddenBase = root.appendingPathComponent("versions/.sim-base-26.3/resources")
        try FileManager.default.createDirectory(at: hiddenBase, withIntermediateDirectories: true)
        try Data(#"{"schema":7,"full_simulation":true}"#.utf8)
            .write(to: hiddenBase.appendingPathComponent("horizon-resources.json"))
        XCTAssertFalse(GameFolder.hasGameFiles(in: root), "Hidden .sim-base-26.3 must not count as a user version")

        let partial = root.appendingPathComponent("versions/26.3/resources")
        try FileManager.default.createDirectory(at: partial, withIntermediateDirectories: true)
        try Data(#"{"schema":7,"full_simulation":false}"#.utf8)
            .write(to: partial.appendingPathComponent("horizon-resources.json"))
        XCTAssertFalse(GameFolder.hasGameFiles(in: root), "Partial Mojang install must require sim-base completion")

        try Data(#"{"schema":7,"full_simulation":true}"#.utf8)
            .write(to: partial.appendingPathComponent("horizon-resources.json"))
        XCTAssertTrue(GameFolder.hasGameFiles(in: root))
    }

    @MainActor
    func testMojangPlusSimBaseModeRepairsPartialAndDownloadsExplicitly() async {
        let marker = GameFolder.resources.appendingPathComponent("horizon-resources.json")
        let previous = try? Data(contentsOf: marker)
        try? FileManager.default.removeItem(at: marker)
        let versionsBackup = GameFolder.documents.appendingPathComponent("versions.test-backup-\(UUID().uuidString)")
        let hadVersions = FileManager.default.fileExists(atPath: GameFolder.versions.path)
        if hadVersions { try? FileManager.default.moveItem(at: GameFolder.versions, to: versionsBackup) }
        defer {
            if let previous { try? previous.write(to: marker) }
            else { try? FileManager.default.removeItem(at: marker) }
            try? FileManager.default.removeItem(at: GameFolder.versions)
            if hadVersions { try? FileManager.default.moveItem(at: versionsBackup, to: GameFolder.versions) }
        }

        // 1. Fresh install (`partial_mojang == false`): `startAutomaticallyIfNeeded()` stays `.idle`,
        // while explicit `startAutomatic()` dispatches `download-mojang`.
        var commands: [String] = []
        let fresh = GameFilesDownload(
            automaticSource: { ["mode": "mojang_plus_sim_base", "version": "26.3", "partial_mojang": false] },
            versionBridge: { line in
                commands.append(line)
                if line.contains(#""action":"job""#) {
                    return ["active": false, "done": true, "version_id": "26.3"]
                }
                return ["ok": true]
            },
            diagnostic: { _ in }
        )
        await fresh.startAutomaticallyIfNeeded()
        XCTAssertEqual(fresh.state, .idle)
        XCTAssertTrue(commands.isEmpty)

        await fresh.startAutomatic()
        XCTAssertTrue(commands.first?.contains("download-mojang") == true)
        XCTAssertEqual(fresh.state, .done("Minecraft 26.3 is installed and ready to play."))

        // 2. Partial Mojang install (`partial_mojang == true`): `startAutomaticallyIfNeeded()`
        // automatically runs `repair` to fetch `sim-base-26.3.zip` (~45 MB).
        var repairCommands: [String] = []
        let partial = GameFilesDownload(
            automaticSource: { ["mode": "mojang_plus_sim_base", "version": "1.20.4", "partial_mojang": true] },
            versionBridge: { line in
                repairCommands.append(line)
                if line.contains(#""action":"job""#) {
                    return ["active": false, "done": true, "version_id": "1.20.4"]
                }
                return ["ok": true]
            },
            diagnostic: { _ in }
        )
        await partial.startAutomaticallyIfNeeded()
        XCTAssertTrue(repairCommands.first?.contains(#""action":"repair""#) == true)
        XCTAssertEqual(partial.state, .done("Minecraft 1.20.4 is installed and ready to play."))
    }

    @MainActor
    func testVersionStoreListsSwitchesAndBrowsesMojangCatalog() async {
        var selected = "26.3"
        let store = VersionStore { line in
            XCTAssertTrue(line.hasPrefix("launcher-versions "))
            let raw = String(line.dropFirst("launcher-versions ".count))
            let json = (try? JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any]) ?? [:]
            switch json["action"] as? String {
            case "select":
                selected = json["id"] as? String ?? selected
                return [
                    "active": selected,
                    "older_servers": true,
                    "has_simulation_base": true,
                    "versions": [
                        ["id": "26.3", "source": "horizon-prepared", "support_tier": "native-26.3", "ready": true, "active": selected == "26.3"],
                        ["id": "1.21.4", "source": "mojang-official", "support_tier": "protocol-adapter", "ready": true, "active": selected == "1.21.4"],
                    ],
                ]
            case "mojang":
                return [
                    "latest_release": "26.3",
                    "latest_snapshot": "26.4-snapshot-8",
                    "has_simulation_base": true,
                    "total": 2,
                    "versions": [
                        ["id": "26.3", "type": "release", "release_time": "2026-09-15T10:00:00Z", "protocol": 777, "support_tier": "native-26.3", "installed": true, "ready": true, "active": selected == "26.3"],
                        ["id": "1.12.2", "type": "release", "release_time": "2017-09-18T08:39:46Z", "support_tier": "world-datafix-assets", "installed": false, "ready": false, "active": false],
                    ],
                ]
            default:
                return [
                    "active": selected,
                    "older_servers": true,
                    "has_simulation_base": true,
                    "versions": [
                        ["id": "26.3", "source": "horizon-prepared", "support_tier": "native-26.3", "ready": true, "active": true],
                        ["id": "1.21.4", "source": "mojang-official", "support_tier": "protocol-adapter", "ready": true, "active": false],
                    ],
                ]
            }
        }

        await store.reloadInstalled()
        XCTAssertEqual(store.activeVersion, "26.3")
        XCTAssertEqual(store.installed.count, 2)

        await store.select("1.21.4")
        XCTAssertEqual(store.activeVersion, "1.21.4")
        XCTAssertEqual(store.installed.first(where: { $0.id == "1.21.4" })?.active, true)

        await store.loadMojang(kind: "release", query: "")
        XCTAssertEqual(store.mojangTotal, 2)
        XCTAssertEqual(store.mojangVersions.last?.tierLabel, "Saves + Assets")
    }

    @MainActor
    func testModStoreSearchesInstallsAndTogglesMods() async {
        var enabled = true
        let store = ModStore { line in
            XCTAssertTrue(line.hasPrefix("launcher-mods "))
            let raw = String(line.dropFirst("launcher-mods ".count))
            let json = (try? JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any]) ?? [:]
            switch json["action"] as? String {
            case "modrinth-search":
                return [
                    "hits": [
                        [
                            "project_id": "terralith",
                            "slug": "terralith",
                            "title": "Terralith",
                            "description": "Overworld worldgen overhaul",
                            "author": "Stardust Labs",
                            "downloads": 42_000_000,
                            "categories": ["datapack"],
                            "versions": ["26.3"],
                            "compatibility_hint": "high",
                        ],
                    ],
                ]
            case "modrinth-install":
                return [
                    "ok": true,
                    "mod_id": "terralith",
                    "name": "Terralith",
                    "version": "2.5.8",
                    "ports": ["92 worldgen files", "48 structures"],
                    "installed": [
                        ["id": "terralith", "name": "Terralith", "version": "2.5.8", "enabled": enabled, "origin": "modrinth", "loader": "datapack", "verdict": "works", "ports": ["92 worldgen files"]],
                    ],
                ]
            case "toggle":
                enabled = json["enabled"] as? Bool ?? false
                return [
                    "ok": true,
                    "mods": [
                        ["id": "terralith", "name": "Terralith", "version": "2.5.8", "enabled": enabled, "origin": "modrinth", "loader": "datapack", "verdict": "works"],
                    ],
                ]
            default:
                return ["mods": []]
            }
        }

        await store.search(source: .modrinth, query: "terralith")
        XCTAssertEqual(store.modrinthResults.first?.title, "Terralith")
        XCTAssertEqual(store.modrinthResults.first?.hintLabel, "Pure Data / High")

        let installResult = await store.installModrinth(projectId: "terralith", title: "Terralith")
        if case let .failure(err) = installResult {
            XCTFail("Unexpected failure: \(err.message)")
        }
        XCTAssertEqual(store.installed.first?.id, "terralith")
        XCTAssertEqual(store.installed.first?.enabled, true)

        let toggleErr = await store.toggle("terralith", enabled: false)
        XCTAssertNil(toggleErr)
        XCTAssertEqual(store.installed.first?.enabled, false)
    }

    @MainActor
    func testHorizonRegistryCategoryFilterAndMultiKindMetadata() async {
        var seenKinds: [String] = []
        let store = ModStore { line in
            let raw = String(line.dropFirst("launcher-mods ".count))
            let json = (try? JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any]) ?? [:]
            switch json["action"] as? String {
            case "horizon-browse":
                seenKinds.append(json["kind"] as? String ?? "")
                return [
                    "hits": [
                        [
                            "id": "sky_lanterns",
                            "title": "Sky Lanterns",
                            "version": "0.1.0",
                            "summary": "Warm paper lanterns",
                            "authors": ["Horizon"],
                            "kind": "mod",
                            "installable": true,
                        ],
                        [
                            "id": "warm_evening",
                            "title": "Warm Evening",
                            "version": "0.1.0",
                            "summary": "Golden hour sky shader",
                            "authors": ["Horizon"],
                            "kind": "shader",
                            "installable": true,
                        ],
                        [
                            "id": "ferrite_core_hz",
                            "title": "FerriteCore",
                            "version": "0.1.0",
                            "summary": "Desktop mixin",
                            "authors": ["Horizon"],
                            "kind": "native-mod",
                            "installable": false,
                        ],
                    ],
                ]
            case "horizon-install":
                return [
                    "ok": true,
                    "kind": "shader",
                    "mod_id": "warm_evening",
                    "version": "0.1.0",
                    "verified": true,
                    "message": "Warm Evening 0.1.0 verified, installed into shaderpacks/, and activated!",
                    "installed": [],
                ]
            default:
                return ["mods": []]
            }
        }

        await store.search(source: .horizon, query: "", horizonKind: "all")
        XCTAssertEqual(seenKinds, ["all"])
        XCTAssertEqual(store.horizonResults.count, 3)
        XCTAssertEqual(store.horizonResults[0].title, "Sky Lanterns")
        XCTAssertEqual(store.horizonResults[0].kindLabel, "Content Mod")
        XCTAssertEqual(store.horizonResults[1].kindLabel, "Shader")
        XCTAssertEqual(store.horizonResults[2].kindLabel, "Desktop Mixin")
        XCTAssertFalse(store.horizonResults[2].installable)

        let res = await store.installHorizon(store.horizonResults[1])
        if case let .success(msg) = res {
            XCTAssertTrue(msg.contains("shaderpacks/"))
        } else {
            XCTFail("Expected shader install success")
        }
    }
}
