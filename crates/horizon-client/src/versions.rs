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
    io::Read,
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
const SIM_BASE_DIR_NAME: &str = ".sim-base-26.3";
const SCHEMA_VERSION: u32 = 7;

/// Complete list of required top-level simulation/datagen files produced by `horizon-prepare`
/// (`SCHEMA = 7`, `OUTPUT_VERSION = 4`) and read by `load_resources_with_access`, `Assets::load`,
/// `load_world_rest`, and `GameCpuResources::load`.
pub const REQUIRED_SIMULATION_FILES: &[&str] = &[
    "facts.json",
    "block-tags.json",
    "movement-properties.nbt",
    "attributes.json",
    "landing-definitions.json",
    "damage-types.json",
    "damage-tags.json",
    "damage-catalog.json",
    "player-float-ids.json",
    "cached-collision.nbt",
    "render-definitions.json",
    "occlusion.nbt",
    "gameplay-facts.json",
    "entity-types.json",
    "block-behaviours.json",
    "datafix-tables.json",
    "model-states.json",
    "entity-models.json",
    "mob-data.json",
    "command-registries.json",
    "registries.json",
    "commands.json",
    "items.json",
    "creative-inventory.json",
];

/// Optional metadata files written by `horizon-prepare` and linked when present.
const OPTIONAL_SIMULATION_FILES: &[&str] =
    &["horizon-prepare.json", "asset-index.json", "pack.png"];

/// Required nested worldgen & font files verified before declaring a resource directory playable.
const REQUIRED_NESTED_FILES: &[&str] = &[
    "worldgen/blocks/block-properties.nbt",
    "worldgen/blocks/cached-collision.nbt",
    "worldgen/blocks/cached-support.nbt",
    "assets/minecraft/font/default.json",
];

/// Top-level simulation folders linked from the prepared Horizon base.
const BASE_SIMULATION_DIRS: &[&str] = &["worldgen", "biome-parameters", "packs"];

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

pub struct JobState {
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

/// Validate that `resources_dir` contains a complete, engine-loadable Horizon prepared tree:
/// - `horizon-resources.json` exists, parses as JSON, is not marked `full_simulation: false`,
///   and matches `horizon_world::DATA_VERSION`.
/// - Every required `horizon-prepare` simulation table (`REQUIRED_SIMULATION_FILES`) and nested
///   worldgen/font file (`REQUIRED_NESTED_FILES`) exists on disk.
pub fn validate_playable_root(resources_dir: &Path) -> Result<(), String> {
    let manifest_path = resources_dir.join("horizon-resources.json");
    if !manifest_path.is_file() {
        return Err("No game files installed yet (missing horizon-resources.json).".into());
    }
    let manifest_text = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Could not read horizon-resources.json: {e}"))?;
    let manifest: Value = serde_json::from_str(&manifest_text)
        .map_err(|e| format!("Invalid horizon-resources.json: {e}"))?;
    if manifest.get("full_simulation").and_then(Value::as_bool) == Some(false) {
        let ver = manifest
            .get("source_minecraft")
            .or_else(|| manifest.get("minecraft"))
            .and_then(Value::as_str)
            .unwrap_or("selected version");
        return Err(format!(
            "Minecraft {ver} was downloaded without Horizon's simulation runtime tables. Tap Complete Setup in Game Versions to finish installing (~45 MB)."
        ));
    }
    let expected_dv = i64::from(horizon_world::DATA_VERSION);
    match manifest.get("data_version").and_then(Value::as_i64) {
        Some(dv) if dv == expected_dv => {}
        Some(other) => {
            return Err(format!(
                "Game files data_version ({other}) does not match Horizon {} ({expected_dv}).",
                horizon_core::VANILLA_VERSION
            ));
        }
        None => {
            return Err(
                "Game files are incomplete (missing data_version in horizon-resources.json)."
                    .into(),
            );
        }
    }
    for rel in REQUIRED_SIMULATION_FILES
        .iter()
        .chain(REQUIRED_NESTED_FILES.iter())
    {
        if !resources_dir.join(rel).is_file() {
            return Err(format!(
                "Game files are incomplete (missing required engine table '{rel}')."
            ));
        }
    }
    Ok(())
}

pub fn has_simulation_tables(resources_dir: &Path) -> bool {
    validate_playable_root(resources_dir).is_ok()
}

pub fn is_ready_root(resources_dir: &Path) -> bool {
    validate_playable_root(resources_dir).is_ok()
}

/// True when `resources_dir` holds an incomplete/partial Mojang download (`horizon-resources.json`
/// exists on disk, but `validate_playable_root` has not passed yet).
pub fn has_partial_mojang_root(resources_dir: &Path) -> bool {
    resources_dir.join("horizon-resources.json").is_file() && !is_ready_root(resources_dir)
}

/// True when at least one partial Mojang version exists under `home/versions/*/resources`.
pub fn has_any_partial_mojang(home: &Path) -> bool {
    let Ok(entries) = fs::read_dir(home.join("versions")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let hidden = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with('.'));
        !hidden && has_partial_mojang_root(&path.join("resources"))
    })
}

fn has_installed_marker(resources_dir: &Path) -> bool {
    resources_dir.join("horizon-resources.json").is_file()
}

fn read_profile(resources_dir: &Path, fallback_id: &str) -> VersionProfile {
    let full = has_simulation_tables(resources_dir);
    if let Ok(text) = fs::read_to_string(resources_dir.join(PROFILE_FILE))
        && let Ok(mut parsed) = serde_json::from_str::<VersionProfile>(&text)
    {
        parsed.full_simulation = full;
        return parsed;
    }
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
            "Partial Mojang install — needs Horizon simulation runtime (~45 MB).".into()
        },
    }
}

fn write_profile(resources_dir: &Path, profile: &VersionProfile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(profile).map_err(|e| e.to_string())?;
    write_unlinked(&resources_dir.join(PROFILE_FILE), &bytes)
        .map_err(|e| format!("{}: {e}", resources_dir.display()))
}

