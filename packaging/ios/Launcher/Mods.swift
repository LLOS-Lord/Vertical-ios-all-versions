import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// Installed Horizon `.hmod` or ported Java folder mod in `Documents/horizon-mods/`.
struct InstalledModItem: Identifiable, Equatable {
    let id: String
    let name: String
    let version: String
    let description: String
    let authors: [String]
    let enabled: Bool
    let format: String
    let origin: String
    let loader: String
    let verdict: String
    let summary: String
    let ports: [String]
    let skipped: [String]
    let clashWarning: String?
    let iconURL: URL?
    let bytes: Int64
    /// Whether this mod can change anything in the game today, from `horizon-mod-diagnostics`.
    /// False for a mod that is installed and switched on but sits in a folder the game never reads,
    /// or one the porter could only carry assets from. `nil` when the game has not said (no
    /// diagnosis came back: an older build, or the game is not running).
    var effective: Bool?
    /// The one-sentence reason, and what to do about it.
    var diagnosis: String

    init?(_ json: [String: Any]) {
        guard let id = json["id"] as? String, !id.isEmpty else { return nil }
        self.id = id
        name = json["name"] as? String ?? id
        version = json["version"] as? String ?? ""
        description = json["description"] as? String ?? ""
        authors = json["authors"] as? [String] ?? []
        enabled = json["enabled"] as? Bool ?? false
        format = json["format"] as? String ?? "hmod"
        origin = json["origin"] as? String ?? "horizon"
        loader = json["loader"] as? String ?? ""
        verdict = json["verdict"] as? String ?? "works"
        summary = json["summary"] as? String ?? ""
        ports = json["ports"] as? [String] ?? []
        skipped = json["skipped"] as? [String] ?? []
        clashWarning = json["clash_warning"] as? String
        iconURL = (json["icon_url"] as? String).flatMap(URL.init(string:))
        bytes = (json["bytes"] as? NSNumber)?.int64Value ?? 0
        // Merged in from the `diagnostics` array by id (`VersionStore`/`ModsStore` `applyDiagnostics`).
        effective = json["effective"] as? Bool
        diagnosis = json["diagnosis"] as? String ?? ""
    }

    /// A copy carrying the game's diagnosis, which arrives separately from the mod list.
    func diagnosed(effective: Bool, diagnosis: String) -> InstalledModItem {
        var copy = self
        copy.effective = effective
        copy.diagnosis = diagnosis
        return copy
    }

    var badgeText: String {
        switch origin {
        case "horizon": return verdict == "verified" ? "Horizon Signed" : "Horizon .hmod"
        case "modrinth": return loader.isEmpty ? "Modrinth Port" : "Ported \(loader.capitalized)"
        case "local-jar": return "Local .jar Port"
        default: return "Local .hmod"
        }
    }

    var badgeTone: Chip.Tone {
        if clashWarning != nil { return .bad }
        // The game's own answer wins: a mod that is on but cannot act is not "good", however
        // promising its verdict from the porter reads.
        if effective == false { return .warn }
        switch verdict {
        case "verified", "works": return .good
        case "partly": return .warn
        default: return .quiet
        }
    }
}

/// Package listing from Horizon's official signed registry (`horizon-rust.github.io/horizon-mods`).
struct HorizonRegistryMod: Identifiable, Equatable {
    let id: String
    let kind: String
    let installable: Bool
    let title: String
    let summary: String
    let authors: [String]
    let categories: [String]
    let version: String
    let downloads: Int
    let installedVersion: String?
    let updateAvailable: Bool

    init?(_ json: [String: Any]) {
        guard let id = json["id"] as? String, !id.isEmpty else { return nil }
        self.id = id
        kind = json["kind"] as? String ?? "mod"
        installable = json["installable"] as? Bool ?? true
        title = json["title"] as? String ?? id
        summary = json["summary"] as? String ?? ""
        authors = json["authors"] as? [String] ?? []
        categories = json["categories"] as? [String] ?? []
        version = json["version"] as? String ?? ""
        downloads = (json["downloads"] as? NSNumber)?.intValue ?? 0
        installedVersion = json["installed_version"] as? String
        updateAvailable = json["update_available"] as? Bool ?? false
    }

