import SwiftUI
import UIKit

/// Installed Minecraft version entry managed by `horizon_client::versions` (`Documents/versions/<id>/resources`
/// or legacy `Documents/resources`).
struct InstalledGameVersion: Identifiable, Equatable {
    let id: String
    let releaseType: String
    let source: String
    let protocolVersion: Int?
    let protocolAdapter: String?
    let supportTier: String
    let fullSimulation: Bool
    let ready: Bool
    let active: Bool
    let legacyRoot: Bool
    let files: Int
    let bytes: Int64
    let notes: String

    init?(_ json: [String: Any]) {
        guard let id = json["id"] as? String, !id.isEmpty else { return nil }
        self.id = id
        releaseType = json["release_type"] as? String ?? "release"
        source = json["source"] as? String ?? "horizon-prepared"
        protocolVersion = (json["protocol"] as? NSNumber)?.intValue
        protocolAdapter = json["protocol_adapter"] as? String
        supportTier = json["support_tier"] as? String ?? "native-26.3"
        fullSimulation = json["full_simulation"] as? Bool ?? false
        ready = json["ready"] as? Bool ?? false
        active = json["active"] as? Bool ?? false
        legacyRoot = json["legacy_root"] as? Bool ?? false
        files = (json["files"] as? NSNumber)?.intValue ?? 0
        bytes = (json["bytes"] as? NSNumber)?.int64Value ?? 0
        notes = json["notes"] as? String ?? ""
    }

    var badgeText: String {
        switch supportTier {
        case "native-26.3": return "Native Engine"
        case "protocol-adapter": return "Protocol Adapter"
        case "world-datafix-assets": return "DataFix + Assets"
        default: return "Classic Assets"
        }
    }

    var badgeTone: Chip.Tone {
        if !ready { return .warn }
        switch supportTier {
        case "native-26.3", "protocol-adapter": return .good
        default: return .quiet
        }
    }
}

/// Downloadable prepared Horizon bundle mirror / catalog item.
struct PreparedMirrorItem: Identifiable, Equatable {
    let id: String
    let label: String
    let version: String
    let url: String
    let notes: String
    let sha256: String?
    let bytes: Int64?
    let installed: Bool

    init?(_ json: [String: Any]) {
        guard let url = json["url"] as? String, !url.isEmpty else { return nil }
        id = json["id"] as? String ?? url
        label = json["label"] as? String ?? "Prepared Horizon Bundle"
        version = json["version"] as? String ?? "26.3"
        self.url = url
        notes = json["notes"] as? String ?? ""
        sha256 = json["sha256"] as? String
        bytes = (json["bytes"] as? NSNumber)?.int64Value
        installed = json["installed"] as? Bool ?? false
    }
}

/// Entry from Mojang's official `version_manifest_v2.json`.
struct MojangVersionItem: Identifiable, Equatable {
    let id: String
    let type: String
    let releaseTime: String
    let protocolVersion: Int?
    let protocolAdapter: String?
    let supportTier: String
    let installed: Bool
    let ready: Bool
    let active: Bool

    init?(_ json: [String: Any]) {
        guard let id = json["id"] as? String, !id.isEmpty else { return nil }
        self.id = id
        type = json["type"] as? String ?? "release"
        releaseTime = json["release_time"] as? String ?? ""
        protocolVersion = (json["protocol"] as? NSNumber)?.intValue
        protocolAdapter = json["protocol_adapter"] as? String
        supportTier = json["support_tier"] as? String ?? "classic-assets"
        installed = json["installed"] as? Bool ?? false
        ready = json["ready"] as? Bool ?? false
        active = json["active"] as? Bool ?? false
    }

    var shortDate: String {
        String(releaseTime.prefix(10))
    }

    var tierLabel: String {
        switch supportTier {
        case "native-26.3": return "Pinned 26.3"
        case "protocol-adapter": return "Wire + Saves"
        case "world-datafix-assets": return "Saves + Assets"
        default: return "Classic Assets"
        }
    }
}

