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
    path::{Component, Path, PathBuf},
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
const VERSION_PACK_FILE: &str = "version-pack.json";
const VERSION_PACK_SCHEMA: u64 = 1;
const VERSION_PACK_PRODUCER: &str = "tools/prepare_version_pack.py";

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

/// Cosmetic-only asset subtrees that may fill missing files in a foreign version. Schema-bound
/// `items/`, `models/`, `blockstates/`, `font/`, `shaders/`, `data/` and simulation tables are
/// deliberately absent: a 26.3 file in one of those paths is not a foreign-version registry.
const COSMETIC_ASSET_DIRS: &[&str] = &["assets/minecraft/textures", "assets/minecraft/sounds"];

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

/// Validate that `resources_dir` contains a complete, engine-loadable prepared tree.
///
/// For the pinned 26.3 engine this means the 26.3 data version and all required tables. For any
/// other selected version it means a *real*, parity-verified per-version Rust pack: a 26.3
/// `data_version`, registry or hard-linked table under a foreign label is refused instead of
/// being shown as playable.
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
            "Minecraft {ver} is not fully prepared. A real per-version Rust version pack is required; 26.3 tables are not a substitute."
        ));
    }
    let pinned = horizon_core::VANILLA_VERSION;
    let selected = version_id_for_root(resources_dir);
    let manifest_mc = manifest
        .get("minecraft")
        .and_then(Value::as_str)
        .unwrap_or(pinned);
    if let Some(src) = manifest.get("source_minecraft").and_then(Value::as_str)
        && src != manifest_mc
    {
        return Err(format!(
            "fake version display: source_minecraft={src}, but minecraft={manifest_mc}; refusing to run 26.3 tables under a foreign label"
        ));
    }
    let expected_dv = if selected == pinned {
        if manifest_mc != pinned {
            return Err(format!(
                "fake version display: selected Minecraft {selected} does not match manifest {manifest_mc}"
            ));
        }
        i64::from(horizon_world::DATA_VERSION)
    } else {
        if manifest_mc != selected {
            return Err(format!(
                "fake version display: selected Minecraft {selected} does not match manifest {manifest_mc}; a real version pack is required"
            ));
        }
        verify_real_version_pack(resources_dir, &selected, &manifest)?
    };
    match manifest.get("data_version").and_then(Value::as_i64) {
        Some(dv) if dv == expected_dv => {}
        Some(other) => {
            return Err(format!(
                "Game files data_version ({other}) does not match the verified version data_version ({expected_dv})."
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

/// Pre-build `horizon-data.bin` at install time so the very first launch of `1.21.1` (or an
/// imported `26.3` archive whose embedded desktop path doesn't match the iPhone sandbox) reads
/// from a single indexed file and never spawns a 120 MB background disk writer mid-gameplay.
fn build_prepared_data_cache(resources_dir: &Path) {
    if !resources_dir.join("horizon-resources.json").is_file() {
        return;
    }
    let _ = fs::remove_file(resources_dir.join(horizon_core::prepared_files::FILE_NAME));
    match horizon_core::prepared_files::write_with_readers(resources_dir, 2) {
        Ok(()) => {
            horizon_core::prepared_files::invalidate(resources_dir);
            log::info!(
                "Versions: indexed {} at {}",
                horizon_core::prepared_files::FILE_NAME,
                resources_dir.display()
            );
        }
        Err(e) => {
            log::warn!(
                "Versions: could not pre-index {} at {}: {e}",
                horizon_core::prepared_files::FILE_NAME,
                resources_dir.display()
            );
        }
    }
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
                    support_tier: if !ready && id != horizon_core::VANILLA_VERSION {
                        "needs-real-rust-pack".into()
                    } else {
                        support_tier_for_version(&id).into()
                    },
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
                    support_tier: if !ready && inferred_id != horizon_core::VANILLA_VERSION {
                        "needs-real-rust-pack".into()
                    } else {
                        support_tier_for_version(&inferred_id).into()
                    },
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

/// Return the ID of a genuinely playable active root, not merely a label from `active.json`.
/// A stale or manually edited selection that names a foreign version without a verified
/// per-version Rust pack must not be reported as the engine's active simulation.
pub fn active_version_id(home: &Path) -> String {
    list_installed(home)
        .into_iter()
        .find(|v| v.active && v.ready)
        .map(|v| v.id)
        .unwrap_or_else(|| horizon_core::VANILLA_VERSION.to_owned())
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
    if installed.iter().any(|v| v.id == clean && !v.ready) {
        return Err(format!(
            "Minecraft {clean} is installed but has no verified per-version Rust version pack. A 26.3 simulation base cannot run it; install or build a real version pack first."
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

/// Build an honest `horizon-resources.json`. The pinned 26.3 base keeps its real engine facts;
/// another version is explicitly partial until a real per-version Rust pack is installed.
fn build_manifest_from_base(base: &Path, version_id: &str) -> Value {
    let pinned = horizon_core::VANILLA_VERSION;
    if version_id != pinned {
        return json!({
            "schema": SCHEMA_VERSION,
            "minecraft": version_id,
            "simulation_version": version_id,
            "source_minecraft": version_id,
            "prepared_by": "horizon-on-device",
            "full_simulation": false,
            "simulation_status": "awaiting-real-version-pack",
        });
    }
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
    obj.insert("minecraft".to_owned(), json!(pinned));
    obj.insert("simulation_version".to_owned(), json!(pinned));
    obj.insert("full_simulation".to_owned(), json!(true));
    obj.remove("source_minecraft");
    Value::Object(obj)
}

/// Layer a pinned 26.3 simulation base into a pinned 26.3 root. For any other version, only
/// missing cosmetic asset fallbacks are linked: registries, data schemas, worldgen and simulation
/// tables are *not* 26.3 tables wearing a foreign label.
fn populate_from_simulation_base(base: &Path, target_resources: &Path, version_id: &str) -> usize {
    let pinned = version_id == horizon_core::VANILLA_VERSION;
    let mut linked = 0usize;
    if pinned {
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
        for sub in ["assets", "data"] {
            if let Ok(n) = link_missing_tree(&base.join(sub), &target_resources.join(sub)) {
                linked += n;
            }
        }
    } else {
        // Authentic version assets stay untouched; only missing cosmetic textures/audio may be
        // borrowed from 26.3. Never link 26.3 item definitions, models, blockstates, fonts,
        // shaders, data schemas or simulation tables into a foreign root.
        for rel in COSMETIC_ASSET_DIRS {
            let src = base.join(rel);
            let dst = target_resources.join(rel);
            if let Ok(n) = link_missing_tree(&src, &dst) {
                linked += n;
            }
        }
        let sound_meta_src = base.join("assets/minecraft/sounds.json");
        let sound_meta_dst = target_resources.join("assets/minecraft/sounds.json");
        if sound_meta_src.is_file()
            && !sound_meta_dst.exists()
            && link_or_copy_file(&sound_meta_src, &sound_meta_dst).is_ok()
        {
            linked += 1;
        }
    }
    let manifest = build_manifest_from_base(base, version_id);
    if let Ok(bytes) = serde_json::to_vec_pretty(&manifest) {
        let _ = write_unlinked(&target_resources.join("horizon-resources.json"), &bytes);
    }
    if pinned {
        build_prepared_data_cache(target_resources);
    }
    linked
}

/// Resolve the effective Minecraft version label for a prepared `resources` directory.
/// A real foreign version names itself consistently in `minecraft`, `simulation_version`,
/// `source_minecraft` and `version-profile.json`; a 26.3 manifest under a foreign profile is
/// treated as a fake display and refused by [`validate_playable_root`].
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
                    .join(hash[..2].to_ascii_lowercase())
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

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv_update(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= u64::from(*b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    fnv_update(&mut h, bytes);
    h
}

fn fnv1a64_files(root: &Path, names: &[&str]) -> Result<u64, String> {
    let mut h = FNV_OFFSET;
    for name in names {
        let path = root.join(name);
        if !path.is_file() {
            return Err(format!("missing version table '{name}'"));
        }
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        fnv_update(&mut h, &bytes);
    }
    Ok(h)
}

fn collect_version_pack_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let path = entry.path();
        if ty.is_file() {
            out.push(path);
        } else if ty.is_dir() {
            collect_version_pack_files(&path, out)?;
        }
    }
    Ok(())
}

fn fnv1a64_worldgen(root: &Path) -> Result<u64, String> {
    let folder = root.join("worldgen");
    if !folder.is_dir() {
        return Err("missing worldgen/ folder".into());
    }
    let mut files = Vec::new();
    collect_version_pack_files(&folder, &mut files)?;
    files.sort_by_key(|p| {
        p.strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    });
    let mut h = FNV_OFFSET;
    for path in files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        fnv_update(&mut h, rel.as_bytes());
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        fnv_update(&mut h, &bytes);
    }
    Ok(h)
}

fn is_real_sha256(value: Option<&str>) -> bool {
    value.is_some_and(|h| {
        h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) && h.bytes().any(|b| b != b'0')
    })
}

fn version_pack_str<'a>(pack: &'a Value, key: &str) -> Result<&'a str, String> {
    pack.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("version-pack.json: missing string '{key}'"))
}

fn version_pack_u64(pack: &Value, key: &str) -> Result<u64, String> {
    pack.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("version-pack.json: missing unsigned integer '{key}'"))
}

fn version_pack_i64(pack: &Value, key: &str) -> Result<i64, String> {
    pack.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("version-pack.json: missing integer '{key}'"))
}

/// Verify a *real* translated version. The 26.3 engine cannot run another version by changing
/// a label: this binds a prepared root to its own data version, protocol, Rust build, parity
/// harness, registries/items/worldgen digests and the exact prepared artifact bytes.
fn verify_real_version_pack(
    resources_dir: &Path,
    version_id: &str,
    manifest: &Value,
) -> Result<i64, String> {
    let pack_path = resources_dir.join(VERSION_PACK_FILE);
    let text = fs::read_to_string(&pack_path).map_err(|e| {
        format!(
            "Minecraft {version_id} needs a real per-version Rust version pack ({VERSION_PACK_FILE}): {e}"
        )
    })?;
    let pack: Value = serde_json::from_str(&text)
        .map_err(|e| format!("version-pack.json for {version_id} is invalid JSON: {e}"))?;
    if pack.get("schema").and_then(Value::as_u64) != Some(VERSION_PACK_SCHEMA) {
        return Err(format!(
            "version-pack.json for {version_id} has an unknown or missing schema"
        ));
    }
    for key in ["id", "simulation_version", "minecraft"] {
        if version_pack_str(&pack, key)? != version_id {
            return Err(format!(
                "version-pack.json field '{key}' does not name Minecraft {version_id}"
            ));
        }
    }
    if version_pack_str(&pack, "engine_pin")? != horizon_core::VANILLA_VERSION {
        return Err("version-pack.json was built against another engine pin".into());
    }
    let data_version = version_pack_i64(&pack, "data_version")?;
    if data_version <= 0 || data_version == i64::from(horizon_world::DATA_VERSION) {
        return Err(format!(
            "version-pack.json for {version_id} must name its own data_version, not 26.3's {}",
            horizon_world::DATA_VERSION
        ));
    }
    let protocol = version_pack_i64(&pack, "protocol")?;
    if protocol <= 0 {
        return Err("version-pack.json has no real protocol version".into());
    }
    if pack.get("parity_harness_passed").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "Minecraft {version_id} has no passed per-version Rust parity harness"
        ));
    }
    for key in [
        "engine_build_sha256",
        "rust_sources_sha256",
        "artifact_sha256",
    ] {
        if !is_real_sha256(pack.get(key).and_then(Value::as_str)) {
            return Err(format!("version-pack.json '{key}' is not a real SHA-256"));
        }
    }
    let built_by = version_pack_str(&pack, "built_by")?;
    if !built_by.contains(VERSION_PACK_PRODUCER) {
        return Err(format!(
            "version-pack.json for {version_id} has no verified version-pack producer provenance"
        ));
    }
    let manifest_mc = manifest
        .get("minecraft")
        .and_then(Value::as_str)
        .unwrap_or(horizon_core::VANILLA_VERSION);
    if manifest_mc != version_id {
        return Err(format!(
            "fake version display: manifest says Minecraft {manifest_mc}, selection says {version_id}"
        ));
    }
    if let Some(sim) = manifest.get("simulation_version").and_then(Value::as_str)
        && sim != version_id
    {
        return Err("prepared root simulation_version disagrees with the selected version".into());
    }
    if let Some(src) = manifest.get("source_minecraft").and_then(Value::as_str)
        && src != version_id
    {
        return Err("prepared root source_minecraft disagrees with the selected version".into());
    }
    if manifest.get("full_simulation").and_then(Value::as_bool) != Some(true) {
        return Err("prepared root is not marked as a complete version simulation".into());
    }
    if manifest.get("data_version").and_then(Value::as_i64) != Some(data_version) {
        return Err("version-pack.json data_version disagrees with horizon-resources.json".into());
    }
    if manifest.get("protocol").and_then(Value::as_i64) != Some(protocol) {
        return Err("version-pack.json protocol disagrees with horizon-resources.json".into());
    }

    let artifact_rel = version_pack_str(&pack, "artifact_path")?;
    let artifact_path = Path::new(artifact_rel);
    if artifact_rel.is_empty()
        || artifact_path.is_absolute()
        || artifact_path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("version-pack.json artifact_path must stay inside the prepared root".into());
    }
    let artifact = fs::read(resources_dir.join(artifact_path))
        .map_err(|e| format!("could not read version artifact {artifact_rel}: {e}"))?;
    let actual_sha = hex_lower(&Sha256::digest(&artifact));
    let expected_sha = version_pack_str(&pack, "artifact_sha256")?.to_ascii_lowercase();
    if actual_sha != expected_sha {
        return Err(format!(
            "version artifact SHA-256 mismatch (expected {expected_sha}, got {actual_sha})"
        ));
    }
    if fnv1a64(&artifact) != version_pack_u64(&pack, "artifact_digest")? {
        return Err("version artifact FNV digest mismatch".into());
    }
    let registries = fnv1a64_files(
        resources_dir,
        &[
            "registries.json",
            "entity-types.json",
            "command-registries.json",
        ],
    )?;
    if registries != version_pack_u64(&pack, "registries_digest")? {
        return Err("version registries digest mismatch".into());
    }
    let items = fnv1a64_files(
        resources_dir,
        &["items.json", "creative-inventory.json", "model-states.json"],
    )?;
    if items != version_pack_u64(&pack, "items_digest")? {
        return Err("version item registry digest mismatch".into());
    }
    if fnv1a64_worldgen(resources_dir)? != version_pack_u64(&pack, "worldgen_digest")? {
        return Err("version worldgen digest mismatch".into());
    }
    Ok(data_version)
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
    if let Err(e) = validate_playable_root(&installed.path) {
        let _ = fs::remove_dir_all(&version_home);
        return Err(format!(
            "Prepared bundle cannot be installed as Minecraft {version_id}: {e}"
        ));
    }
    build_prepared_data_cache(&installed.path);
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

