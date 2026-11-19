//! Real version bindings for a native Rust reimplementation of Minecraft: Java Edition.
//!
//! ## The bug this module exists to kill
//!
//! Before the fail-closed launcher patch,
//! `crates/horizon-client/src/versions.rs::build_manifest_from_base` wrote, for *every*
//! selected version `X`:
//!
//! ```json
//! { "minecraft": "26.3", "source_minecraft": "X", "data_version": 5023,
//!   "full_simulation": true }
//! ```
//!
//! and `populate_from_simulation_base` hard-linked the 26.3 registries / items /
//! worldgen / datafix tables into `X`'s resource root. Only textures and sounds came
//! from `X`. The lobby therefore showed "Minecraft X" while the world still contained
//! 26.3 items, blocks and world generation. That is a **label swap, not a version**.
//!
//! ## The contract here
//!
//! A version is real only when a *version-specific simulation pack* exists: its own
//! `data_version`, its own protocol, its own `registries`, `items` and `worldgen` digests,
//! produced by a real per-version Rust translation pipeline (`tools/version_port.py`
//! plus parity harnesses) and bound by
//! `tools/prepare_version_pack.py --version X` from a verified prepared root. The
//! current Python tool is a host-side contract producer/verifier with a self-test; it
//! does **not** regenerate Rust sources, produce the parity report, or build an engine
//! binary. Nothing is inferred from a folder name, a SwiftUI row or `source_minecraft`.
//!
//! Fail-closed: an unregistered / mismatched / fake version is **refused**, never
//! silently activated. See `tools/prepare_version_pack.py` for the producer side.

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// The engine's pinned vanilla version (`crates/horizon-core/src/version.rs`).
pub const ENGINE_PIN: &str = "26.3";
pub const ENGINE_DATA_VERSION: u32 = 5023;
pub const ENGINE_PROTOCOL: i32 = 777;

/// Deterministic 64-bit FNV-1a digest.
///
/// This host-side contract crate uses FNV-1a as a deterministic content identifier;
/// `SimulationPack` also carries full SHA-256 artifact hashes. The producer
/// (`tools/prepare_version_pack.py`) fills the identifiers from the real artifact
/// bytes (or the `DigestVerifier` trait below can be used to plug in a real hasher).
/// No engine, Swift, winit, MetalFX or iOS-device caller is wired to this policy yet.
pub fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Full SHA-256 of a version artifact, lowercase hex. Used to bind a pack to the
/// bytes that were actually generated; FNV `digest()` alone is only an identifier.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let sum = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in sum {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// The on-disk `version-pack.json` written by `tools/prepare_version_pack.py`.
/// It is *not* a launcher label: it names real tables, a real Rust build and a
/// passed parity harness for that exact version.
#[derive(Clone, Debug, Deserialize)]
pub struct VersionPackJson {
    pub schema: u32,
    pub id: String,
    pub simulation_version: String,
    pub minecraft: String,
    pub data_version: u32,
    pub protocol: i32,
    pub engine_pin: String,
    pub engine_build_sha256: String,
    pub rust_sources_sha256: String,
    pub parity_harness_passed: bool,
    pub registries_digest: u64,
    pub items_digest: u64,
    pub worldgen_digest: u64,
    pub artifact_digest: u64,
    pub artifact_sha256: String,
    pub artifact_path: String,
    pub built_by: String,
}

/// What supplied a version's simulation tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulationKind {
    /// The engine's own pinned 26.3 code and tables. Genuinely native.
    NativePinned,
    /// A version-specific pack built from that version's own jars. Genuinely translated.
    VersionPack,
}

/// A verified, version-specific simulation pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationPack {
    /// The version this pack simulates, e.g. `"1.20.4"`.
    pub simulation_version: String,
    /// That version's own world data version (NOT 26.3's 5023).
    pub data_version: u32,
    /// That version's own wire protocol number.
    pub protocol: i32,
    pub registries_digest: u64,
    pub items_digest: u64,
    pub worldgen_digest: u64,
    /// Digest of the prepared artifact (`horizon-data.bin` / version jar) the pack was built from.
    pub artifact_digest: u64,
    /// Provenance: must name a real generator, never a launcher label.
    pub built_by: String,
}

