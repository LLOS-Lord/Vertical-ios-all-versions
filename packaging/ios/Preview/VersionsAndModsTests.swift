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
}