/// Install/repair the pinned 26.3 base when possible. Foreign versions remain partial until a
/// real per-version Rust pack is present: 26.3 tables are never promoted to a foreign version.
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
        "Checking verified per-version runtime…",
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
        validate_playable_root(&res).map_err(|e| {
            format!("Minecraft {tid} is not playable: {e}. A real per-version Rust version pack is required.")
        })?;
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
            build_prepared_data_cache(&v26_dir);
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
                    if done_now.is_multiple_of(8) || done_now == total_files {
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
/// - For older/snapshot versions (`!= 26.3`): keeps authentic non-GUI textures, title panorama
///   faces and `.ogg` audio from that version, with pre-1.13 texture flattening renames. Missing
///   cosmetic texture/sound files may be borrowed from the 26.3 base, but 26.3 schema-bound tables,
///   registries, `data/`, `models/`, `blockstates/`, `items/`, `shaders/` and `font/` files are
///   never linked. The install stays partial until a real per-version Rust pack is verified.
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
            // panorama faces) and audio. Cosmetic gaps may be filled later from `sim_base`, but
            // 26.3 schema-bound files must never be linked into a foreign root.
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
                .join(hash[..2].to_ascii_lowercase())
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
    let mut notes = format!("Official Mojang {clean} assets unpacked on device.");
    if let Some(base) = sim_base.as_deref() {
        let linked = populate_from_simulation_base(base, &staging, &clean);
        files += linked;
        if is_native_pinned {
            validate_playable_root(&staging)?;
            full_simulation = true;
            notes = format!(
                "Mojang {clean} assets + pinned Horizon 26.3 simulation runtime ({linked} files)."
            );
        } else {
            notes = format!(
                "Mojang {clean} authentic assets + {linked} 26.3 cosmetic fallbacks. A real per-version Rust version pack is required to play; 26.3 simulation tables were not relabelled."
            );
        }
    } else {
        let manifest = json!({
            "schema": SCHEMA_VERSION,
            "minecraft": clean,
            "simulation_version": clean,
            "source_minecraft": clean,
            "prepared_by": "horizon-on-device",
            "full_simulation": false,
            "simulation_status": "awaiting-real-version-pack",
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        write_unlinked(&staging.join("horizon-resources.json"), &manifest_bytes)
            .map_err(|e| e.to_string())?;
        notes.push_str(" A real per-version Rust version pack is required to play.");
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
    if full_simulation {
        build_prepared_data_cache(&final_root);
    }

    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::adapter::{
        ServerVersion, preferred_server_version, set_preferred_server_version,
    };
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Mutex,
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());

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
        validate_playable_root(root).expect("pinned 26.3 fixture must pass");
    }

    /// A *contract* fixture: synthetic bytes prove the binding logic, not that these bytes are
    /// really Minecraft 1.20.4. A real pack comes from version_port.py + parity harnesses.
    fn write_real_version_pack_fixture(
        root: &Path,
        version_id: &str,
        data_version: i64,
        protocol: i64,
    ) {
        for rel in REQUIRED_SIMULATION_FILES
            .iter()
            .chain(REQUIRED_NESTED_FILES.iter())
        {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, format!("{version_id}/{rel}").as_bytes()).unwrap();
        }
        let artifact = format!("{version_id}/prepared-horizon-data.bin").into_bytes();
        fs::write(root.join("horizon-data.bin"), &artifact).unwrap();
        let manifest = json!({
            "schema": 7,
            "minecraft": version_id,
            "simulation_version": version_id,
            "source_minecraft": version_id,
            "data_version": data_version,
            "protocol": protocol,
            "full_simulation": true,
        });
        fs::write(
            root.join("horizon-resources.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let pack = json!({
            "schema": VERSION_PACK_SCHEMA,
            "id": version_id,
            "simulation_version": version_id,
            "minecraft": version_id,
            "data_version": data_version,
            "protocol": protocol,
            "engine_pin": horizon_core::VANILLA_VERSION,
            "engine_build_sha256": "a".repeat(64),
            "rust_sources_sha256": "b".repeat(64),
            "parity_harness_passed": true,
            "registries_digest": fnv1a64_files(root, &["registries.json", "entity-types.json", "command-registries.json"]).unwrap(),
            "items_digest": fnv1a64_files(root, &["items.json", "creative-inventory.json", "model-states.json"]).unwrap(),
            "worldgen_digest": fnv1a64_worldgen(root).unwrap(),
            "artifact_digest": fnv1a64(&artifact),
            "artifact_sha256": hex_lower(&Sha256::digest(&artifact)),
            "artifact_path": "horizon-data.bin",
            "built_by": format!("{VERSION_PACK_PRODUCER} --version {version_id}"),
            "translation_report": "rust-translation-report.json",
        });
        fs::write(
            root.join(VERSION_PACK_FILE),
            serde_json::to_vec_pretty(&pack).unwrap(),
        )
        .unwrap();
        validate_playable_root(root).expect("verified synthetic version pack must pass");
    }

    #[test]
    fn a_real_version_pack_changes_the_playable_version_binding() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_preferred_server_version(None);
        let home = temp_home("real-version-pack");
        let foreign = home.join("versions/1.20.4/resources");
        write_real_version_pack_fixture(&foreign, "1.20.4", 3700, 765);
        let selected = select_version(&home, "1.20.4").expect("real 1.20.4 pack is playable");
        assert!(selected.ready && selected.full_simulation);
        assert_eq!(selected.support_tier, "protocol-adapter");
        assert_eq!(active_ready_root(&home), Some(foreign.clone()));
        assert_eq!(preferred_server_version(), Some(ServerVersion::V1_20_3));
        assert_eq!(
            crate::import::resources_root(&foreign, Some(&home)),
            Some(foreign)
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_tampered_version_pack_and_a_fake_manifest_are_refused() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_preferred_server_version(None);
        let home = temp_home("refuse-fake-pack");
        let foreign = home.join("versions/1.21.1/resources");
        write_real_version_pack_fixture(&foreign, "1.21.1", 3953, 767);
        fs::write(foreign.join("horizon-data.bin"), b"tampered").unwrap();
        let err = validate_playable_root(&foreign).unwrap_err();
        assert!(err.contains("SHA-256") || err.contains("digest"), "{err}");
        assert!(select_version(&home, "1.21.1").is_err());

        // Restore the artifact, then re-label 26.3 tables as 1.21.1: still refused.
        let artifact = b"1.21.1/prepared-horizon-data.bin".to_vec();
        fs::write(foreign.join("horizon-data.bin"), &artifact).unwrap();
        let fake = json!({
            "schema": 7,
            "minecraft": horizon_core::VANILLA_VERSION,
            "source_minecraft": "1.21.1",
            "data_version": horizon_world::DATA_VERSION,
            "full_simulation": true,
        });
        fs::write(
            foreign.join("horizon-resources.json"),
            serde_json::to_vec_pretty(&fake).unwrap(),
        )
        .unwrap();
        let err = validate_playable_root(&foreign).unwrap_err();
        assert!(
            err.contains("fake version") || err.contains("version pack"),
            "{err}"
        );
        assert!(select_version(&home, "1.21.1").is_err());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn mojang_on_device_preparation_keeps_classic_textures_and_refuses_fake_upgrade() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_preferred_server_version(None);
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

        let prof = prepare_mojang_on_device(
            &home,
            "1.12.2",
            "release",
            &jar_path,
            &asset_index,
            &objects_dir,
        )
        .expect("prepare 1.12.2 authentic assets");
        assert!(!prof.full_simulation);
        let ver_root = home.join("versions/1.12.2/resources");
        assert!(has_partial_mojang_root(&ver_root));
        assert!(has_any_partial_mojang(&home));
        assert!(!is_ready_root(&ver_root));
        assert!(select_version(&home, "1.12.2").is_err());
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

        let base_26_3 = home.join("versions/26.3/resources");
        write_simulated_prepared_base(&base_26_3);
        let upgraded = upgrade_partial_mojang_versions(&home, &base_26_3);
        assert_eq!(
            upgraded, 0,
            "26.3 tables must not upgrade a foreign version"
        );
        assert!(!is_ready_root(&ver_root));
        assert!(has_any_partial_mojang(&home));
        assert!(!ver_root.join("model-states.json").is_file());
        assert!(!ver_root.join("creative-inventory.json").is_file());
        assert!(!ver_root.join("assets/minecraft/items/stone.json").is_file());
        assert!(
            !ver_root
                .join(horizon_core::prepared_files::FILE_NAME)
                .is_file()
        );
        assert!(!ver_root.join("data/minecraft/recipes/stone.json").exists());
        let base_manifest: Value =
            serde_json::from_slice(&fs::read(base_26_3.join("horizon-resources.json")).unwrap())
                .unwrap();
        assert!(base_manifest.get("source_minecraft").is_none());

        select_version(&home, "26.3").unwrap();
        assert_eq!(active_version_id(&home), "26.3");
        assert_eq!(
            crate::import::resources_root(&ver_root, Some(&home)),
            Some(base_26_3.clone())
        );
        delete_version(&home, "1.12.2").unwrap();
        assert!(!ver_root.exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn active_version_id_refuses_a_stale_foreign_selection_without_a_verified_root() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_preferred_server_version(None);
        let home = temp_home("stale-active-selection");
        save_selection(
            &home,
            &ActiveSelection {
                active: "1.20.4".into(),
                older_servers: true,
            },
        )
        .expect("write stale selection");
        assert!(active_ready_root(&home).is_none());
        assert_eq!(active_version_id(&home), horizon_core::VANILLA_VERSION);
        apply_active_protocol_preference(&home);
        assert_eq!(preferred_server_version(), None);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn downloads_prepared_zip_from_live_local_http_server_with_sha256_verification() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_preferred_server_version(None);
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
        run_prepared_download(&job, &home, &url, "26.3", Some(&expected_sha256))
            .expect("HTTP prepared download & SHA-256 verify");
        server.join().unwrap();

        let installed = list_installed(&home);
        let v = installed
            .iter()
            .find(|i| i.id == "26.3")
            .expect("26.3 installed");
        assert!(v.ready && v.full_simulation && v.active);
        assert_eq!(preferred_server_version(), None);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_26_3_zip_cannot_be_downloaded_as_a_foreign_version() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_preferred_server_version(None);
        let home = temp_home("refuse-relabelled-zip");
        let src_tree = home.join("pc-prepared-26.3");
        write_simulated_prepared_base(&src_tree);
        let zip_file = home.join("served-26.3.zip");
        crate::import::pack(&src_tree, &zip_file, |_, _| {}).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let zip_bytes = fs::read(&zip_file).unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req = [0u8; 2048];
                let _ = stream.read(&mut req);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    zip_bytes.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&zip_bytes);
            }
        });
        let job = global_job().clone();
        let url = format!("http://127.0.0.1:{port}/fake-1.21.4.zip");
        let err = run_prepared_download(&job, &home, &url, "1.21.4", None).unwrap_err();
        assert!(
            err.contains("cannot be installed as Minecraft 1.21.4"),
            "{err}"
        );
        assert!(
            err.contains("fake version") || err.contains("version pack"),
            "{err}"
        );
        server.join().unwrap();
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn prepare_mojang_overwrites_jar_placeholders_and_heals_existing_installs() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_preferred_server_version(None);
        let home = temp_home("heal-pano");
        let base_26_3 = home.join("versions/.sim-base-26.3/resources");
        write_simulated_prepared_base(&base_26_3);

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

        prepare_mojang_on_device(
            &home,
            "1.21.1",
            "release",
            &jar_path,
            &asset_index,
            &objects_dir,
        )
        .expect("prepare 1.21.1 authentic assets");
        let root_1_21_1 = home.join("versions/1.21.1/resources");
        assert_eq!(version_id_for_root(&root_1_21_1), "1.21.1");
        assert!(validate_playable_root(&root_1_21_1).is_err());
        assert!(!root_1_21_1.join("model-states.json").is_file());

        let _ = fs::remove_dir_all(&home);
    }
}