impl SimulationPack {
    /// The pinned 26.3 engine, the only version that needs no translation.
    pub fn for_pinned() -> Self {
        Self {
            simulation_version: ENGINE_PIN.into(),
            data_version: ENGINE_DATA_VERSION,
            protocol: ENGINE_PROTOCOL,
            registries_digest: digest(b"26.3/registries"),
            items_digest: digest(b"26.3/items"),
            worldgen_digest: digest(b"26.3/worldgen"),
            artifact_digest: digest(b"26.3/artifact"),
            built_by: "horizon-engine-pinned-26.3".into(),
        }
    }

    /// A version-specific pack. `parts` are the real per-version prepared tables.
    /// The caller supplies digests computed from those bytes; no values are invented.
    pub fn from_parts(
        simulation_version: impl Into<String>,
        data_version: u32,
        protocol: i32,
        parts: &VersionPackParts<'_>,
    ) -> Self {
        let id: String = simulation_version.into();
        Self {
            simulation_version: id.clone(),
            data_version,
            protocol,
            registries_digest: digest(parts.registries),
            items_digest: digest(parts.items),
            worldgen_digest: digest(parts.worldgen),
            artifact_digest: digest(parts.artifact),
            built_by: format!("tools/prepare_version_pack.py --version {id}"),
        }
    }

    /// Read a real `version-pack.json` and bind it to the artifact bytes on disk.
    /// This is the only way a non-pinned version may become playable.
    pub fn from_pack_json(path: &Path, artifact: &[u8]) -> Result<(String, Self), VersionError> {
        let bytes = fs::read(path).map_err(|e| VersionError::PackFile(e.to_string()))?;
        let j: VersionPackJson =
            serde_json::from_slice(&bytes).map_err(|e| VersionError::PackSchema(e.to_string()))?;
        if j.schema != 1 {
            return Err(VersionError::PackSchema(format!(
                "unknown schema {}",
                j.schema
            )));
        }
        if j.id != j.simulation_version || j.id != j.minecraft {
            return Err(VersionError::PackSchema(
                "id, simulation_version and minecraft must name the same version".into(),
            ));
        }
        if j.engine_pin != ENGINE_PIN {
            return Err(VersionError::PackSchema(format!(
                "pack was built against engine {}, this runtime pins {ENGINE_PIN}",
                j.engine_pin
            )));
        }
        let verified_hash = |h: &str| {
            h.len() == 64
                && h.bytes().all(|b| b.is_ascii_hexdigit())
                && h.bytes().any(|b| b != b'0')
        };
        if !verified_hash(&j.engine_build_sha256) || !verified_hash(&j.rust_sources_sha256) {
            return Err(VersionError::PackNotVerified(
                "missing or all-zero engine/Rust source SHA-256".into(),
            ));
        }
        if !j.parity_harness_passed {
            return Err(VersionError::PackNotVerified(
                "parity harness did not pass for this version".into(),
            ));
        }
        if !j.built_by.contains("tools/prepare_version_pack.py") {
            return Err(VersionError::PackNotVerified(
                "version pack has no verified version-pack producer provenance".into(),
            ));
        }
        let actual_sha = sha256_hex(artifact);
        if actual_sha != j.artifact_sha256.to_ascii_lowercase() {
            return Err(VersionError::ArtifactSha256Mismatch {
                expected: j.artifact_sha256,
                actual: actual_sha,
            });
        }
        if digest(artifact) != j.artifact_digest {
            return Err(VersionError::ArtifactMismatch {
                expected: j.artifact_digest,
                actual: digest(artifact),
            });
        }
        let pack = Self {
            simulation_version: j.simulation_version.clone(),
            data_version: j.data_version,
            protocol: j.protocol,
            registries_digest: j.registries_digest,
            items_digest: j.items_digest,
            worldgen_digest: j.worldgen_digest,
            artifact_digest: j.artifact_digest,
            built_by: j.built_by.clone(),
        };
        pack.verify(&j.id)?;
        Ok((j.id, pack))
    }

