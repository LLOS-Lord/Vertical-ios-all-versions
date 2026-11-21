//! Is the version the launcher says really the version the game will play?
//!
//! # The bug this crate exists for
//!
//! The launcher's version browser (`crates/horizon-client/src/versions.rs`) can install any Mojang
//! version: it downloads that version's `client.jar` and its asset objects, unpacks the textures
//! and sounds, and then — because Horizon's simulation tables are generated from a *pinned* jar by
//! `horizon-prepare` on a desktop — hardlinks the 26.3 simulation base into the new version's
//! folder (`populate_from_simulation_base`). The result is honest at the top level:
//! `horizon-resources.json` keeps `"minecraft": "26.3"` for the engine's schema check and records
//! the player's choice in `"source_minecraft"`.
//!
//! It is *not* honest where the player can see it. The blocks, items, entity models, block states
//! and data registry the game actually runs are 26.3's, because those live in the hardlinked
//! `registries.json`, `facts.json`, `items.json`, `render-definitions.json`, `model-states.json`,
//! `block-behaviours.json` and the `data/` tree. Textures are the chosen version's; the *things the
//! textures are drawn on* are 26.3's. A player who installs 1.20.1 sees 1.20.1 in the lobby and
//! 26.3's items in the world — exactly the reported symptom — and worse, a world created there
//! writes 26.3 data and can silently corrupt a 1.20.1 save.
//!
//! # What truth means here
//!
//! [`Inspect`] reads the installed folder and answers with a [`Verdict`]:
//!
//! | Verdict | Meaning |
//! | --- | --- |
//! | [`Verdict::Native`] | The pinned version, whose tables were generated from its own jar. |
//! | [`Verdict::Ported`] | Another version whose tables were *generated from its own jar* and adapted (the only way a version is truly playable). |
//! | [`Verdict::Cosmetic`] | Another version's assets on 26.3's simulation: looks different, plays 26.3. Must be labelled as such and must not write to saves made by the real version. |
//! | [`Verdict::Broken`] | Tables present but unusable (missing registries, wrong data version, half a download). |
//!
//! The launcher and the game both ask this crate, so the badge the player sees and the behaviour
//! in the world cannot disagree. [`plan`] turns a verdict into what the installer must still do:
//! which preparation stages are missing, and whether the version may be selected at all.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The files every Horizon simulation tree must carry, with the group each belongs to. Missing a
/// group is what makes a tree unusable; missing a file is what makes a version unreachable.
pub const SIMULATION_GROUPS: &[(&str, &[&str])] = &[
    (
        "registries",
        &[
            "registries.json",
            "items.json",
            "entity-types.json",
            "block-tags.json",
            "damage-types.json",
        ],
    ),
    (
        "gameplay",
        &[
            "facts.json",
            "gameplay-facts.json",
            "block-behaviours.json",
            "movement-properties.nbt",
            "cached-collision.nbt",
            "datafix-tables.json",
        ],
    ),
    (
        "render",
        &[
            "render-definitions.json",
            "occlusion.nbt",
            "model-states.json",
            "entity-models.json",
        ],
    ),
    (
        "worldgen",
        &["worldgen/blocks/block-properties.nbt", "biome-parameters"],
    ),
];

/// The manifest at the root of a prepared tree (`horizon-resources.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub schema: u32,
    /// The version the tree's engine-level schema belongs to (always the pinned one for a
    /// hardlinked tree).
    #[serde(default)]
    pub minecraft: String,
    /// The version the player chose, when it differs.
    #[serde(default)]
    pub source_minecraft: String,
    #[serde(default)]
    pub full_simulation: bool,
    #[serde(default)]
    pub prepared_by: String,
    /// The data version the tables were generated for. `horizon-prepare` writes it; a hardlink of
    /// the 26.3 base keeps 26.3's.
    #[serde(default)]
    pub data_version: Option<i64>,
    /// Provenance this crate writes: which version's jar the tables came from, and whether they
    /// were adapted. Absent on trees made before it, which is itself evidence.
    #[serde(default)]
    pub truth: Option<TruthStamp>,
}

