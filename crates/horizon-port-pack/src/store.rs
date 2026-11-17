//! Live Modrinth discovery, SHA-1 verified `.jar` download, and pure-Rust on-device Java mod
//! porting into `horizon-mods/<mod_id>/` for mobile and desktop launchers.
//!
//! Uses only Modrinth's public v2 API (`api.modrinth.com`) and `.mrpack`-allowed download hosts
//! (`cdn.modrinth.com`, `github.com`, `raw.githubusercontent.com`, `gitlab.com`).

use crate::{
    carry,
    check::{self, Verdict},
    fetch::{HOSTS as ALLOWED_DOWNLOAD_HOSTS, store_download},
    jar,
    modrinth::{self, API},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Preferred Minecraft version order when picking a Modrinth `.jar` to port into Horizon.
const PREFERRED_VERSIONS: &[&str] = &[
    "26.3", "26.2", "26.1.2", "26.1.1", "26.1", "1.21.11", "1.21.10", "1.21.9", "1.21.8",
    "1.21.7", "1.21.6", "1.21.5", "1.21.4", "1.21.3", "1.21.2", "1.21.1", "1.21", "1.20.6",
    "1.20.5", "1.20.4", "1.20.2", "1.20.1",
];

/// Metadata stored in `horizon-mods/.sources/<mod_id>.json` so the launcher can show Modrinth
/// provenance, compatibility verdict, ported feature counts, and update checks.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceMetadata {
    pub mod_id: String,
    pub name: String,
    pub version: String,
    pub loader: String,
    /// `"modrinth"` or `"local-jar"`.
    pub origin: String,
    pub verdict: String,
    pub summary: String,
    pub ports: Vec<String>,
    pub skipped: Vec<String>,
    pub namespaces: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// Outcome of inspecting or installing a Java `.jar` / Modrinth mod.
#[derive(Clone, Debug)]
pub struct InstalledPort {
    pub metadata: SourceMetadata,
    pub installed_dir: Option<PathBuf>,
}

/// Percent-encode a URL query component or path segment.
fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn get_json(url: &str) -> Result<Value, String> {
    let response = crate::net::agent(url)
        .get(url)
        .set("User-Agent", modrinth::AGENT)
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let text = response.into_string().map_err(|e| format!("{url}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{url}: {e}"))
}

fn verdict_slug(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Works => "works",
        Verdict::Partly => "partly",
        Verdict::No => "no",
    }
}

/// Search Modrinth for mods (`project_type:mod`) and return a clean launcher JSON payload.
pub fn search_projects(
    query: &str,
    minecraft_version: Option<&str>,
    loader: Option<&str>,
    sort: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<String, String> {
    let limit = limit.clamp(1, 50);
    let index = match sort.unwrap_or("downloads") {
        "relevance" | "downloads" | "follows" | "newest" | "updated" => sort.unwrap_or("downloads"),
        _ => "downloads",
    };
    let mut facets: Vec<Vec<String>> = vec![vec!["project_type:mod".into()]];
    if let Some(mc) = minecraft_version.map(str::trim).filter(|s| !s.is_empty() && *s != "all") {
        // When browsing for 26.3 ("compatible"), include 1.20.x + 1.21.x + 26.x mods that
        // Horizon's pure-Rust carry pipeline upgrades automatically; otherwise filter to exact.
        if mc == "compatible" {
            facets.push(
                PREFERRED_VERSIONS
                    .iter()
                    .map(|v| format!("versions:{v}"))
                    .collect(),
            );
        } else {
            facets.push(vec![format!("versions:{mc}")]);
        }
    }
    if let Some(loader) = loader
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "all")
    {
        facets.push(vec![format!("categories:{loader}")]);
    } else if query.trim().is_empty() {
        // Default Modrinth browse (empty query): surface content, worldgen, structure, food,
        // decoration, equipment, magic, and datapack mods rather than pure-Java API libraries.
        facets.push(
            [
                "datapack",
                "worldgen",
                "adventure",
                "decoration",
                "food",
                "equipment",
                "magic",
                "mobs",
                "game-mechanics",
                "storage",
                "technology",
                "transportation",
            ]
            .into_iter()
            .map(|c| format!("categories:{c}"))
            .collect(),
        );
    }
    let facets_json = serde_json::to_string(&facets).map_err(|e| e.to_string())?;
    let url = format!(
        "{API}/search?query={}&index={}&limit={limit}&offset={offset}&facets={}",
        encode_component(query.trim()),
        encode_component(index),
        encode_component(&facets_json)
    );
    let raw = get_json(&url)?;
    let hits: Vec<Value> = raw["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| {
            let categories: Vec<&str> = h["categories"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let versions: Vec<&str> = h["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let compatibility_hint = estimate_search_compatibility(&categories, &versions);
            json!({
                "project_id": h["project_id"].as_str().unwrap_or_default(),
                "slug": h["slug"].as_str().unwrap_or_default(),
                "title": h["title"].as_str().unwrap_or_default(),
                "description": h["description"].as_str().unwrap_or_default(),
                "author": h["author"].as_str().unwrap_or_default(),
                "downloads": h["downloads"].as_u64().unwrap_or(0),
                "follows": h["follows"].as_u64().unwrap_or(0),
                "icon_url": h["icon_url"].as_str(),
                "client_side": h["client_side"].as_str().unwrap_or_default(),
                "server_side": h["server_side"].as_str().unwrap_or_default(),
                "categories": categories,
                "versions": versions,
                "latest_version": h["latest_version"].as_str(),
                "license": h["license"].as_str().unwrap_or_default(),
                "compatibility_hint": compatibility_hint,
            })
        })
        .collect();
    let payload = json!({
        "source": "modrinth",
        "total_hits": raw["total_hits"].as_u64().unwrap_or(hits.len() as u64),
        "offset": raw["offset"].as_u64().unwrap_or(offset as u64),
        "limit": raw["limit"].as_u64().unwrap_or(limit as u64),
        "hits": hits,
    });
    serde_json::to_string(&payload).map_err(|e| e.to_string())
}

/// Heuristic hint before downloading the `.jar` (the exact bytecode scan runs during check/install).
pub fn estimate_search_compatibility(categories: &[&str], versions: &[&str]) -> &'static str {
    let has_modern = versions.iter().any(|v| {
        v.starts_with("26.") || v.starts_with("1.21") || v.starts_with("1.20")
    });
    if !has_modern && !versions.is_empty() {
        return "legacy-version";
    }
    let content_like = categories.iter().any(|c| {
        matches!(
            *c,
            "datapack"
                | "worldgen"
                | "decoration"
                | "building"
                | "food"
                | "equipment"
                | "adventure"
                | "magic"
                | "mobs"
                | "storage"
                | "technology"
                | "transportation"
                | "game-mechanics"
        )
    });
    let pure_code_like = categories
        .iter()
        .any(|c| matches!(*c, "optimization" | "library" | "gui" | "social"))
        && !content_like;
    if pure_code_like {
        "code-only"
    } else if categories.contains(&"datapack") {
        "high"
    } else {
        "port-ready"
    }
}

