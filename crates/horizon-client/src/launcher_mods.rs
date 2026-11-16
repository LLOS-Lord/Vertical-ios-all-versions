//! In-launcher mod discovery, installation, toggling, and local import for mobile (`Documents/`)
//! and desktop launchers.
//!
//! Combines two live stores plus local file import:
//! 1. **Horizon Native Registry (`horizon`)**: Ed25519 + SHA-256 verified `.hmod` content mods
//!    from `mod_registry`.
//! 2. **Modrinth + Pure-Rust Java Mod Porter (`modrinth`)**: Searches Modrinth, downloads SHA-1
//!    verified Fabric / NeoForge / Forge / Quilt `.jar` files, inspects Java `.class` bytecode
//!    without a JVM (`horizon_hooks::porter`), upgrades 1.20/1.21+ data & assets into
//!    `Documents/horizon-mods/<mod_id>/`, and turns the mod on in `horizon-mods.txt`.
//! 3. **Local `.hmod` & `.jar` Import**: Validates `.hmod` archives or ports `.jar` files picked
//!    from the iOS Files app (or dropped into `Documents/`) without leaving the launcher.

use crate::{mod_list::ModList, mod_registry};
use horizon_mods::{Content, Package, content::clash};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const MOD_BROWSER_MANIFEST: &str = "mod-browser.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredSourceMeta {
    #[serde(default)]
    mod_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    loader: String,
    #[serde(default)]
    origin: String,
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    skipped: Vec<String>,
    #[serde(default)]
    namespaces: Vec<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    version_id: Option<String>,
    #[serde(default)]
    icon_url: Option<String>,
}

/// One installed mod row returned to the SwiftUI launcher.
#[derive(Clone, Debug, Serialize)]
pub struct InstalledModRow {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub authors: Vec<String>,
    pub enabled: bool,
    /// `"hmod"` or `"folder"`.
    pub format: String,
    /// `"horizon"`, `"modrinth"`, `"local-jar"`, or `"local-hmod"`.
    pub origin: String,
    pub loader: String,
    /// `"works"`, `"partly"`, or `"verified"`.
    pub verdict: String,
    pub summary: String,
    pub ports: Vec<String>,
    pub skipped: Vec<String>,
    pub namespaces: Vec<String>,
    pub clash_warning: Option<String>,
    pub project_id: Option<String>,
    pub icon_url: Option<String>,
    pub bytes: u64,
}

fn mods_dir(home: &Path) -> PathBuf {
    home.join(crate::mod_list::DIR)
}

fn read_source_meta(home: &Path, mod_id: &str) -> Option<StoredSourceMeta> {
    let path = mods_dir(home)
        .join(".sources")
        .join(format!("{mod_id}.json"));
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_source_meta(home: &Path, meta: &StoredSourceMeta) {
    let dir = mods_dir(home).join(".sources");
    if fs::create_dir_all(&dir).is_ok()
        && let Ok(bytes) = serde_json::to_vec_pretty(meta)
    {
        let _ = fs::write(dir.join(format!("{}.json", meta.mod_id)), bytes);
    }
}

fn dir_or_file_size(path: &Path) -> u64 {
    let Ok(meta) = fs::metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    let mut sum = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            sum += dir_or_file_size(&entry.path());
        }
    }
    sum
}

fn owned_namespaces(id: &str, extra: &[String]) -> Vec<String> {
    let mut out = vec![id.to_owned()];
    for ns in extra {
        if !out.contains(ns) {
            out.push(ns.clone());
        }
    }
    out
}