/// Live progress of a background Rust version download/preparation job.
struct VersionDownloadJob: Equatable {
    var active = false
    var versionId = ""
    var source = ""
    var stage = "idle"
    var detail = ""
    var receivedBytes: Int64 = 0
    var totalBytes: Int64 = 0
    var completedFiles = 0
    var totalFiles = 0
    var done = false
    var error: String?

    static let idle = VersionDownloadJob()

    init() {}

    init(_ json: [String: Any]) {
        active = json["active"] as? Bool ?? false
        versionId = json["version_id"] as? String ?? ""
        source = json["source"] as? String ?? ""
        stage = json["stage"] as? String ?? "idle"
        detail = json["detail"] as? String ?? ""
        receivedBytes = (json["received_bytes"] as? NSNumber)?.int64Value ?? 0
        totalBytes = (json["total_bytes"] as? NSNumber)?.int64Value ?? 0
        completedFiles = (json["completed_files"] as? NSNumber)?.intValue ?? 0
        totalFiles = (json["total_files"] as? NSNumber)?.intValue ?? 0
        done = json["done"] as? Bool ?? false
        error = json["error"] as? String
    }

    var fraction: Double? {
        if totalFiles > 0, stage == "assets" {
            return Double(completedFiles) / Double(max(1, totalFiles))
        }
        if totalBytes > 0 {
            return min(1.0, Double(receivedBytes) / Double(totalBytes))
        }
        return nil
    }
}

/// Multi-version catalog, Mojang server browser, and active version switcher backed by `launcher-versions`.
@MainActor
final class VersionStore: ObservableObject {
    @Published private(set) var installed: [InstalledGameVersion] = []
    @Published private(set) var activeVersion = "26.3"
    @Published private(set) var olderServers = true
    @Published private(set) var hasSimulationBase = false
    @Published private(set) var mirrors: [PreparedMirrorItem] = []
    @Published private(set) var mojangVersions: [MojangVersionItem] = []
    @Published private(set) var mojangTotal = 0
    @Published private(set) var latestRelease = "26.3"
    @Published private(set) var latestSnapshot = ""
    @Published private(set) var job: VersionDownloadJob = .idle
    @Published private(set) var loadingMirrors = false
    @Published private(set) var loadingMojang = false
    @Published var issue: String?
    @Published var notice: String?

    typealias Send = @MainActor (String) async -> [String: Any]
    private let send: Send
    private var pollTask: Task<Void, Never>?

    init(send: @escaping Send = { await Bridge.send($0) }) {
        self.send = send
    }

    private func command(_ payload: [String: Any]) async -> [String: Any] {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let text = String(data: data, encoding: .utf8) else {
            return ["error": "Could not encode version command"]
        }
        return await send("launcher-versions \(text)")
    }

    func reloadInstalled() async {
        let reply = await command(["action": "list"])
        applySnapshot(reply)
    }

    private func applySnapshot(_ reply: [String: Any]) {
        if let err = reply["error"] as? String {
            issue = err
            return
        }
        if let active = reply["active"] as? String, !active.isEmpty {
            activeVersion = active
        }
        if let older = reply["older_servers"] as? Bool {
            olderServers = older
        }
        if let base = reply["has_simulation_base"] as? Bool {
            hasSimulationBase = base
        }
        if let rawList = reply["versions"] as? [[String: Any]] {
            installed = rawList.compactMap(InstalledGameVersion.init)
        }
        if let jobJson = reply["job"] as? [String: Any] {
            job = VersionDownloadJob(jobJson)
            if job.active { ensurePolling() }
        }
    }

    func select(_ versionId: String, onChanged: (@MainActor () async -> Void)? = nil) async {
        issue = nil
        let reply = await command(["action": "select", "id": versionId])
        if let err = reply["error"] as? String {
            issue = err
            return
        }
        applySnapshot(reply)
        await onChanged?()
    }

    func delete(_ versionId: String, onChanged: (@MainActor () async -> Void)? = nil) async {
        issue = nil
        let reply = await command(["action": "delete", "id": versionId])
        if let err = reply["error"] as? String {
            issue = err
            return
        }
        applySnapshot(reply)
        await onChanged?()
    }

    func setOlderServers(_ enabled: Bool) async {
        let reply = await command(["action": "older-servers", "enabled": enabled])
        applySnapshot(reply)
    }