/// Fetch and rank `.jar` versions for one Modrinth project.
pub fn project_versions(
    project_id: &str,
    minecraft_version: Option<&str>,
    loader: Option<&str>,
) -> Result<String, String> {
    let id = project_id.trim();
    if id.is_empty() {
        return Err("missing Modrinth project id".into());
    }
    let url = format!("{API}/project/{}/version", encode_component(id));
    let list = get_json(&url)?;
    let versions = filter_and_rank_versions(&list, minecraft_version, loader);
    let out: Vec<Value> = versions
        .into_iter()
        .take(30)
        .filter_map(|v| {
            let file = pick_jar_file(&v)?;
            let required_deps: Vec<&str> = v["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|d| d["dependency_type"] == "required")
                .filter_map(|d| d["project_id"].as_str())
                .collect();
            Some(json!({
                "id": v["id"].as_str().unwrap_or_default(),
                "name": v["name"].as_str().unwrap_or_default(),
                "version_number": v["version_number"].as_str().unwrap_or_default(),
                "version_type": v["version_type"].as_str().unwrap_or("release"),
                "game_versions": v["game_versions"],
                "loaders": v["loaders"],
                "downloads": v["downloads"].as_u64().unwrap_or(0),
                "date_published": v["date_published"].as_str().unwrap_or_default(),
                "file_name": file["filename"].as_str().unwrap_or_default(),
                "size": file["size"].as_u64().unwrap_or(0),
                "sha1": file["hashes"]["sha1"].as_str(),
                "required_dependencies": required_deps,
            }))
        })
        .collect();
    serde_json::to_string(&json!({
        "project_id": id,
        "versions": out,
    }))
    .map_err(|e| e.to_string())
}

fn pick_jar_file(version: &Value) -> Option<&Value> {
    let files = version["files"].as_array()?;
    let ok_file = |f: &&Value| {
        let name = f["filename"].as_str().unwrap_or_default();
        let url = f["url"].as_str().unwrap_or_default();
        name.ends_with(".jar")
            && !name.ends_with("-sources.jar")
            && !name.ends_with("-javadoc.jar")
            && ALLOWED_DOWNLOAD_HOSTS.iter().any(|h| url.starts_with(h))
    };
    files
        .iter()
        .find(|f| f["primary"].as_bool().unwrap_or(false) && ok_file(f))
        .or_else(|| files.iter().find(ok_file))
}

fn version_rank(version: &Value, target_mc: Option<&str>, target_loader: Option<&str>) -> Option<usize> {
    pick_jar_file(version)?;
    let game_versions: Vec<&str> = version["game_versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let loaders: Vec<&str> = version["loaders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if let Some(want_loader) = target_loader
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "all")
        && !loaders.iter().any(|l| l.eq_ignore_ascii_case(want_loader))
    {
        return None;
    }
    let mc_score = if let Some(want_mc) = target_mc
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "all" && *s != "compatible")
    {
        if game_versions.contains(&want_mc) {
            0
        } else {
            PREFERRED_VERSIONS
                .iter()
                .position(|p| game_versions.contains(p))
                .map(|idx| idx + 1)
                .unwrap_or(100)
        }
    } else {
        PREFERRED_VERSIONS
            .iter()
            .position(|p| game_versions.contains(p))
            .unwrap_or(100)
    };
    let loader_score = if loaders.contains(&"datapack") {
        0
    } else if loaders.contains(&"fabric") {
        1
    } else if loaders.contains(&"neoforge") {
        2
    } else if loaders.contains(&"quilt") {
        3
    } else {
        4
    };
    let release_score = match version["version_type"].as_str().unwrap_or("release") {
        "release" => 0,
        "beta" => 1,
        _ => 2,
    };
    Some(mc_score * 20 + loader_score * 3 + release_score)
}

fn filter_and_rank_versions(
    raw_list: &Value,
    minecraft_version: Option<&str>,
    loader: Option<&str>,
) -> Vec<Value> {
    let mut ranked: Vec<(usize, usize, Value)> = raw_list
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(idx, v)| {
            let score = version_rank(v, minecraft_version, loader)?;
            Some((score, idx, v.clone()))
        })
        .collect();
    ranked.sort_by_key(|(score, idx, _)| (*score, *idx));
    ranked.into_iter().map(|(_, _, v)| v).collect()
}

/// Download a Modrinth mod `.jar`, verify SHA-1, run pure-Rust bytecode analysis + content carry,
/// and publish into `mods_dir/<mod_id>/`.
pub fn install_from_modrinth(
    project_id: &str,
    version_id: Option<&str>,
    minecraft_version: Option<&str>,
    mods_dir: &Path,
    work_dir: &Path,
) -> Result<InstalledPort, String> {
    let id = project_id.trim();
    if id.is_empty() {
        return Err("missing Modrinth project id".into());
    }
    let project_url = format!("{API}/project/{}", encode_component(id));
    let project = get_json(&project_url).ok();
    let icon_url = project
        .as_ref()
        .and_then(|p| p["icon_url"].as_str())
        .map(str::to_owned);
    let canonical_project_id = project
        .as_ref()
        .and_then(|p| p["id"].as_str())
        .unwrap_or(id)
        .to_owned();

    let version = if let Some(vid) = version_id.map(str::trim).filter(|s| !s.is_empty()) {
        get_json(&format!("{API}/version/{}", encode_component(vid)))?
    } else {
        let list = get_json(&format!(
            "{API}/project/{}/version",
            encode_component(&canonical_project_id)
        ))?;
        filter_and_rank_versions(&list, minecraft_version, None)
            .into_iter()
            .next()
            .ok_or_else(|| format!("no downloadable .jar release found on Modrinth for {id}"))?
    };

    let chosen_version_id = version["id"].as_str().map(str::to_owned);
    let file = pick_jar_file(&version)
        .ok_or_else(|| format!("Modrinth version for {id} has no downloadable .jar"))?;
    let download_url = file["url"]
        .as_str()
        .ok_or("Modrinth file entry has no download URL")?;
    let sha1 = file["hashes"]["sha1"]
        .as_str()
        .filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("Modrinth file entry has no valid SHA-1 checksum")?;
    let raw_name = file["filename"].as_str().unwrap_or("mod.jar");
    let safe_name: String = raw_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();

    let stage = work_dir.join(format!(
        "modrinth-{}",
        chosen_version_id.as_deref().unwrap_or("latest")
    ));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let jar_dest = stage.join(if safe_name.ends_with(".jar") {
        safe_name
    } else {
        format!("{safe_name}.jar")
    });

    let response = crate::net::agent(download_url)
        .get(download_url)
        .set("User-Agent", modrinth::AGENT)
        .call()
        .map_err(|e| format!("{download_url}: {e}"))?;
    store_download(response.into_reader(), Some(sha1), &jar_dest, true)
        .map_err(|e| format!("{download_url}: {e}"))?;

    let mut result = port_local_jar(&jar_dest, Some(mods_dir), &stage.join("port"))?;
    result.metadata.origin = "modrinth".into();
    result.metadata.project_id = Some(canonical_project_id);
    result.metadata.version_id = chosen_version_id;
    result.metadata.icon_url = icon_url;
    if let Some(title) = project
        .as_ref()
        .and_then(|p| p["title"].as_str())
        .filter(|s| !s.is_empty())
    {
        result.metadata.name = title.to_owned();
    }
    if result.installed_dir.is_some() {
        let _ = save_source_metadata(mods_dir, &result.metadata);
    }
    let _ = fs::remove_dir_all(&stage);
    Ok(result)
}

