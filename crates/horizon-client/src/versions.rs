//! Multi-version catalog, Mojang official server downloader, on-device resource preparation, and
//! version profile switching for mobile (`Documents/versions/<id>/resources`) and desktop.
//!
//! Supports two live server sources without leaving the launcher:
//! 1. **Official Mojang Version Servers** (`piston-meta.mojang.com` + `resources.download.minecraft.net`):
//!    browses all official Releases, Snapshots, Beta, and Alpha versions, downloads SHA-1 verified
//!    `<id>.json`, `client.jar`, and shared content-addressed asset objects across worker threads,
//!    and builds a Horizon resource tree on device (linking simulation tables from any installed
//!    prepared base via zero-copy hardlinks when available).
//! 2. **Prepared Horizon Mirrors & Custom Catalog Servers** (`horizon_hooks::porter`):
//!    fetches prepared bundle catalogs or custom `.zip` URLs with resumable HTTP `Range` / `ETag`
//!    transfers and optional SHA-256 verification.

use horizon_protocol::adapter::ServerVersion;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Official Mojang version manifest v2 endpoint (whitelisted in `tools/addresses.json` `vanilla`).
pub const MOJANG_VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

/// Official Mojang content-addressed asset CDN (whitelisted in `tools/addresses.json` `vanilla`).
pub const MOJANG_RESOURCES_BASE_URL: &str = "https://resources.download.minecraft.net";

const SELECTION_FILE: &str = "versions/active.json";
const PROFILE_FILE: &str = "version-profile.json";
const MANIFEST_CACHE_FILE: &str = "versions/mojang-manifest-cache.json";
const SCHEMA_VERSION: u32 = 7;

/// Top-level simulation/datagen files produced by `horizon-prepare` (`SCHEMA = 7`).
/// When a full prepared Horizon base is present on the device, `prepare_mojang_on_device` links
/// (via APFS hardlink, falling back to copy) these simulation tables into newly downloaded Mojang
/// version directories so the native Rust engine has complete block/entity/registry/datafix tables.
const BASE_SIMULATION_FILES: &[&str] = &[
    "registries.json",
    "commands.json",
    "command-registries.json",
    "facts.json",
    "block-behaviours.json",
    "block-tags.json",
    "items.json",
    "entity-models.json",
    "render-definitions.json",
    "cached-collision.nbt",
    "occlusion.nbt",
    "movement-properties.nbt",
    "datafix-tables.json",
    "horizon-prepare.json",
];

/// Top-level simulation folders linked when absent from an older Mojang `client.jar`.
const BASE_SIMULATION_DIRS: &[&str] = &["worldgen", "packs"];

/// Active version & cross-version protocol preference stored in `Documents/versions/active.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveSelection {
    pub active: String,
    #[serde(default = "default_true")]
    pub older_servers: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ActiveSelection {
    fn default() -> Self {
        Self {
            active: horizon_core::VANILLA_VERSION.to_owned(),
            older_servers: true,
        }
    }
}

/// Metadata persisted inside `<resources_root>/version-profile.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionProfile {
    pub id: String,
    #[serde(default = "default_release_type")]
    pub release_type: String,
    /// `"horizon-prepared"`, `"mojang-official"`, or `"imported-zip"`.
    pub source: String,
    #[serde(default)]
    pub protocol: Option<i32>,
    #[serde(default)]
    pub protocol_adapter: Option<String>,
    /// `true` when core `horizon-prepare` simulation tables (`registries.json`, `facts.json`, ...)
    /// are present in this version's `resources/` root.
    pub full_simulation: bool,
    pub files: usize,
    pub bytes: u64,
    pub installed_epoch: u64,
    #[serde(default)]
    pub notes: String,
}

fn default_release_type() -> String {
    "release".into()
}

/// Installed version row returned to the SwiftUI launcher.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledVersion {
    pub id: String,
    pub release_type: String,
    pub source: String,
    pub protocol: Option<i32>,
    pub protocol_adapter: Option<String>,
    /// `"native-26.3"`, `"protocol-adapter"`, `"world-datafix-assets"`, or `"classic-assets"`.
    pub support_tier: String,
    pub full_simulation: bool,
    pub ready: bool,
    pub active: bool,
    pub legacy_root: bool,
    pub files: usize,
    pub bytes: u64,
    pub installed_epoch: u64,
    pub path: String,
    pub notes: String,
}

/// Downloadable prepared-bundle mirror or remote catalog entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub label: String,
    pub version: String,
    pub url: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub installed: bool,
}

/// Live status of the background version download/preparation worker.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VersionJobStatus {
    pub active: bool,
    pub version_id: String,
    pub source: String,
    pub stage: String,
    pub detail: String,
    pub received_bytes: u64,
    pub total_bytes: u64,
    pub completed_files: usize,
    pub total_files: usize,
    pub done: bool,
    pub error: Option<String>,
}

struct JobState {
    status: Mutex<VersionJobStatus>,
    cancel: AtomicBool,
}

fn global_job() -> &'static Arc<JobState> {
    static JOB: OnceLock<Arc<JobState>> = OnceLock::new();
    JOB.get_or_init(|| {
        Arc::new(JobState {
            status: Mutex::new(VersionJobStatus {
                stage: "idle".into(),
                ..VersionJobStatus::default()
            }),
            cancel: AtomicBool::new(false),
        })
    })
}

fn sanitize_version_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." || trimmed.len() > 64 {
        return None;
    }
    let clean: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let clean = clean.trim_matches(['.', '-']).to_owned();
    (!clean.is_empty()).then_some(clean)
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Classify how Horizon's native Rust engine supports a given Minecraft version identifier.
pub fn support_tier_for_version(version_id: &str) -> &'static str {
    if version_id.trim() == horizon_core::VANILLA_VERSION {
        return "native-26.3";
    }
    if ServerVersion::from_version_id(version_id).is_some() {
        return "protocol-adapter";
    }
    let v = version_id.trim();
    if v.starts_with("26.")
        || v.starts_with("25w")
        || v.starts_with("24w")
        || v.starts_with("1.21")
        || v.starts_with("1.20")
        || v.starts_with("1.19")
        || v.starts_with("1.18")
        || v.starts_with("1.17")
        || v.starts_with("1.16")
        || v.starts_with("1.15")
        || v.starts_with("1.14")
        || v.starts_with("1.13")
        || v.starts_with("1.12")
        || v.starts_with("1.11")
        || v.starts_with("1.10")
        || v.starts_with("1.9")
        || v.starts_with("1.8")
    {
        "world-datafix-assets"
    } else {
        "classic-assets"
    }
}