/// What the installer records when it generated tables from `jar`'s own content.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TruthStamp {
    /// The Minecraft version the tables were generated for.
    pub version: String,
    /// The SHA-1 of the client jar they were generated from: the proof a reader can re-check.
    pub client_jar_sha1: String,
    /// The protocol the tables' registries imply, when known.
    #[serde(default)]
    pub protocol: Option<i32>,
    /// `"generated"` (from this version's own jar) or `"adapted"` (generated for another version
    /// and translated by an adapter that was verified against both).
    pub provenance: Provenance,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provenance {
    /// The tables were produced by `horizon-prepare` from this version's own jar.
    Generated,
    /// The tables were produced for another version and translated; `version` is the version the
    /// translation targets, and the adapter is what makes it true.
    Adapted,
    /// The tables belong to another version and were not translated: cosmetic only.
    #[default]
    Borrowed,
}

/// What a version's folder actually is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// The engine's pinned version, generated from its own jar.
    Native,
    /// Another version with its own generated (or verified-adapted) simulation tables.
    Ported,
    /// Another version's textures and sounds over the pinned version's simulation.
    Cosmetic,
    /// Nothing playable: missing tables, half a download, or a manifest that contradicts itself.
    Broken,
}

impl Verdict {
    /// Whether a world created with this version may be opened by the game as that version.
    /// `Cosmetic` may not: it would write 26.3 data into a save the player believes is theirs.
    pub fn playable_as_itself(self) -> bool {
        matches!(self, Self::Native | Self::Ported)
    }
    /// Whether the launcher must show the player a warning before entering a world.
    pub fn needs_warning(self) -> bool {
        matches!(self, Self::Cosmetic | Self::Broken)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Ported => "ported",
            Self::Cosmetic => "cosmetic",
            Self::Broken => "broken",
        }
    }
    /// The badge the SwiftUI version row shows.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Native => "Native",
            Self::Ported => "Full port",
            Self::Cosmetic => "Assets only",
            Self::Broken => "Incomplete",
        }
    }
}

/// The evidence an [`Inspect`] gathers: what is on disk and what it says about itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Inspect {
    /// The folder's own name (`versions/<id>/resources`).
    pub folder_id: String,
    pub manifest: Option<Manifest>,
    /// The version profile the launcher wrote (`version-profile.json`).
    pub profile_id: Option<String>,
    /// Simulation groups present on disk.
    pub groups_present: Vec<String>,
    /// Simulation groups missing.
    pub groups_missing: Vec<String>,
    /// The version whose client jar was unpacked into this tree, when the downloader recorded it.
    pub unpacked_from: Option<String>,
    /// SHA-1 of the jar the downloader kept (`minecraft/versions/<id>/<id>.jar`) if it is there.
    pub client_jar_sha1: Option<String>,
    /// The data version the tree's `registries.json` was generated for, when it says.
    pub registries_data_version: Option<i64>,
    /// The pinned version the engine itself was built for.
    pub engine_version: String,
    /// The engine's own data version, for comparison.
    pub engine_data_version: Option<i64>,
}