/// Sanitize any Fabric/Quilt/Forge/NeoForge mod id (e.g. `"farmers-delight"`, `"BetterEnd"`)
/// into a valid Horizon `[a-z0-9_]{1,64}` identifier.
fn sanitize_mod_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(64));
    for ch in raw.trim().chars() {
        let mapped = match ch {
            'A'..='Z' => Some(ch.to_ascii_lowercase()),
            'a'..='z' | '0'..='9' | '_' => Some(ch),
            '-' | '.' | ' ' => Some('_'),
            _ => None,
        };
        if let Some(c) = mapped {
            if c == '_' && out.ends_with('_') {
                continue;
            }
            out.push(c);
            if out.len() >= 64 {
                break;
            }
        }
    }
    let trimmed = out.trim_matches('_').to_owned();
    if trimmed.is_empty() {
        "ported_mod".into()
    } else if trimmed.as_bytes()[0].is_ascii_digit() {
        let mut p = format!("m_{trimmed}");
        p.truncate(64);
        p
    } else {
        trimmed
    }
}

fn valid_horizon_ns(ns: &str) -> bool {
    (1..=64).contains(&ns.len())
        && ns
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

struct PreparedJar {
    path: PathBuf,
    effective_id: String,
    display_name: String,
    version: String,
    original_loader: String,
    resource_pack_entries: Vec<(String, Vec<u8>)>,
    nested_jars_merged: usize,
    old_recipes_upgraded: usize,
}

/// Normalize a `.jar` before `check::check` / `carry::carry`:
/// 1. Flatten nested Jar-in-Jar sub-archives (`META-INF/jars/*.jar`, `META-INF/jarjar/*.jar`).
/// 2. Sanitize hyphenated/uppercase `mod_id`s and resolve the real primary content namespace
///    when a wrapper id (such as Modrinth's `mr_*` datapack wrapper) owns 0 files of its own.
/// 3. Upgrade 1.20.x plural data folders (`recipes/`, `loot_tables/`, `structures/`, `tags/blocks/`…)
///    and pre-1.20.5 `"result": {"item": ...}` recipes into 26.3's layout.
/// 4. Collect `assets/` visual/sound/model/lang files so pure resource-pack `.jar` mods can be
///    bridged directly into `Documents/resourcepacks/mod-<id>.zip` when they declare no registries.
fn prepare_jar_for_porting(jar_path: &Path, stage_dir: &Path) -> Result<PreparedJar, String> {
    use std::io::{Cursor, Write};
    let info = jar::read(jar_path)?;
    let raw_file = fs::File::open(jar_path).map_err(|e| format!("{}: {e}", jar_path.display()))?;
    let mut zip =
        zip::ZipArchive::new(raw_file).map_err(|e| format!("{}: {e}", jar_path.display()))?;
    let outer_names: Vec<String> = zip
        .file_names()
        .map(|n| n.map(str::to_owned))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", jar_path.display()))?;
    let original_loader = if outer_names.iter().any(|n| n == "META-INF/neoforge.mods.toml") {
        "neoforge"
    } else if outer_names.iter().any(|n| n == "META-INF/mods.toml") {
        "forge"
    } else if outer_names.iter().any(|n| n == "fabric.mod.json") {
        "fabric"
    } else if outer_names.iter().any(|n| n == "quilt.mod.json") {
        "quilt"
    } else {
        "jar"
    }
    .to_owned();

    let mut budget = crate::entry::Budget::archive();
    let mut flat_entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut nested_jars: Vec<Vec<u8>> = Vec::new();

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        if name.ends_with('/') {
            continue;
        }
        let declared = entry.size();
        if (name.starts_with("META-INF/jars/") || name.starts_with("META-INF/jarjar/"))
            && name.ends_with(".jar")
            && nested_jars.len() < 24
        {
            if let Ok(bytes) = budget.read(&mut entry, declared, 64 << 20) {
                nested_jars.push(bytes);
            }
            continue;
        }
        // Keep data/, assets/, pack.mcmeta, and .class files (for accurate behaviour census).
        if name.starts_with("assets/")
            || name.starts_with("data/")
            || name == "pack.mcmeta"
            || name == "pack.png"
            || name.ends_with(".class")
        {
            let cap = if name.ends_with(".class") {
                (1 << 24) - 1
            } else {
                crate::entry::MAX_FILE
            };
            match budget.read(&mut entry, declared, cap) {
                Ok(bytes) => {
                    flat_entries.insert(name, bytes);
                }
                Err(e) if crate::entry::is_too_big(&e) && declared < cap => {
                    return Err(format!(
                        "{}: archive unpacks past safety budget",
                        jar_path.display()
                    ));
                }
                Err(_) => {}
            }
        }
    }

    let mut nested_jars_merged = 0usize;
    for nested_bytes in nested_jars {
        let Ok(mut inner) = zip::ZipArchive::new(Cursor::new(nested_bytes)) else {
            continue;
        };
        let mut contributed = false;
        for j in 0..inner.len() {
            let Ok(mut entry) = inner.by_index(j) else {
                continue;
            };
            let Ok(name) = entry.name().map(str::to_owned) else {
                continue;
            };
            if name.ends_with('/') || flat_entries.contains_key(&name) {
                continue;
            }
            if name.starts_with("assets/") || name.starts_with("data/") {
                let declared = entry.size();
                if let Ok(bytes) = budget.read(&mut entry, declared, crate::entry::MAX_FILE) {
                    flat_entries.insert(name, bytes);
                    contributed = true;
                }
            }
        }
        if contributed {
            nested_jars_merged += 1;
        }
    }

    // Upgrade 1.20.x plural folder paths (`recipes/` -> `recipe/`, `loot_tables/` -> `loot_table/`,
    // `structures/` -> `structure/`, `tags/blocks/` -> `tags/block/`, etc.) and pre-1.20.5 recipe
    // `"result": {"item": ...}` fields so 1.20.x–26.3 jars all port through one clean pipeline.
    let mut upgraded_entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut old_recipes_upgraded = 0usize;
    let existing_keys: std::collections::BTreeSet<String> = flat_entries.keys().cloned().collect();
    for (name, mut bytes) in flat_entries {
        let target_name = match carry::renamed_in_1_21(&name) {
            Some(new_name) if !existing_keys.contains(&new_name) => new_name,
            Some(_) => continue,
            None => name,
        };
        if target_name.starts_with("data/")
            && target_name.contains("/recipe/")
            && target_name.ends_with(".json")
            && let Ok(mut recipe) = serde_json::from_slice::<Value>(&bytes)
        {
            let mut touched = false;
            if let Some(res_obj) = recipe.get_mut("result").and_then(Value::as_object_mut)
                && !res_obj.contains_key("id")
                && let Some(item_val) = res_obj.remove("item")
            {
                res_obj.insert("id".to_owned(), item_val);
                touched = true;
            }
            if touched {
                old_recipes_upgraded += 1;
                bytes = serde_json::to_vec_pretty(&recipe).unwrap_or(bytes);
            }
        }
        upgraded_entries.insert(target_name, bytes);
    }

    // Count non-shared content files per valid namespace so wrapper ids (`mr_*` or hyphenated ids
    // that differ from `assets/<ns>/`) adopt the jar's actual primary namespace.
    const SHARED_NS: &[&str] = &[
        "minecraft",
        "c",
        "forge",
        "neoforge",
        "fabric",
        "quilt",
        "horizon",
    ];
    let mut ns_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut resource_pack_entries: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, bytes) in &upgraded_entries {
        if name.starts_with("assets/") || name == "pack.mcmeta" || name == "pack.png" {
            resource_pack_entries.push((name.clone(), bytes.clone()));
        }
        let mut parts = name.splitn(3, '/');
        let (Some(root), Some(ns), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if (root == "assets" || root == "data")
            && !SHARED_NS.contains(&ns)
            && valid_horizon_ns(ns)
            && !(root == "data" && rest.starts_with("tags/"))
        {
            *ns_counts.entry(ns.to_owned()).or_default() += 1;
        }
    }

    let sanitized = sanitize_mod_id(&info.mod_id);
    let effective_id = if ns_counts.contains_key(&sanitized) || ns_counts.is_empty() {
        sanitized
    } else {
        // Prefer a namespace matching the sanitized id without underscores (e.g. `farmers_delight` -> `farmersdelight`),
        // otherwise pick the namespace owning the most content files.
        let collapsed = sanitized.replace('_', "");
        ns_counts
            .keys()
            .find(|ns| **ns == collapsed)
            .cloned()
            .or_else(|| {
                ns_counts
                    .iter()
                    .max_by_key(|(_, count)| *count)
                    .map(|(ns, _)| ns.clone())
            })
            .unwrap_or(sanitized)
    };

    // Ensure sibling content namespaces inside multi-namespace jars (e.g. `towns_and_towers` +
    // `kaisyn`) are cross-referenced so `carry::extra_namespaces` claims all of them.
    let sibling_ns: Vec<String> = ns_counts
        .keys()
        .filter(|ns| *ns != &effective_id)
        .cloned()
        .collect();
    if !sibling_ns.is_empty() {
        let claim_path = format!("data/{effective_id}/tags/worldgen/biome/_ported_siblings.json");
        if !upgraded_entries.contains_key(&claim_path) {
            let values: Vec<Value> = sibling_ns
                .iter()
                .map(|ns| json!({ "id": format!("#{ns}:_ported"), "required": false }))
                .collect();
            if let Ok(claim_bytes) = serde_json::to_vec(&json!({ "replace": false, "values": values }))
            {
                upgraded_entries.insert(claim_path, claim_bytes);
            }
        }
    }

    let display_name = if info.name.trim().is_empty() || info.name.contains("${") {
        effective_id.replace('_', " ")
    } else {
        info.name.clone()
    };
    let version = if info.version.trim().is_empty() || info.version.contains("${") {
        "1.0.0".to_owned()
    } else {
        info.version.clone()
    };

    fs::create_dir_all(stage_dir).map_err(|e| format!("{}: {e}", stage_dir.display()))?;
    let normalized_path = stage_dir.join("normalized-port.jar");
    let out_file = fs::File::create(&normalized_path)
        .map_err(|e| format!("{}: {e}", normalized_path.display()))?;
    let mut writer = zip::ZipWriter::new(out_file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let fabric_meta = json!({
        "schemaVersion": 1,
        "id": effective_id,
        "version": version,
        "name": display_name,
        "license": if info.license.is_empty() { "All-Rights-Reserved" } else { &info.license },
        "depends": info.depends.iter().map(|d| (d.clone(), json!("*"))).collect::<serde_json::Map<_, _>>(),
    });
    writer
        .start_file("fabric.mod.json", opts)
        .map_err(|e| e.to_string())?;
    writer
        .write_all(
            serde_json::to_vec_pretty(&fabric_meta)
                .unwrap_or_default()
                .as_slice(),
        )
        .map_err(|e| e.to_string())?;

    for (name, bytes) in upgraded_entries {
        writer.start_file(&name, opts).map_err(|e| e.to_string())?;
        writer.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    writer.finish().map_err(|e| e.to_string())?;

    Ok(PreparedJar {
        path: normalized_path,
        effective_id,
        display_name,
        version,
        original_loader,
        resource_pack_entries,
        nested_jars_merged,
        old_recipes_upgraded,
    })
}

fn infer_item_template(item_name: &str) -> &'static str {
    let n = item_name.to_ascii_lowercase();
    if n.ends_with("_sword")
        || n.ends_with("_dagger")
        || n.ends_with("_knife")
        || n.ends_with("_katana")
        || n.ends_with("_rapier")
        || n.ends_with("_cutlass")
        || n.ends_with("_machete")
        || n.ends_with("_cleaver")
        || n.ends_with("_spear")
        || n.ends_with("_halberd")
    {
        "minecraft:iron_sword"
    } else if n.ends_with("_pickaxe") || n.ends_with("_hammer") || n.ends_with("_paxel") {
        "minecraft:iron_pickaxe"
    } else if n.ends_with("_axe") || n.ends_with("_hatchet") {
        "minecraft:iron_axe"
    } else if n.ends_with("_shovel") || n.ends_with("_spade") || n.ends_with("_excavator") {
        "minecraft:iron_shovel"
    } else if n.ends_with("_hoe") || n.ends_with("_scythe") || n.ends_with("_sickle") {
        "minecraft:iron_hoe"
    } else if n.ends_with("_helmet") || n.ends_with("_crown") || n.ends_with("_hat") {
        "minecraft:iron_helmet"
    } else if n.ends_with("_chestplate") || n.ends_with("_tunic") || n.ends_with("_vest") {
        "minecraft:iron_chestplate"
    } else if n.ends_with("_leggings") || n.ends_with("_pants") || n.ends_with("_greaves") {
        "minecraft:iron_leggings"
    } else if n.ends_with("_boots") || n.ends_with("_shoes") {
        "minecraft:iron_boots"
    } else if n.ends_with("_shield") {
        "minecraft:shield"
    } else if n.ends_with("_crossbow") {
        "minecraft:crossbow"
    } else if n.ends_with("_bow") {
        "minecraft:bow"
    } else if n.contains("juice")
        || n.ends_with("_tea")
        || n.ends_with("_coffee")
        || n.ends_with("_cider")
        || n.ends_with("_soda")
        || n.ends_with("_smoothie")
        || n.ends_with("_milkshake")
        || n.ends_with("_wine")
        || n.ends_with("_beer")
    {
        "minecraft:honey_bottle"
    } else if [
        "_stew",
        "_soup",
        "_pie",
        "_bread",
        "_cookie",
        "_berry",
        "_berries",
        "_apple",
        "_salad",
        "_sandwich",
        "_burger",
        "_pizza",
        "_noodle",
        "_noodles",
        "_ramen",
        "_sushi",
        "_roll",
        "_rice",
        "_pasta",
        "_steak",
        "_roast",
        "_fillet",
        "_chop",
        "_bacon",
        "_sausage",
        "_cheese",
        "_chocolate",
        "_cake_slice",
        "_pie_slice",
        "_pancake",
        "_waffle",
        "_donut",
        "_muffin",
        "_tart",
        "_curry",
        "_taco",
        "_burrito",
        "_dumpling",
    ]
    .iter()
    .any(|pat| n.ends_with(pat))
        || n.starts_with("cooked_")
        || n.starts_with("roasted_")
        || n.starts_with("baked_")
        || n.starts_with("fried_")
        || n.starts_with("grilled_")
    {
        "minecraft:cooked_beef"
    } else {
        "minecraft:stick"
    }
}

/// Enrich a carried folder mod (`work_dir/carry/`) with:
/// - Functional vanilla item templates (weapons, tools, armor, bows, shields, foods, drinks);
/// - Emissive block lighting (`light: 15`) & non-occluding cutouts (`occludes: false`) + Survival
///   self-drop block loot tables (`data/<id>/loot_table/blocks/<b>.json`);
/// - An automatic Creative Mode inventory tab (`data/<id>/horizon/creative_tab/main.json`).
fn enhance_carried_mod(carried: &Path, mod_id: &str, mod_name: &str, ports: &mut Vec<String>) {
    let horizon_dir = carried.join(format!("data/{mod_id}/horizon"));
    let mut tab_items: Vec<String> = Vec::new();
    let mut loot_added = 0usize;
    let mut smart_items = 0usize;

    let block_dir = horizon_dir.join("block");
    if let Ok(entries) = fs::read_dir(&block_dir) {
        let mut names: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let stem = p.file_stem()?.to_str()?.to_owned();
                (p.extension().and_then(|x| x.to_str()) == Some("json")).then_some((stem, p))
            })
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (stem, path) in names {
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(mut val) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let has_item = val.get("item").and_then(Value::as_bool).unwrap_or(true);
            if has_item {
                tab_items.push(format!("{mod_id}:{stem}"));
            }
            let lower = stem.to_ascii_lowercase();
            let emits_light = [
                "lamp",
                "lantern",
                "torch",
                "candle",
                "glow",
                "shroomlight",
                "campfire",
                "brazier",
                "sconce",
                "chandelier",
                "light",
            ]
            .iter()
            .any(|k| lower.contains(k));
            let non_occluding = [
                "glass",
                "pane",
                "lantern",
                "torch",
                "candle",
                "chain",
                "bars",
                "leaves",
                "flower",
                "sapling",
                "crop",
                "bush",
                "fern",
                "vine",
                "ladder",
                "chandelier",
                "sconce",
            ]
            .iter()
            .any(|k| lower.contains(k));
            let mut changed = false;
            if emits_light || non_occluding {
                let props = val
                    .as_object_mut()
                    .map(|o| o.entry("properties").or_insert_with(|| json!({})));
                if let Some(p_obj) = props.and_then(Value::as_object_mut) {
                    if emits_light && !p_obj.contains_key("light") {
                        p_obj.insert("light".into(), json!(14));
                        changed = true;
                    }
                    if non_occluding && !p_obj.contains_key("occludes") {
                        p_obj.insert("occludes".into(), json!(false));
                        changed = true;
                    }
                }
            }
            if changed && let Ok(out) = serde_json::to_vec_pretty(&val) {
                let _ = fs::write(&path, out);
            }
            if has_item
                && !lower.contains("glass")
                && !lower.ends_with("_leaves")
                && !lower.ends_with("_ice")
            {
                let loot_path =
                    carried.join(format!("data/{mod_id}/loot_table/blocks/{stem}.json"));
                if !loot_path.exists() {
                    if let Some(parent) = loot_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let full_id = format!("{mod_id}:{stem}");
                    let table = json!({
                        "type": "minecraft:block",
                        "pools": [{
                            "rolls": 1,
                            "entries": [{"type": "minecraft:item", "name": full_id}],
                            "conditions": [{"type": "minecraft:survives_explosion"}]
                        }],
                        "random_sequence": format!("{mod_id}:blocks/{stem}")
                    });
                    if let Ok(out) = serde_json::to_vec_pretty(&table)
                        && fs::write(&loot_path, out).is_ok()
                    {
                        loot_added += 1;
                    }
                }
            }
        }
    }

    let item_dir = horizon_dir.join("item");
    if let Ok(entries) = fs::read_dir(&item_dir) {
        let mut names: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let stem = p.file_stem()?.to_str()?.to_owned();
                (p.extension().and_then(|x| x.to_str()) == Some("json")).then_some((stem, p))
            })
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (stem, path) in names {
            let full_id = format!("{mod_id}:{stem}");
            if !tab_items.contains(&full_id) {
                tab_items.push(full_id);
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(mut val) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if val.get("template").and_then(Value::as_str) == Some("minecraft:stick")
                && val.get("components").is_none()
            {
                let inferred = infer_item_template(&stem);
                if inferred != "minecraft:stick" {
                    val["template"] = json!(inferred);
                    if let Ok(out) = serde_json::to_vec_pretty(&val)
                        && fs::write(&path, out).is_ok()
                    {
                        smart_items += 1;
                    }
                }
            }
        }
    }

    if !tab_items.is_empty() {
        let tab_dir = horizon_dir.join("creative_tab");
        if !tab_dir.exists() && fs::create_dir_all(&tab_dir).is_ok() {
            let tab = json!({
                "title": if mod_name.trim().is_empty() { mod_id } else { mod_name.trim() },
                "icon": tab_items[0],
                "items": tab_items,
            });
            if let Ok(out) = serde_json::to_vec_pretty(&tab) {
                let _ = fs::write(tab_dir.join("main.json"), out);
                ports.push("creative inventory tab".into());
            }
        }
    }
    if loot_added > 0 {
        ports.push(format!("{loot_added} Survival block drops synthesized"));
    }
    if smart_items > 0 {
        ports.push(format!("{smart_items} tool/weapon/food behaviours mapped"));
    }
}

