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
        // When browsing for 26.3 ("compatible"), include 1.21.x + 26.x mods that Horizon's
        // pure-Rust carry pipeline upgrades automatically; otherwise filter to the exact version.
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
        )
    });
    let pure_code_like = categories.iter().any(|c| {
        matches!(*c, "optimization" | "library")
    }) && !content_like;
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

/// Inspect a local `.jar` and optionally port + publish its content into `mods_dir/<mod_id>/`.
pub fn port_local_jar(
    jar_path: &Path,
    mods_dir: Option<&Path>,
    work_dir: &Path,
) -> Result<InstalledPort, String> {
    let _ = fs::remove_dir_all(work_dir);
    fs::create_dir_all(work_dir).map_err(|e| format!("{}: {e}", work_dir.display()))?;
    let mut c = check::check(jar_path, work_dir)?;

    // Legacy 1.20.x content/worldgen jars (`recipes/`, `loot_tables/`, `structures/`): `carry::carry`
    // already renames those folders to 1.21+ layout (`renamed_in_1_21`), so try carrying them when
    // the player explicitly imports or installs one.
    if c.verdict == Verdict::No && c.summary == carry::OLD_LAYOUT {
        if let Ok(info) = jar::read(jar_path) {
            let alone = |m: &str| m == info.mod_id || m == "minecraft";
            let pack = carry::Pack {
                loaded: &alone,
                mods: &alone,
            };
            if let Ok(report) = carry::carry(
                jar_path,
                &info,
                work_dir,
                None,
                &|_| true,
                &pack,
                &BTreeMap::new(),
            ) {
                let total = report.blocks.len()
                    + report.items
                    + report.recipes_upgraded
                    + report.loot_tables_upgraded
                    + report.loot_tables_added
                    + report.worldgen
                    + info.census.structures;
                if total > 0 {
                    c.verdict = if c.code.behaviour() {
                        Verdict::Partly
                    } else {
                        Verdict::Works
                    };
                    c.namespaces = std::iter::once(info.mod_id.clone())
                        .chain(report.namespaces.iter().cloned())
                        .collect();
                    if !report.blocks.is_empty() {
                        c.ports.push(format!("{} blocks", report.blocks.len()));
                    }
                    if report.items > 0 {
                        c.ports.push(format!("{} items", report.items));
                    }
                    if report.recipes_upgraded > 0 {
                        c.ports
                            .push(format!("{} recipes upgraded", report.recipes_upgraded));
                    }
                    if report.worldgen + info.census.structures > 0 {
                        c.ports.push(format!(
                            "{} worldgen/structure files",
                            report.worldgen + info.census.structures
                        ));
                    }
                    c.summary = format!(
                        "Upgraded 1.20 layout automatically ({} folders renamed).",
                        report.folders_renamed
                    );
                }
            }
        }
    }

    let carried = work_dir.join("carry");
    let mut metadata = SourceMetadata {
        mod_id: c.mod_id.clone(),
        name: if c.name.is_empty() {
            c.mod_id.clone()
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
            Some(_) => Err(format!(
                "{} cannot be installed as a Horizon mod: {}",
                metadata.name, metadata.summary
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
}