    var kindLabel: String {
        switch kind {
        case "shader", "shaderpack": return "Shader"
        case "resourcepack": return "Resource Pack"
        case "modpack": return "Modpack"
        case "native-mod": return "Desktop Mixin"
        default: return "Content Mod"
        }
    }

    var kindIcon: String {
        switch kind {
        case "shader", "shaderpack": return "sun.max.fill"
        case "resourcepack": return "paintbrush.pointed.fill"
        case "modpack": return "shippingbox.fill"
        case "native-mod": return "desktopcomputer"
        default: return "cube.box.fill"
        }
    }

    var kindTone: Chip.Tone {
        if !installable { return .quiet }
        switch kind {
        case "shader", "shaderpack": return .warn
        default: return .good
        }
    }
}

/// Modrinth `project_type:mod` search hit ported on-device via `horizon-port-pack`.
struct ModrinthModItem: Identifiable, Equatable {
    let id: String
    let slug: String
    let title: String
    let summary: String
    let author: String
    let downloads: Int
    let iconURL: URL?
    let categories: [String]
    let versions: [String]
    let compatibilityHint: String

    init?(_ json: [String: Any]) {
        guard let id = json["project_id"] as? String, !id.isEmpty else { return nil }
        self.id = id
        slug = json["slug"] as? String ?? id
        title = json["title"] as? String ?? id
        summary = json["description"] as? String ?? ""
        author = json["author"] as? String ?? ""
        downloads = (json["downloads"] as? NSNumber)?.intValue ?? 0
        iconURL = (json["icon_url"] as? String).flatMap(URL.init(string:))
        categories = json["categories"] as? [String] ?? []
        versions = json["versions"] as? [String] ?? []
        compatibilityHint = json["compatibility_hint"] as? String ?? "port-ready"
    }

    var hintLabel: String {
        switch compatibilityHint {
        case "high": return "Pure Data / High"
        case "port-ready": return "Content Auto-Port"
        case "code-only": return "Java Code Only"
        default: return "Legacy Version"
        }
    }

    var hintTone: Chip.Tone {
        switch compatibilityHint {
        case "high", "port-ready": return .good
        case "code-only": return .warn
        default: return .quiet
        }
    }
}

/// Specific Modrinth `.jar` release candidate for inspection/version selection.
struct ModrinthModVersion: Identifiable, Equatable {
    let id: String
    let versionNumber: String
    let name: String
    let gameVersions: [String]
    let loaders: [String]
    let fileName: String
    let size: Int64

    init?(_ json: [String: Any]) {
        guard let id = json["id"] as? String, !id.isEmpty else { return nil }
        self.id = id
        versionNumber = json["version_number"] as? String ?? id
        name = json["name"] as? String ?? versionNumber
        gameVersions = json["game_versions"] as? [String] ?? []
        loaders = json["loaders"] as? [String] ?? []
        fileName = json["file_name"] as? String ?? "mod.jar"
        size = (json["size"] as? NSNumber)?.int64Value ?? 0
    }
}

/// Dual-source in-launcher mod browser, installer, and `horizon-mods.txt` manager backed by `launcher-mods`.
@MainActor
final class ModStore: ObservableObject {
    enum CatalogSource: String, CaseIterable, Identifiable {
        case horizon = "Horizon Registry"
        case modrinth = "Modrinth (Java Port)"
        var id: String { rawValue }
    }

    @Published private(set) var installed: [InstalledModItem] = []
    @Published private(set) var horizonResults: [HorizonRegistryMod] = []
    @Published private(set) var modrinthResults: [ModrinthModItem] = []
    @Published private(set) var searching = false
    @Published private(set) var busyIds: Set<String> = []
    @Published var failed: String?

    typealias Send = @MainActor (String) async -> [String: Any]
    private let send: Send
    private var searchRevision: UInt64 = 0

    init(send: @escaping Send = { await Bridge.send($0) }) {
        self.send = send
    }

    private func command(_ payload: [String: Any]) async -> [String: Any] {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let text = String(data: data, encoding: .utf8) else {
            return ["error": "Could not encode mod command"]
        }
        return await send("launcher-mods \(text)")
    }

    func reloadInstalled() async {
        let reply = await command(["action": "list"])
        if let raw = reply["mods"] as? [[String: Any]] {
            installed = Self.diagnosed(raw.compactMap(InstalledModItem.init),
                                       from: reply["diagnostics"])
        }
    }