    /// Reject every self-inconsistency before a version can be registered.
    pub fn verify(&self, id: &str) -> Result<(), VersionError> {
        if id.trim().is_empty() {
            return Err(VersionError::EmptyId);
        }
        if self.simulation_version.trim() != id.trim() {
            return Err(VersionError::SimulationVersionMismatch {
                label: id.into(),
                simulation: self.simulation_version.clone(),
            });
        }
        let verified_producer = self.built_by.contains("tools/prepare_version_pack.py")
            || self.built_by.starts_with("horizon-engine-pinned-");
        if self.built_by.trim().is_empty() || !verified_producer {
            return Err(VersionError::MissingProvenance);
        }
        if self.data_version == 0 || self.protocol <= 0 {
            return Err(VersionError::MissingFacts);
        }
        // The 26.3 tables under another version's label is *the* fake this module refuses.
        if id != ENGINE_PIN && self.data_version == ENGINE_DATA_VERSION {
            return Err(VersionError::PinnedTablesUnderForeignLabel { label: id.into() });
        }
        if id != ENGINE_PIN
            && (self.registries_digest == Self::for_pinned().registries_digest
                || self.items_digest == Self::for_pinned().items_digest
                || self.worldgen_digest == Self::for_pinned().worldgen_digest)
        {
            return Err(VersionError::PinnedTablesUnderForeignLabel { label: id.into() });
        }
        Ok(())
    }
}

/// The real per-version prepared bytes a pack is computed from.
pub struct VersionPackParts<'a> {
    pub registries: &'a [u8],
    pub items: &'a [u8],
    pub worldgen: &'a [u8],
    pub artifact: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionError {
    EmptyId,
    PackFile(String),
    PackSchema(String),
    PackNotVerified(String),
    ArtifactSha256Mismatch { expected: String, actual: String },
    MissingProvenance,
    MissingFacts,
    SimulationVersionMismatch { label: String, simulation: String },
    PinnedTablesUnderForeignLabel { label: String },
    ArtifactMismatch { expected: u64, actual: u64 },
    NotRegistered(String),
    FakeManifest { label: String, minecraft: String },
}

impl std::fmt::Display for VersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId => write!(f, "empty version id"),
            Self::PackFile(e) => write!(f, "could not read version-pack.json: {e}"),
            Self::PackSchema(e) => write!(f, "invalid version-pack.json: {e}"),
            Self::PackNotVerified(e) => write!(
                f,
                "version pack is not verified by a per-version Rust build and parity harness: {e}"
            ),
            Self::ArtifactSha256Mismatch { expected, actual } => write!(
                f,
                "version artifact SHA-256 mismatch: expected {expected}, got {actual}"
            ),
            Self::MissingProvenance => write!(
                f,
                "no verified version-pack provenance: a version pack must be built from that version's own jars"
            ),
            Self::MissingFacts => write!(f, "missing data_version / protocol facts"),
            Self::SimulationVersionMismatch { label, simulation } => write!(
                f,
                "lobby label {label:?} disagrees with simulation_version {simulation:?}"
            ),
            Self::PinnedTablesUnderForeignLabel { label } => write!(
                f,
                "Minecraft {label} still carries 26.3 simulation tables; this is a label swap, not a version. Build a version pack with tools/prepare_version_pack.py --version {label}"
            ),
            Self::ArtifactMismatch { expected, actual } => write!(
                f,
                "version artifact digest mismatch: expected {expected:#018x}, got {actual:#018x}"
            ),
            Self::NotRegistered(id) => write!(
                f,
                "Minecraft {id} has no verified version pack; refusing to activate a fake version"
            ),
            Self::FakeManifest { label, minecraft } => write!(
                f,
                "manifest claims label {label:?} but its engine tables are {minecraft:?}"
            ),
        }
    }
}
impl std::error::Error for VersionError {}

/// Detect the exact fake produced by `build_manifest_from_base` in the upstream tree.
///
/// `label` is `source_minecraft` (what the lobby shows), `minecraft` is the engine-schema
/// field, `data_version` is the world format the tables were prepared for.
pub fn detect_fake_manifest(
    label: &str,
    minecraft: &str,
    data_version: u32,
    simulation_version: Option<&str>,
) -> Result<(), VersionError> {
    if label.trim() != minecraft.trim() {
        return Err(VersionError::FakeManifest {
            label: label.into(),
            minecraft: minecraft.into(),
        });
    }
    if let Some(sim) = simulation_version
        && sim.trim() != label.trim()
    {
        return Err(VersionError::SimulationVersionMismatch {
            label: label.into(),
            simulation: sim.into(),
        });
    }
    if label != ENGINE_PIN && data_version == ENGINE_DATA_VERSION {
        return Err(VersionError::PinnedTablesUnderForeignLabel {
            label: label.into(),
        });
    }
    Ok(())
}