/// Enable or disable `"file/<zip_name>"` inside `home/options.txt`'s `resourcePacks:[...]` list.
pub fn set_companion_resource_pack_enabled(home: &Path, zip_name: &str, enabled: bool) {
    let options_path = home.join("options.txt");
    let entry = format!("file/{zip_name}");
    let existing = fs::read_to_string(&options_path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    for line in existing.lines() {
        if let Some(rest) = line.strip_prefix("resourcePacks:") {
            found = true;
            let mut list: Vec<String> = serde_json::from_str(rest.trim()).unwrap_or_default();
            list.retain(|x| x != &entry);
            if enabled {
                if list.is_empty() {
                    list.push("vanilla".into());
                }
                list.push(entry.clone());
            }
            if let Ok(serialized) = serde_json::to_string(&list) {
                lines.push(format!("resourcePacks:{serialized}"));
            }
        } else {
            lines.push(line.to_owned());
        }
    }
    if !found && enabled {
        let list = vec!["vanilla".to_owned(), entry];
        if let Ok(serialized) = serde_json::to_string(&list) {
            lines.push(format!("resourcePacks:{serialized}"));
        }
    }
    if !lines.is_empty() {
        let mut out = lines.join("\n");
        out.push('\n');
        let _ = fs::write(&options_path, out);
    }
}

/// Bridge a visual / audio / model / OptiFine / Polytone `.jar` mod (`assets/minecraft/...` etc.)
/// into `Documents/resourcepacks/mod-<id>.zip` + a companion `horizon-mods/<id>/horizon-mod.toml`
/// when the `.jar` registers no custom server blocks/items/worldgen.
fn try_bridge_visual_audio_jar(
    prep: &PreparedJar,
    mods_dir: Option<&Path>,
    work_dir: &Path,
) -> Result<Option<InstalledPort>, String> {
    use std::io::Write;
    let asset_count = prep
        .resource_pack_entries
        .iter()
        .filter(|(n, _)| {
            n.starts_with("assets/")
                && (n.ends_with(".png")
                    || n.ends_with(".ogg")
                    || n.ends_with(".json")
                    || n.ends_with(".properties")
                    || n.ends_with(".jem")
                    || n.ends_with(".jpm")
                    || n.ends_with(".fsh")
                    || n.ends_with(".vsh"))
        })
        .count();
    if asset_count == 0 {
        return Ok(None);
    }
    let carried = work_dir.join("carry-visual");
    let _ = fs::remove_dir_all(&carried);
    fs::create_dir_all(&carried).map_err(|e| e.to_string())?;
    let q = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let manifest = format!(
        "id = {}\nname = {}\nversion = {}\ndescription = {}\napi = \"0.2\"\nside = \"client\"\ncapabilities = [\"registries\"]\n",
        q(&prep.effective_id),
        q(&prep.display_name),
        q(&prep.version),
        q("Bridged visual/audio assets into Horizon Resource Packs."),
    );
    fs::write(carried.join("horizon-mod.toml"), manifest.as_bytes()).map_err(|e| e.to_string())?;

    let zip_name = format!("mod-{}.zip", prep.effective_id);
    let metadata = SourceMetadata {
        mod_id: prep.effective_id.clone(),
        name: prep.display_name.clone(),
        version: prep.version.clone(),
        loader: prep.original_loader.clone(),
        origin: "local-jar".into(),
        verdict: "works".into(),
        summary: format!(
            "Visual/audio mod assets ({asset_count} files) bridged & enabled as resource pack {zip_name}."
        ),
        ports: vec![
            format!("{asset_count} visual/audio asset files"),
            format!("resourcepacks/{zip_name} enabled"),
        ],
        skipped: Vec::new(),
        namespaces: vec![prep.effective_id.clone()],
        project_id: None,
        version_id: None,
        icon_url: None,
    };

    let Some(mods_dir) = mods_dir else {
        return Ok(Some(InstalledPort {
            metadata,
            installed_dir: None,
        }));
    };

    if let Some(home) = mods_dir.parent() {
        let rp_dir = home.join("resourcepacks");
        fs::create_dir_all(&rp_dir).map_err(|e| format!("{}: {e}", rp_dir.display()))?;
        let rp_path = rp_dir.join(&zip_name);
        let rp_file =
            fs::File::create(&rp_path).map_err(|e| format!("{}: {e}", rp_path.display()))?;
        let mut zw = zip::ZipWriter::new(rp_file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut has_mcmeta = false;
        for (name, bytes) in &prep.resource_pack_entries {
            if name == "pack.mcmeta" {
                has_mcmeta = true;
            }
            zw.start_file(name, opts).map_err(|e| e.to_string())?;
            zw.write_all(bytes).map_err(|e| e.to_string())?;
        }
        if !has_mcmeta {
            let mcmeta = json!({
                "pack": {
                    "pack_format": 55,
                    "supported_formats": [15, 99],
                    "min_format": 15,
                    "max_format": 99,
                    "description": format!("{} (Bridged by Horizon)", prep.display_name)
                }
            });
            zw.start_file("pack.mcmeta", opts)
                .map_err(|e| e.to_string())?;
            zw.write_all(
                serde_json::to_vec_pretty(&mcmeta)
                    .unwrap_or_default()
                    .as_slice(),
            )
            .map_err(|e| e.to_string())?;
        }
        zw.finish().map_err(|e| e.to_string())?;
        set_companion_resource_pack_enabled(home, &zip_name, true);
    }

    fs::create_dir_all(mods_dir).map_err(|e| format!("{}: {e}", mods_dir.display()))?;
    let target = mods_dir.join(&metadata.mod_id);
    let _ = fs::remove_dir_all(&target);
    copy_or_move_dir(&carried, &target)?;
    let _ = save_source_metadata(mods_dir, &metadata);
    Ok(Some(InstalledPort {
        metadata,
        installed_dir: Some(target),
    }))
}

fn explain_pure_code_jar(mod_id: &str, name: &str, fallback_summary: &str) -> String {
    let key = format!(
        "{} {}",
        mod_id.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    );
    if [
        "sodium",
        "lithium",
        "ferritecore",
        "immediatelyfast",
        "entityculling",
        "moreculling",
        "modernfix",
        "noisium",
        "c2me",
        "starlight",
    ]
    .iter()
    .any(|k| key.contains(k))
    {
        format!(
            "{name} is a Java JVM performance mod — Horizon's native Rust + Apple Metal engine already includes multithreaded chunk meshing, frustum/cave/leaf culling, and mimalloc memory optimization built-in (no mod needed)."
        )
    } else if [
        "fabric_api",
        "fabric-api",
        "cloth_config",
        "cloth-config",
        "architectury",
        "modmenu",
        "mod_menu",
        "yet_another_config",
        "yacl",
        "owo",
        "balm",
        "bookshelf",
        "geckolib",
        "moonlight",
        "cristellib",
    ]
    .iter()
    .any(|k| key.contains(k))
    {
        format!(
            "{name} is a Java loader/API library — Horizon ports content, worldgen, recipe, structure, and visual mods directly without needing library dependencies."
        )
    } else if key.contains("iris") || key.contains("optifine") || key.contains("oculus") {
        format!(
            "{name} is a Java shader loader — Horizon already loads .hshader and Iris/OptiFine .zip shader packs natively in the Shaders / Horizon Registry tab."
        )
    } else {
        format!("{name} cannot be installed as a Horizon mod: {fallback_summary}")
    }
}

/// Inspect a local `.jar` and optionally port + publish its content into `mods_dir/<mod_id>/`.
pub fn port_local_jar(
    jar_path: &Path,
    mods_dir: Option<&Path>,
    work_dir: &Path,
) -> Result<InstalledPort, String> {
    let _ = fs::remove_dir_all(work_dir);
    fs::create_dir_all(work_dir).map_err(|e| format!("{}: {e}", work_dir.display()))?;
    let prep = prepare_jar_for_porting(jar_path, &work_dir.join("prep"))?;
    let mut c = check::check(&prep.path, work_dir)?;
    c.loader = prep.original_loader.clone();
    if prep.nested_jars_merged > 0 {
        c.ports.push(format!(
            "{} nested Jar-in-Jar module(s) unpacked",
            prep.nested_jars_merged
        ));
    }
    if prep.old_recipes_upgraded > 0 {
        c.ports.push(format!(
            "{} legacy 1.20 recipe result(s) upgraded",
            prep.old_recipes_upgraded
        ));
    }

    let carried = work_dir.join("carry");
    if c.verdict != Verdict::No && carried.join(horizon_mods::manifest::MANIFEST).is_file() {
        enhance_carried_mod(&carried, &c.mod_id, &prep.display_name, &mut c.ports);
    } else if let Some(bridged) = try_bridge_visual_audio_jar(&prep, mods_dir, work_dir)? {
        return Ok(bridged);
    }

    let mut metadata = SourceMetadata {
        mod_id: c.mod_id.clone(),
        name: if c.name.is_empty() {
            prep.display_name.clone()
        } else {
            c.name.clone()
        },
        version: c.version.clone(),
        loader: c.loader.clone(),
        origin: "local-jar".into(),
        verdict: verdict_slug(c.verdict).into(),
        summary: c.summary.clone(),
        ports: c.ports.clone(),
        skipped: c.skipped.clone(),
        namespaces: c.namespaces.clone(),
        project_id: None,
        version_id: None,
        icon_url: None,
    };

    if c.verdict == Verdict::No || !carried.join(horizon_mods::manifest::MANIFEST).is_file() {
        return match mods_dir {
            Some(_) => Err(explain_pure_code_jar(
                &metadata.mod_id,
                &metadata.name,
                &metadata.summary,
            )),
            None => Ok(InstalledPort {
                metadata,
                installed_dir: None,
            }),
        };
    }

    // Validate with `horizon_mods::Package` + `Content` before touching the live `horizon-mods/` dir.
    let package = horizon_mods::Package::read(&carried)?;
    let _ = horizon_mods::Content::read(&carried, &package.manifest)?;
    metadata.mod_id = package.manifest.id.clone();
    if !package.manifest.name.is_empty() {
        metadata.name = package.manifest.name.clone();
    }
    metadata.version = package.manifest.version.clone();

    let Some(mods_dir) = mods_dir else {
        return Ok(InstalledPort {
            metadata,
            installed_dir: None,
        });
    };

    fs::create_dir_all(mods_dir).map_err(|e| format!("{}: {e}", mods_dir.display()))?;
    let target = mods_dir.join(&metadata.mod_id);
    let backup = mods_dir.join(format!(".{}.previous", metadata.mod_id));
    let _ = fs::remove_dir_all(&backup);
    if target.exists() {
        if target.is_dir() {
            fs::rename(&target, &backup).map_err(|e| format!("{}: {e}", target.display()))?;
        } else {
            fs::remove_file(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        }
    }
    if let Err(e) = copy_or_move_dir(&carried, &target) {
        if backup.exists() {
            let _ = fs::rename(&backup, &target);
        }
        return Err(e);
    }
    let _ = fs::remove_dir_all(&backup);
    // If a `.hmod` archive with the same mod id existed earlier, remove it so `horizon_mods::find`
    // sees one authoritative copy.
    let old_hmod = mods_dir.join(format!("{}.hmod", metadata.mod_id));
    if old_hmod.is_file() {
        let _ = fs::remove_file(old_hmod);
    }
    let _ = save_source_metadata(mods_dir, &metadata);
    Ok(InstalledPort {
        metadata,
        installed_dir: Some(target),
    })
}

fn copy_or_move_dir(src: &Path, dst: &Path) -> Result<(), String> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    fs::create_dir_all(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let out = dst.join(entry.file_name());
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        if ty.is_dir() {
            copy_or_move_dir(&path, &out)?;
        } else if ty.is_file() {
            fs::copy(&path, &out).map_err(|e| format!("{}: {e}", out.display()))?;
        }
    }
    let _ = fs::remove_dir_all(src);
    Ok(())
}

pub fn save_source_metadata(mods_dir: &Path, meta: &SourceMetadata) -> Result<(), String> {
    let dir = mods_dir.join(".sources");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(meta).map_err(|e| e.to_string())?;
    fs::write(dir.join(format!("{}.json", meta.mod_id)), bytes)
        .map_err(|e| format!("{}: {e}", dir.display()))
}

pub fn load_source_metadata(mods_dir: &Path, mod_id: &str) -> Option<SourceMetadata> {
    let path = mods_dir.join(".sources").join(format!("{mod_id}.json"));
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ports_fabric_jar_into_valid_horizon_folder_mod() {
        let scratch =
            std::env::temp_dir().join(format!("horizon-store-port-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).unwrap();
        let jar_path = scratch.join("ruby.jar");
        let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("fabric.mod.json", opts).unwrap();
        zip.write_all(
            br#"{"schemaVersion":1,"id":"ruby_plus","version":"1.2.0","name":"Ruby Plus","license":"MIT"}"#,
        )
        .unwrap();
        zip.start_file("assets/ruby_plus/blockstates/ruby_block.json", opts)
            .unwrap();
        zip.write_all(br#"{"variants":{"":{"model":"ruby_plus:block/ruby_block"}}}"#)
            .unwrap();
        zip.start_file("assets/ruby_plus/models/item/ruby.json", opts)
            .unwrap();
        zip.write_all(br#"{"parent":"minecraft:item/generated"}"#)
            .unwrap();
        zip.finish().unwrap();

        let mods_dir = scratch.join("horizon-mods");
        let result =
            port_local_jar(&jar_path, Some(&mods_dir), &scratch.join("work")).expect("port works");
        assert_eq!(result.metadata.mod_id, "ruby_plus");
        assert_eq!(result.metadata.verdict, "works");
        let installed = result.installed_dir.expect("installed dir");
        assert!(installed.join("horizon-mod.toml").is_file());
        let loaded = load_source_metadata(&mods_dir, "ruby_plus").expect("saved source meta");
        assert_eq!(loaded.name, "Ruby Plus");
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn ranks_modern_versions_ahead_of_older_ones() {
        let host = ALLOWED_DOWNLOAD_HOSTS[0];
        let raw = json!([
            {
                "id": "old",
                "version_type": "release",
                "game_versions": ["1.20.1"],
                "loaders": ["forge"],
                "files": [{"filename": "mod-1.20.1.jar", "url": format!("{host}data/x/mod-1.20.1.jar"), "primary": true}]
            },
            {
                "id": "modern",
                "version_type": "release",
                "game_versions": ["26.3", "1.21.11"],
                "loaders": ["fabric"],
                "files": [{"filename": "mod-26.3.jar", "url": format!("{host}data/x/mod-26.3.jar"), "primary": true}]
            }
        ]);
        let ranked = filter_and_rank_versions(&raw, None, None);
        assert_eq!(ranked[0]["id"], "modern");
    }

    #[test]
    fn ports_nested_jar_in_jar_hyphenated_wrapper_1_20_recipes_and_creative_tab() {
        let scratch = std::env::temp_dir().join(format!(
            "horizon-store-nested-port-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).unwrap();

        let opts = zip::write::SimpleFileOptions::default();
        let mut inner_buf = std::io::Cursor::new(Vec::new());
        {
            let mut inner = zip::ZipWriter::new(&mut inner_buf);
            inner
                .start_file("assets/farmersdelight/blockstates/stove_lamp.json", opts)
                .unwrap();
            inner
                .write_all(br#"{"variants":{"":{"model":"farmersdelight:block/stove_lamp"}}}"#)
                .unwrap();
            inner
                .start_file("assets/farmersdelight/models/item/iron_knife.json", opts)
                .unwrap();
            inner
                .write_all(br#"{"parent":"minecraft:item/handheld"}"#)
                .unwrap();
            inner
                .start_file("assets/farmersdelight/items/beef_stew.json", opts)
                .unwrap();
            inner
                .write_all(
                    br#"{"model":{"type":"minecraft:model","model":"farmersdelight:item/beef_stew"}}"#,
                )
                .unwrap();
            inner
                .start_file("data/farmersdelight/recipes/beef_stew.json", opts)
                .unwrap();
            inner
                .write_all(
                    br#"{"type":"minecraft:crafting_shapeless","ingredients":[{"item":"minecraft:beef"}],"result":{"item":"farmersdelight:beef_stew","count":1}}"#,
                )
                .unwrap();
            inner.finish().unwrap();
        }

        let jar_path = scratch.join("mr-farmers-delight.jar");
        let mut outer = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        outer.start_file("fabric.mod.json", opts).unwrap();
        outer
            .write_all(
                br#"{"schemaVersion":1,"id":"mr-farmers-delight","version":"2.0.0","name":"Farmer's Delight","license":"MIT"}"#,
            )
            .unwrap();
        outer
            .start_file("META-INF/jars/content-inner.jar", opts)
            .unwrap();
        outer.write_all(inner_buf.get_ref()).unwrap();
        outer.finish().unwrap();

        let mods_dir = scratch.join("horizon-mods");
        let res = port_local_jar(&jar_path, Some(&mods_dir), &scratch.join("work"))
            .expect("nested hyphenated 1.20 jar ports cleanly");
        assert_eq!(res.metadata.mod_id, "farmersdelight");
        assert_eq!(res.metadata.verdict, "works");
        let installed = res.installed_dir.expect("installed folder");
        assert!(
            installed
                .join("data/farmersdelight/horizon/creative_tab/main.json")
                .is_file()
        );
        assert!(
            installed
                .join("data/farmersdelight/loot_table/blocks/stove_lamp.json")
                .is_file()
        );
        let knife: Value = serde_json::from_slice(
            &fs::read(installed.join("data/farmersdelight/horizon/item/iron_knife.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(knife["template"], "minecraft:iron_sword");
        let stew: Value = serde_json::from_slice(
            &fs::read(installed.join("data/farmersdelight/horizon/item/beef_stew.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stew["template"], "minecraft:cooked_beef");
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn bridges_visual_audio_jar_into_enabled_resource_pack() {
        let scratch = std::env::temp_dir().join(format!(
            "horizon-store-visual-bridge-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).unwrap();

        let jar_path = scratch.join("eating-anim.jar");
        let opts = zip::write::SimpleFileOptions::default();
        let mut zw = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
        zw.start_file("quilt.mod.json", opts).unwrap();
        zw.write_all(
            br#"{"schema_version":1,"quilt_loader":{"id":"eating-animation","version":"1.1.0","metadata":{"name":"Eating Animation","license":"MIT"}}}"#,
        )
        .unwrap();
        zw.start_file("assets/minecraft/textures/item/apple.png", opts)
            .unwrap();
        zw.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
        zw.finish().unwrap();

        let mods_dir = scratch.join("horizon-mods");
        let res = port_local_jar(&jar_path, Some(&mods_dir), &scratch.join("work"))
            .expect("visual jar bridged");
        assert_eq!(res.metadata.mod_id, "eating_animation");
        assert_eq!(res.metadata.loader, "quilt");
        assert!(
            scratch
                .join("resourcepacks/mod-eating_animation.zip")
                .is_file()
        );
        let options_txt = fs::read_to_string(scratch.join("options.txt")).unwrap();
        assert!(options_txt.contains("file/mod-eating_animation.zip"));
        let _ = fs::remove_dir_all(&scratch);
    }
}
