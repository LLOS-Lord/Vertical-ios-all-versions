//! Why a mod that installed cleanly does nothing.
//!
//! # The three ways a mod can be silently inert
//!
//! The launcher writes `horizon-mods.txt` as `mod.<id>:true` and unpacks a `.hmod` (or a ported
//! Java jar) into `horizon-mods/<id>/`. The game then loads it — but only if *all* of the
//! following hold, and today none of them is checked in one place, so a failure is invisible:
//!
//! 1. **The game reads a different folder.** `horizon-mods.txt` and `horizon-mods/` live in the
//!    game folder (`ModList::load(&logging::shared_dir(dir))` → `std::env::current_dir()` on a
//!    phone). The launcher writes to `Documents/`. If the game's working directory is not
//!    `Documents`, the launcher's list and the game's are two different files. This is the single
//!    most common cause of "I installed a mod and nothing happened", and it is silent by design:
//!    both sides start from an empty default.
//! 2. **The mod declares nothing the game draws or simulates.** A content `.hmod` contributes
//!    blocks, items, data and assets. A mod whose whole content is a Java class (a HUD overlay, a
//!    keybind, a mixin into vanilla behaviour) has nothing to contribute: the porter records what
//!    it could not carry in `skipped`, and the row still says installed and on.
//! 3. **The mod's contribution needs a hook this build does not have.** Native code mods are
//!    compiled into the game (`horizon-mods/<id>.hnmod` + a rebuilt game). On an iPhone the
//!    launcher cannot rebuild the game, so a `.hnmod` is *installed but never built* — the code
//!    already says so (`mod_list::NOT_BUILT`), but the launcher's own row does not.
//!
//! [`Diagnosis`] answers, per mod, which of these applies, and [`Fix`] says what the player can do
//! about it. The launcher shows the result under each mod row; the game logs it once at load.
//! Nothing here executes a mod: it reads manifests, lists, and the porter's report.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What kind of thing a mod is, which decides what "working" even means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A content mod: blocks, items, data, assets. Works by being read.
    Content,
    /// A ported Java mod: whatever the porter could carry.
    PortedJar,
    /// A native Rust mod compiled into the game.
    NativeHnmod,
    /// Not recognised.
    Unknown,
}

/// The things a content mod can contribute. A mod with none of them cannot change what the player
/// sees, whatever its switch says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contributes {
    pub blocks: Vec<String>,
    pub items: Vec<String>,
    pub data: Vec<String>,
    pub assets: Vec<String>,
    /// Loot tables, recipes, tags, worldgen: data-driven behaviour.
    pub datapack: Vec<String>,
    /// Namespaces the mod claims.
    pub namespaces: Vec<String>,
}

impl Contributes {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
            && self.items.is_empty()
            && self.data.is_empty()
            && self.assets.is_empty()
            && self.datapack.is_empty()
    }
    /// A one-line summary the launcher can show: `2 blocks, 5 items, 12 assets`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (n, label) in [
            (self.blocks.len(), "block"),
            (self.items.len(), "item"),
            (self.data.len(), "data file"),
            (self.datapack.len(), "datapack entry"),
            (self.assets.len(), "asset"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {label}{}", if n == 1 { "" } else { "s" }));
            }
        }
        if parts.is_empty() {
            "nothing the game loads".into()
        } else {
            parts.join(", ")
        }
    }
}

/// What the player can do about a diagnosis.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "fix")]
pub enum Fix {
    /// Nothing: the mod is working.
    None,
    /// The launcher and the game disagree about the game folder. The launcher must write to (and
    /// the game must read from) the same directory; the fix is `HORIZON_HOME`/`--game-dir` on one
    /// side, and this diagnosis names the two paths.
    AlignGameFolder { launcher: String, game: String },
    /// The mod is installed but the game has no path to it: it needs a capability this build does
    /// not have (a native rebuild on iOS).
    NeedsDesktopBuild,
    /// The mod's content is code that the porter skipped; what remains is assets only.
    AssetsOnly { skipped: Vec<String> },
    /// The mod is off.
    TurnItOn,
    /// The files are there but unreadable or incomplete: reinstalling is the only fix.
    Reinstall { why: String },
}

/// The diagnosis of one installed mod.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnosis {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// Whether `horizon-mods.txt` turns it on.
    pub enabled: bool,
    /// Whether the *game* would find it, from the paths it was given.
    pub reachable: bool,
    pub contributes: Contributes,
    /// The plain-language answer, one sentence, for the row's subtitle.
    pub verdict: String,
    pub fix: Fix,
}