/// One registered, verified binding between a lobby label and real engine data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionBinding {
    pub id: String,
    pub pack: SimulationPack,
    pub resource_root: PathBuf,
    pub kind: SimulationKind,
}

impl VersionBinding {
    /// The digest the world's item registry must use while this version is active.
    pub fn items_digest(&self) -> u64 {
        self.pack.items_digest
    }
    pub fn protocol(&self) -> i32 {
        self.pack.protocol
    }
}

/// What the launcher is allowed to show / run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activation {
    Native {
        id: String,
    },
    Translated {
        id: String,
        protocol: i32,
        items_digest: u64,
    },
    Refused {
        id: String,
        reason: String,
    },
}
impl Activation {
    pub fn is_playable(&self) -> bool {
        !matches!(self, Activation::Refused { .. })
    }
}

/// The version runtime: single source of truth for "which Minecraft is actually running".
#[derive(Clone, Debug, Default)]
pub struct VersionRuntime {
    bindings: BTreeMap<String, VersionBinding>,
    active: Option<String>,
    pub refusals: BTreeMap<String, String>,
}

impl VersionRuntime {
    pub fn new() -> Self {
        let mut s = Self::default();
        let pinned = SimulationPack::for_pinned();
        // 26.3 is the engine itself; its artifact is the pinned engine, already verified by CI.
        s.bindings.insert(
            ENGINE_PIN.into(),
            VersionBinding {
                id: ENGINE_PIN.into(),
                pack: pinned.clone(),
                resource_root: PathBuf::from("versions/26.3/resources"),
                kind: SimulationKind::NativePinned,
            },
        );
        s.active = Some(ENGINE_PIN.into());
        s
    }

    /// Register a version pack. `artifact` is the prepared artifact the digests were made from:
    /// a launcher cannot register a version by writing a JSON label alone.
    pub fn register(
        &mut self,
        id: &str,
        pack: SimulationPack,
        root: impl AsRef<Path>,
        artifact: &[u8],
    ) -> Result<(), VersionError> {
        pack.verify(id)?;
        let actual = digest(artifact);
        if actual != pack.artifact_digest {
            return Err(VersionError::ArtifactMismatch {
                expected: pack.artifact_digest,
                actual,
            });
        }
        let kind = if id == ENGINE_PIN {
            SimulationKind::NativePinned
        } else {
            SimulationKind::VersionPack
        };
        self.bindings.insert(
            id.to_owned(),
            VersionBinding {
                id: id.to_owned(),
                pack,
                resource_root: root.as_ref().to_owned(),
                kind,
            },
        );
        self.refusals.remove(id);
        Ok(())
    }

    /// Register the version described by a verified `version-pack.json`.
    /// `artifact_path` is the prepared artifact whose SHA-256 was recorded by the tool.
    pub fn register_pack_file(
        &mut self,
        pack_path: &Path,
        artifact_path: &Path,
    ) -> Result<(), VersionError> {
        let artifact = fs::read(artifact_path)
            .map_err(|e| VersionError::PackFile(format!("{}: {e}", artifact_path.display())))?;
        let (id, pack) = SimulationPack::from_pack_json(pack_path, &artifact)?;
        let root = pack_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_owned();
        self.register(&id, pack, root, &artifact)
    }

    /// Activate a version. Fail-closed: no binding → refusal, never a 26.3 fallback.
    pub fn activate(&mut self, id: &str) -> Activation {
        match self.bindings.get(id) {
            None => {
                let reason = VersionError::NotRegistered(id.into()).to_string();
                self.refusals.insert(id.into(), reason.clone());
                Activation::Refused {
                    id: id.into(),
                    reason,
                }
            }
            Some(binding) => {
                self.active = Some(id.to_owned());
                match binding.kind {
                    SimulationKind::NativePinned => Activation::Native { id: id.into() },
                    SimulationKind::VersionPack => Activation::Translated {
                        id: id.into(),
                        protocol: binding.protocol(),
                        items_digest: binding.items_digest(),
                    },
                }
            }
        }
    }