    /// Fold the game's `diagnostics` rows into the mod list: `effective` and `diagnosis` come from
    /// the game, which is the only thing that knows which folder it actually reads.
    static func diagnosed(_ mods: [InstalledModItem],
                          from rows: Any?) -> [InstalledModItem] {
        guard let rows = rows as? [[String: Any]] else { return mods }
        var byId: [String: (Bool, String)] = [:]
        for row in rows {
            guard let id = row["id"] as? String else { continue }
            byId[id] = (row["effective"] as? Bool ?? false,
                        row["verdict"] as? String ?? "")
        }
        return mods.map { mod in
            guard let (effective, verdict) = byId[mod.id] else { return mod }
            return mod.diagnosed(effective: effective, diagnosis: verdict)
        }
    }

    func toggle(_ modId: String, enabled: Bool) async -> String? {
        let reply = await command(["action": "toggle", "id": modId, "enabled": enabled])
        if let err = reply["error"] as? String { return err }
        if let raw = reply["mods"] as? [[String: Any]] {
            // The toggle reply carries no diagnostics: keep the ones already on screen rather than
            // blanking every warning line until the next reload.
            let known = Dictionary(uniqueKeysWithValues: installed.compactMap { mod -> (String, InstalledModItem)? in
                guard mod.effective != nil else { return nil }
                return (mod.id, mod)
            })
            installed = raw.compactMap(InstalledModItem.init).map { fresh in
                guard let old = known[fresh.id] else { return fresh }
                return fresh.diagnosed(effective: old.effective ?? false, diagnosis: old.diagnosis)
            }
        }
        return nil
    }

    func delete(_ modId: String) async -> String? {
        let reply = await command(["action": "delete", "id": modId])
        if let err = reply["error"] as? String { return err }
        if let raw = reply["mods"] as? [[String: Any]] {
            installed = raw.compactMap(InstalledModItem.init)
        }
        return nil
    }

    func search(source: CatalogSource, query: String, loader: String = "all",
                horizonKind: String = "installable", version: String = "compatible") async {
        searchRevision &+= 1
        let rev = searchRevision
        searching = true
        failed = nil
        defer { if rev == searchRevision { searching = false } }

        switch source {
        case .horizon:
            let reply = await command(["action": "horizon-browse", "query": query, "kind": horizonKind])
            guard rev == searchRevision, !Task.isCancelled else { return }
            if let err = reply["error"] as? String {
                failed = err
                return
            }
            horizonResults = (reply["hits"] as? [[String: Any]] ?? []).compactMap(HorizonRegistryMod.init)

        case .modrinth:
            let reply = await command([
                "action": "modrinth-search",
                "query": query,
                "version": version,
                "loader": loader,
                "sort": query.trimmingCharacters(in: .whitespaces).isEmpty ? "downloads" : "relevance",
                "limit": 30,
                "offset": 0,
            ])
            guard rev == searchRevision, !Task.isCancelled else { return }
            if let err = reply["error"] as? String {
                failed = err
                return
            }
            modrinthResults = (reply["hits"] as? [[String: Any]] ?? []).compactMap(ModrinthModItem.init)
        }
    }

    func installHorizon(_ mod: HorizonRegistryMod) async -> Result<String, PackError> {
        guard !busyIds.contains(mod.id) else { return .failure(PackError("Already installing \(mod.title).")) }
        busyIds.insert(mod.id)
        defer { busyIds.remove(mod.id) }

        let reply = await command(["action": "horizon-install", "id": mod.id])
        if let err = reply["error"] as? String {
            return .failure(PackError(err))
        }
        if let raw = reply["installed"] as? [[String: Any]] {
            installed = raw.compactMap(InstalledModItem.init)
        } else {
            await reloadInstalled()
        }
        if let msg = reply["message"] as? String, !msg.isEmpty {
            return .success(msg)
        }
        let ver = reply["version"] as? String ?? mod.version
        let verified = (reply["verified"] as? Bool ?? false) ? " (Ed25519 verified)" : ""
        return .success("\(mod.title) \(ver)\(verified) is installed and enabled.")
    }