/// List every installed `.hmod` and carried folder mod in `home/horizon-mods/`, enriched with
/// source provenance (`.sources/<id>.json` / `mod-browser.json`), enabled state (`horizon-mods.txt`),
/// and active namespace-clash checks.
pub fn list_installed(home: &Path) -> Vec<InstalledModRow> {
    let dir = mods_dir(home);
    let mod_list = ModList::load(home);
    let browser_manifest: BTreeMap<String, Value> =
        fs::read_to_string(home.join(MOD_BROWSER_MANIFEST))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v["entries"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| {
                let id = e["id"].as_str()?.to_owned();
                Some((id, e))
            })
            .collect();

    let mut rows = Vec::new();
    for path in horizon_mods::find(&dir) {
        let Ok(pkg) = Package::read(&path) else {
            continue;
        };
        let id = pkg.manifest.id.clone();
        let meta = read_source_meta(home, &id);
        let from_browser = browser_manifest.get(&id);
        let format = if path.is_dir() { "folder" } else { "hmod" }.to_owned();

        let origin = meta
            .as_ref()
            .filter(|m| !m.origin.is_empty())
            .map(|m| m.origin.clone())
            .or_else(|| from_browser.map(|_| "horizon".to_owned()))
            .unwrap_or_else(|| {
                if path.is_dir() {
                    "local-jar".into()
                } else {
                    "local-hmod".into()
                }
            });
        let loader = meta
            .as_ref()
            .filter(|m| !m.loader.is_empty())
            .map(|m| m.loader.clone())
            .unwrap_or_else(|| {
                if origin == "horizon" || format == "hmod" {
                    "horizon".into()
                } else {
                    "java-ported".into()
                }
            });
        let verdict = meta
            .as_ref()
            .filter(|m| !m.verdict.is_empty())
            .map(|m| m.verdict.clone())
            .or_else(|| {
                from_browser.and_then(|b| {
                    (b["verdict"].as_str() == Some("Verified")).then(|| "verified".to_owned())
                })
            })
            .unwrap_or_else(|| "works".into());
        let summary = meta
            .as_ref()
            .filter(|m| !m.summary.is_empty())
            .map(|m| m.summary.clone())
            .unwrap_or_else(|| pkg.manifest.description.clone());
        let namespaces = owned_namespaces(&id, &pkg.manifest.namespaces);
        let enabled = mod_list.enabled.contains(&id);

        rows.push(InstalledModRow {
            name: if pkg.manifest.name.is_empty() {
                meta.as_ref()
                    .filter(|m| !m.name.is_empty())
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| id.clone())
            } else {
                pkg.manifest.name.clone()
            },
            version: pkg.manifest.version.clone(),
            description: pkg.manifest.description.clone(),
            authors: pkg.manifest.authors.clone(),
            enabled,
            format,
            origin,
            loader,
            verdict,
            summary,
            ports: meta.as_ref().map(|m| m.ports.clone()).unwrap_or_default(),
            skipped: meta.as_ref().map(|m| m.skipped.clone()).unwrap_or_default(),
            namespaces,
            clash_warning: None,
            project_id: meta.as_ref().and_then(|m| m.project_id.clone()),
            icon_url: meta.as_ref().and_then(|m| m.icon_url.clone()),
            bytes: dir_or_file_size(&path),
            id,
        });
    }

    // Detect namespace clashes among enabled mods using `horizon_mods::content::clash`.
    let active_ns: Vec<(String, Vec<String>)> = rows
        .iter()
        .filter(|r| r.enabled)
        .map(|r| (r.id.clone(), r.namespaces.clone()))
        .collect();
    for row in &mut rows {
        if !row.enabled {
            continue;
        }
        for (other_id, other_ns) in &active_ns {
            if *other_id == row.id {
                continue;
            }
            if let Some(msg) = clash((&row.id, &row.namespaces), (other_id, other_ns)) {
                row.clash_warning = Some(msg);
                break;
            }
        }
    }

    rows.sort_by(|a, b| {
        b.enabled.cmp(&a.enabled).then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        })
    });
    rows
}

fn clean_mod_id(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()
        && trimmed.len() <= 64
        && !trimmed.contains(['/', '\\', '.'])
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-')))
    .then_some(trimmed)
}

/// Enable or disable an installed mod in `Documents/horizon-mods.txt`.
pub fn set_enabled(
    home: &Path,
    mod_id: &str,
    enabled: bool,
) -> Result<Vec<InstalledModRow>, String> {
    let id = clean_mod_id(mod_id).ok_or("invalid mod id")?;
    let mut list = ModList::load(home);
    if enabled {
        list.disabled.remove(id);
        list.enabled.insert(id.to_owned());
    } else {
        list.enabled.remove(id);
    }
    list.save(home);
    Ok(list_installed(home))
}