    pub fn active(&self) -> Option<&VersionBinding> {
        self.active.as_ref().and_then(|id| self.bindings.get(id))
    }
    /// The label the lobby shows must come from the *binding*, not from a profile string.
    pub fn lobby_label(&self) -> String {
        self.active()
            .map(|b| b.id.clone())
            .unwrap_or_else(|| "no verified version".into())
    }
    pub fn active_items_digest(&self) -> Option<u64> {
        self.active().map(VersionBinding::items_digest)
    }
    pub fn binding(&self, id: &str) -> Option<&VersionBinding> {
        self.bindings.get(id)
    }
    pub fn registered(&self) -> impl Iterator<Item = &String> {
        self.bindings.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_pack(id: &str, dv: u32, protocol: i32) -> (SimulationPack, Vec<u8>) {
        let artifact = format!("{id}/client+server jars").into_bytes();
        let registries = format!("{id}/registries");
        let items = format!("{id}/items");
        let worldgen = format!("{id}/worldgen");
        let parts = VersionPackParts {
            registries: registries.as_bytes(),
            items: items.as_bytes(),
            worldgen: worldgen.as_bytes(),
            artifact: &artifact,
        };
        (
            SimulationPack::from_parts(id, dv, protocol, &parts),
            artifact,
        )
    }

    #[test]
    fn the_upstream_fake_manifest_is_detected() {
        // Exactly what build_manifest_from_base writes for a 1.20.4 install.
        let err = detect_fake_manifest("1.20.4", "26.3", ENGINE_DATA_VERSION, None).unwrap_err();
        assert_eq!(
            err,
            VersionError::FakeManifest {
                label: "1.20.4".into(),
                minecraft: "26.3".into()
            }
        );
        // A relabelled manifest with a mismatched simulation_version is refused too.
        assert_eq!(
            detect_fake_manifest("1.20.4", "1.20.4", ENGINE_DATA_VERSION, Some("26.3"))
                .unwrap_err(),
            VersionError::SimulationVersionMismatch {
                label: "1.20.4".into(),
                simulation: "26.3".into()
            }
        );
    }

    #[test]
    fn pinned_tables_under_a_foreign_label_are_refused() {
        let mut pack = SimulationPack::for_pinned();
        pack.simulation_version = "1.20.4".into();
        assert_eq!(
            pack.verify("1.20.4").unwrap_err(),
            VersionError::PinnedTablesUnderForeignLabel {
                label: "1.20.4".into()
            }
        );
    }

    #[test]
    fn a_real_version_pack_changes_the_item_registry() {
        let mut rt = VersionRuntime::new();
        let pinned_items = rt.active_items_digest().unwrap();
        let (pack, artifact) = real_pack("1.20.4", 3700, 765);
        rt.register("1.20.4", pack, "versions/1.20.4/resources", &artifact)
            .unwrap();
        let activation = rt.activate("1.20.4");
        assert_eq!(
            activation,
            Activation::Translated {
                id: "1.20.4".into(),
                protocol: 765,
                items_digest: digest(b"1.20.4/items"),
            }
        );
        assert_eq!(rt.lobby_label(), "1.20.4");
        assert_ne!(rt.active_items_digest().unwrap(), pinned_items);
        assert_eq!(rt.active().unwrap().protocol(), 765);
    }

    #[test]
    fn an_unregistered_version_is_refused_not_silently_faked() {
        let mut rt = VersionRuntime::new();
        let a = rt.activate("1.12.2");
        assert!(!a.is_playable());
        assert_eq!(
            rt.lobby_label(),
            "26.3",
            "a refusal must not move the active version"
        );
        assert!(rt.refusals["1.12.2"].contains("no verified version pack"));
    }

    #[test]
    fn artifact_digest_mismatch_is_refused() {
        let mut rt = VersionRuntime::new();
        let (pack, _) = real_pack("1.20.2", 3578, 764);
        let err = rt
            .register("1.20.2", pack, "versions/1.20.2/resources", b"tampered")
            .unwrap_err();
        assert!(matches!(err, VersionError::ArtifactMismatch { .. }));
        assert!(rt.binding("1.20.2").is_none());
    }

    #[test]
    fn pinned_26_3_is_native_and_its_manifest_is_not_fake() {
        assert!(detect_fake_manifest("26.3", "26.3", ENGINE_DATA_VERSION, Some("26.3")).is_ok());
        let mut rt = VersionRuntime::new();
        assert_eq!(
            rt.activate("26.3"),
            Activation::Native { id: "26.3".into() }
        );
    }

    #[test]
    fn a_pack_without_provenance_or_with_zero_facts_is_refused() {
        let (mut pack, _) = real_pack("1.21.1", 3953, 767);
        pack.built_by = "some launcher UI".into();
        assert_eq!(
            pack.verify("1.21.1").unwrap_err(),
            VersionError::MissingProvenance
        );
        let (mut pack, _) = real_pack("1.21.1", 3953, 767);
        pack.data_version = 0;
        assert_eq!(
            pack.verify("1.21.1").unwrap_err(),
            VersionError::MissingFacts
        );
    }

    #[test]
    fn a_version_pack_file_is_only_registered_with_a_verified_rust_build() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("hios-pack-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let artifact = b"1.20.4/prepared-horizon-data.bin".to_vec();
        let artifact_path = dir.join("horizon-data.bin");
        fs::write(&artifact_path, &artifact).unwrap();
        let engine = "a".repeat(64);
        let sources = "b".repeat(64);
        let pack = serde_json::json!({
            "schema": 1,
            "id": "1.20.4",
            "simulation_version": "1.20.4",
            "minecraft": "1.20.4",
            "data_version": 3700,
            "protocol": 765,
            "engine_pin": ENGINE_PIN,
            "engine_build_sha256": engine,
            "rust_sources_sha256": sources,
            "parity_harness_passed": true,
            "registries_digest": digest(b"1.20.4/registries"),
            "items_digest": digest(b"1.20.4/items"),
            "worldgen_digest": digest(b"1.20.4/worldgen"),
            "artifact_digest": digest(&artifact),
            "artifact_sha256": sha256_hex(&artifact),
            "artifact_path": "horizon-data.bin",
            "built_by": "tools/prepare_version_pack.py --version 1.20.4"
        });
        let pack_path = dir.join("version-pack.json");
        fs::write(&pack_path, serde_json::to_vec_pretty(&pack).unwrap()).unwrap();
        let mut rt = VersionRuntime::new();
        rt.register_pack_file(&pack_path, &artifact_path).unwrap();
        assert_eq!(
            rt.activate("1.20.4"),
            Activation::Translated {
                id: "1.20.4".into(),
                protocol: 765,
                items_digest: digest(b"1.20.4/items")
            }
        );
        assert_ne!(
            rt.active_items_digest().unwrap(),
            SimulationPack::for_pinned().items_digest
        );

        // A tampered artifact must not activate a version.
        fs::write(&artifact_path, b"tampered").unwrap();
        let mut rt2 = VersionRuntime::new();
        assert!(matches!(
            rt2.register_pack_file(&pack_path, &artifact_path),
            Err(VersionError::ArtifactSha256Mismatch { .. })
                | Err(VersionError::ArtifactMismatch { .. })
        ));
        assert!(!rt2.activate("1.20.4").is_playable());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pack_without_a_passed_parity_harness_is_refused() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("hios-bad-pack-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let artifact = b"x".to_vec();
        let artifact_path = dir.join("artifact.bin");
        fs::write(&artifact_path, &artifact).unwrap();
        let pack = serde_json::json!({
            "schema": 1, "id": "1.21.1", "simulation_version": "1.21.1", "minecraft": "1.21.1",
            "data_version": 3953, "protocol": 767, "engine_pin": ENGINE_PIN,
            "engine_build_sha256": "c".repeat(64), "rust_sources_sha256": "d".repeat(64),
            "parity_harness_passed": false,
            "registries_digest": 1u64, "items_digest": 2u64, "worldgen_digest": 3u64,
            "artifact_digest": digest(&artifact), "artifact_sha256": sha256_hex(&artifact),
            "artifact_path": "artifact.bin", "built_by": "tools/prepare_version_pack.py --version 1.21.1"
        });
        let pack_path = dir.join("version-pack.json");
        fs::write(&pack_path, serde_json::to_vec_pretty(&pack).unwrap()).unwrap();
        let mut rt = VersionRuntime::new();
        assert!(matches!(
            rt.register_pack_file(&pack_path, &artifact_path),
            Err(VersionError::PackNotVerified(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