    func fetchModrinthVersions(projectId: String, loader: String = "all") async -> [ModrinthModVersion] {
        let reply = await command([
            "action": "modrinth-versions",
            "project_id": projectId,
            "version": "compatible",
            "loader": loader,
        ])
        return (reply["versions"] as? [[String: Any]] ?? []).compactMap(ModrinthModVersion.init)
    }

    func installModrinth(projectId: String, title: String, versionId: String? = nil,
                         minecraftVersion: String = "compatible") async -> Result<String, PackError> {
        guard !busyIds.contains(projectId) else { return .failure(PackError("Already installing \(title).")) }
        busyIds.insert(projectId)
        defer { busyIds.remove(projectId) }

        var payload: [String: Any] = [
            "action": "modrinth-install",
            "project_id": projectId,
            "minecraft_version": minecraftVersion,
        ]
        if let versionId, !versionId.isEmpty { payload["version_id"] = versionId }

        let reply = await command(payload)
        if let err = reply["error"] as? String {
            return .failure(PackError(err))
        }
        if let raw = reply["installed"] as? [[String: Any]] {
            installed = raw.compactMap(InstalledModItem.init)
        } else {
            await reloadInstalled()
        }
        let name = reply["name"] as? String ?? title
        let ver = reply["version"] as? String ?? ""
        let summary = reply["summary"] as? String ?? ""
        let ports = (reply["ports"] as? [String] ?? []).prefix(3).joined(separator: ", ")
        var msg = "\(name) \(ver) ported & enabled."
        if !ports.isEmpty { msg += " Ported: \(ports)." }
        else if !summary.isEmpty { msg += " \(summary)" }
        return .success(msg)
    }

    func importLocalModFile(_ url: URL) async -> Result<String, PackError> {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }

        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent("horizon-mod-import-\(UUID().uuidString)")
        do {
            try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
            let localCopy = tempDir.appendingPathComponent(url.lastPathComponent)
            try FileManager.default.copyItem(at: url, to: localCopy)
            defer { try? FileManager.default.removeItem(at: tempDir) }

            let reply = await command(["action": "import", "path": localCopy.path])
            if let err = reply["error"] as? String {
                return .failure(PackError(err))
            }
            if let raw = reply["installed"] as? [[String: Any]] {
                installed = raw.compactMap(InstalledModItem.init)
            } else {
                await reloadInstalled()
            }
            let name = reply["name"] as? String ?? url.lastPathComponent
            let summary = reply["summary"] as? String ?? "Installed and enabled."
            return .success("\(name): \(summary)")
        } catch {
            try? FileManager.default.removeItem(at: tempDir)
            return .failure(PackError(error.localizedDescription))
        }
    }
}

/// Complete In-Launcher Mod Discovery, Auto-Porter & Mod Manager view inside `PacksPage`.
struct ModsSectionView: View {
    @ObservedObject var link: GameLink
    @StateObject private var store = ModStore()
    @State private var source: ModStore.CatalogSource = .horizon
    @State private var query = ""
    @State private var loader = "all"
    @State private var horizonKind = "installable"
    @State private var importingFile = false
    @State private var notice: String?
    @State private var inspectingProject: ModrinthModItem?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                SectionHeader(title: "Installed Mods")
                Spacer()
                Button {
                    importingFile = true
                } label: {
                    Label("Import .hmod / .jar", systemImage: "square.and.arrow.down")
                        .font(Theme.sans(12, .semiBold))
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
                .accessibilityIdentifier("mods-import-file-button")
            }

            installedModsList

            SectionHeader(title: "Browse & Install Mods")
            Picker("Mod Store", selection: $source) {
                ForEach(ModStore.CatalogSource.allCases) { item in
                    Text(item.rawValue).tag(item)
                }
            }
            .pickerStyle(.segmented)
            .accessibilityIdentifier("mods-source-picker")