/// Remove an installed mod (`.hmod` or folder), its `.sources/<id>.json` metadata, and its entry
/// in `horizon-mods.txt` and `mod-browser.json`.
pub fn delete_mod(home: &Path, mod_id: &str) -> Result<Vec<InstalledModRow>, String> {
    let id = clean_mod_id(mod_id).ok_or("invalid mod id")?;
    let dir = mods_dir(home);
    let folder = dir.join(id);
    let archive = dir.join(format!("{id}.hmod"));
    let mut removed = false;
    if folder.is_dir() {
        fs::remove_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        removed = true;
    }
    if archive.is_file() {
        fs::remove_file(&archive).map_err(|e| format!("{}: {e}", archive.display()))?;
        removed = true;
    }
    if !removed {
        return Err(format!("Mod {id} is not installed"));
    }
    let _ = fs::remove_file(dir.join(".sources").join(format!("{id}.json")));

    let mut list = ModList::load(home);
    list.enabled.remove(id);
    list.disabled.remove(id);
    list.save(home);

    let browser_path = home.join(MOD_BROWSER_MANIFEST);
    if let Ok(text) = fs::read_to_string(&browser_path)
        && let Ok(mut val) = serde_json::from_str::<Value>(&text)
        && let Some(arr) = val.get_mut("entries").and_then(Value::as_array_mut)
    {
        arr.retain(|e| e["id"].as_str() != Some(id));
        if let Ok(bytes) = serde_json::to_vec_pretty(&val) {
            let _ = fs::write(&browser_path, bytes);
        }
    }

    Ok(list_installed(home))
}

/// Browse content mods from the official signed Horizon Registry.
pub fn browse_horizon_registry(home: &Path, query: &str) -> Result<Value, String> {
    let at = mod_registry::location();
    let index = mod_registry::load_index(home, &at)?;
    let listings = mod_registry::listings(&index, home);
    let q = query.trim().to_ascii_lowercase();

    let hits: Vec<Value> = listings
        .into_iter()
        .filter(|item| item.matches(&q))
        .map(|item| {
            let update_available = item.has_update();
            json!({
                "id": item.id,
                "title": item.name,
                "summary": item.summary,
                "description": item.description,
                "authors": item.authors,
                "license": item.license,
                "categories": item.categories,
                "tags": item.tags,
                "version": item.version,
                "published": item.published,
                "downloads": item.downloads,
                "page": item.page,
                "installed_version": item.installed,
                "update_available": update_available,
                "source": "horizon",
            })
        })
        .collect();

    Ok(json!({
        "source": "horizon",
        "total_hits": hits.len(),
        "hits": hits,
    }))
}

/// Install or update a content mod from the Horizon Registry and enable it in `horizon-mods.txt`.
pub fn install_from_horizon_registry(home: &Path, mod_id: &str) -> Result<Value, String> {
    let at = mod_registry::location();
    let index = mod_registry::load_index(home, &at)?;
    let done = mod_registry::install(&index, home, mod_id.trim(), &mod_registry::download)?;
    write_source_meta(
        home,
        &StoredSourceMeta {
            mod_id: done.id.clone(),
            name: index
                .project(&done.id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| done.id.clone()),
            version: done.version.clone(),
            loader: "horizon".into(),
            origin: "horizon".into(),
            verdict: if done.verified {
                "verified".into()
            } else {
                "works".into()
            },
            summary: if done.verified {
                "Signed Horizon native content package (Ed25519 + SHA-256 verified).".into()
            } else {
                "Horizon content package (SHA-256 verified).".into()
            },
            ports: Vec::new(),
            skipped: Vec::new(),
            namespaces: vec![done.id.clone()],
            project_id: Some(done.id.clone()),
            version_id: Some(done.version.clone()),
            icon_url: None,
        },
    );
    let installed = set_enabled(home, &done.id, true)?;
    Ok(json!({
        "ok": true,
        "mod_id": done.id,
        "version": done.version,
        "verified": done.verified,
        "installed": installed,
    }))
}