    func loadCatalog(customURL: String = "") async {
        loadingMirrors = true
        defer { loadingMirrors = false }
        var payload: [String: Any] = ["action": "catalog"]
        let trimmed = customURL.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { payload["url"] = trimmed }
        let reply = await command(payload)
        if let err = reply["error"] as? String {
            issue = err
            return
        }
        mirrors = (reply["mirrors"] as? [[String: Any]] ?? []).compactMap(PreparedMirrorItem.init)
    }

    func loadMojang(kind: String, query: String, refresh: Bool = false) async {
        loadingMojang = true
        defer { loadingMojang = false }
        let reply = await command([
            "action": "mojang",
            "kind": kind,
            "query": query,
            "limit": 100,
            "offset": 0,
            "refresh": refresh,
        ])
        if let err = reply["error"] as? String {
            issue = err
            return
        }
        latestRelease = reply["latest_release"] as? String ?? latestRelease
        latestSnapshot = reply["latest_snapshot"] as? String ?? latestSnapshot
        hasSimulationBase = reply["has_simulation_base"] as? Bool ?? hasSimulationBase
        mojangTotal = (reply["total"] as? NSNumber)?.intValue ?? 0
        mojangVersions = (reply["versions"] as? [[String: Any]] ?? []).compactMap(MojangVersionItem.init)
    }

    func startPreparedDownload(url: String, version: String, sha256: String? = nil,
                               onFinished: (@MainActor () async -> Void)? = nil) async {
        issue = nil
        var payload: [String: Any] = [
            "action": "download-prepared",
            "url": url.trimmingCharacters(in: .whitespacesAndNewlines),
            "version": version.trimmingCharacters(in: .whitespacesAndNewlines),
        ]
        if let sha256, !sha256.isEmpty { payload["sha256"] = sha256 }
        let reply = await command(payload)
        if let err = reply["error"] as? String {
            issue = err
            return
        }
        if let jobJson = reply["job"] as? [String: Any] {
            job = VersionDownloadJob(jobJson)
        }
        ensurePolling(onFinished: onFinished)
    }

    func startMojangDownload(versionId: String,
                             onFinished: (@MainActor () async -> Void)? = nil) async {
        issue = nil
        let reply = await command([
            "action": "download-mojang",
            "id": versionId,
        ])
        if let err = reply["error"] as? String {
            issue = err
            return
        }
        if let jobJson = reply["job"] as? [String: Any] {
            job = VersionDownloadJob(jobJson)
        }
        ensurePolling(onFinished: onFinished)
    }

    func cancelDownload() async {
        let reply = await command(["action": "cancel"])
        job = VersionDownloadJob(reply)
    }

    private func ensurePolling(onFinished: (@MainActor () async -> Void)? = nil) {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(350))
                if Task.isCancelled { return }
                let snap = await self.command(["action": "job"])
                let next = VersionDownloadJob(snap)
                self.job = next
                if !next.active {
                    await self.reloadInstalled()
                    if let err = next.error {
                        self.issue = err
                    } else if next.done, !next.versionId.isEmpty {
                        self.notice = "Minecraft \(next.versionId) is installed."
                        await onFinished?()
                    }
                    return
                }
            }
        }
    }
}

/// Full native sheet for browsing, downloading, switching, and managing all Minecraft versions on iOS.
struct VersionBrowserSheet: View {
    @ObservedObject var link: GameLink
    @StateObject private var store = VersionStore()
    @Environment(\.dismiss) private var dismiss
    @State private var tab: BrowserTab = .mojang
    @State private var mojangFilter = "release"
    @State private var query = ""
    @State private var customZipURL = ""
    @State private var customZipVersion = "26.3"
    @State private var customCatalogURL = ""

    enum BrowserTab: String, CaseIterable, Identifiable {
        case installed = "Installed"
        case mojang = "Mojang Servers"
        case mirrors = "Horizon Mirrors"
        var id: String { rawValue }
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    Picker("Source", selection: $tab) {
                        ForEach(BrowserTab.allCases) { item in
                            Text(item.rawValue).tag(item)
                        }
                    }
                    .pickerStyle(.segmented)
                    .accessibilityIdentifier("versions-source-picker")