/// Read `Documents/versions/active.json` (defaults to `26.3` + `older_servers: true`).
pub fn load_selection(home: &Path) -> ActiveSelection {
    fs::read_to_string(home.join(SELECTION_FILE))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomically persist `Documents/versions/active.json`.
pub fn save_selection(home: &Path, selection: &ActiveSelection) -> Result<(), String> {
    let path = home.join(SELECTION_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(selection).map_err(|e| e.to_string())?;
    fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
}

fn has_simulation_tables(resources_dir: &Path) -> bool {
    resources_dir.join("horizon-resources.json").is_file()
        && resources_dir.join("registries.json").is_file()
        && resources_dir.join("facts.json").is_file()
}

fn is_ready_root(resources_dir: &Path) -> bool {
    resources_dir.join("horizon-resources.json").is_file()
}

fn read_profile(resources_dir: &Path, fallback_id: &str) -> VersionProfile {
    if let Ok(text) = fs::read_to_string(resources_dir.join(PROFILE_FILE))
        && let Ok(mut parsed) = serde_json::from_str::<VersionProfile>(&text)
    {
        parsed.full_simulation = parsed.full_simulation || has_simulation_tables(resources_dir);
        return parsed;
    }
    let full = has_simulation_tables(resources_dir);
    let protocol = ServerVersion::protocol_for_version_id(fallback_id);
    let adapter = ServerVersion::from_version_id(fallback_id).map(|v| format!("{v:?}"));
    VersionProfile {
        id: fallback_id.to_owned(),
        release_type: "release".into(),
        source: "horizon-prepared".into(),
        protocol,
        protocol_adapter: adapter,
        full_simulation: full,
        files: 0,
        bytes: 0,
        installed_epoch: 0,
        notes: if full {
            "Prepared Horizon runtime & assets.".into()
        } else {
            "Imported Horizon resource directory.".into()
        },
    }
}

fn write_profile(resources_dir: &Path, profile: &VersionProfile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(profile).map_err(|e| e.to_string())?;
    fs::write(resources_dir.join(PROFILE_FILE), bytes)
        .map_err(|e| format!("{}: {e}", resources_dir.display()))
}

/// List all installed versions under `home/versions/*/resources` plus legacy `home/resources`.
pub fn list_installed(home: &Path) -> Vec<InstalledVersion> {
    let sel = load_selection(home);
    let mut map: BTreeMap<String, InstalledVersion> = BTreeMap::new();

    let versions_dir = home.join("versions");
    if let Ok(entries) = fs::read_dir(&versions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(folder_id) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if folder_id.starts_with('.') {
                continue;
            }
            let res = path.join("resources");
            if !is_ready_root(&res) {
                continue;
            }
            let prof = read_profile(&res, folder_id);
            let id = prof.id.clone();
            let ready = is_ready_root(&res);
            map.insert(
                id.clone(),
                InstalledVersion {
                    support_tier: support_tier_for_version(&id).into(),
                    protocol: prof
                        .protocol
                        .or_else(|| ServerVersion::protocol_for_version_id(&id)),
                    protocol_adapter: prof
                        .protocol_adapter
                        .or_else(|| ServerVersion::from_version_id(&id).map(|v| format!("{v:?}"))),
                    id,
                    release_type: prof.release_type,
                    source: prof.source,
                    full_simulation: prof.full_simulation,
                    ready,
                    active: false,
                    legacy_root: false,
                    files: prof.files,
                    bytes: prof.bytes,
                    installed_epoch: prof.installed_epoch,
                    path: res.to_string_lossy().into_owned(),
                    notes: prof.notes,
                },
            );
        }
    }

    let legacy = home.join("resources");
    if is_ready_root(&legacy) {
        let inferred_id = fs::read_to_string(legacy.join("horizon-resources.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| {
                v.get("source_minecraft")
                    .or_else(|| v.get("minecraft"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| horizon_core::VANILLA_VERSION.to_owned());
        if !map.contains_key(&inferred_id) {
            let prof = read_profile(&legacy, &inferred_id);
            map.insert(
                inferred_id.clone(),
                InstalledVersion {
                    support_tier: support_tier_for_version(&inferred_id).into(),
                    protocol: prof
                        .protocol
                        .or_else(|| ServerVersion::protocol_for_version_id(&inferred_id)),
                    protocol_adapter: prof.protocol_adapter.or_else(|| {
                        ServerVersion::from_version_id(&inferred_id).map(|v| format!("{v:?}"))
                    }),
                    id: inferred_id,
                    release_type: prof.release_type,
                    source: prof.source,
                    full_simulation: prof.full_simulation,
                    ready: true,
                    active: false,
                    legacy_root: true,
                    files: prof.files,
                    bytes: prof.bytes,
                    installed_epoch: prof.installed_epoch,
                    path: legacy.to_string_lossy().into_owned(),
                    notes: prof.notes,
                },
            );
        }
    }

    let mut list: Vec<InstalledVersion> = map.into_values().collect();
    list.sort_by(|a, b| {
        (a.id != horizon_core::VANILLA_VERSION)
            .cmp(&(b.id != horizon_core::VANILLA_VERSION))
            .then_with(|| b.installed_epoch.cmp(&a.installed_epoch))
            .then_with(|| b.id.cmp(&a.id))
    });

    if let Some(chosen) = list.iter_mut().find(|v| v.id == sel.active && v.ready) {
        chosen.active = true;
    } else if let Some(first) = list.iter_mut().find(|v| v.ready) {
        first.active = true;
    }
    list
}

/// Return the active version's `resources/` directory when at least one ready version exists.
pub fn active_ready_root(home: &Path) -> Option<PathBuf> {
    list_installed(home)
        .into_iter()
        .find(|v| v.active && v.ready)
        .map(|v| PathBuf::from(v.path))
}

/// Return the active version ID (falling back to `26.3` before any files are installed).
pub fn active_version_id(home: &Path) -> String {
    list_installed(home)
        .into_iter()
        .find(|v| v.active)
        .map(|v| v.id)
        .unwrap_or_else(|| load_selection(home).active)
}

/// Synchronize `horizon_protocol::adapter`'s preferred protocol and `datafix` resources root with
/// the active version profile in `home`.
pub fn apply_active_protocol_preference(home: &Path) {
    let sel = load_selection(home);
    let active_id = active_version_id(home);
    if sel.older_servers {
        let target = ServerVersion::from_version_id(&active_id)
            .filter(|v| *v != ServerVersion::Current);
        horizon_protocol::adapter::set_preferred_server_version(target);
    } else {
        horizon_protocol::adapter::set_preferred_server_version(None);
    }
    if let Some(root) = active_ready_root(home) {
        horizon_world::datafix::set_resources(&root);
    }
}

/// Switch the active version in `Documents/versions/active.json` and update protocol preferences.
pub fn select_version(home: &Path, version_id: &str) -> Result<InstalledVersion, String> {
    let clean = sanitize_version_id(version_id).ok_or("invalid version id")?;
    let installed = list_installed(home);
    let found = installed
        .into_iter()
        .find(|v| v.id == clean && v.ready)
        .ok_or_else(|| format!("Minecraft {clean} is not installed yet"))?;
    let mut sel = load_selection(home);
    sel.active = clean;
    save_selection(home, &sel)?;
    apply_active_protocol_preference(home);
    let mut updated = found;
    updated.active = true;
    Ok(updated)
}

/// Delete an installed version directory (and fall back cleanly to another installed version).
pub fn delete_version(home: &Path, version_id: &str) -> Result<(), String> {
    let clean = sanitize_version_id(version_id).ok_or("invalid version id")?;
    let version_dir = home.join("versions").join(&clean);
    let mut removed = false;
    if version_dir.exists() {
        fs::remove_dir_all(&version_dir)
            .map_err(|e| format!("{}: {e}", version_dir.display()))?;
        removed = true;
    }
    let legacy = home.join("resources");
    if !removed && is_ready_root(&legacy) {
        let list = list_installed(home);
        if list.iter().any(|v| v.id == clean && v.legacy_root) {
            fs::remove_dir_all(&legacy).map_err(|e| format!("{}: {e}", legacy.display()))?;
            removed = true;
        }
    }
    if !removed {
        return Err(format!("Minecraft {clean} is not installed"));
    }
    let mut sel = load_selection(home);
    if sel.active == clean {
        sel.active = list_installed(home)
            .into_iter()
            .find(|v| v.ready)
            .map(|v| v.id)
            .unwrap_or_else(|| horizon_core::VANILLA_VERSION.to_owned());
        let _ = save_selection(home, &sel);
    }
    apply_active_protocol_preference(home);
    Ok(())
}

/// Locate any installed resource root that holds full `horizon-prepare` simulation tables.
pub fn find_simulation_base(home: &Path) -> Option<PathBuf> {
    let legacy = home.join("resources");
    if has_simulation_tables(&legacy) {
        return Some(legacy);
    }
    let versions_dir = home.join("versions");
    let entries = fs::read_dir(versions_dir).ok()?;
    for entry in entries.flatten() {
        let res = entry.path().join("resources");
        if has_simulation_tables(&res) {
            return Some(res);
        }
    }
    None
}

fn link_or_copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst).map(|_| ())
}

fn link_missing_tree(src_dir: &Path, dst_dir: &Path) -> std::io::Result<usize> {
    if !src_dir.is_dir() {
        return Ok(0);
    }
    fs::create_dir_all(dst_dir)?;
    let mut count = 0usize;
    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst_dir.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            count += link_missing_tree(&src_path, &dst_path)?;
        } else if ty.is_file() && !dst_path.exists() {
            link_or_copy_file(&src_path, &dst_path)?;
            count += 1;
        }
    }
    Ok(count)
}