/// Search Modrinth via the registered `horizon-official` pure-Rust porter.
pub fn search_modrinth(
    query: &str,
    minecraft_version: Option<&str>,
    loader: Option<&str>,
    sort: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Value, String> {
    let porter = horizon_hooks::porter::engine::active()
        .ok_or("Modrinth bridge is not registered in this build")?;
    let raw = porter.search_modrinth(query, minecraft_version, loader, sort, limit, offset)?;
    serde_json::from_str(&raw).map_err(|e| e.to_string())
}

/// List compatible `.jar` versions of a Modrinth project.
pub fn modrinth_versions(
    project_id: &str,
    minecraft_version: Option<&str>,
    loader: Option<&str>,
) -> Result<Value, String> {
    let porter = horizon_hooks::porter::engine::active()
        .ok_or("Modrinth bridge is not registered in this build")?;
    let raw = porter.modrinth_versions(project_id, minecraft_version, loader)?;
    serde_json::from_str(&raw).map_err(|e| e.to_string())
}

/// Download a Modrinth `.jar`, run pure-Rust bytecode check + content carry into `horizon-mods/`,
/// and automatically enable the ported mod in `horizon-mods.txt`.
pub fn install_from_modrinth(
    home: &Path,
    project_id: &str,
    version_id: Option<&str>,
    minecraft_version: Option<&str>,
) -> Result<Value, String> {
    let porter = horizon_hooks::porter::engine::active()
        .ok_or("Modrinth installer is not registered in this build")?;
    let work_dir = home.join(".mod-porter-work");
    let out = porter.install_modrinth(
        project_id,
        version_id,
        minecraft_version,
        &mods_dir(home),
        &work_dir,
    )?;
    let _ = fs::remove_dir_all(&work_dir);
    let installed = set_enabled(home, &out.mod_id, true)?;
    Ok(json!({
        "ok": true,
        "mod_id": out.mod_id,
        "name": out.name,
        "version": out.version,
        "loader": out.loader,
        "verdict": out.verdict,
        "summary": out.summary,
        "ports": out.ports,
        "skipped": out.skipped,
        "installed": installed,
    }))
}

/// Import a local `.hmod` package or Fabric / NeoForge / Forge / Quilt `.jar` picked from Files.
pub fn import_local_mod_file(home: &Path, source_path: &Path) -> Result<Value, String> {
    let ext = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "hmod" => {
            let pkg = Package::read(source_path)?;
            if pkg.wasm.is_some()
                || pkg
                    .manifest
                    .capabilities
                    .iter()
                    .any(|c| *c != horizon_mods::Capability::Registries)
            {
                return Err(format!(
                    "{} contains legacy WebAssembly code capabilities; Horizon runs content .hmod mods",
                    pkg.manifest.name
                ));
            }
            let _ = Content::read(source_path, &pkg.manifest)?;
            let id = pkg.manifest.id.clone();
            let dir = mods_dir(home);
            fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let dest = dir.join(format!("{id}.hmod"));
            let temp = dir.join(format!(".{id}.hmod.importing"));
            let _ = fs::remove_file(&temp);
            fs::copy(source_path, &temp).map_err(|e| format!("{}: {e}", source_path.display()))?;
            fs::rename(&temp, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            let _ = fs::remove_dir_all(dir.join(&id));

            write_source_meta(
                home,
                &StoredSourceMeta {
                    mod_id: id.clone(),
                    name: pkg.manifest.name.clone(),
                    version: pkg.manifest.version.clone(),
                    loader: "horizon".into(),
                    origin: "local-hmod".into(),
                    verdict: "works".into(),
                    summary: if pkg.manifest.description.is_empty() {
                        "Imported local .hmod content package.".into()
                    } else {
                        pkg.manifest.description.clone()
                    },
                    ports: Vec::new(),
                    skipped: Vec::new(),
                    namespaces: owned_namespaces(&id, &pkg.manifest.namespaces),
                    project_id: None,
                    version_id: Some(pkg.manifest.version.clone()),
                    icon_url: None,
                },
            );
            let installed = set_enabled(home, &id, true)?;
            Ok(json!({
                "ok": true,
                "mod_id": id,
                "name": pkg.manifest.name,
                "version": pkg.manifest.version,
                "verdict": "works",
                "summary": "Imported .hmod content mod and enabled it.",
                "installed": installed,
            }))
        }
        "jar" => {
            let porter = horizon_hooks::porter::engine::active()
                .ok_or("Java .jar mod porter is not registered in this build")?;
            let work_dir = home.join(".mod-porter-work");
            let out = porter.port_jar(source_path, Some(&mods_dir(home)), &work_dir)?;
            let _ = fs::remove_dir_all(&work_dir);
            let installed = set_enabled(home, &out.mod_id, true)?;
            Ok(json!({
                "ok": true,
                "mod_id": out.mod_id,
                "name": out.name,
                "version": out.version,
                "loader": out.loader,
                "verdict": out.verdict,
                "summary": out.summary,
                "ports": out.ports,
                "skipped": out.skipped,
                "installed": installed,
            }))
        }
        _ => Err("Unsupported mod file extension: select a .hmod or .jar file.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_home(tag: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "horizon-launcher-mods-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_hmod(path: &Path, id: &str, name: &str, extra_ns: &[&str]) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("horizon-mod.toml", opts).unwrap();
        let ns_toml = if extra_ns.is_empty() {
            String::new()
        } else {
            let items: Vec<String> = extra_ns.iter().map(|s| format!("{s:?}")).collect();
            format!("namespaces = [{}]\n", items.join(", "))
        };
        let manifest = format!(
            "id = {id:?}\nname = {name:?}\nversion = \"1.0.0\"\ndescription = \"Test mod\"\napi = \"0.2\"\ncapabilities = [\"registries\"]\n{ns_toml}"
        );
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file(format!("assets/{id}/models/item/gem.json"), opts)
            .unwrap();
        zip.write_all(br#"{"parent":"minecraft:item/generated"}"#)
            .unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn imports_hmod_toggles_detects_namespace_clashes_and_deletes_cleanly() {
        let home = temp_home("lifecycle");
        let hmod_a = home.join("amber_lamps.hmod");
        let hmod_b = home.join("amber_extra.hmod");
        write_hmod(&hmod_a, "amber_lamps", "Amber Lamps", &["shared_amber"]);
        write_hmod(&hmod_b, "amber_extra", "Amber Extra", &["shared_amber"]);

        let res_a = import_local_mod_file(&home, &hmod_a).expect("import amber_lamps");
        assert_eq!(res_a["ok"], true);
        assert_eq!(res_a["mod_id"], "amber_lamps");

        let res_b = import_local_mod_file(&home, &hmod_b).expect("import amber_extra");
        assert_eq!(res_b["ok"], true);

        // Both mods enabled and sharing `shared_amber` must trigger `clash_warning`!
        let list = list_installed(&home);
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|m| m.enabled && m.clash_warning.is_some()));

        // Turning one off resolves the namespace clash immediately and persists to `horizon-mods.txt`.
        let after_toggle = set_enabled(&home, "amber_extra", false).unwrap();
        let lamps = after_toggle.iter().find(|m| m.id == "amber_lamps").unwrap();
        assert!(lamps.enabled);
        assert!(lamps.clash_warning.is_none());
        let mods_txt = fs::read_to_string(home.join("horizon-mods.txt")).unwrap();
        assert!(mods_txt.contains("mod.amber_lamps:true"));
        assert!(!mods_txt.contains("mod.amber_extra:true"));

        // Deleting removes `.hmod`, `.sources/amber_extra.json`, and cleans the list.
        let after_del = delete_mod(&home, "amber_extra").unwrap();
        assert_eq!(after_del.len(), 1);
        assert_eq!(after_del[0].id, "amber_lamps");
        let _ = fs::remove_dir_all(&home);
    }
}