/// List all installed versions under `home/versions/*/resources` plus legacy `home/resources`.
/// Includes partial Mojang installs (`ready: false`) so the launcher can repair or remove them,
/// while only marking genuinely playable (`ready: true`) versions as `active` for engine launch.
pub fn list_installed(home: &Path) -> Vec<InstalledVersion> {
    if let Some(base) = find_simulation_base(home) {
        let _ = upgrade_partial_mojang_versions(home, &base);
    }

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
            if !has_installed_marker(&res) {
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
                    full_simulation: ready,
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
    if has_installed_marker(&legacy) {
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
            let ready = is_ready_root(&legacy);
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
                    full_simulation: ready,
                    ready,
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
        b.ready
            .cmp(&a.ready)
            .then_with(|| {
                (a.id != horizon_core::VANILLA_VERSION)
                    .cmp(&(b.id != horizon_core::VANILLA_VERSION))
            })
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

/// Return the active version's `resources/` directory when at least one genuinely playable
/// (`validate_playable_root == Ok`) version exists.
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
        let target =
            ServerVersion::from_version_id(&active_id).filter(|v| *v != ServerVersion::Current);
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
    if let Some(partial) = installed.iter().find(|v| v.id == clean && !v.ready) {
        let _ = partial;
        return Err(format!(
            "Minecraft {clean} needs Horizon's simulation runtime (~45 MB). Tap Complete Setup first."
        ));
    }
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
        fs::remove_dir_all(&version_dir).map_err(|e| format!("{}: {e}", version_dir.display()))?;
        removed = true;
    }
    let legacy = home.join("resources");
    if !removed && has_installed_marker(&legacy) {
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

/// Locate a pristine installed resource root that holds full `horizon-prepare` simulation tables.
/// Checks dedicated `.sim-base-26.3/resources`, `26.3/resources`, and `resources` first so classic
/// version texture overlays never bleed across other versions.
pub fn find_simulation_base(home: &Path) -> Option<PathBuf> {
    let preferred = [
        home.join("versions")
            .join(SIM_BASE_DIR_NAME)
            .join("resources"),
        home.join("versions")
            .join(horizon_core::VANILLA_VERSION)
            .join("resources"),
        home.join("resources"),
    ];
    for cand in preferred {
        if has_simulation_tables(&cand) {
            return Some(cand);
        }
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

/// Write `bytes` to `dst` after unlinking `dst` first so hardlinked base files are never mutated.
fn write_unlinked(dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if dst.exists() {
        let _ = fs::remove_file(dst);
    }
    fs::write(dst, bytes)
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

/// Replace `dst` with a hardlink/copy of `src`, unlinking `dst` first so any prior hardlink to a
/// shared simulation base remains completely untouched.
fn replace_link_or_copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if dst.exists() {
        let _ = fs::remove_file(dst);
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

/// Build a schema-valid `horizon-resources.json` derived from `base`'s manifest, preserving
/// `data_version`, `schema`, and hashes while recording `source_minecraft`.
fn build_manifest_from_base(base: &Path, version_id: &str) -> Value {
    let mut obj = fs::read_to_string(base.join("horizon-resources.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    obj.entry("schema".to_owned())
        .or_insert_with(|| json!(SCHEMA_VERSION));
    obj.insert(
        "data_version".to_owned(),
        json!(i64::from(horizon_world::DATA_VERSION)),
    );
    obj.insert("minecraft".to_owned(), json!(horizon_core::VANILLA_VERSION));
    obj.insert("source_minecraft".to_owned(), json!(version_id));
    obj.insert("full_simulation".to_owned(), json!(true));
    Value::Object(obj)
}

fn populate_from_simulation_base(base: &Path, target_resources: &Path, version_id: &str) -> usize {
    let mut linked = 0usize;
    for file_name in REQUIRED_SIMULATION_FILES
        .iter()
        .chain(OPTIONAL_SIMULATION_FILES.iter())
    {
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
    if version_id != horizon_core::VANILLA_VERSION {
        // Older Mojang versions (< 26.3) carry older JSON schemas in `data/minecraft/*`,
        // `assets/minecraft/models/*`, `assets/minecraft/blockstates/*`, `assets/minecraft/items/*`,
        // `assets/minecraft/textures/gui/sprites/*`, and `assets/minecraft/lang/*.lang`.
        // Replace schema-bound directories with the 26.3 simulation base while preserving all
        // authentic version textures & sounds already unpacked in `target_resources`!
        for schema_bound in [
            "data",
            "assets/minecraft/models",
            "assets/minecraft/blockstates",
            "assets/minecraft/items",
            "assets/minecraft/atlases",
            "assets/minecraft/equipment",
            "assets/minecraft/shaders",
            "assets/minecraft/font",
            "assets/minecraft/textures/gui/sprites",
        ] {
            let src = base.join(schema_bound);
            let dst = target_resources.join(schema_bound);
            if src.is_dir() {
                let _ = fs::remove_dir_all(&dst);
                if let Ok(n) = link_missing_tree(&src, &dst) {
                    linked += n;
                }
            }
        }
        for schema_file in [
            "assets/pack.mcmeta",
            "assets/minecraft/sounds.json",
            "assets/minecraft/lang/en_us.json",
        ] {
            let src = base.join(schema_file);
            let dst = target_resources.join(schema_file);
            if src.is_file() && replace_link_or_copy_file(&src, &dst).is_ok() {
                linked += 1;
            }
        }
    }
    // Fill any remaining missing assets/data files (e.g. blocks/items added after `version_id`,
    // panorama fallback, Unifont) from the 26.3 base without overwriting `version_id`'s textures/sounds.
    for sub in ["assets", "data"] {
        if let Ok(n) = link_missing_tree(&base.join(sub), &target_resources.join(sub)) {
            linked += n;
        }
    }
    let manifest = build_manifest_from_base(base, version_id);
    if let Ok(bytes) = serde_json::to_vec_pretty(&manifest) {
        let _ = write_unlinked(&target_resources.join("horizon-resources.json"), &bytes);
    }
    linked
}

/// Resolve the effective Minecraft version label for a prepared `resources` directory.
/// Multi-version installs keep `"minecraft": "26.3"` for engine schema checks and store the
/// user-selected version in `"source_minecraft"` (as well as `version-profile.json`).
pub fn version_id_for_root(root: &Path) -> String {
    let profile_id = || {
        let mut candidates = vec![root.join("version-profile.json")];
        if let Some(parent) = root.parent() {
            candidates.push(parent.join("version-profile.json"));
        }
        candidates.into_iter().find_map(|p| {
            let ptext = fs::read_to_string(p).ok()?;
            let pval: Value = serde_json::from_str(&ptext).ok()?;
            pval["id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_owned())
        })
    };
    if let Ok(text) = fs::read_to_string(root.join("horizon-resources.json"))
        && let Ok(val) = serde_json::from_str::<Value>(&text)
    {
        if let Some(src) = val["source_minecraft"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
        {
            return src.trim().to_owned();
        }
        if let Some(id) = profile_id() {
            return id;
        }
        if let Some(mc) = val["minecraft"].as_str().filter(|s| !s.trim().is_empty()) {
            return mc.trim().to_owned();
        }
    }
    if let Some(id) = profile_id() {
        return id;
    }
    horizon_core::VANILLA_VERSION.to_owned()
}

/// Heal any 1x1 `client.jar` placeholder assets (such as `panorama_0..5.png` or `unifont.json`) and
/// missing `assets/pack.mcmeta` in an already-installed `resources` directory using the cached
/// Mojang `minecraft/assets/objects` store (and `find_simulation_base` as fallback), without any
/// network download. Writes `.horizon-assets-healed-v1` once verified so subsequent launches are O(1).
pub fn heal_placeholder_assets(home: &Path, root: &Path) -> usize {
    if !root.is_dir() {
        return 0;
    }
    let marker = root.join(".horizon-assets-healed-v1");
    let pano0 = root.join("assets/minecraft/textures/gui/title/background/panorama_0.png");
    let pack_mcmeta = root.join("assets/pack.mcmeta");
    let pano_valid = |p: &Path| fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() >= 4096);
    if marker.is_file() && pano_valid(&pano0) && pack_mcmeta.is_file() {
        return 0;
    }

    let version_id = version_id_for_root(root);
    let is_native_pinned = version_id == horizon_core::VANILLA_VERSION;
    let mut healed = 0usize;

    for cache_root in [home.join("minecraft"), home.to_path_buf()] {
        let objects_dir = cache_root.join("assets/objects");
        if !objects_dir.is_dir() {
            continue;
        }
        let index_id = fs::read_to_string(
            cache_root
                .join("versions")
                .join(&version_id)
                .join(format!("{version_id}.json")),
        )
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["assetIndex"]["id"].as_str().map(str::to_owned));

        let mut candidate_indexes = Vec::new();
        if let Some(id) = index_id {
            candidate_indexes.push(cache_root.join("assets/indexes").join(format!("{id}.json")));
        }
        candidate_indexes.push(
            cache_root
                .join("assets/indexes")
                .join(format!("{version_id}.json")),
        );
        if is_native_pinned && let Ok(entries) = fs::read_dir(cache_root.join("assets/indexes")) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json")
                    && !candidate_indexes.contains(&p)
                {
                    candidate_indexes.push(p);
                }
            }
        }

        let Some(asset_index) = candidate_indexes.into_iter().find_map(|p| {
            fs::read_to_string(p)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        }) else {
            continue;
        };

        if let Some(objs) = asset_index["objects"].as_object() {
            for (logical, meta) in objs {
                let Some(hash) = meta["hash"].as_str().filter(|h| h.len() >= 2) else {
                    continue;
                };
                let clean_rel = logical.trim_start_matches('/');
                if clean_rel.is_empty() || clean_rel.contains("..") {
                    continue;
                }
                if !is_native_pinned {
                    let allowed_non_native = clean_rel.contains("/sounds/")
                        || clean_rel.starts_with("minecraft/sounds/")
                        || clean_rel.contains("textures/gui/title/background/panorama_")
                        || (clean_rel.contains("/textures/")
                            && !clean_rel.contains("/textures/gui/"));
                    if !allowed_non_native {
                        continue;
                    }
                }
                let src = objects_dir
                    .join(&hash[..2].to_ascii_lowercase())
                    .join(hash.to_ascii_lowercase());
                if !src.is_file() {
                    continue;
                }
                let dst = if clean_rel.starts_with("assets/") {
                    root.join(clean_rel)
                } else if clean_rel == "pack.mcmeta" {
                    root.join("assets").join("pack.mcmeta")
                } else if clean_rel.starts_with("pack.") {
                    root.join(clean_rel)
                } else {
                    root.join("assets").join(clean_rel)
                };
                let want_size = meta["size"].as_u64().unwrap_or(0);
                let needs_replace = match fs::metadata(&dst) {
                    Ok(m) if m.is_file() => want_size > 0 && m.len() != want_size,
                    _ => true,
                };
                if needs_replace && replace_link_or_copy_file(&src, &dst).is_ok() {
                    healed += 1;
                }
            }
        }
        break;
    }

    if let Some(base) = find_simulation_base(home)
        && base != root
    {
        for i in 0..6 {
            let rel = format!("assets/minecraft/textures/gui/title/background/panorama_{i}.png");
            let dst = root.join(&rel);
            let src = base.join(&rel);
            if !pano_valid(&dst)
                && pano_valid(&src)
                && replace_link_or_copy_file(&src, &dst).is_ok()
            {
                healed += 1;
            }
        }
        if !pack_mcmeta.is_file() {
            let src_pack = base.join("assets/pack.mcmeta");
            if src_pack.is_file() && replace_link_or_copy_file(&src_pack, &pack_mcmeta).is_ok() {
                healed += 1;
            }
        }
    }

    if pano_valid(&pano0) && pack_mcmeta.is_file() {
        let _ = fs::write(&marker, b"1\n");
    }
    if healed > 0 {
        log::info!(
            "Horizon: healed {healed} placeholder/missing asset(s) in {} ({version_id})",
            root.display()
        );
    }
    healed
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
        let path = entry.path();
        let Some(folder_id) = path.file_name().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        if folder_id.starts_with('.') {
            continue;
        }
        let res = path.join("resources");
        if !has_installed_marker(&res) || res == base_resources || has_simulation_tables(&res) {
            continue;
        }
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

fn fetch_bytes_verified(
    url: &str,
    expected_sha1: Option<&str>,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
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
            return Err(format!(
                "{url}: SHA-1 mismatch (expected {want}, got {got})"
            ));
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
                        if let Some(existing) =
                            out.iter_mut().find(|e| e.id == id || e.url == entry_url)
                        {
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

fn begin_job(
    version_id: &str,
    source: &str,
    initial_detail: &str,
) -> Result<Arc<JobState>, String> {
    let job = global_job().clone();
    {
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
    }
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
        let outcome =
            run_prepared_download(&job, &home, &url, &version_id, expected_sha256.as_deref());
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
    let work_dir = home
        .join("versions")
        .join(format!(".download-{version_id}"));
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
        let fetched = crate::import::download::fetch_resumable(url, &work_dir, &received, &total);
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
    let pack = crate::import::Pack::open(&archive_path).map_err(|e| e.to_string())?;
    if pack.kind != crate::import::Kind::Resources {
        let _ = fs::remove_dir_all(&work_dir);
        return Err(
            "Archive is not a prepared Horizon game-files bundle (missing horizon-resources.json)."
                .into(),
        );
    }
    let version_home = home.join("versions").join(version_id);
    fs::create_dir_all(&version_home).map_err(|e| format!("{}: {e}", version_home.display()))?;
    let installed = pack.install(&version_home).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&work_dir);

    let full_sim = has_simulation_tables(&installed.path);
    if !full_sim {
        return Err(validate_playable_root(&installed.path)
            .err()
            .unwrap_or_else(|| {
                "Downloaded archive is missing required Horizon simulation tables.".into()
            }));
    }
    let profile = VersionProfile {
        id: version_id.to_owned(),
        release_type: "release".into(),
        source: "horizon-prepared".into(),
        protocol: ServerVersion::protocol_for_version_id(version_id),
        protocol_adapter: ServerVersion::from_version_id(version_id).map(|v| format!("{v:?}")),
        full_simulation: true,
        files: installed.files,
        bytes: installed.bytes,
        installed_epoch: now_epoch(),
        notes: "Prepared Horizon runtime & assets.".into(),
    };
    write_profile(&installed.path, &profile)?;
    let _ = upgrade_partial_mojang_versions(home, &installed.path);

    let mut sel = load_selection(home);
    sel.active = version_id.to_owned();
    save_selection(home, &sel)?;
    apply_active_protocol_preference(home);
    Ok(())
}

fn simulation_base_archive_url() -> Option<String> {
    if let Ok(env_url) = std::env::var("HORIZON_SIM_BASE_URL")
        && !env_url.trim().is_empty()
    {
        return Some(env_url.trim().to_owned());
    }
    horizon_hooks::porter::engine::simulation_base_url()
        .or_else(horizon_hooks::porter::engine::default_prepared_url)
}

/// Ensure a complete `26.3` simulation runtime base exists on disk (`find_simulation_base`),
/// downloading the compact `sim-base-26.3.zip` archive into `versions/.sim-base-26.3/resources`
/// if no full prepared runtime is installed yet.
pub fn ensure_simulation_base_with_job(
    job: &Arc<JobState>,
    home: &Path,
    override_url: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(existing) = find_simulation_base(home) {
        return Ok(existing);
    }
    let url = override_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(simulation_base_archive_url)
        .ok_or_else(|| {
            "No Horizon simulation runtime URL is configured. Import a prepared 26.3 bundle or configure a mirror URL.".to_owned()
        })?;

    check_cancelled(job)?;
    let work_dir = home.join("versions").join(".download-sim-base");
    fs::create_dir_all(&work_dir).map_err(|e| format!("{}: {e}", work_dir.display()))?;

    let archive_path = if let Some(local_path) = url.strip_prefix("file://") {
        let copy = work_dir.join("sim-base.zip");
        fs::copy(local_path, &copy).map_err(|e| format!("{local_path}: {e}"))?;
        copy
    } else {
        update_job(job, |st| {
            st.stage = "sim-base".into();
            st.received_bytes = 0;
            st.total_bytes = 0;
            st.detail = "Downloading Horizon 26.3 simulation runtime tables…".into();
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
        let fetched = crate::import::download::fetch_resumable(&url, &work_dir, &received, &total);
        done_flag.store(true, Ordering::Relaxed);
        let _ = monitor.join();
        update_job(job, |st| {
            st.received_bytes = received.load(Ordering::Relaxed);
            st.total_bytes = total.load(Ordering::Relaxed);
        });
        fetched?
    };

    check_cancelled(job)?;
    update_job(job, |st| {
        st.stage = "sim-base-install".into();
        st.detail = "Installing Horizon 26.3 simulation runtime tables…".into();
    });
    let pack = crate::import::Pack::open(&archive_path).map_err(|e| e.to_string())?;
    if pack.kind != crate::import::Kind::Resources {
        let _ = fs::remove_dir_all(&work_dir);
        return Err("Simulation base archive did not contain horizon-resources.json.".into());
    }
    let sim_home = home.join("versions").join(SIM_BASE_DIR_NAME);
    fs::create_dir_all(&sim_home).map_err(|e| format!("{}: {e}", sim_home.display()))?;
    let installed = pack.install(&sim_home).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&work_dir);

    validate_playable_root(&installed.path)?;
    Ok(installed.path)
}

/// Repair / complete any partial Mojang install(s) on the device by fetching the compact
/// `sim-base-26.3.zip` runtime (if missing) and rebuilding/linking simulation tables without
/// re-downloading Mojang's `client.jar` or `assets/objects`.
pub fn start_repair_versions(
    home: PathBuf,
    target_id: Option<String>,
    custom_sim_base_url: Option<String>,
) -> Result<VersionJobStatus, String> {
    let clean_target = target_id.as_deref().and_then(sanitize_version_id);
    let label = clean_target
        .clone()
        .unwrap_or_else(|| horizon_core::VANILLA_VERSION.to_owned());
    let job = begin_job(
        &label,
        "horizon-repair",
        "Checking Horizon simulation runtime…",
    )?;
    let initial = job_status();

    std::thread::spawn(move || {
        let outcome = run_repair_versions(
            &job,
            &home,
            clean_target.as_deref(),
            custom_sim_base_url.as_deref(),
        );
        update_job(&job, |st| {
            st.active = false;
            match outcome {
                Ok(repaired_id) => {
                    st.stage = "done".into();
                    st.done = true;
                    st.version_id = repaired_id.clone();
                    st.detail = format!("Minecraft {repaired_id} is ready to play.");
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

fn run_repair_versions(
    job: &Arc<JobState>,
    home: &Path,
    target_id: Option<&str>,
    custom_sim_base_url: Option<&str>,
) -> Result<String, String> {
    let base = ensure_simulation_base_with_job(job, home, custom_sim_base_url)?;
    check_cancelled(job)?;

    update_job(job, |st| {
        st.stage = "preparing".into();
        st.detail = "Linking simulation runtime into installed Mojang version(s)…".into();
    });

    // If Mojang's cached `versions/<id>/<id>.jar` and `assets/indexes/<idx>.json` are still in
    // `Documents/minecraft/`, re-run `prepare_mojang_on_device` cleanly for full schema-safe layering.
    let mc_cache = home.join("minecraft");
    let objects_dir = mc_cache.join("assets/objects");
    if let Some(tid) = target_id {
        let jar = mc_cache
            .join("versions")
            .join(tid)
            .join(format!("{tid}.jar"));
        let meta_path = mc_cache
            .join("versions")
            .join(tid)
            .join(format!("{tid}.json"));
        if jar.is_file()
            && let Ok(meta_text) = fs::read_to_string(&meta_path)
            && let Ok(meta) = serde_json::from_str::<Value>(&meta_text)
        {
            let idx_id = meta["assetIndex"]["id"].as_str().unwrap_or(tid);
            let idx_path = mc_cache
                .join("assets/indexes")
                .join(format!("{idx_id}.json"));
            let asset_index = fs::read_to_string(&idx_path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .unwrap_or_else(|| json!({ "objects": {} }));
            let rtype = meta["type"].as_str().unwrap_or("release");
            let _ = prepare_mojang_on_device(home, tid, rtype, &jar, &asset_index, &objects_dir)?;
        }
    }

    let _ = upgrade_partial_mojang_versions(home, &base);

    let chosen = if let Some(tid) = target_id {
        let res = home.join("versions").join(tid).join("resources");
        validate_playable_root(&res)?;
        let mut sel = load_selection(home);
        sel.active = tid.to_owned();
        save_selection(home, &sel)?;
        apply_active_protocol_preference(home);
        tid.to_owned()
    } else if let Some(first_ready) = list_installed(home).into_iter().find(|v| v.ready) {
        let mut sel = load_selection(home);
        sel.active = first_ready.id.clone();
        let _ = save_selection(home, &sel);
        apply_active_protocol_preference(home);
        first_ready.id
    } else {
        // Promote `.sim-base-26.3` into `versions/26.3/resources` if no user version existed yet!
        let v26_dir = home
            .join("versions")
            .join(horizon_core::VANILLA_VERSION)
            .join("resources");
        if !is_ready_root(&v26_dir) {
            let linked = link_missing_tree(&base, &v26_dir).unwrap_or(0);
            let prof = VersionProfile {
                id: horizon_core::VANILLA_VERSION.to_owned(),
                release_type: "release".into(),
                source: "horizon-prepared".into(),
                protocol: ServerVersion::protocol_for_version_id(horizon_core::VANILLA_VERSION),
                protocol_adapter: None,
                full_simulation: true,
                files: linked,
                bytes: 0,
                installed_epoch: now_epoch(),
                notes: "Horizon 26.3 compact simulation runtime.".into(),
            };
            let _ = write_profile(&v26_dir, &prof);
        }
        let mut sel = load_selection(home);
        sel.active = horizon_core::VANILLA_VERSION.to_owned();
        let _ = save_selection(home, &sel);
        apply_active_protocol_preference(home);
        horizon_core::VANILLA_VERSION.to_owned()
    };

    Ok(chosen)
}

/// Start downloading & preparing an official Mojang version (`client.jar` + asset index + objects)
/// directly on the device into `Documents/versions/<id>/resources/`.
pub fn start_mojang_download(
    home: PathBuf,
    version_id: String,
) -> Result<VersionJobStatus, String> {
    let clean = sanitize_version_id(&version_id).ok_or("invalid Mojang version id")?;
    let job = begin_job(
        &clean,
        "mojang-official",
        "Fetching Mojang version manifest…",
    )?;
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
    let version_json_bytes = fetch_bytes_verified(
        version_json_json_url(version_json_url),
        version_json_sha1,
        16 << 20,
    )?;
    let version_meta: Value = serde_json::from_slice(&version_json_bytes)
        .map_err(|e| format!("{version_id}.json: {e}"))?;

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
    fs::write(
        jar_dir.join(format!("{version_id}.json")),
        &version_json_bytes,
    )
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
                        || first_error
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_some()
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
                                if let Err(e) = fs::write(&tmp, &bytes)
                                    .and_then(|()| fs::rename(&tmp, &obj_path))
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
                    let bytes_now = downloaded_bytes.fetch_add(size, Ordering::Relaxed) + size;
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

    // Automatically ensure the Horizon 26.3 simulation runtime base is installed on disk BEFORE
    // finalizing `prepare_mojang_on_device` so every Mojang download completes ready to play!
    let _sim_base = ensure_simulation_base_with_job(job, home, None)?;
    check_cancelled(job)?;

    update_job(job, |st| {
        st.stage = "preparing".into();
        st.detail = format!("Preparing {version_id} resources on device…");
    });

    let prof = prepare_mojang_on_device(
        home,
        version_id,
        &release_type,
        &jar_path,
        &asset_index,
        &objects_dir,
    )?;
    if !prof.full_simulation {
        return Err(
            "Mojang assets were unpacked, but Horizon's simulation runtime tables are still missing."
                .into(),
        );
    }

    let mut sel = load_selection(home);
    sel.active = version_id.to_owned();
    save_selection(home, &sel)?;
    apply_active_protocol_preference(home);
    Ok(())
}

fn version_json_json_url(url: &str) -> &str {
    url
}

/// Pre-1.13 ("The Flattening") block texture renames (`textures/blocks/<old>.png` -> `textures/block/<new>.png`).
const CLASSIC_BLOCK_TEXTURE_ALIASES: &[(&str, &str)] = &[
    ("grass_top", "grass_block_top"),
    ("grass_side", "grass_block_side"),
    ("grass_side_overlay", "grass_block_side_overlay"),
    ("grass_side_snowed", "grass_block_snow"),
    ("dirt_podzol_top", "podzol_top"),
    ("dirt_podzol_side", "podzol_side"),
    ("stone_granite", "granite"),
    ("stone_granite_smooth", "polished_granite"),
    ("stone_diorite", "diorite"),
    ("stone_diorite_smooth", "polished_diorite"),
    ("stone_andesite", "andesite"),
    ("stone_andesite_smooth", "polished_andesite"),
    ("cobblestone_mossy", "mossy_cobblestone"),
    ("planks_oak", "oak_planks"),
    ("planks_spruce", "spruce_planks"),
    ("planks_birch", "birch_planks"),
    ("planks_jungle", "jungle_planks"),
    ("planks_acacia", "acacia_planks"),
    ("planks_big_oak", "dark_oak_planks"),
    ("log_oak", "oak_log"),
    ("log_oak_top", "oak_log_top"),
    ("log_spruce", "spruce_log"),
    ("log_spruce_top", "spruce_log_top"),
    ("log_birch", "birch_log"),
    ("log_birch_top", "birch_log_top"),
    ("log_jungle", "jungle_log"),
    ("log_jungle_top", "jungle_log_top"),
    ("log_acacia", "acacia_log"),
    ("log_acacia_top", "acacia_log_top"),
    ("log_big_oak", "dark_oak_log"),
    ("log_big_oak_top", "dark_oak_log_top"),
    ("leaves_oak", "oak_leaves"),
    ("leaves_spruce", "spruce_leaves"),
    ("leaves_birch", "birch_leaves"),
    ("leaves_jungle", "jungle_leaves"),
    ("leaves_acacia", "acacia_leaves"),
    ("leaves_big_oak", "dark_oak_leaves"),
    ("sapling_oak", "oak_sapling"),
    ("sapling_spruce", "spruce_sapling"),
    ("sapling_birch", "birch_sapling"),
    ("sapling_jungle", "jungle_sapling"),
    ("sapling_acacia", "acacia_sapling"),
    ("sapling_roofed_oak", "dark_oak_sapling"),
    ("brick", "bricks"),
    ("nether_brick", "nether_bricks"),
    ("red_nether_brick", "red_nether_bricks"),
    ("sandstone_normal", "sandstone"),
    ("sandstone_carved", "chiseled_sandstone"),
    ("sandstone_smooth", "cut_sandstone"),
    ("red_sandstone_normal", "red_sandstone"),
    ("red_sandstone_carved", "chiseled_red_sandstone"),
    ("red_sandstone_smooth", "cut_red_sandstone"),
    ("stonebrick", "stone_bricks"),
    ("stonebrick_mossy", "mossy_stone_bricks"),
    ("stonebrick_cracked", "cracked_stone_bricks"),
    ("stonebrick_carved", "chiseled_stone_bricks"),
    ("quartz_block_side", "quartz_block_side"),
    ("quartz_block_chiseled", "chiseled_quartz_block"),
    ("quartz_block_lines", "quartz_pillar"),
    ("reeds", "sugar_cane"),
    ("web", "cobweb"),
    ("tallgrass", "short_grass"),
    ("deadbush", "dead_bush"),
    ("waterlily", "lily_pad"),
    ("torch_on", "torch"),
    ("redstone_torch_on", "redstone_torch"),
    ("redstone_lamp_off", "redstone_lamp"),
    ("furnace_front_off", "furnace_front"),
    ("workbench_top", "crafting_table_top"),
    ("workbench_front", "crafting_table_front"),
    ("workbench_side", "crafting_table_side"),
    ("noteblock", "note_block"),
    ("mob_spawner", "spawner"),
    ("ice_packed", "packed_ice"),
    ("melon_side", "melon_side"),
    ("wool_colored_white", "white_wool"),
    ("wool_colored_orange", "orange_wool"),
    ("wool_colored_magenta", "magenta_wool"),
    ("wool_colored_light_blue", "light_blue_wool"),
    ("wool_colored_yellow", "yellow_wool"),
    ("wool_colored_lime", "lime_wool"),
    ("wool_colored_pink", "pink_wool"),
    ("wool_colored_gray", "gray_wool"),
    ("wool_colored_silver", "light_gray_wool"),
    ("wool_colored_cyan", "cyan_wool"),
    ("wool_colored_purple", "purple_wool"),
    ("wool_colored_blue", "blue_wool"),
    ("wool_colored_brown", "brown_wool"),
    ("wool_colored_green", "green_wool"),
    ("wool_colored_red", "red_wool"),
    ("wool_colored_black", "black_wool"),
];

/// Pre-1.13 item texture renames (`textures/items/<old>.png` -> `textures/item/<new>.png`).
const CLASSIC_ITEM_TEXTURE_ALIASES: &[(&str, &str)] = &[
    ("apple_golden", "golden_apple"),
    ("beef_cooked", "cooked_beef"),
    ("beef_raw", "beef"),
    ("porkchop_cooked", "cooked_porkchop"),
    ("porkchop_raw", "porkchop"),
    ("chicken_cooked", "cooked_chicken"),
    ("chicken_raw", "chicken"),
    ("mutton_cooked", "cooked_mutton"),
    ("mutton_raw", "mutton"),
    ("wood_sword", "wooden_sword"),
    ("wood_pickaxe", "wooden_pickaxe"),
    ("wood_axe", "wooden_axe"),
    ("wood_shovel", "wooden_shovel"),
    ("wood_hoe", "wooden_hoe"),
    ("gold_sword", "golden_sword"),
    ("gold_pickaxe", "golden_pickaxe"),
    ("gold_axe", "golden_axe"),
    ("gold_shovel", "golden_shovel"),
    ("gold_hoe", "golden_hoe"),
    ("gold_helmet", "golden_helmet"),
    ("gold_chestplate", "golden_chestplate"),
    ("gold_leggings", "golden_leggings"),
    ("gold_boots", "golden_boots"),
    ("potion_bottle_empty", "glass_bottle"),
    ("fireball", "fire_charge"),
    ("slimeball", "slime_ball"),
    ("totem", "totem_of_undying"),
    ("reeds", "sugar_cane"),
    ("bucket_empty", "bucket"),
    ("bucket_water", "water_bucket"),
    ("bucket_lava", "lava_bucket"),
    ("bucket_milk", "milk_bucket"),
];

fn apply_classic_texture_aliases(staging: &Path) -> usize {
    let mut added = 0usize;
    for (legacy_sub, modern_sub, renames) in [
        (
            "assets/minecraft/textures/blocks",
            "assets/minecraft/textures/block",
            CLASSIC_BLOCK_TEXTURE_ALIASES,
        ),
        (
            "assets/minecraft/textures/items",
            "assets/minecraft/textures/item",
            CLASSIC_ITEM_TEXTURE_ALIASES,
        ),
    ] {
        let src_dir = staging.join(legacy_sub);
        let dst_dir = staging.join(modern_sub);
        if !src_dir.is_dir() {
            continue;
        }
        if let Ok(n) = link_missing_tree(&src_dir, &dst_dir) {
            added += n;
        }
        for &(old_stem, new_stem) in renames {
            for ext in ["png", "png.mcmeta"] {
                let src = src_dir.join(format!("{old_stem}.{ext}"));
                let dst = dst_dir.join(format!("{new_stem}.{ext}"));
                if src.is_file() && !dst.exists() && link_or_copy_file(&src, &dst).is_ok() {
                    added += 1;
                }
            }
        }
    }
    added
}

/// Pure-Rust on-device resource builder for any Mojang `client.jar` + verified `assetIndex`:
/// - For `26.3`: unpacks complete `26.3` `assets/` & `data/`, materializes `assetIndex` objects,
///   and links all `horizon-prepare` simulation tables from `find_simulation_base`.
/// - For older/snapshot versions (`!= 26.3`): performs Schema-Safe Multi-Version Layering — keeping
///   26.3's schema-bound `data/`, `models/`, `blockstates/`, `items/`, `shaders/`, `font/`, and GUI
///   sprites intact while overlaying the downloaded version's authentic block/item/entity/environment
///   textures (with pre-1.13 flattening renames) and `.ogg` sounds/music!
pub fn prepare_mojang_on_device(
    home: &Path,
    version_id: &str,
    release_type: &str,
    jar_path: &Path,
    asset_index: &Value,
    objects_dir: &Path,
) -> Result<VersionProfile, String> {
    let clean = sanitize_version_id(version_id).ok_or("invalid version id")?;
    let is_native_pinned = clean == horizon_core::VANILLA_VERSION;
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
        let name = entry.name().map_err(|e| e.to_string())?.replace('\\', "/");
        if entry.is_symlink() {
            continue;
        }
        let keep = if is_native_pinned {
            name == "pack.mcmeta"
                || name == "pack.png"
                || name.starts_with("assets/")
                || name.starts_with("data/")
        } else {
            // For non-26.3 versions, keep authentic non-GUI textures (plus title background
            // panorama faces) and audio; 26.3 HUD/menu sprites & JSON schemas come from `sim_base`.
            name == "pack.png"
                || (name.starts_with("assets/minecraft/textures/")
                    && (!name.starts_with("assets/minecraft/textures/gui/")
                        || name.starts_with(
                            "assets/minecraft/textures/gui/title/background/panorama_",
                        )))
                || name.starts_with("assets/minecraft/sounds/")
        };
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
            if !is_native_pinned {
                let allowed_non_native = clean_rel.contains("/sounds/")
                    || clean_rel.starts_with("minecraft/sounds/")
                    || clean_rel.contains("textures/gui/title/background/panorama_")
                    || (clean_rel.contains("/textures/") && !clean_rel.contains("/textures/gui/"));
                if !allowed_non_native {
                    continue;
                }
            }
            let src = objects_dir
                .join(&hash[..2].to_ascii_lowercase())
                .join(hash.to_ascii_lowercase());
            if !src.is_file() {
                continue;
            }
            let dst = if clean_rel.starts_with("assets/") {
                staging.join(clean_rel)
            } else if clean_rel == "pack.mcmeta" {
                staging.join("assets").join("pack.mcmeta")
            } else if clean_rel.starts_with("pack.") {
                staging.join(clean_rel)
            } else {
                staging.join("assets").join(clean_rel)
            };
            // `client.jar` ships tiny 1x1 placeholder PNGs (`panorama_0..5.png`, icons) and stub
            // font metadata at paths whose real content lives in Mojang's `assetIndex` CDN objects;
            // overwrite those placeholders (`replace_link_or_copy_file` unlinks first so shared
            // hardlinks are never mutated in place).
            if replace_link_or_copy_file(&src, &dst).is_ok() {
                files += 1;
                bytes += meta["size"].as_u64().unwrap_or(0);
            }
        }
    }

    // Pre-1.13 texture folder aliases & classic block/item texture flattening renames.
    files += apply_classic_texture_aliases(&staging);

    let mut full_simulation = false;
    let mut notes = format!("Official Mojang {clean} assets & data unpacked on device.");
    if let Some(base) = sim_base.as_deref() {
        let linked = populate_from_simulation_base(base, &staging, &clean);
        validate_playable_root(&staging)?;
        full_simulation = true;
        files += linked;
        notes = format!(
            "Mojang {clean} assets + linked Horizon simulation runtime ({linked} base files)."
        );
    } else {
        let manifest = json!({
            "schema": SCHEMA_VERSION,
            "minecraft": clean,
            "source_minecraft": clean,
            "prepared_by": "horizon-on-device",
            "full_simulation": false,
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        write_unlinked(&staging.join("horizon-resources.json"), &manifest_bytes)
            .map_err(|e| e.to_string())?;
        notes.push_str(" Tap Complete Setup to download Horizon's simulation runtime (~45 MB).");
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
    use std::{io::Write, net::TcpListener};

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
        fs::create_dir_all(root.join("assets/minecraft/font")).unwrap();
        fs::create_dir_all(root.join("assets/minecraft/models/block")).unwrap();
        fs::create_dir_all(root.join("assets/minecraft/blockstates")).unwrap();
        fs::create_dir_all(root.join("assets/minecraft/textures/gui/sprites")).unwrap();
        fs::create_dir_all(root.join("assets/minecraft/lang")).unwrap();
        fs::create_dir_all(root.join("data/minecraft")).unwrap();
        fs::create_dir_all(root.join("worldgen/blocks")).unwrap();
        fs::create_dir_all(root.join("biome-parameters")).unwrap();
        let manifest = json!({
            "schema": 7,
            "data_version": horizon_world::DATA_VERSION,
            "minecraft": horizon_core::VANILLA_VERSION,
        });
        fs::write(
            root.join("horizon-resources.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        for name in REQUIRED_SIMULATION_FILES
            .iter()
            .chain(OPTIONAL_SIMULATION_FILES.iter())
        {
            fs::write(root.join(name), b"{}").unwrap();
        }
        for rel in REQUIRED_NESTED_FILES {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, b"{}").unwrap();
        }
        fs::write(
            root.join("assets/pack.mcmeta"),
            br#"{"pack":{"pack_format":55}}"#,
        )
        .unwrap();
        fs::write(root.join("assets/minecraft/sounds.json"), b"{}").unwrap();
        fs::write(root.join("assets/minecraft/lang/en_us.json"), b"{}").unwrap();
        fs::write(
            root.join("assets/minecraft/items/stone.json"),
            r#"{"model":{"type":"minecraft:model","model":"minecraft:block/stone"}}"#,
        )
        .unwrap();
        validate_playable_root(root).expect("fixture must pass validate_playable_root");
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
            zip.start_file("assets/minecraft/textures/blocks/grass_top.png", opts)
                .unwrap();
            zip.write_all(b"PNG112GRASS").unwrap();
            // Old 1.12 recipe schema that MUST NOT overwrite 26.3's `data/minecraft/`!
            zip.start_file("data/minecraft/recipes/stone.json", opts)
                .unwrap();
            zip.write_all(br#"{"type":"crafting_shaped","old_1_12":true}"#)
                .unwrap();
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
        assert!(has_partial_mojang_root(&ver_root));
        assert!(has_any_partial_mojang(&home));
        assert!(!is_ready_root(&ver_root));
        // Selecting an incomplete version must fail cleanly instead of letting Play hit missing tables!
        assert!(select_version(&home, "1.12.2").is_err());

        // Pre-1.13 `textures/blocks/stone.png` and `grass_top.png` aliased to modern 26.3 names!
        assert_eq!(
            fs::read(ver_root.join("assets/minecraft/textures/block/stone.png")).unwrap(),
            b"PNG112STONE"
        );
        assert_eq!(
            fs::read(ver_root.join("assets/minecraft/textures/block/grass_block_top.png")).unwrap(),
            b"PNG112GRASS"
        );
        assert_eq!(
            fs::read(ver_root.join("assets/minecraft/sounds/random/click.ogg")).unwrap(),
            sound_bytes
        );

        // 2. Now install a full 26.3 prepared base and verify `upgrade_partial_mojang_versions`
        //    automatically upgrades 1.12.2 to `full_simulation = true` and `is_ready_root == true`!
        let base_26_3 = home.join("versions/26.3/resources");
        write_simulated_prepared_base(&base_26_3);
        let upgraded = upgrade_partial_mojang_versions(&home, &base_26_3);
        assert_eq!(upgraded, 1);
        assert!(is_ready_root(&ver_root));
        assert!(!has_any_partial_mojang(&home));
        assert!(ver_root.join("model-states.json").is_file());
        assert!(ver_root.join("creative-inventory.json").is_file());
        assert!(ver_root.join("assets/minecraft/items/stone.json").is_file());
        assert!(!ver_root.join("data/minecraft/recipes/stone.json").exists());
        // Verify base `horizon-resources.json` was NOT mutated via hardlink (`source_minecraft` absent in base)!
        let base_manifest: Value =
            serde_json::from_slice(&fs::read(base_26_3.join("horizon-resources.json")).unwrap())
                .unwrap();
        assert!(base_manifest.get("source_minecraft").is_none());

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
        let v = installed
            .iter()
            .find(|i| i.id == "1.21.4")
            .expect("1.21.4 installed");
        assert!(v.ready && v.full_simulation && v.active);
        assert_eq!(v.protocol, Some(769));
        assert_eq!(
            horizon_protocol::adapter::preferred_server_version(),
            Some(ServerVersion::V1_21_4)
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn prepare_mojang_overwrites_jar_placeholders_and_heals_existing_installs() {
        let home = temp_home("heal-pano");
        let base_26_3 = home.join("versions/.sim-base-26.3/resources");
        write_fake_prepared_root(&base_26_3, "26.3");

        // Build a synthetic client.jar containing a 150-byte 1x1 placeholder panorama_0.png
        let jar_path = home.join("minecraft/versions/26.3/26.3.jar");
        fs::create_dir_all(jar_path.parent().unwrap()).unwrap();
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file(
                "assets/minecraft/textures/gui/title/background/panorama_0.png",
                opts,
            )
            .unwrap();
            std::io::Write::write_all(&mut zw, &[0u8; 150]).unwrap();
            zw.finish().unwrap();
        }

        // Write real 8,192-byte panorama_0.png + pack.mcmeta into `minecraft/assets/objects`
        let real_pano = vec![0x89u8; 8192];
        let pano_hash = hex_lower(&Sha1::digest(&real_pano));
        let pack_bytes = br#"{"pack":{"description":"Minecraft","min_format":84,"max_format":84}}"#;
        let pack_hash = hex_lower(&Sha1::digest(pack_bytes));
        let objects_dir = home.join("minecraft/assets/objects");
        for (h, data) in [(&pano_hash, real_pano.as_slice()), (&pack_hash, pack_bytes)] {
            let obj = objects_dir.join(&h[..2]).join(h);
            fs::create_dir_all(obj.parent().unwrap()).unwrap();
            fs::write(obj, data).unwrap();
        }
        let asset_index = json!({
            "objects": {
                "minecraft/textures/gui/title/background/panorama_0.png": {
                    "hash": pano_hash,
                    "size": 8192
                },
                "pack.mcmeta": {
                    "hash": pack_hash,
                    "size": pack_bytes.len()
                }
            }
        });

        prepare_mojang_on_device(
            &home,
            "26.3",
            "release",
            &jar_path,
            &asset_index,
            &objects_dir,
        )
        .expect("prepare 26.3");
        let root_26_3 = home.join("versions/26.3/resources");
        let pano_path =
            root_26_3.join("assets/minecraft/textures/gui/title/background/panorama_0.png");
        assert_eq!(fs::metadata(&pano_path).unwrap().len(), 8192);
        assert!(root_26_3.join("assets/pack.mcmeta").is_file());
        assert_eq!(version_id_for_root(&root_26_3), "26.3");

        // Simulate a legacy `ios-build-5.1` install whose panorama_0.png was still 150 bytes:
        write_unlinked(&pano_path, &[0u8; 150]).unwrap();
        let _ = fs::remove_file(root_26_3.join("assets/pack.mcmeta"));
        let _ = fs::remove_file(root_26_3.join(".horizon-assets-healed-v1"));
        fs::write(
            home.join("minecraft/versions/26.3/26.3.json"),
            r#"{"assetIndex":{"id":"26.3"}}"#,
        )
        .unwrap();
        let idx_dir = home.join("minecraft/assets/indexes");
        fs::create_dir_all(&idx_dir).unwrap();
        fs::write(
            idx_dir.join("26.3.json"),
            serde_json::to_vec(&asset_index).unwrap(),
        )
        .unwrap();

        let healed = heal_placeholder_assets(&home, &root_26_3);
        assert!(healed >= 2, "expected >= 2 healed assets, got {healed}");
        assert_eq!(fs::metadata(&pano_path).unwrap().len(), 8192);
        assert!(root_26_3.join("assets/pack.mcmeta").is_file());
        assert_eq!(heal_placeholder_assets(&home, &root_26_3), 0);

        // Verify `version_id_for_root` returns `"1.21.1"` for multi-version layered roots:
        prepare_mojang_on_device(
            &home,
            "1.21.1",
            "release",
            &jar_path,
            &asset_index,
            &objects_dir,
        )
        .expect("prepare 1.21.1");
        let root_1_21_1 = home.join("versions/1.21.1/resources");
        assert_eq!(version_id_for_root(&root_1_21_1), "1.21.1");

        let _ = fs::remove_dir_all(&home);
    }
}