            if source == .modrinth {
                Picker("Loader", selection: $loader) {
                    Text("All Loaders").tag("all")
                    Text("Datapack").tag("datapack")
                    Text("Fabric").tag("fabric")
                    Text("NeoForge").tag("neoforge")
                }
                .pickerStyle(.segmented)
                .onChange(of: loader) { _ in
                    Task { await store.search(source: source, query: query, loader: loader, horizonKind: horizonKind) }
                }
                Text("Horizon's pure-Rust porter unpacks Fabric / NeoForge / Forge / Quilt .jar mods (including nested Jar-in-Jar, 1.20–26.3 data, tools, foods, Creative tabs, and visual/audio resource bundles) directly on your iPhone.")
                    .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
            } else {
                Picker("Category", selection: $horizonKind) {
                    Text("Installable").tag("installable")
                    Text("Mods").tag("mod")
                    Text("Shaders").tag("shader")
                    Text("Packs").tag("pack")
                    Text("All").tag("all")
                }
                .pickerStyle(.segmented)
                .onChange(of: horizonKind) { _ in
                    Task { await store.search(source: source, query: query, loader: loader, horizonKind: horizonKind) }
                }
                Text("Official Horizon packages (.hmod content mods, .hshader Metal shaders, resource packs & modpack bundles), verified with Ed25519 signatures and SHA-256 checksums.")
                    .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
            }

            Field(title: "SEARCH MODS",
                  placeholder: source == .horizon ? "Search Horizon mods, shaders, packs…" : "Terralith, Farmer's Delight, Towns and Towers…",
                  text: $query, keyboard: .default)
                .onSubmit {
                    Task { await store.search(source: source, query: query, loader: loader, horizonKind: horizonKind) }
                }

            if let failed = store.failed {
                Label(failed, systemImage: "wifi.exclamationmark")
                    .font(Theme.sans(12)).foregroundStyle(Theme.warn)
            }
            if store.searching && currentCount == 0 {
                ProgressView().tint(Theme.accentText).frame(maxWidth: .infinity).padding(24)
            }