                    if store.job.active {
                        activeJobBanner
                    }
                    if let issue = store.issue {
                        Label(issue, systemImage: "exclamationmark.triangle.fill")
                            .font(Theme.sans(12)).foregroundStyle(Theme.danger)
                            .padding(12).frame(maxWidth: .infinity, alignment: .leading).card()
                    }
                    if let notice = store.notice {
                        Label(notice, systemImage: "checkmark.circle.fill")
                            .font(Theme.sans(12)).foregroundStyle(Theme.ready)
                            .padding(12).frame(maxWidth: .infinity, alignment: .leading).card()
                    }

                    switch tab {
                    case .installed: installedSection
                    case .mojang: mojangSection
                    case .mirrors: mirrorsSection
                    }
                }
                .padding(18)
            }
            .background(Theme.background.ignoresSafeArea())
            .navigationTitle("Game Versions")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                        .accessibilityIdentifier("versions-done-button")
                }
            }
        }
        .task {
            await store.reloadInstalled()
            if !store.installed.isEmpty { tab = .installed }
            await store.loadMojang(kind: mojangFilter, query: query)
            await store.loadCatalog()
        }
        .accessibilityIdentifier("launcher-version-browser")
    }

    private var activeJobBanner: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                ProgressView().tint(Theme.accent)
                Text("Installing Minecraft \(store.job.versionId) (\(store.job.stage))")
                    .font(Theme.sans(13, .semiBold)).foregroundStyle(Theme.text)
                Spacer()
                Button("Cancel") { Task { await store.cancelDownload() } }
                    .font(Theme.sans(12, .semiBold)).foregroundStyle(Theme.danger)
            }
            if let fraction = store.job.fraction {
                ProgressView(value: fraction).tint(Theme.accent)
            }
            if !store.job.detail.isEmpty {
                Text(store.job.detail).font(Theme.mono(10)).foregroundStyle(Theme.textMuted)
            }
            HStack(spacing: 12) {
                if store.job.receivedBytes > 0 {
                    Text(ByteCountFormatter.string(fromByteCount: store.job.receivedBytes, countStyle: .file))
                }
                if store.job.totalFiles > 0 {
                    Text("\(store.job.completedFiles)/\(store.job.totalFiles) asset objects")
                }
            }
            .font(Theme.mono(10)).foregroundStyle(Theme.textFaint)
        }
        .padding(14)
        .card()
    }

    @ViewBuilder private var installedSection: some View {
        SectionHeader(title: "Active & Installed Versions")
        if store.installed.isEmpty {
            EmptyNote(icon: "tray.and.arrow.down",
                      text: "No game versions installed yet. Download directly from Mojang Servers or Horizon Mirrors.")
        } else {
            VStack(spacing: 0) {
                ForEach(store.installed) { ver in
                    HStack(alignment: .top, spacing: 12) {
                        VStack(alignment: .leading, spacing: 4) {
                            HStack(spacing: 8) {
                                Text("Minecraft \(ver.id)")
                                    .font(Theme.sans(15, .semiBold)).foregroundStyle(Theme.text)
                                if ver.active {
                                    Chip(tone: .good, text: "Active")
                                }
                                Chip(tone: ver.badgeTone, text: ver.badgeText)
                            }
                            HStack(spacing: 6) {
                                Text(ver.source)
                                if let proto = ver.protocolVersion {
                                    Text("· protocol \(proto)")
                                }
                                if ver.bytes > 0 {
                                    Text("· \(ByteCountFormatter.string(fromByteCount: ver.bytes, countStyle: .file))")
                                }
                            }
                            .font(Theme.mono(10)).foregroundStyle(Theme.textMuted)
                            if !ver.notes.isEmpty {
                                Text(ver.notes).font(Theme.sans(11)).foregroundStyle(Theme.textFaint).lineLimit(2)
                            }
                        }
                        Spacer(minLength: 8)
                        if ver.ready && !ver.active {
                            Button("Use") {
                                Haptics.tap()
                                Task {
                                    await store.select(ver.id) { await link.refresh() }
                                }
                            }
                            .buttonStyle(.borderedProminent)
                            .controlSize(.small)
                            .accessibilityIdentifier("version-select-\(ver.id)")
                        }
                    }
                    .padding(14)
                    .contextMenu {
                        Button(role: .destructive) {
                            Task { await store.delete(ver.id) { await link.refresh() } }
                        } label: { Label("Delete Version", systemImage: "trash") }
                    }
                    if ver != store.installed.last {
                        Divider().overlay(Theme.border).padding(.leading, 14)
                    }
                }
            }
            .card()
        }

        SectionHeader(title: "Cross-Version Multiplayer")
        Toggle(isOn: Binding(
            get: { store.olderServers },
            set: { enabled in Task { await store.setOlderServers(enabled) } }
        )) {
            VStack(alignment: .leading, spacing: 3) {
                Text("Native Multi-Protocol Wire Adapter (1.20.2 – 26.3)")
                    .font(Theme.sans(14, .semiBold)).foregroundStyle(Theme.text)
                Text("Translates packet IDs, block states, registries and pre-1.20.5 NBT item stacks automatically when joining older servers. Singleplayer worlds from 1.8 through 26.3 upgrade via built-in DataFixer.")
                    .font(Theme.sans(11)).foregroundStyle(Theme.textMuted)
            }
        }
        .tint(Theme.accent)
        .padding(14)
        .card()
    }

    @ViewBuilder private var mojangSection: some View {
        SectionHeader(title: "Official Mojang Version Server")
        if !store.hasSimulationBase {
            Label("Install one full Prepared Horizon Base (under Horizon Mirrors or from a PC .zip) so on-device Mojang downloads can link simulation & registry tables automatically.",
                  systemImage: "info.circle.fill")
                .font(Theme.sans(11)).foregroundStyle(Theme.warn)
                .padding(12).frame(maxWidth: .infinity, alignment: .leading).card()
        }
        Picker("Channel", selection: $mojangFilter) {
            Text("Releases").tag("release")
            Text("Snapshots").tag("snapshot")
            Text("Beta / Alpha").tag("historical")
            Text("All").tag("all")
        }
        .pickerStyle(.segmented)
        .onChange(of: mojangFilter) { _ in
            Task { await store.loadMojang(kind: mojangFilter, query: query) }
        }

        HStack(spacing: 10) {
            Field(title: "SEARCH VERSION", placeholder: "26.3, 1.21.4, 1.20.1, 1.12.2, 1.8.9…",
                  text: $query, keyboard: .default)
                .onSubmit { Task { await store.loadMojang(kind: mojangFilter, query: query) } }
            Button {
                Task { await store.loadMojang(kind: mojangFilter, query: query, refresh: true) }
            } label: {
                Image(systemName: "arrow.clockwise").frame(width: 42, height: 42)
            }
            .buttonStyle(.bordered)
            .accessibilityLabel("Refresh Mojang versions")
        }

        Text("Latest release: \(store.latestRelease)\(store.latestSnapshot.isEmpty ? "" : " · Latest snapshot: \(store.latestSnapshot)") · \(store.mojangTotal) matching versions")
            .font(Theme.mono(10)).foregroundStyle(Theme.textFaint)

        if store.loadingMojang && store.mojangVersions.isEmpty {
            ProgressView().tint(Theme.accent).frame(maxWidth: .infinity).padding(24)
        } else {
            VStack(spacing: 0) {
                ForEach(store.mojangVersions) { item in
                    HStack(spacing: 12) {
                        VStack(alignment: .leading, spacing: 3) {
                            HStack(spacing: 6) {
                                Text(item.id).font(Theme.sans(15, .semiBold)).foregroundStyle(Theme.text)
                                Chip(tone: item.supportTier == "native-26.3" || item.supportTier == "protocol-adapter" ? .good : .quiet,
                                     text: item.tierLabel)
                            }
                            HStack(spacing: 6) {
                                Text(item.type)
                                if !item.shortDate.isEmpty { Text("· \(item.shortDate)") }
                                if let proto = item.protocolVersion { Text("· protocol \(proto)") }
                            }
                            .font(Theme.mono(10)).foregroundStyle(Theme.textMuted)
                        }
                        Spacer(minLength: 8)
                        if item.ready {
                            Chip(tone: .good, text: item.active ? "Active" : "Installed")
                        } else {
                            Button {
                                Haptics.tap()
                                Task {
                                    await store.startMojangDownload(versionId: item.id) {
                                        await link.refresh()
                                        await store.loadMojang(kind: mojangFilter, query: query)
                                    }
                                }
                            } label: {
                                Image(systemName: "arrow.down.circle.fill")
                                    .font(.system(size: 26)).foregroundStyle(Theme.accentBright)
                            }
                            .disabled(store.job.active)
                            .accessibilityLabel("Download Minecraft \(item.id)")
                            .accessibilityIdentifier("mojang-download-\(item.id)")
                        }
                    }
                    .padding(12)
                    if item != store.mojangVersions.last {
                        Divider().overlay(Theme.border).padding(.leading, 12)
                    }
                }
            }
            .card()
        }
    }

    @ViewBuilder private var mirrorsSection: some View {
        SectionHeader(title: "Prepared Horizon Runtime Mirrors")
        Text("Prepared bundles include both Java-25 simulation tables (registries, block behaviours, collisions, worldgen) and game assets. Installing one full bundle also powers on-device Mojang downloads for every other version.")
            .font(Theme.sans(12)).foregroundStyle(Theme.textMuted)

        if store.loadingMirrors && store.mirrors.isEmpty {
            ProgressView().tint(Theme.accent).frame(maxWidth: .infinity).padding(20)
        }
        VStack(spacing: 0) {
            ForEach(store.mirrors) { mirror in
                HStack(alignment: .top, spacing: 12) {
                    VStack(alignment: .leading, spacing: 4) {
                        HStack(spacing: 8) {
                            Text(mirror.label).font(Theme.sans(14, .semiBold)).foregroundStyle(Theme.text)
                            Chip(tone: mirror.installed ? .good : .quiet, text: mirror.version)
                        }
                        if !mirror.notes.isEmpty {
                            Text(mirror.notes).font(Theme.sans(11)).foregroundStyle(Theme.textMuted)
                        }
                        Text(mirror.url).font(Theme.mono(9)).foregroundStyle(Theme.textFaint).lineLimit(1)
                    }
                    Spacer(minLength: 8)
                    Button {
                        Haptics.tap()
                        Task {
                            await store.startPreparedDownload(url: mirror.url, version: mirror.version, sha256: mirror.sha256) {
                                await link.refresh()
                            }
                        }
                    } label: {
                        Image(systemName: mirror.installed ? "arrow.clockwise.circle.fill" : "arrow.down.circle.fill")
                            .font(.system(size: 26)).foregroundStyle(Theme.accentBright)
                    }
                    .disabled(store.job.active)
                    .accessibilityIdentifier("mirror-download-\(mirror.id)")
                }
                .padding(14)
                if mirror != store.mirrors.last {
                    Divider().overlay(Theme.border).padding(.leading, 14)
                }
            }
        }
        .card()

        SectionHeader(title: "Custom Prepared Bundle or Catalog Server")
        VStack(alignment: .leading, spacing: 12) {
            Field(title: "PREPARED ZIP SERVER URL", placeholder: "https://your-server.example/horizon-26.3.zip",
                  text: $customZipURL, keyboard: .URL)
            Field(title: "TARGET VERSION TAG", placeholder: "26.3", text: $customZipVersion, keyboard: .default)
            PrimaryButton(title: "Download & Install Prepared Bundle", icon: "arrow.down.circle") {
                Task {
                    await store.startPreparedDownload(url: customZipURL, version: customZipVersion) {
                        await link.refresh()
                    }
                }
            }
            .disabled(store.job.active || customZipURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)

            Divider().overlay(Theme.border)
            Field(title: "CUSTOM CATALOG JSON URL", placeholder: "https://your-server.example/versions-catalog.json",
                  text: $customCatalogURL, keyboard: .URL)
            SecondaryButton(title: "Load Version Catalog", icon: "globe") {
                Task { await store.loadCatalog(customURL: customCatalogURL) }
            }
            .disabled(customCatalogURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .padding(14)
        .card()
    }
}