fn populate_from_simulation_base(base: &Path, target_resources: &Path, version_id: &str) -> usize {
    let mut linked = 0usize;
    for file_name in BASE_SIMULATION_FILES {
        let src = base.join(file_name);
        let dst = target_resources.join(file_name);
        if src.is_file() && !dst.exists() && link_or_copy_file(&src, &dst).is_ok() {
            linked += 1;
        }
    }
    for dir_name in BASE_SIMULATION_DIRS {
        let src = base.join(dir_name);
        let dst = target_resources.join(dir_name);
        if let Ok(n) = link_missing_tree(&src, &dst) {
            linked += n;
        }
    }
    // Older Mojang jars (<1.21.4) do not have modern `assets/minecraft/items/*.json` item model
    // definitions or newer GUI sprite slices: fill ONLY missing files from the base so the
    // downloaded version's own textures/models/sounds always take priority!
    for sub in [
        "assets/minecraft/items",
        "assets/minecraft/textures/gui/sprites",
        "assets/minecraft/shaders",
        "assets/minecraft/font",
        "data/minecraft",
    ] {
        if let Ok(n) = link_missing_tree(&base.join(sub), &target_resources.join(sub)) {
            linked += n;
        }
    }
    // Update `horizon-resources.json` so `KnownCore::load` recognizes the linked 26.3 core
    // registries while preserving `source_minecraft`.
    let manifest = json!({
        "schema": SCHEMA_VERSION,
        "minecraft": horizon_core::VANILLA_VERSION,
        "source_minecraft": version_id,
        "prepared_by": "horizon-on-device",
        "full_simulation": true,
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&manifest) {
        let _ = fs::write(target_resources.join("horizon-resources.json"), bytes);
    }
    linked
}

/// Upgrade any previously downloaded Mojang versions that were prepared before a full simulation
/// base arrived on the device.
pub fn upgrade_partial_mojang_versions(home: &Path, base_resources: &Path) -> usize {
    if !has_simulation_tables(base_resources) {
        return 0;
    }
    let Ok(entries) = fs::read_dir(home.join("versions")) else {
        return 0;
    };
    let mut upgraded = 0usize;
    for entry in entries.flatten() {
        let res = entry.path().join("resources");
        if !is_ready_root(&res) || res == base_resources || has_simulation_tables(&res) {
            continue;
        }
        let Some(folder_id) = entry.path().file_name().and_then(|s| s.to_str()).map(str::to_owned)
        else {
            continue;
        };
        let mut prof = read_profile(&res, &folder_id);
        let added = populate_from_simulation_base(base_resources, &res, &prof.id);
        if has_simulation_tables(&res) {
            prof.full_simulation = true;
            prof.files += added;
            prof.notes = format!(
                "Mojang {} assets + linked Horizon simulation base ({added} runtime files linked).",
                prof.id
            );
            let _ = write_profile(&res, &prof);
            upgraded += 1;
        }
    }
    upgraded
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(45))
        .timeout_write(Duration::from_secs(30))
        .build()
}