            catalogList
        }
        .task(id: source) {
            await store.reloadInstalled()
            await store.search(source: source, query: query, loader: loader, horizonKind: horizonKind)
        }
        .fileImporter(
            isPresented: $importingFile,
            allowedContentTypes: [
                UTType(filenameExtension: "hmod") ?? .data,
                UTType(filenameExtension: "hshader") ?? .data,
                UTType(filenameExtension: "jar") ?? .zip,
                .zip,
                .data,
            ]
        ) { result in
            switch result {
            case let .success(url):
                Task {
                    switch await store.importLocalModFile(url) {
                    case let .success(msg): Haptics.done(); notice = msg
                    case let .failure(err): Haptics.failed(); notice = err.message
                    }
                }
            case let .failure(err):
                notice = err.localizedDescription
            }
        }
        .sheet(item: $inspectingProject) { project in
            ModrinthVersionSheet(project: project, store: store) { message in
                notice = message
            }
        }
        .alert("Mods", isPresented: Binding(get: { notice != nil }, set: { if !$0 { notice = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(notice ?? "")
        }
    }

    private var currentCount: Int {
        source == .horizon ? store.horizonResults.count : store.modrinthResults.count
    }

    @ViewBuilder private var installedModsList: some View {
        if store.installed.isEmpty {
            EmptyNote(icon: "puzzlepiece.extension",
                      text: "No mods installed yet. Browse Horizon Registry or Modrinth below, or import a .hmod / .jar file directly.")
        } else {
            VStack(spacing: 0) {
                ForEach(store.installed) { mod in
                    VStack(alignment: .leading, spacing: 6) {
                        HStack(spacing: 12) {
                            VStack(alignment: .leading, spacing: 3) {
                                HStack(spacing: 6) {
                                    Text(mod.name).font(Theme.sans(14, .semiBold)).foregroundStyle(Theme.text).lineLimit(1)
                                    if !mod.version.isEmpty {
                                        Text(mod.version).font(Theme.mono(10)).foregroundStyle(Theme.textMuted)
                                    }
                                    Chip(tone: mod.badgeTone, text: mod.badgeText)
                                }
                                if !mod.ports.isEmpty {
                                    Text(mod.ports.prefix(4).joined(separator: " · "))
                                        .font(Theme.mono(10)).foregroundStyle(Theme.textMuted).lineLimit(1)
                                } else if !mod.description.isEmpty {
                                    Text(mod.description)
                                        .font(Theme.sans(11)).foregroundStyle(Theme.textMuted).lineLimit(1)
                                }
                                if let clash = mod.clashWarning {
                                    Text(clash).font(Theme.sans(11)).foregroundStyle(Theme.danger)
                                }
                                // The game's own answer, when it disagrees with the switch: a mod
                                // that is on but reads from a folder the game never opens, or one
                                // the porter could only carry assets from. This is the line that
                                // turns "mods don't work" into something the player can act on.
                                if mod.effective == false, !mod.diagnosis.isEmpty {
                                    Label(mod.diagnosis, systemImage: "exclamationmark.triangle.fill")
                                        .font(Theme.sans(11))
                                        .foregroundStyle(Theme.warnText)
                                        .fixedSize(horizontal: false, vertical: true)
                                }
                            }
                            Spacer(minLength: 8)
                            Toggle("", isOn: Binding(
                                get: { mod.enabled },
                                set: { enabled in
                                    Haptics.tap()
                                    Task {
                                        if let err = await store.toggle(mod.id, enabled: enabled) {
                                            notice = err
                                        }
                                    }
                                }
                            ))
                            .labelsHidden()
                            .tint(Theme.accent)
                            .accessibilityIdentifier("mod-toggle-\(mod.id)")
                        }
                    }
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .contextMenu {
                        Button(role: .destructive) {
                            Task {
                                if let err = await store.delete(mod.id) {
                                    notice = err
                                }
                            }
                        } label: { Label("Delete Mod", systemImage: "trash") }
                    }
                    if mod != store.installed.last {
                        Divider().overlay(Theme.border).padding(.leading, 14)
                    }
                }
            }
            .card()
            Text("Enabled mods load automatically when you press Play. Hold a mod to delete it.")
                .font(Theme.sans(11)).foregroundStyle(Theme.textFaint)
        }
    }

    @ViewBuilder private var catalogList: some View {
        switch source {
        case .horizon:
            if !store.horizonResults.isEmpty {
                VStack(spacing: 0) {
                    ForEach(store.horizonResults) { mod in
                        HStack(alignment: .top, spacing: 12) {
                            Image(systemName: mod.kindIcon)
                                .font(.system(size: 22))
                                .foregroundStyle(Theme.accent)
                                .frame(width: 44, height: 44)
                                .background(Theme.surfaceSunken, in: RoundedRectangle(cornerRadius: 8))
                            VStack(alignment: .leading, spacing: 3) {
                                HStack(spacing: 6) {
                                    Text(mod.title).font(Theme.sans(15, .semiBold)).foregroundStyle(Theme.text).lineLimit(1)
                                    Text(mod.version).font(Theme.mono(10)).foregroundStyle(Theme.textMuted)
                                    Chip(tone: mod.kindTone, text: mod.kindLabel)
                                }
                                Text(mod.summary).font(Theme.sans(11)).foregroundStyle(Theme.textMuted).lineLimit(2)
                                HStack(spacing: 6) {
                                    if !mod.authors.isEmpty { Text("by \(mod.authors.joined(separator: ", "))") }
                                    Text("· \(mod.downloads.formatted(.number.notation(.compactName))) downloads")
                                }
                                .font(Theme.mono(9)).foregroundStyle(Theme.textFaint).lineLimit(1)
                            }
                            Spacer(minLength: 6)
                            if store.busyIds.contains(mod.id) {
                                ProgressView().tint(Theme.accentText).frame(width: 44, height: 44)
                            } else if !mod.installable {
                                Chip(tone: .quiet, text: "Desktop Only")
                            } else if mod.installedVersion != nil && !mod.updateAvailable {
                                Chip(tone: .good, text: "Installed")
                            } else {
                                Button {
                                    Haptics.tap()
                                    Task {
                                        switch await store.installHorizon(mod) {
                                        case let .success(msg): Haptics.done(); notice = msg
                                        case let .failure(err): Haptics.failed(); notice = err.message
                                        }
                                        await store.search(source: .horizon, query: query, loader: loader, horizonKind: horizonKind)
                                    }
                                } label: {
                                    Image(systemName: mod.updateAvailable ? "arrow.up.circle.fill" : "arrow.down.circle.fill")
                                        .font(.system(size: 26)).foregroundStyle(Theme.accentBright)
                                }
                                .frame(width: 44, height: 44)
                                .accessibilityLabel("Install \(mod.title)")
                                .accessibilityIdentifier("horizon-mod-install-\(mod.id)")
                            }
                        }
                        .padding(12)
                        if mod != store.horizonResults.last {
                            Divider().overlay(Theme.border).padding(.leading, 68)
                        }
                    }
                }
                .card()
            }

        case .modrinth:
            if !store.modrinthResults.isEmpty {
                VStack(spacing: 0) {
                    ForEach(store.modrinthResults) { item in
                        HStack(alignment: .top, spacing: 12) {
                            AsyncImage(url: item.iconURL) { image in
                                image.resizable().interpolation(.none).scaledToFill()
                            } placeholder: {
                                Theme.surfaceSunken
                            }
                            .frame(width: 46, height: 46)
                            .clipShape(RoundedRectangle(cornerRadius: 8))
                            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.border))

                            VStack(alignment: .leading, spacing: 3) {
                                HStack(spacing: 6) {
                                    Text(item.title).font(Theme.sans(15, .semiBold)).foregroundStyle(Theme.text).lineLimit(1)
                                    Chip(tone: item.hintTone, text: item.hintLabel)
                                }
                                Text(item.summary).font(Theme.sans(11)).foregroundStyle(Theme.textMuted).lineLimit(2)
                                HStack(spacing: 6) {
                                    Text("by \(item.author)")
                                    Text("·")
                                    Text("\(item.downloads.formatted(.number.notation(.compactName))) downloads")
                                    Button("Versions") { inspectingProject = item }
                                        .font(Theme.mono(9)).foregroundStyle(Theme.accentText)
                                }
                                .font(Theme.mono(9)).foregroundStyle(Theme.textFaint).lineLimit(1)
                            }
                            Spacer(minLength: 4)
                            if store.busyIds.contains(item.id) {
                                ProgressView().tint(Theme.accentText).frame(width: 44, height: 44)
                            } else if store.installed.contains(where: { $0.id == item.slug }) {
                                Chip(tone: .good, text: "Installed")
                            } else {
                                Button {
                                    Haptics.tap()
                                    Task {
                                        switch await store.installModrinth(projectId: item.id, title: item.title) {
                                        case let .success(msg): Haptics.done(); notice = msg
                                        case let .failure(err): Haptics.failed(); notice = err.message
                                        }
                                    }
                                } label: {
                                    Image(systemName: "arrow.down.circle.fill")
                                        .font(.system(size: 26)).foregroundStyle(Theme.accentBright)
                                }
                                .frame(width: 44, height: 44)
                                .accessibilityLabel("Install and port \(item.title)")
                                .accessibilityIdentifier("modrinth-mod-install-\(item.id)")
                            }
                        }
                        .padding(12)
                        if item != store.modrinthResults.last {
                            Divider().overlay(Theme.border).padding(.leading, 70)
                        }
                    }
                }
                .card()
            }
        }
    }
}