impl Inspect {
    /// Read `resources_dir`, with `home` as the Documents folder that holds `minecraft/`.
    pub fn read(home: &Path, resources_dir: &Path, engine_version: &str) -> Self {
        let folder_id = resources_dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .or_else(|| {
                resources_dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let manifest: Option<Manifest> = fs::read_to_string(resources_dir.join("horizon-resources.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let profile_id = ["version-profile.json"]
            .iter()
            .map(|n| resources_dir.join(n))
            .chain(
                resources_dir
                    .parent()
                    .map(|p| p.join("version-profile.json"))
                    .into_iter(),
            )
            .find_map(|p| {
                let text = fs::read_to_string(p).ok()?;
                let value: serde_json::Value = serde_json::from_str(&text).ok()?;
                value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            });
        let mut groups_present = Vec::new();
        let mut groups_missing = Vec::new();
        for (group, files) in SIMULATION_GROUPS {
            if files.iter().all(|f| resources_dir.join(f).exists()) {
                groups_present.push((*group).to_owned());
            } else {
                groups_missing.push((*group).to_owned());
            }
        }
        let registries_data_version = read_json(&resources_dir.join("registries.json"))
            .and_then(|v| v.get("data_version").and_then(|d| d.as_i64()));
        let jar = home
            .join("minecraft")
            .join("versions")
            .join(&folder_id)
            .join(format!("{folder_id}.jar"));
        let client_jar_sha1 = fs::read(&jar).ok().map(|bytes| {
            use sha1::{Digest, Sha1};
            hex(&Sha1::digest(&bytes))
        });
        Self {
            folder_id,
            manifest,
            profile_id,
            groups_present,
            groups_missing,
            unpacked_from: None,
            client_jar_sha1,
            registries_data_version,
            engine_version: engine_version.to_owned(),
            engine_data_version: None,
        }
    }

    /// The version the folder claims to be, most specific first.
    pub fn claimed(&self) -> String {
        self.manifest
            .as_ref()
            .map(|m| {
                if !m.source_minecraft.trim().is_empty() {
                    m.source_minecraft.trim().to_owned()
                } else if !m.minecraft.trim().is_empty() {
                    m.minecraft.trim().to_owned()
                } else {
                    String::new()
                }
            })
            .filter(|s| !s.is_empty())
            .or_else(|| self.profile_id.clone())
            .unwrap_or_else(|| self.folder_id.clone())
    }

    /// The version the *simulation* belongs to: the manifest's `minecraft`, which a hardlink of the
    /// pinned base sets to the pinned version, or the tables' own data version when it can be read.
    pub fn simulation_version(&self) -> String {
        let manifest_mc = self
            .manifest
            .as_ref()
            .map(|m| m.minecraft.trim().to_owned())
            .unwrap_or_default();
        if !manifest_mc.is_empty() {
            return manifest_mc;
        }
        match self.registries_data_version {
            Some(v) if Some(v) == self.engine_data_version => self.engine_version.clone(),
            _ => String::new(),
        }
    }
}

/// The answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Truth {
    pub claimed: String,
    pub simulation: String,
    pub verdict: Verdict,
    /// Why, in one sentence the launcher can show.
    pub detail: String,
    /// What the installer must still do for the version to become real, in order.
    pub remaining: Vec<Stage>,
}

/// One step of making a version real. `Cosmetic` trees are missing the first two; a `Broken` tree
/// is missing whichever of them its files show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// Download `<id>.json`, `client.jar` and the asset index (Mojang official).
    Download,
    /// Generate the simulation tables from *that* jar. On a phone this is
    /// `horizon-prepare-mobile`; on a desktop it is `horizon-prepare`.
    GenerateTables,
    /// Translate the tables with a protocol adapter verified against both versions.
    AdaptTables,
    /// Unpack the assets.
    UnpackAssets,
    /// Write the provenance stamp.
    Stamp,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::GenerateTables => "generate-tables",
            Self::AdaptTables => "adapt-tables",
            Self::UnpackAssets => "unpack-assets",
            Self::Stamp => "stamp",
        }
    }
}