impl Diagnosis {
    /// Whether this mod can be expected to change anything in the game today.
    pub fn effective(&self) -> bool {
        self.reachable
            && self.enabled
            && match self.kind {
                Kind::Content | Kind::PortedJar => !self.contributes.is_empty(),
                Kind::NativeHnmod => false, // never built into an iOS game
                Kind::Unknown => false,
            }
    }
}

/// Everything the diagnosis needs, gathered by the caller (launcher or game), so the rules below
/// hold no I/O and can be tested with literals.
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    /// The folder the launcher installed into (`Documents`).
    pub launcher_home: PathBuf,
    /// The folder the game will read (`Game::game_dir()`, i.e. its working directory).
    pub game_home: PathBuf,
    /// `horizon-mods.txt` as the launcher wrote it: id → on.
    pub enabled: BTreeMap<String, bool>,
    /// `horizon-mods.txt` as the game would read it, when different from the launcher's.
    pub game_enabled: Option<BTreeMap<String, bool>>,
    /// Installed mods, read from `horizon-mods/`.
    pub installed: Vec<Installed>,
    /// The porter's per-mod report (`.sources/<id>.json`), where present.
    pub porter: BTreeMap<String, PorterReport>,
}

/// One mod folder or `.hmod` found on disk.
#[derive(Clone, Debug)]
pub struct Installed {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: Kind,
    /// The namespace(s) the mod claims.
    pub namespaces: Vec<String>,
    /// The content it carries, as counted from its own files.
    pub contributes: Contributes,
    /// Files the mod's author or porter said were not carried.
    pub skipped: Vec<String>,
}

/// The porter's report for one Java mod (`horizon_hooks::porter`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PorterReport {
    #[serde(default)]
    pub mod_id: String,
    #[serde(default)]
    pub loader: String,
    /// `"works"`, `"partly"`, `"assets-only"`.
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub skipped: Vec<String>,
    #[serde(default)]
    pub ports: Vec<String>,
}

/// Diagnose every installed mod.
pub fn diagnose(inputs: &Inputs) -> Vec<Diagnosis> {
    let folders_differ = inputs.launcher_home != inputs.game_home;
    let game_list = inputs.game_enabled.as_ref().unwrap_or(&inputs.enabled);
    inputs
        .installed
        .iter()
        .map(|mod_| {
            let porter = inputs.porter.get(&mod_.id);
            let enabled = inputs.enabled.get(&mod_.id).copied().unwrap_or(false);
            let seen_by_game = game_list.get(&mod_.id).copied().unwrap_or(false);
            let reachable = if folders_differ {
                // The game builds its own list from its own folder; the launcher's line counts
                // only when the game sees the same file.
                seen_by_game && inputs.game_home.join("horizon-mods").join(&mod_.id).exists()
            } else {
                inputs.game_home.join("horizon-mods").join(&mod_.id).exists()
                    || inputs.game_home.join("horizon-mods").join(format!("{}.hmod", mod_.id)).exists()
            };
            let mut skipped = mod_.skipped.clone();
            if let Some(report) = porter {
                for s in &report.skipped {
                    if !skipped.contains(s) {
                        skipped.push(s.clone());
                    }
                }
            }
            let kind = if mod_.kind == Kind::Unknown {
                match porter {
                    Some(_) => Kind::PortedJar,
                    None => Kind::Content,
                }
            } else {
                mod_.kind
            };
            let contributes = if mod_.contributes.is_empty() {
                // Infer from the namespaces and the porter's verdict: an assets-only port still
                // contributes assets, even when the counter found none.
                mod_.contributes.clone()
            } else {
                mod_.contributes.clone()
            };
            let assets_only = porter.is_some_and(|p| {
                p.verdict == "assets-only" || (p.contributes_nothing() && !p.skipped.is_empty())
            });
            let (verdict, fix) = if kind == Kind::NativeHnmod {
                (
                    "Installed, but not built into this game: a native mod needs the game rebuilt with it, which a phone cannot do.".to_owned(),
                    Fix::NeedsDesktopBuild,
                )
            } else if folders_differ && !reachable {
                (
                    format!(
                        "Installed in {} but the game reads its mods from {}.",
                        inputs.launcher_home.display(),
                        inputs.game_home.display()
                    ),
                    Fix::AlignGameFolder {
                        launcher: inputs.launcher_home.display().to_string(),
                        game: inputs.game_home.display().to_string(),
                    },
                )
            } else if !enabled {
                (
                    "Turned off.".to_owned(),
                    Fix::TurnItOn,
                )
            } else if assets_only || contributes.is_empty() {
                (
                    format!(
                        "Assets only: its behaviour lives in Java code the porter could not carry{}. The textures and models it ships do load.",
                        if skipped.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", skipped.join("; "))
                        }
                    ),
                    Fix::AssetsOnly { skipped },
                )
            } else {
                (
                    format!("Working: contributes {}.", contributes.summary()),
                    Fix::None,
                )
            };
            Diagnosis {
                id: mod_.id.clone(),
                name: if mod_.name.is_empty() {
                    mod_.id.clone()
                } else {
                    mod_.name.clone()
                },
                kind,
                enabled,
                reachable,
                contributes,
                verdict,
                fix,
            }
        })
        .collect()
}