/// Sheet allowing the user to pick a specific `.jar` release of a Modrinth mod before porting.
private struct ModrinthVersionSheet: View {
    let project: ModrinthModItem
    @ObservedObject var store: ModStore
    var onInstalled: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var versions: [ModrinthModVersion] = []
    @State private var loading = true

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text(project.summary).font(.footnote).foregroundStyle(.secondary)
                }
                Section("Available .jar Releases") {
                    if loading {
                        ProgressView()
                    } else if versions.isEmpty {
                        Text("No downloadable .jar versions found.")
                    } else {
                        ForEach(versions) { ver in
                            HStack {
                                VStack(alignment: .leading, spacing: 3) {
                                    Text("\(ver.versionNumber) — \(ver.name)").font(.subheadline.weight(.semibold))
                                    Text("\(ver.loaders.joined(separator: ", ")) · MC \(ver.gameVersions.prefix(4).joined(separator: ", "))")
                                        .font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer()
                                Button("Port & Install") {
                                    Task {
                                        switch await store.installModrinth(projectId: project.id, title: project.title, versionId: ver.id) {
                                        case let .success(msg):
                                            dismiss()
                                            onInstalled(msg)
                                        case let .failure(err):
                                            dismiss()
                                            onInstalled(err.message)
                                        }
                                    }
                                }
                                .buttonStyle(.borderedProminent)
                                .controlSize(.small)
                            }
                        }
                    }
                }
            }
            .navigationTitle(project.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Close") { dismiss() }
                }
            }
        }
        .task {
            versions = await store.fetchModrinthVersions(projectId: project.id)
            loading = false
        }
    }
}