fn fetch_json_url(url: &str) -> Result<Value, String> {
    if let Some(path) = url.strip_prefix("file://") {
        let text = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        return serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"));
    }
    let resp = http_agent()
        .get(url)
        .set("User-Agent", "horizon-ios-multiversion/1.0")
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let mut body = String::new();
    resp.into_reader()
        .take(32 << 20)
        .read_to_string(&mut body)
        .map_err(|e| format!("{url}: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("{url}: {e}"))
}

fn fetch_bytes_verified(url: &str, expected_sha1: Option<&str>, max_bytes: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    if let Some(path) = url.strip_prefix("file://") {
        fs::File::open(path)
            .map_err(|e| format!("{path}: {e}"))?
            .take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{path}: {e}"))?;
    } else {
        let resp = http_agent()
            .get(url)
            .set("User-Agent", "horizon-ios-multiversion/1.0")
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        resp.into_reader()
            .take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{url}: {e}"))?;
    }
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{url}: exceeds size limit ({max_bytes} bytes)"));
    }
    if let Some(want) = expected_sha1.filter(|s| !s.is_empty()) {
        let got = hex_lower(&Sha1::digest(&bytes));
        if !got.eq_ignore_ascii_case(want) {
            return Err(format!("{url}: SHA-1 mismatch (expected {want}, got {got})"));
        }
    }
    Ok(bytes)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Fetch built-in and remote prepared Horizon mirrors (`versions-catalog.json`).
pub fn fetch_prepared_catalog(
    home: &Path,
    custom_catalog_url: Option<&str>,
) -> Result<Vec<CatalogEntry>, String> {
    let installed_ids: std::collections::HashSet<String> = list_installed(home)
        .into_iter()
        .filter(|v| v.ready && v.full_simulation)
        .map(|v| v.id)
        .collect();

    let mut out: Vec<CatalogEntry> = horizon_hooks::porter::engine::prepared_mirrors()
        .into_iter()
        .map(|m| CatalogEntry {
            installed: installed_ids.contains(&m.version),
            id: m.id,
            label: m.label,
            version: m.version,
            url: m.url,
            notes: m.notes,
            sha256: m.sha256,
            bytes: m.bytes,
        })
        .collect();

    let mut urls = Vec::new();
    if let Some(custom) = custom_catalog_url.map(str::trim).filter(|s| !s.is_empty()) {
        urls.push(custom.to_owned());
    } else if let Ok(env_url) = std::env::var("HORIZON_VERSION_CATALOG_URL")
        && !env_url.trim().is_empty()
    {
        urls.push(env_url.trim().to_owned());
    } else {
        urls.extend(horizon_hooks::porter::engine::catalog_urls());
    }

    let mut last_err = None;
    for url in urls {
        match fetch_json_url(&url) {
            Ok(doc) => {
                let items = doc
                    .get("versions")
                    .or_else(|| doc.get("mirrors"))
                    .and_then(Value::as_array);
                if let Some(arr) = items {
                    for raw in arr {
                        let Some(entry_url) = raw["url"].as_str().filter(|u| !u.is_empty()) else {
                            continue;
                        };
                        let version = raw["version"]
                            .as_str()
                            .unwrap_or(horizon_core::VANILLA_VERSION)
                            .to_owned();
                        let id = raw["id"].as_str().unwrap_or(entry_url).to_owned();
                        if let Some(existing) = out.iter_mut().find(|e| e.id == id || e.url == entry_url) {
                            if let Some(lbl) = raw["label"].as_str() {
                                existing.label = lbl.to_owned();
                            }
                            if let Some(notes) = raw["notes"].as_str() {
                                existing.notes = notes.to_owned();
                            }
                            existing.sha256 = raw["sha256"].as_str().map(str::to_owned);
                            existing.bytes = raw["bytes"].as_u64();
                            existing.installed = installed_ids.contains(&version);
                        } else {
                            out.push(CatalogEntry {
                                installed: installed_ids.contains(&version),
                                id,
                                label: raw["label"]
                                    .as_str()
                                    .unwrap_or("Prepared Horizon Bundle")
                                    .to_owned(),
                                version,
                                url: entry_url.to_owned(),
                                notes: raw["notes"].as_str().unwrap_or_default().to_owned(),
                                sha256: raw["sha256"].as_str().map(str::to_owned),
                                bytes: raw["bytes"].as_u64(),
                            });
                        }
                    }
                }
                if custom_catalog_url.is_some() {
                    return Ok(out);
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    if custom_catalog_url.is_some()
        && let Some(err) = last_err
    {
        return Err(err);
    }
    Ok(out)
}

fn load_mojang_manifest(home: &Path, refresh: bool) -> Result<Value, String> {
    let cache_path = home.join(MANIFEST_CACHE_FILE);
    let url = std::env::var("HORIZON_MOJANG_MANIFEST_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| MOJANG_VERSION_MANIFEST_URL.to_owned());

    if !refresh
        && let Ok(meta) = fs::metadata(&cache_path)
        && meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < Duration::from_secs(900))
        && let Ok(text) = fs::read_to_string(&cache_path)
        && let Ok(val) = serde_json::from_str::<Value>(&text)
    {
        return Ok(val);
    }

    match fetch_json_url(&url) {
        Ok(val) => {
            if let Some(parent) = cache_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(bytes) = serde_json::to_vec(&val) {
                let _ = fs::write(&cache_path, bytes);
            }
            Ok(val)
        }
        Err(err) => {
            if let Ok(text) = fs::read_to_string(&cache_path)
                && let Ok(val) = serde_json::from_str::<Value>(&text)
            {
                Ok(val)
            } else {
                Err(err)
            }
        }
    }
}

/// Browse/search official Mojang releases, snapshots, beta, and alpha versions.
pub fn fetch_mojang_versions(
    home: &Path,
    kind_filter: Option<&str>,
    query: &str,
    limit: usize,
    offset: usize,
    refresh: bool,
) -> Result<Value, String> {
    let manifest = load_mojang_manifest(home, refresh)?;
    let installed = list_installed(home);
    let installed_map: BTreeMap<String, &InstalledVersion> =
        installed.iter().map(|v| (v.id.clone(), v)).collect();

    let kind = kind_filter.unwrap_or("release").trim();
    let q = query.trim().to_ascii_lowercase();
    let limit = limit.clamp(1, 250);

    let filtered: Vec<&Value> = manifest["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| {
            let vtype = v["type"].as_str().unwrap_or_default();
            let type_ok = match kind {
                "" | "all" => true,
                "release" => vtype == "release",
                "snapshot" => vtype == "snapshot",
                "historical" | "old" => vtype == "old_beta" || vtype == "old_alpha",
                other => vtype == other,
            };
            if !type_ok {
                return false;
            }
            if q.is_empty() {
                return true;
            }
            let id = v["id"].as_str().unwrap_or_default().to_ascii_lowercase();
            id.contains(&q) || vtype.to_ascii_lowercase().contains(&q)
        })
        .collect();

    let total = filtered.len();
    let rows: Vec<Value> = filtered
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|v| {
            let id = v["id"].as_str().unwrap_or_default();
            let inst = installed_map.get(id).copied();
            json!({
                "id": id,
                "type": v["type"].as_str().unwrap_or("release"),
                "url": v["url"].as_str().unwrap_or_default(),
                "sha1": v["sha1"].as_str(),
                "release_time": v["releaseTime"].as_str().unwrap_or_default(),
                "protocol": ServerVersion::protocol_for_version_id(id),
                "protocol_adapter": ServerVersion::from_version_id(id).map(|s| format!("{s:?}")),
                "support_tier": support_tier_for_version(id),
                "installed": inst.is_some(),
                "ready": inst.is_some_and(|i| i.ready),
                "active": inst.is_some_and(|i| i.active),
            })
        })
        .collect();

    Ok(json!({
        "latest_release": manifest["latest"]["release"].as_str().unwrap_or(horizon_core::VANILLA_VERSION),
        "latest_snapshot": manifest["latest"]["snapshot"].as_str().unwrap_or_default(),
        "has_simulation_base": find_simulation_base(home).is_some(),
        "total": total,
        "offset": offset,
        "limit": limit,
        "versions": rows,
    }))
}

/// Snapshot current background download/preparation status.
pub fn job_status() -> VersionJobStatus {
    global_job()
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Request cancellation of an in-flight version download.
pub fn cancel_job() {
    let job = global_job();
    job.cancel.store(true, Ordering::Relaxed);
    if let Ok(mut st) = job.status.lock()
        && st.active
    {
        st.detail = "Cancelling download…".into();
    }
}

fn begin_job(version_id: &str, source: &str, initial_detail: &str) -> Result<Arc<JobState>, String> {
    let job = global_job().clone();
    let mut st = job.status.lock().unwrap_or_else(|e| e.into_inner());
    if st.active {
        return Err(format!(
            "Already installing Minecraft {} ({}). Cancel or wait for it to finish.",
            st.version_id, st.stage
        ));
    }
    job.cancel.store(false, Ordering::Relaxed);
    *st = VersionJobStatus {
        active: true,
        version_id: version_id.to_owned(),
        source: source.to_owned(),
        stage: "starting".into(),
        detail: initial_detail.to_owned(),
        received_bytes: 0,
        total_bytes: 0,
        completed_files: 0,
        total_files: 0,
        done: false,
        error: None,
    };
    Ok(job)
}

fn update_job(job: &JobState, f: impl FnOnce(&mut VersionJobStatus)) {
    if let Ok(mut st) = job.status.lock() {
        f(&mut st);
    }
}

fn check_cancelled(job: &JobState) -> Result<(), String> {
    if job.cancel.load(Ordering::Relaxed) {
        Err("Download cancelled.".into())
    } else {
        Ok(())
    }
}

fn verify_file_sha256(path: &Path, expected_hex: &str) -> Result<(), String> {
    let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex_lower(&hasher.finalize());
    if !got.eq_ignore_ascii_case(expected_hex.trim()) {
        return Err(format!(
            "SHA-256 mismatch for {} (expected {}, got {got})",
            path.display(),
            expected_hex.trim()
        ));
    }
    Ok(())
}

/// Start downloading & installing a prepared Horizon `.zip` bundle into `Documents/versions/<version>/resources/`.
pub fn start_prepared_download(
    home: PathBuf,
    url: String,
    version_hint: Option<String>,
    expected_sha256: Option<String>,
) -> Result<VersionJobStatus, String> {
    let url = url.trim().to_owned();
    if url.is_empty() {
        return Err("missing prepared archive URL".into());
    }
    let version_id = version_hint
        .as_deref()
        .and_then(sanitize_version_id)
        .unwrap_or_else(|| horizon_core::VANILLA_VERSION.to_owned());

    let job = begin_job(&version_id, "horizon-prepared", &url)?;
    let initial = job_status();

    std::thread::spawn(move || {
        let outcome = run_prepared_download(
            &job,
            &home,
            &url,
            &version_id,
            expected_sha256.as_deref(),
        );
        update_job(&job, |st| {
            st.active = false;
            match outcome {
                Ok(()) => {
                    st.stage = "done".into();
                    st.done = true;
                    st.detail = format!("Minecraft {version_id} is ready.");
                }
                Err(err) => {
                    st.stage = "failed".into();
                    st.done = false;
                    st.error = Some(err);
                }
            }
        });
    });

    Ok(initial)
}

fn run_prepared_download(
    job: &Arc<JobState>,
    home: &Path,
    url: &str,
    version_id: &str,
    expected_sha256: Option<&str>,
) -> Result<(), String> {
    let work_dir = home.join("versions").join(format!(".download-{version_id}"));
    fs::create_dir_all(&work_dir).map_err(|e| format!("{}: {e}", work_dir.display()))?;

    let archive_path = if let Some(local_path) = url.strip_prefix("file://") {
        let copy = work_dir.join("local-prepared.zip");
        fs::copy(local_path, &copy).map_err(|e| format!("{local_path}: {e}"))?;
        copy
    } else {
        update_job(job, |st| {
            st.stage = "downloading".into();
            st.detail = format!("Downloading prepared bundle for {version_id}…");
        });
        let received = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mon_rec = received.clone();
        let mon_tot = total.clone();
        let mon_job = job.clone();
        let done_flag = Arc::new(AtomicBool::new(false));
        let mon_done = done_flag.clone();
        let monitor = std::thread::spawn(move || {
            while !mon_done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                update_job(&mon_job, |st| {
                    st.received_bytes = mon_rec.load(Ordering::Relaxed);
                    st.total_bytes = mon_tot.load(Ordering::Relaxed);
                });
            }
        });
        let fetched =
            crate::import::download::fetch_resumable(url, &work_dir, &received, &total);
        done_flag.store(true, Ordering::Relaxed);
        let _ = monitor.join();
        update_job(job, |st| {
            st.received_bytes = received.load(Ordering::Relaxed);
            st.total_bytes = total.load(Ordering::Relaxed);
        });
        fetched?
    };

    check_cancelled(job)?;
    if let Some(sha) = expected_sha256.filter(|s| !s.trim().is_empty()) {
        update_job(job, |st| {
            st.stage = "verifying".into();
            st.detail = "Verifying SHA-256 checksum…".into();
        });
        verify_file_sha256(&archive_path, sha)?;
    }

    update_job(job, |st| {
        st.stage = "installing".into();
        st.detail = format!("Unpacking prepared game files for {version_id}…");
    });
    let pack = crate::import::Pack::open(&archive_path)?;
    if pack.kind() != crate::import::Kind::Resources {
        let _ = fs::remove_dir_all(&work_dir);
        return Err("Archive is not a prepared Horizon game-files bundle (missing horizon-resources.json).".into());
    }
    let version_home = home.join("versions").join(version_id);
    fs::create_dir_all(&version_home).map_err(|e| format!("{}: {e}", version_home.display()))?;
    let installed = pack.install(&version_home)?;
    let _ = fs::remove_dir_all(&work_dir);

    let full_sim = has_simulation_tables(&installed.path);
    let profile = VersionProfile {
        id: version_id.to_owned(),
        release_type: "release".into(),
        source: "horizon-prepared".into(),
        protocol: ServerVersion::protocol_for_version_id(version_id),
        protocol_adapter: ServerVersion::from_version_id(version_id).map(|v| format!("{v:?}")),
        full_simulation: full_sim,
        files: installed.files,
        bytes: installed.bytes,
        installed_epoch: now_epoch(),
        notes: "Prepared Horizon runtime & assets.".into(),
    };
    write_profile(&installed.path, &profile)?;

    if full_sim {
        let _ = upgrade_partial_mojang_versions(home, &installed.path);
    }

    let mut sel = load_selection(home);
    sel.active = version_id.to_owned();
    save_selection(home, &sel)?;
    apply_active_protocol_preference(home);
    Ok(())
}

/// Start downloading & preparing an official Mojang version (`client.jar` + asset index + objects)
/// directly on the device into `Documents/versions/<id>/resources/`.
pub fn start_mojang_download(home: PathBuf, version_id: String) -> Result<VersionJobStatus, String> {
    let clean = sanitize_version_id(&version_id).ok_or("invalid Mojang version id")?;
    let job = begin_job(&clean, "mojang-official", "Fetching Mojang version manifest…")?;
    let initial = job_status();

    std::thread::spawn(move || {
        let outcome = run_mojang_download(&job, &home, &clean);
        update_job(&job, |st| {
            st.active = false;
            match outcome {
                Ok(()) => {
                    st.stage = "done".into();
                    st.done = true;
                    st.detail = format!("Minecraft {clean} prepared on device.");
                }
                Err(err) => {
                    st.stage = "failed".into();
                    st.done = false;
                    st.error = Some(err);
                }
            }
        });
    });

    Ok(initial)
}

fn run_mojang_download(job: &Arc<JobState>, home: &Path, version_id: &str) -> Result<(), String> {
    update_job(job, |st| {
        st.stage = "manifest".into();
        st.detail = format!("Locating {version_id} in Mojang manifest…");
    });
    let manifest = load_mojang_manifest(home, false)?;
    let entry = manifest["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|v| v["id"].as_str() == Some(version_id))
        .ok_or_else(|| format!("Version {version_id} was not found in Mojang's manifest"))?;

    let release_type = entry["type"].as_str().unwrap_or("release").to_owned();
    let version_json_url = entry["url"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Mojang manifest entry for {version_id} has no version URL"))?;
    let version_json_sha1 = entry["sha1"].as_str();

    check_cancelled(job)?;
    let version_json_bytes = fetch_bytes_verified(version_json_json_url(version_json_url), version_json_sha1, 16 << 20)?;
    let version_meta: Value =
        serde_json::from_slice(&version_json_bytes).map_err(|e| format!("{version_id}.json: {e}"))?;

    let client_dl = &version_meta["downloads"]["client"];
    let client_url = client_dl["url"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{version_id}.json has no client.jar download URL"))?;
    let client_sha1 = client_dl["sha1"].as_str();
    let client_size = client_dl["size"].as_u64().unwrap_or(0);

    let mc_cache = home.join("minecraft");
    let jar_dir = mc_cache.join("versions").join(version_id);
    fs::create_dir_all(&jar_dir).map_err(|e| format!("{}: {e}", jar_dir.display()))?;
    fs::write(jar_dir.join(format!("{version_id}.json")), &version_json_bytes)
        .map_err(|e| e.to_string())?;

    let jar_path = jar_dir.join(format!("{version_id}.jar"));
    let jar_ok = client_sha1.is_some_and(|want| {
        fs::read(&jar_path)
            .ok()
            .is_some_and(|b| hex_lower(&Sha1::digest(&b)).eq_ignore_ascii_case(want))
    });
    if !jar_ok {
        update_job(job, |st| {
            st.stage = "client-jar".into();
            st.total_bytes = client_size;
            st.detail = format!("Downloading Mojang client.jar for {version_id}…");
        });
        let bytes = fetch_bytes_verified(client_url, client_sha1, 512 << 20)?;
        update_job(job, |st| {
            st.received_bytes = bytes.len() as u64;
        });
        fs::write(&jar_path, &bytes).map_err(|e| format!("{}: {e}", jar_path.display()))?;
    }

    check_cancelled(job)?;
    let asset_index_info = &version_meta["assetIndex"];
    let asset_index_id = asset_index_info["id"].as_str().unwrap_or(version_id);
    let asset_index_url = asset_index_info["url"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{version_id}.json has no assetIndex URL"))?;
    let asset_index_sha1 = asset_index_info["sha1"].as_str();

    let indexes_dir = mc_cache.join("assets/indexes");
    fs::create_dir_all(&indexes_dir).map_err(|e| e.to_string())?;
    let index_path = indexes_dir.join(format!("{asset_index_id}.json"));
    let index_bytes = fetch_bytes_verified(asset_index_url, asset_index_sha1, 32 << 20)?;
    fs::write(&index_path, &index_bytes).map_err(|e| e.to_string())?;
    let asset_index: Value =
        serde_json::from_slice(&index_bytes).map_err(|e| format!("assetIndex: {e}"))?;

    let objects_dir = mc_cache.join("assets/objects");
    fs::create_dir_all(&objects_dir).map_err(|e| e.to_string())?;

    let mut tasks: Vec<(String, String, u64)> = Vec::new();
    if let Some(objs) = asset_index["objects"].as_object() {
        for (logical_name, meta) in objs {
            let Some(hash) = meta["hash"]
                .as_str()
                .filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            else {
                continue;
            };
            let size = meta["size"].as_u64().unwrap_or(0);
            tasks.push((logical_name.clone(), hash.to_ascii_lowercase(), size));
        }
    }

    let total_files = tasks.len();
    let total_asset_bytes: u64 = tasks.iter().map(|(_, _, sz)| *sz).sum();
    update_job(job, |st| {
        st.stage = "assets".into();
        st.completed_files = 0;
        st.total_files = total_files;
        st.received_bytes = 0;
        st.total_bytes = total_asset_bytes;
        st.detail = format!("Downloading {total_files} Mojang asset objects…");
    });

    let resources_base = std::env::var("HORIZON_MOJANG_RESOURCES_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| MOJANG_RESOURCES_BASE_URL.to_owned());
    let resources_base = resources_base.trim_end_matches('/').to_owned();

    let next_idx = AtomicUsize::new(0);
    let completed = AtomicUsize::new(0);
    let downloaded_bytes = AtomicU64::new(0);
    let first_error: Mutex<Option<String>> = Mutex::new(None);

    let worker_count = tasks.len().clamp(1, 8);
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    if job.cancel.load(Ordering::Relaxed)
                        || first_error.lock().unwrap_or_else(|e| e.into_inner()).is_some()
                    {
                        break;
                    }
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                    if idx >= tasks.len() {
                        break;
                    }
                    let (_, ref hash, size) = tasks[idx];
                    let prefix = &hash[..2];
                    let obj_path = objects_dir.join(prefix).join(hash);
                    let already_valid = fs::metadata(&obj_path)
                        .ok()
                        .is_some_and(|m| size == 0 || m.len() == size);
                    if !already_valid {
                        let url = format!("{resources_base}/{prefix}/{hash}");
                        match fetch_bytes_verified(&url, Some(hash), 128 << 20) {
                            Ok(bytes) => {
                                if let Some(parent) = obj_path.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }
                                let tmp = obj_path.with_extension("part");
                                if let Err(e) =
                                    fs::write(&tmp, &bytes).and_then(|()| fs::rename(&tmp, &obj_path))
                                {
                                    *first_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(format!("{}: {e}", obj_path.display()));
                                    break;
                                }
                            }
                            Err(e) => {
                                *first_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                                break;
                            }
                        }
                    }
                    let done_now = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let bytes_now =
                        downloaded_bytes.fetch_add(size, Ordering::Relaxed) + size;
                    if done_now % 8 == 0 || done_now == total_files {
                        update_job(job, |st| {
                            st.completed_files = done_now;
                            st.received_bytes = bytes_now;
                        });
                    }
                }
            });
        }
    });

    check_cancelled(job)?;
    if let Some(err) = first_error.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(err);
    }

    update_job(job, |st| {
        st.stage = "preparing".into();
        st.detail = format!("Preparing {version_id} resources on device…");
    });

    prepare_mojang_on_device(
        home,
        version_id,
        &release_type,
        &jar_path,
        &asset_index,
        &objects_dir,
    )?;

    let mut sel = load_selection(home);
    sel.active = version_id.to_owned();
    save_selection(home, &sel)?;
    apply_active_protocol_preference(home);
    Ok(())
}

fn version_json_json_url(url: &str) -> &str {
    url
}

/// Pure-Rust on-device resource builder for any Mojang `client.jar` + verified `assetIndex`:
/// - Unpacks `assets/`, `data/`, and `pack.mcmeta` from `client.jar`.
/// - Materializes indexed Mojang assets (`sounds/`, `lang/`, `font/`, `textures/gui/title/background/`, `pack.png`, etc.)
///   into `assets/<namespace>/...` via zero-copy APFS hardlinks from `minecraft/assets/objects/`.
/// - Creates pre-1.13 flattening aliases (`textures/blocks/*` -> `textures/block/*`,
///   `textures/items/*` -> `textures/item/*`) so classic/1.8/1.12 Mojang textures map onto
///   modern resource locations cleanly.
/// - Links simulation/datagen tables from any installed Horizon base runtime (`find_simulation_base`).
pub fn prepare_mojang_on_device(
    home: &Path,
    version_id: &str,
    release_type: &str,
    jar_path: &Path,
    asset_index: &Value,
    objects_dir: &Path,
) -> Result<VersionProfile, String> {
    let clean = sanitize_version_id(version_id).ok_or("invalid version id")?;
    let sim_base = find_simulation_base(home);

    let version_dir = home.join("versions").join(&clean);
    let staging = version_dir.join(".resources.staging");
    let final_root = version_dir.join("resources");
    let backup = version_dir.join(".resources.old");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;

    let jar_file = fs::File::open(jar_path).map_err(|e| format!("{}: {e}", jar_path.display()))?;
    let mut archive =
        zip::ZipArchive::new(jar_file).map_err(|e| format!("{}: {e}", jar_path.display()))?;

    let mut files = 0usize;
    let mut bytes = 0u64;

    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let keep = name == "pack.mcmeta"
            || name == "pack.png"
            || name.starts_with("assets/")
            || name.starts_with("data/");
        if !keep || name.contains("..") || name.starts_with('/') {
            continue;
        }
        let out_path = staging.join(&name);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut out_file =
            fs::File::create(&out_path).map_err(|e| format!("{}: {e}", out_path.display()))?;
        let written = std::io::copy(&mut entry, &mut out_file).map_err(|e| e.to_string())?;
        files += 1;
        bytes += written;
    }

    // Materialize indexed Mojang objects into `staging/assets/<logical_path>`.
    if let Some(objs) = asset_index["objects"].as_object() {
        for (logical, meta) in objs {
            let Some(hash) = meta["hash"].as_str().filter(|h| h.len() >= 2) else {
                continue;
            };
            let clean_rel = logical.trim_start_matches('/');
            if clean_rel.is_empty() || clean_rel.contains("..") {
                continue;
            }
            let src = objects_dir
                .join(&hash[..2].to_ascii_lowercase())
                .join(hash.to_ascii_lowercase());
            if !src.is_file() {
                continue;
            }
            let dst = if clean_rel.starts_with("assets/") {
                staging.join(clean_rel)
            } else if clean_rel == "pack.mcmeta" || clean_rel.starts_with("pack.") {
                staging.join(clean_rel)
            } else {
                staging.join("assets").join(clean_rel)
            };
            if link_or_copy_file(&src, &dst).is_ok() {
                files += 1;
                bytes += meta["size"].as_u64().unwrap_or(0);
            }
        }
    }

    // Pre-1.13 texture folder aliases (`textures/blocks` -> `textures/block`, `textures/items` -> `textures/item`).
    for (legacy_sub, modern_sub) in [
        ("assets/minecraft/textures/blocks", "assets/minecraft/textures/block"),
        ("assets/minecraft/textures/items", "assets/minecraft/textures/item"),
    ] {
        let src = staging.join(legacy_sub);
        let dst = staging.join(modern_sub);
        if src.is_dir()
            && let Ok(added) = link_missing_tree(&src, &dst)
        {
            files += added;
        }
    }

    let mut full_simulation = false;
    let mut notes = format!("Official Mojang {clean} assets & data unpacked on device.");
    if let Some(base) = sim_base.as_deref() {
        let linked = populate_from_simulation_base(base, &staging, &clean);
        full_simulation = has_simulation_tables(&staging);
        if full_simulation {
            files += linked;
            notes = format!(
                "Mojang {clean} assets + linked Horizon simulation runtime ({linked} base files)."
            );
        }
    } else {
        let manifest = json!({
            "schema": SCHEMA_VERSION,
            "minecraft": clean,
            "source_minecraft": clean,
            "prepared_by": "horizon-on-device",
            "full_simulation": false,
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        fs::write(staging.join("horizon-resources.json"), manifest_bytes)
            .map_err(|e| e.to_string())?;
        notes.push_str(
            " Install one Prepared Horizon Base bundle to link simulation tables automatically.",
        );
    }

    let profile = VersionProfile {
        id: clean.clone(),
        release_type: release_type.to_owned(),
        source: "mojang-official".into(),
        protocol: ServerVersion::protocol_for_version_id(&clean),
        protocol_adapter: ServerVersion::from_version_id(&clean).map(|v| format!("{v:?}")),
        full_simulation,
        files,
        bytes,
        installed_epoch: now_epoch(),
        notes,
    };
    write_profile(&staging, &profile)?;

    let _ = fs::remove_dir_all(&backup);
    if final_root.exists() {
        fs::rename(&final_root, &backup).map_err(|e| format!("{}: {e}", final_root.display()))?;
    }
    if let Err(e) = fs::rename(&staging, &final_root) {
        if backup.exists() {
            let _ = fs::rename(&backup, &final_root);
        }
        return Err(format!("{}: {e}", final_root.display()));
    }
    let _ = fs::remove_dir_all(&backup);

    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn temp_home(tag: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "horizon-versions-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_simulated_prepared_base(root: &Path) {
        fs::create_dir_all(root.join("assets/minecraft/items")).unwrap();
        fs::create_dir_all(root.join("data/minecraft")).unwrap();
        fs::create_dir_all(root.join("worldgen")).unwrap();
        fs::write(
            root.join("horizon-resources.json"),
            r#"{"schema":7,"minecraft":"26.3"}"#,
        )
        .unwrap();
        for name in BASE_SIMULATION_FILES {
            fs::write(root.join(name), b"{}").unwrap();
        }
        fs::write(
            root.join("assets/minecraft/items/stone.json"),
            r#"{"model":{"type":"minecraft:model","model":"minecraft:block/stone"}}"#,
        )
        .unwrap();
    }

    #[test]
    fn mojang_on_device_preparation_links_classic_textures_and_upgrades_when_base_arrives() {
        let home = temp_home("mojang-prep");
        let jar_path = home.join("1.12.2.jar");
        {
            let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("pack.mcmeta", opts).unwrap();
            zip.write_all(br#"{"pack":{"pack_format":3,"description":"1.12.2"}}"#)
                .unwrap();
            zip.start_file("assets/minecraft/textures/blocks/stone.png", opts)
                .unwrap();
            zip.write_all(b"PNG112STONE").unwrap();
            zip.finish().unwrap();
        }

        let sound_bytes = b"OGG_CLICK_DATA";
        let sound_sha1 = hex_lower(&Sha1::digest(sound_bytes));
        let objects_dir = home.join("minecraft/assets/objects");
        let obj_file = objects_dir.join(&sound_sha1[..2]).join(&sound_sha1);
        fs::create_dir_all(obj_file.parent().unwrap()).unwrap();
        fs::write(&obj_file, sound_bytes).unwrap();

        let asset_index = json!({
            "objects": {
                "minecraft/sounds/random/click.ogg": {
                    "hash": sound_sha1,
                    "size": sound_bytes.len()
                }
            }
        });

        // 1. Prepare BEFORE any Horizon simulation base exists on the phone.
        let prof = prepare_mojang_on_device(
            &home,
            "1.12.2",
            "release",
            &jar_path,
            &asset_index,
            &objects_dir,
        )
        .expect("prepare 1.12.2");
        assert!(!prof.full_simulation);
        let ver_root = home.join("versions/1.12.2/resources");
        assert!(ver_root.join("horizon-resources.json").is_file());
        // Pre-1.13 `textures/blocks/stone.png` aliased automatically to `textures/block/stone.png`!
        assert_eq!(
            fs::read(ver_root.join("assets/minecraft/textures/block/stone.png")).unwrap(),
            b"PNG112STONE"
        );
        // Indexed sound materialized under `assets/minecraft/sounds/random/click.ogg`!
        assert_eq!(
            fs::read(ver_root.join("assets/minecraft/sounds/random/click.ogg")).unwrap(),
            sound_bytes
        );

        // 2. Now install a full 26.3 prepared base and verify `upgrade_partial_mojang_versions`
        //    automatically upgrades 1.12.2 to `full_simulation = true`!
        let base_26_3 = home.join("versions/26.3/resources");
        write_simulated_prepared_base(&base_26_3);
        let upgraded = upgrade_partial_mojang_versions(&home, &base_26_3);
        assert_eq!(upgraded, 1);
        assert!(ver_root.join("registries.json").is_file());
        assert!(ver_root.join("assets/minecraft/items/stone.json").is_file());

        // 3. Verify switching active versions updates `import::resources_root` & protocol preference!
        select_version(&home, "1.12.2").unwrap();
        assert_eq!(active_version_id(&home), "1.12.2");
        assert_eq!(
            crate::import::resources_root(&home.join("resources"), Some(&home)),
            Some(ver_root.clone())
        );

        select_version(&home, "26.3").unwrap();
        assert_eq!(active_version_id(&home), "26.3");
        assert_eq!(
            crate::import::resources_root(&ver_root, Some(&home)),
            Some(base_26_3)
        );

        delete_version(&home, "1.12.2").unwrap();
        assert!(!ver_root.exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn downloads_prepared_zip_from_live_local_http_server_with_sha256_verification() {
        let home = temp_home("http-prepared");
        let src_tree = home.join("pc-prepared-26.3");
        write_simulated_prepared_base(&src_tree);

        let zip_file = home.join("served-26.3.zip");
        crate::import::pack(&src_tree, &zip_file, |_, _| {}).unwrap();
        let zip_bytes = fs::read(&zip_file).unwrap();
        let expected_sha256 = hex_lower(&Sha256::digest(&zip_bytes));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_bytes = zip_bytes.clone();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req_buf = [0u8; 2048];
                let _ = stream.read(&mut req_buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n",
                    server_bytes.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&server_bytes);
            }
        });

        let job = global_job().clone();
        let url = format!("http://127.0.0.1:{port}/horizon-26.3.zip");
        run_prepared_download(&job, &home, &url, "1.21.4", Some(&expected_sha256))
            .expect("HTTP prepared download & SHA-256 verify");
        server.join().unwrap();

        let installed = list_installed(&home);
        let v = installed.iter().find(|i| i.id == "1.21.4").expect("1.21.4 installed");
        assert!(v.ready && v.full_simulation && v.active);
        assert_eq!(v.protocol, Some(769));
        assert_eq!(
            horizon_protocol::adapter::preferred_server_version(),
            Some(ServerVersion::V1_21_4)
        );
        let _ = fs::remove_dir_all(&home);
    }
}