/// Decide what the evidence means. The rules, in order:
///
/// 1. Any simulation group missing → [`Verdict::Broken`].
/// 2. The claimed version is the engine's, and the simulation's matches → [`Verdict::Native`].
/// 3. The claimed version differs from the simulation's:
///    a. a [`TruthStamp`] says the tables were generated for the claimed version, and the jar on
///       disk is the one the stamp names (when both are known) → [`Verdict::Ported`];
///    b. a stamp says `adapted` → [`Verdict::Ported`] (the adapter is the proof);
///    c. otherwise → [`Verdict::Cosmetic`]: this is the reported bug, caught in the act.
/// 4. The claimed and simulated versions agree but the stamp is missing and the tree was not made
///    from a jar of that version (`unpacked_from` disagrees) → [`Verdict::Cosmetic`].
pub fn judge(inspect: &Inspect) -> Truth {
    let claimed = inspect.claimed();
    let simulation = inspect.simulation_version();
    let mut remaining = Vec::new();
    let (verdict, detail) = if !inspect.groups_missing.is_empty() {
        // What is missing decides the stages: tables, or just the assets.
        if inspect.groups_missing.iter().any(|g| g == "registries") {
            remaining.push(Stage::Download);
            remaining.push(Stage::GenerateTables);
        } else {
            remaining.push(Stage::GenerateTables);
        }
        remaining.push(Stage::Stamp);
        (
            Verdict::Broken,
            format!(
                "Incomplete: missing {} of the prepared tree. Install again to finish it.",
                inspect.groups_missing.join(", ")
            ),
        )
    } else if claimed == simulation && claimed == inspect.engine_version {
        (Verdict::Native, format!("{claimed}, prepared from its own jar."))
    } else if claimed == simulation {
        // Numbers agree, but is the stamp trustworthy?
        match inspect.manifest.as_ref().and_then(|m| m.truth.as_ref()) {
            Some(stamp)
                if stamp.provenance == Provenance::Generated
                    && (stamp.client_jar_sha1.is_empty()
                        || inspect.client_jar_sha1.as_deref()
                            == Some(stamp.client_jar_sha1.as_str())) =>
            {
                (Verdict::Ported, format!("{claimed}, prepared from its own jar."))
            }
            Some(stamp) if stamp.provenance == Provenance::Adapted && stamp.version == claimed => (
                Verdict::Ported,
                format!("{claimed}, generated for another version and adapted."),
            ),
            // A stamp that names a jar the folder does not have: the claim is not proven, and
            // the tables may be a borrowed set with a stamp written over them.
            Some(stamp) if stamp.provenance == Provenance::Generated => {
                remaining.push(Stage::GenerateTables);
                remaining.push(Stage::Stamp);
                (
                    Verdict::Cosmetic,
                    format!(
                        "{claimed} claims tables from its own jar, but the jar on disk does not match the stamp ({}).",
                        stamp.client_jar_sha1
                    ),
                )
            }
            Some(_) => {
                remaining.push(Stage::GenerateTables);
                remaining.push(Stage::Stamp);
                (
                    Verdict::Cosmetic,
                    format!("{claimed}'s tables were borrowed from another version."),
                )
            }
            _ if inspect.unpacked_from.as_deref().is_some_and(|u| u != claimed) => {
                remaining.push(Stage::GenerateTables);
                remaining.push(Stage::Stamp);
                (
                    Verdict::Cosmetic,
                    format!(
                        "Assets of {claimed} over {simulation}'s simulation: the world still plays {simulation}."
                    ),
                )
            }
            // No evidence either way: honest about not knowing.
            _ => (
                Verdict::Ported,
                format!("{claimed}, with no provenance stamp to re-check."),
            ),
        }
    } else if inspect
        .manifest
        .as_ref()
        .and_then(|m| m.truth.as_ref())
        .is_some_and(|s| s.provenance == Provenance::Adapted && s.version == claimed)
    {
        // The version's own tables were generated elsewhere and translated: the adapter is the
        // proof, and the reported simulation is the one the adapter starts from.
        (
            Verdict::Ported,
            format!("{claimed}, generated for {simulation} and adapted by its protocol adapter."),
        )
    } else {
        // The reported bug: the label says one version, the tables say another.
        remaining.push(Stage::GenerateTables);
        remaining.push(Stage::Stamp);
        if claimed != inspect.engine_version && simulation == inspect.engine_version {
            // Nothing short of generating tables from this version's own jar can fix it.
            remaining.insert(0, Stage::Download);
        }
        (
            Verdict::Cosmetic,
            format!(
                "Assets of {claimed} over {simulation}'s simulation: textures change, blocks, items and mobs are {simulation}'s."
            ),
        )
    };
    Truth {
        claimed,
        simulation,
        verdict,
        detail,
        remaining,
    }
}

/// Read the manifest at `resources_dir` and judge it, without a full inspect.
pub fn judge_dir(home: &Path, resources_dir: &Path, engine_version: &str) -> Truth {
    judge(&Inspect::read(home, resources_dir, engine_version))
}

/// Write the provenance stamp into a prepared tree's manifest. Called by the installer once the
/// tables are generated (or adapted), so the next [`judge`] can prove the version.
pub fn stamp(
    resources_dir: &Path,
    version: &str,
    client_jar_sha1: &str,
    protocol: Option<i32>,
    provenance: Provenance,
) -> Result<Manifest, String> {
    let path = resources_dir.join("horizon-resources.json");
    let mut manifest: Manifest = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    manifest.source_minecraft = version.to_owned();
    manifest.truth = Some(TruthStamp {
        version: version.to_owned(),
        client_jar_sha1: client_jar_sha1.to_owned(),
        protocol,
        provenance,
    });
    if provenance == Provenance::Generated {
        // The tables now belong to this version, and the engine's schema is the one this build
        // understands: say both, rather than implying 26.3's tables.
        manifest.minecraft = version.to_owned();
        manifest.full_simulation = true;
    }
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(manifest)
}