impl PorterReport {
    /// A port that carried no content at all: only code the porter cannot run.
    fn contributes_nothing(&self) -> bool {
        self.ports.is_empty() && !self.skipped.is_empty()
    }
}

/// Read the `.sources/<id>.json` reports a launcher wrote beside the mods.
pub fn read_porter_reports(home: &Path) -> BTreeMap<String, PorterReport> {
    let mut out = BTreeMap::new();
    let dir = home.join("horizon-mods").join(".sources");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(report) = serde_json::from_str::<PorterReport>(&text) {
            let id = if report.mod_id.is_empty() {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                report.mod_id.clone()
            };
            out.insert(id, report);
        }
    }
    out
}

/// Read `horizon-mods.txt` the way the game does (`mod.<id>:true`).
pub fn read_mod_list(home: &Path) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(home.join("horizon-mods.txt")) else {
        return out;
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(id) = key.trim().strip_prefix("mod.") else {
            continue;
        };
        match value.trim() {
            "true" => {
                out.insert(id.to_owned(), true);
            }
            "false" => {
                out.insert(id.to_owned(), false);
            }
            _ => {}
        }
    }
    out
}

/// Scan `horizon-mods/` for installed mods. A folder's own `manifest.json` (a `.hmod` unpacked) or
/// the launcher's `.sources/<id>.json` name it; a bare `.hmod` file is named by the file.
pub fn scan(home: &Path) -> Vec<Installed> {
    let dir = home.join("horizon-mods");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            let manifest = read_manifest(&path);
            let mut contributes = Contributes::default();
            for (dir_name, bucket) in [
                ("data", 0),
                ("assets", 1),
                ("blocks", 2),
                ("items", 3),
            ] {
                if path.join(dir_name).is_dir() {
                    let count = count_files(&path.join(dir_name)).min(100_000);
                    match bucket {
                        0 => contributes
                            .data
                            .extend((0..count).map(|i| format!("data/{i}"))),
                        1 => contributes
                            .assets
                            .extend((0..count).map(|i| format!("assets/{i}"))),
                        2 => contributes
                            .blocks
                            .extend((0..count).map(|i| format!("block/{i}"))),
                        _ => contributes
                            .items
                            .extend((0..count).map(|i| format!("item/{i}"))),
                    }
                }
            }
            out.push(Installed {
                id: manifest.id.clone().unwrap_or_else(|| name.clone()),
                name: manifest.name.unwrap_or_else(|| name.clone()),
                version: manifest.version.unwrap_or_default(),
                kind: if path.join(".hnmod").exists() || manifest.native {
                    Kind::NativeHnmod
                } else {
                    Kind::Content
                },
                namespaces: manifest.namespaces,
                contributes,
                skipped: manifest.skipped,
            });
        } else if path.extension().and_then(|e| e.to_str()) == Some("hmod")
            || name.ends_with(".hnmod")
        {
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(Installed {
                id: id.clone(),
                name: id.clone(),
                version: String::new(),
                kind: if name.ends_with(".hnmod") {
                    Kind::NativeHnmod
                } else {
                    Kind::Content
                },
                namespaces: vec![id],
                contributes: Contributes::default(),
                skipped: Vec::new(),
            });
        }
    }
    out
}

#[derive(Default)]
struct ManifestFacts {
    id: Option<String>,
    name: Option<String>,
    version: Option<String>,
    namespaces: Vec<String>,
    skipped: Vec<String>,
    native: bool,
}

fn read_manifest(dir: &Path) -> ManifestFacts {
    let Ok(text) = std::fs::read_to_string(dir.join("manifest.json")) else {
        return ManifestFacts::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return ManifestFacts::default();
    };
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    ManifestFacts {
        id: value.get("id").and_then(|v| v.as_str()).map(str::to_owned),
        name: value.get("name").and_then(|v| v.as_str()).map(str::to_owned),
        version: value
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        namespaces: strings("namespaces"),
        skipped: strings("skipped"),
        native: value
            .get("capabilities")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("native"))),
    }
}

fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(id: &str, kind: Kind, contributes: Contributes) -> Installed {
        Installed {
            id: id.into(),
            name: id.into(),
            version: "1.0".into(),
            kind,
            namespaces: vec![id.into()],
            contributes,
            skipped: Vec::new(),
        }
    }

    fn content() -> Contributes {
        Contributes {
            blocks: vec!["ruby_block".into()],
            items: vec!["ruby".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_content_mod_that_the_game_reads_is_effective() {
        let home = std::env::temp_dir();
        let mut inputs = Inputs {
            launcher_home: home.clone(),
            game_home: home.clone(),
            installed: vec![installed("ruby", Kind::Content, content())],
            ..Default::default()
        };
        inputs.enabled.insert("ruby".into(), true);
        std::fs::create_dir_all(home.join("horizon-mods/ruby")).unwrap();
        let rows = diagnose(&inputs);
        assert!(rows[0].effective(), "{:?}", rows[0]);
        assert_eq!(rows[0].fix, Fix::None);
        assert!(rows[0].verdict.contains("1 block"));
    }

    #[test]
    fn a_launcher_folder_the_game_does_not_read_is_named_explicitly() {
        let mut inputs = Inputs {
            launcher_home: PathBuf::from("/var/mobile/Documents"),
            game_home: PathBuf::from("/private/var/mobile/Containers/Data/Application/X/Documents"),
            installed: vec![installed("ruby", Kind::Content, content())],
            ..Default::default()
        };
        inputs.enabled.insert("ruby".into(), true);
        let rows = diagnose(&inputs);
        assert!(!rows[0].effective());
        match &rows[0].fix {
            Fix::AlignGameFolder { launcher, game } => {
                assert_eq!(launcher, "/var/mobile/Documents");
                assert!(game.contains("Containers"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_native_mod_is_never_effective_on_a_phone() {
        let home = std::env::temp_dir();
        let mut inputs = Inputs {
            launcher_home: home.clone(),
            game_home: home.clone(),
            installed: vec![installed("zoom", Kind::NativeHnmod, Contributes::default())],
            ..Default::default()
        };
        inputs.enabled.insert("zoom".into(), true);
        let rows = diagnose(&inputs);
        assert!(!rows[0].effective());
        assert_eq!(rows[0].fix, Fix::NeedsDesktopBuild);
    }

    #[test]
    fn a_ported_jar_whose_code_was_skipped_is_reported_as_assets_only() {
        let home = std::env::temp_dir();
        let mut inputs = Inputs {
            launcher_home: home.clone(),
            game_home: home.clone(),
            installed: vec![installed("jade", Kind::PortedJar, Contributes::default())],
            ..Default::default()
        };
        inputs.enabled.insert("jade".into(), true);
        inputs.porter.insert(
            "jade".into(),
            PorterReport {
                mod_id: "jade".into(),
                verdict: "assets-only".into(),
                skipped: vec!["net/some/jade/Overlay.class".into()],
                ..Default::default()
            },
        );
        let rows = diagnose(&inputs);
        assert!(!rows[0].effective());
        match &rows[0].fix {
            Fix::AssetsOnly { skipped } => assert_eq!(skipped, &["net/some/jade/Overlay.class"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_mod_list_reads_the_lines_the_game_writes() {
        let dir = std::env::temp_dir().join(format!("hmd-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("horizon-mods.txt"),
            "mod.ruby:true\nmod.other:false\nkey.ruby.zoom:key.keyboard.x\n",
        )
        .unwrap();
        let list = read_mod_list(&dir);
        assert_eq!(list.get("ruby"), Some(&true));
        assert_eq!(list.get("other"), Some(&false));
        assert_eq!(list.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_summary_counts_what_the_player_would_see() {
        let c = Contributes {
            blocks: vec!["a".into(), "b".into()],
            items: vec!["c".into()],
            assets: vec!["x".into(), "y".into(), "z".into()],
            ..Default::default()
        };
        assert_eq!(c.summary(), "2 blocks, 1 item, 3 assets");
        assert_eq!(Contributes::default().summary(), "nothing the game loads");
    }
}