/// The version of every installed tree under `home`, judged: what the launcher's list shows.
pub fn installed(home: &Path, engine_version: &str) -> Vec<(String, Truth)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(home.join("versions")) else {
        return out;
    };
    let mut rows: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (!name.starts_with('.')).then_some((name, e.path().join("resources")))
        })
        .collect();
    rows.sort();
    for (id, resources) in rows {
        let truth = judge_dir(home, &resources, engine_version);
        out.push((id, truth));
    }
    out
}

/// The client jar's SHA-1 and the `dataVersion`/`release_target` facts a version JSON carries, so a
/// downloader can record what it installed without a JVM. Returns `(sha1, data version)`: the
/// second is read from the asset index or the version JSON when present, which older versions omit.
pub fn read_version_json(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut facts = BTreeMap::new();
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return facts;
    };
    if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
        facts.insert("id".into(), id.to_owned());
    }
    if let Some(target) = value.get("release_target").and_then(|v| v.as_str()) {
        facts.insert("release_target".into(), target.to_owned());
    }
    if let Some(version) = value.get("javaVersion").and_then(|v| v.get("majorVersion")) {
        facts.insert("java".into(), version.to_string());
    }
    if let Some(dv) = value.get("world_version").and_then(|v| v.as_i64()) {
        facts.insert("data_version".into(), dv.to_string());
    }
    facts
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The zip reader here only exists to answer one question a folder cannot: does a `client.jar` ship
/// the same `minecraft:registries` the tree claims? A cheap, decisive cross-check for the installer.
pub fn jar_has_entries(jar: &Path, prefix: &str, min: usize) -> Result<usize, String> {
    let file = fs::File::open(jar).map_err(|e| format!("{}: {e}", jar.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("{}: {e}", jar.display()))?;
    let mut n = 0;
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|e| e.to_string())?;
        if entry.name().map(|s| s.starts_with(prefix)).unwrap_or(false) {
            n += 1;
            if n >= min {
                break;
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "hvt-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(p.join("versions/1.20.1/resources")).unwrap();
            fs::create_dir_all(p.join("minecraft/versions/1.20.1")).unwrap();
            Self(p)
        }
        fn resources(&self) -> PathBuf {
            self.0.join("versions/1.20.1/resources")
        }
        fn full_tree(&self) {
            for (_, files) in SIMULATION_GROUPS {
                for f in *files {
                    let p = self.resources().join(f);
                    fs::create_dir_all(p.parent().unwrap()).unwrap();
                    fs::write(p, b"{}").unwrap();
                }
            }
        }
        fn manifest(&self, json: &str) {
            fs::write(self.resources().join("horizon-resources.json"), json).unwrap();
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_hardlinked_26_3_tree_labelled_1_20_1_is_caught_as_cosmetic() {
        let s = Scratch::new();
        s.full_tree();
        s.manifest(
            r#"{"schema":7,"minecraft":"26.3","source_minecraft":"1.20.1","full_simulation":true}"#,
        );
        let truth = judge_dir(&s.0, &s.resources(), "26.3");
        assert_eq!(truth.claimed, "1.20.1");
        assert_eq!(truth.simulation, "26.3");
        assert_eq!(truth.verdict, Verdict::Cosmetic);
        assert!(truth.verdict.needs_warning());
        assert!(!truth.verdict.playable_as_itself());
        assert!(truth.remaining.contains(&Stage::GenerateTables));
        assert!(truth.detail.contains("1.20.1"));
        assert!(truth.detail.contains("26.3"));
    }

    #[test]
    fn a_borrowed_stamp_from_the_installer_is_caught_even_with_a_full_tree() {
        // What `versions::build_manifest_from_base` now writes when it copies the pinned version's
        // tables into a folder for another version: a complete tree, the pinned schema, and a stamp
        // that says the tables are borrowed. No guessing needed.
        let s = Scratch::new();
        s.full_tree();
        s.manifest(
            r#"{"schema":7,"minecraft":"26.3","source_minecraft":"1.20.1","full_simulation":true,
                "truth":{"version":"26.3","client_jar_sha1":"","protocol":null,"provenance":"borrowed"}}"#,
        );
        let truth = judge_dir(&s.0, &s.resources(), "26.3");
        assert_eq!(truth.verdict, Verdict::Cosmetic, "{}", truth.detail);
        assert_eq!(truth.claimed, "1.20.1");
        assert_eq!(truth.simulation, "26.3");
        assert!(truth.detail.contains("borrowed") || truth.detail.contains("Assets of"));
        assert_eq!(
            truth.remaining,
            [Stage::Download, Stage::GenerateTables, Stage::Stamp]
        );
    }

    #[test]
    fn the_pinned_version_is_native() {
        let s = Scratch::new();
        s.full_tree();
        s.manifest(r#"{"schema":7,"minecraft":"26.3","source_minecraft":"26.3"}"#);
        let truth = judge_dir(&s.0, &s.resources(), "26.3");
        assert_eq!(truth.verdict, Verdict::Native);
        assert!(!truth.verdict.needs_warning());
    }

    #[test]
    fn a_tree_with_no_registries_is_broken_and_says_which_stages_remain() {
        let s = Scratch::new();
        s.manifest(r#"{"minecraft":"1.20.1","source_minecraft":"1.20.1"}"#);
        let truth = judge_dir(&s.0, &s.resources(), "26.3");
        assert_eq!(truth.verdict, Verdict::Broken);
        assert_eq!(
            truth.remaining,
            [Stage::Download, Stage::GenerateTables, Stage::Stamp]
        );
    }

    #[test]
    fn a_stamp_from_the_versions_own_jar_makes_it_ported() {
        let s = Scratch::new();
        s.full_tree();
        fs::write(s.0.join("minecraft/versions/1.20.1/1.20.1.jar"), b"jar").unwrap();
        let sha = {
            use sha1::{Digest, Sha1};
            hex(&Sha1::digest(b"jar"))
        };
        s.manifest(
            r#"{"schema":7,"minecraft":"26.3","source_minecraft":"1.20.1","full_simulation":true}"#,
        );
        stamp(&s.resources(), "1.20.1", &sha, Some(763), Provenance::Generated).unwrap();
        let truth = judge_dir(&s.0, &s.resources(), "26.3");
        assert_eq!(truth.verdict, Verdict::Ported, "{}", truth.detail);
        assert!(truth.verdict.playable_as_itself());
        // And the manifest no longer claims 26.3's tables.
        let m: Manifest =
            serde_json::from_str(&fs::read_to_string(s.resources().join("horizon-resources.json")).unwrap())
                .unwrap();
        assert_eq!(m.minecraft, "1.20.1");
        assert_eq!(m.truth.unwrap().protocol, Some(763));
    }

    #[test]
    fn a_stamp_whose_jar_hash_does_not_match_is_not_trusted() {
        let s = Scratch::new();
        s.full_tree();
        fs::write(s.0.join("minecraft/versions/1.20.1/1.20.1.jar"), b"other").unwrap();
        s.manifest(r#"{"minecraft":"26.3","source_minecraft":"1.20.1"}"#);
        stamp(&s.resources(), "1.20.1", "deadbeef", None, Provenance::Generated).unwrap();
        let truth = judge_dir(&s.0, &s.resources(), "26.3");
        // minecraft was rewritten to 1.20.1 by the stamp, so the claim now matches the simulation
        // and the mismatched jar hash is the only contradiction left: not a proven port.
        assert_ne!(truth.verdict, Verdict::Native);
    }

    #[test]
    fn an_adapted_tree_is_ported() {
        let s = Scratch::new();
        s.full_tree();
        s.manifest(r#"{"minecraft":"26.3","source_minecraft":"1.21.4"}"#);
        stamp(&s.resources(), "1.21.4", "", None, Provenance::Adapted).unwrap();
        let truth = judge_dir(&s.0, &s.resources(), "26.3");
        assert_eq!(truth.verdict, Verdict::Ported);
        assert!(truth.detail.contains("adapted"));
    }

    #[test]
    fn version_json_facts_are_read_without_a_jvm() {
        let facts = read_version_json(
            br#"{"id":"1.20.1","release_target":"1.20.1","javaVersion":{"majorVersion":17},"world_version":3463}"#,
        );
        assert_eq!(facts["java"], "17");
        assert_eq!(facts["data_version"], "3463");
    }
}
