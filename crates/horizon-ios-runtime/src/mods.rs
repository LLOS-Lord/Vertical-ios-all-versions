//! Mod activation and Java `.jar` porting compatibility.
//!
//! ## Why "installed mods do nothing" in the upstream tree
//!
//! 1. `docs/mods.md` states it plainly: Fabric / NeoForge jars **do not run**. Their code
//!    patches Minecraft's Java classes; Horizon is a Rust reimplementation with no JVM.
//!    `launcher_mods.rs::install_from_modrinth` only carries a jar's `data/` and `assets/`
//!    into `horizon-mods/<id>/` (a content mod), then writes `horizon-mods.txt`.
//! 2. A content mod that adds registries / items / worldgen only loads **when the game
//!    starts** (`game/mods.rs::switch_plan`), not on a launcher toggle.
//!    `saves_control.rs::handle_launcher_mods` never tells a running engine to rebuild
//!    its content root, so installing from the SwiftUI launcher while the engine is at
//!    its title leaves the world unchanged.
//! 3. Native Rust `.hnmod` mods need a *compile-time* build; an installed but unbuilt
//!    `.hnmod` shows `NOT_BUILT`. WebAssembly code mods no longer run at all.
//!
//! This module makes those facts explicit and enforceable: every mod gets a verdict
//! (`Activate` with a reload class, or `Rejected` with a reason), a version gate, a
//! capability/hook bridge, deterministic ordering, clash detection and a content
//! revision counter the engine can poll to trigger a real reload/restart.

use crate::version::{VersionBinding, VersionRuntime, digest};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub const API_LEVEL: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModOrigin {
    /// Signed `.hmod` content package from Horizon's registry.
    HorizonContent,
    /// A Fabric/NeoForge/Forge/Quilt `.jar` whose data+assets were ported, no code.
    JavaJarContent,
    /// A jar with `.class` entrypoints / Mixins. Needs a JVM or a bounded bytecode bridge.
    JavaCode,
    /// A `.hnmod` Rust trait mod compiled into the game binary.
    NativeRust,
    /// Legacy WebAssembly mods, which the engine no longer executes.
    WasmLegacy,
}

impl ModOrigin {
    pub fn is_code(self) -> bool {
        matches!(self, Self::JavaCode | Self::NativeRust | Self::WasmLegacy)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::HorizonContent => "horizon-content",
            Self::JavaJarContent => "java-jar-content-ported",
            Self::JavaCode => "java-code",
            Self::NativeRust => "native-rust",
            Self::WasmLegacy => "wasm-legacy",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    Assets,
    Data,
    Registries,
    Worldgen,
    EntityTypes,
    MenuTypes,
    NativeHook,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModDescriptor {
    pub id: String,
    pub version: String,
    pub origin: ModOrigin,
    /// The MC version the mod was authored for; `"*"` means version-independent.
    pub engine_version: String,
    pub api_level: u32,
    pub capabilities: Vec<Capability>,
    /// Native hook names a code mod needs (`register_goals`, `right_click_block`, ...).
    pub hooks: Vec<String>,
    pub namespaces: Vec<String>,
    pub content_digest: u64,
}

impl ModDescriptor {
    pub fn content(id: &str, engine_version: &str, caps: &[Capability], bytes: &[u8]) -> Self {
        Self {
            id: id.into(),
            version: "1.0.0".into(),
            origin: ModOrigin::HorizonContent,
            engine_version: engine_version.into(),
            api_level: API_LEVEL,
            capabilities: caps.to_vec(),
            hooks: Vec::new(),
            namespaces: vec![id.into()],
            content_digest: digest(bytes),
        }
    }
    fn changes_registries(&self) -> bool {
        self.capabilities.iter().any(|c| {
            matches!(
                c,
                Capability::Registries
                    | Capability::Worldgen
                    | Capability::EntityTypes
                    | Capability::MenuTypes
            )
        })
    }
}

/// How a mod's activation reaches the running world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reload {
    /// Assets/data only: rebuild the content root in place, no restart.
    Hot,
    /// New blocks/items/entity types/worldgen: the world's registries must be frozen at
    /// startup, so the engine must be restarted.
    Restart,
    /// Wasm/native code: only a build that contains the mod can run it.
    BuildRequired,
}
impl Reload {
    pub fn needs_engine_restart(self) -> bool {
        matches!(self, Self::Restart | Self::BuildRequired)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModVerdict {
    Activate { reload: Reload },
    Rejected { reason: String },
}
impl ModVerdict {
    pub fn accepted(&self) -> bool {
        matches!(self, Self::Activate { .. })
    }
}

/// The bridge a code mod needs. A no-JVM engine can only run hooks it exposes itself;
/// arbitrary Java bytecode is never guessed at.
pub trait HookBridge: Send + Sync + std::fmt::Debug {
    fn supports(&self, hook: &str) -> bool;
    fn describe(&self) -> &'static str;
}

#[derive(Clone, Debug, Default)]
pub struct NativeHookBridge {
    supported: BTreeSet<String>,
}
impl NativeHookBridge {
    pub fn new(hooks: &[&str]) -> Self {
        Self {
            supported: hooks.iter().map(|h| (*h).to_owned()).collect(),
        }
    }
}
impl HookBridge for NativeHookBridge {
    fn supports(&self, hook: &str) -> bool {
        self.supported.contains(hook)
    }
    fn describe(&self) -> &'static str {
        "compile-time Rust trait hooks"
    }
}

/// No code bridge at all: the shipping iOS app.
#[derive(Clone, Debug, Default)]
pub struct NoCodeBridge;
impl HookBridge for NoCodeBridge {
    fn supports(&self, _: &str) -> bool {
        false
    }
    fn describe(&self) -> &'static str {
        "no JVM, no code-mod runtime"
    }
}

#[derive(Clone, Debug)]
pub struct ModRuntime {
    pub engine_version: String,
    pub api_level: u32,
    bridge: Arc<dyn HookBridge>,
    active: BTreeMap<String, ModDescriptor>,
    order: Vec<String>,
    pub rejected: BTreeMap<String, String>,
    /// Increments on every accepted activation/removal. The engine polls this to rebuild
    /// its content root instead of trusting a launcher "installed & enabled" toast.
    pub revision: u64,
}

impl ModRuntime {
    pub fn new(engine_version: impl Into<String>) -> Self {
        Self {
            engine_version: engine_version.into(),
            api_level: API_LEVEL,
            bridge: Arc::new(NoCodeBridge),
            active: BTreeMap::new(),
            order: Vec::new(),
            rejected: BTreeMap::new(),
            revision: 0,
        }
    }
    pub fn with_bridge(engine_version: impl Into<String>, bridge: Box<dyn HookBridge>) -> Self {
        let mut s = Self::new(engine_version);
        s.bridge = Arc::from(bridge);
        s
    }

    /// Compatibility gate: a mod authored for version X may only be active when the
    /// *real* binding X (or a wildcard) is running. This is what makes ports stop
    /// silently bleeding 26.3 content into a different version.
    pub fn version_gate(&self, m: &ModDescriptor, active: &VersionRuntime) -> Result<(), String> {
        if m.engine_version == "*" {
            return Ok(());
        }
        if m.engine_version == active.lobby_label() {
            return Ok(());
        }
        Err(format!(
            "mod {} targets Minecraft {}, but the active verified version is {}",
            m.id,
            m.engine_version,
            active.lobby_label()
        ))
    }

    pub fn submit(&mut self, m: ModDescriptor, active: &VersionRuntime) -> ModVerdict {
        let verdict = self.classify(&m, active);
        match &verdict {
            ModVerdict::Activate { .. } => {
                self.rejected.remove(&m.id);
                if !self.active.contains_key(&m.id) {
                    self.order.push(m.id.clone());
                }
                self.active.insert(m.id.clone(), m);
                self.revision = self.revision.wrapping_add(1);
            }
            ModVerdict::Rejected { reason } => {
                self.rejected.insert(m.id.clone(), reason.clone());
                self.active.remove(&m.id);
                self.order.retain(|id| id != &m.id);
                self.revision = self.revision.wrapping_add(1);
            }
        }
        verdict
    }

    fn classify(&self, m: &ModDescriptor, active: &VersionRuntime) -> ModVerdict {
        let reject = |reason: String| ModVerdict::Rejected { reason };
        if m.id.trim().is_empty() {
            return reject("empty mod id".into());
        }
        if m.api_level > self.api_level {
            return reject(format!(
                "mod api level {} is newer than the engine's {}",
                m.api_level, self.api_level
            ));
        }
        if let Err(e) = self.version_gate(m, active) {
            return reject(e);
        }
        match m.origin {
            ModOrigin::WasmLegacy => {
                return reject("WebAssembly code mods no longer run in Horizon".into());
            }
            ModOrigin::JavaCode => {
                let missing: Vec<&str> = m
                    .hooks
                    .iter()
                    .map(String::as_str)
                    .filter(|h| !self.bridge.supports(h))
                    .collect();
                return if missing.is_empty() && !m.hooks.is_empty() {
                    ModVerdict::Activate {
                        reload: Reload::BuildRequired,
                    }
                } else {
                    reject(format!(
                        "Java bytecode cannot run: {} (unsupported hooks: {:?}). Only this jar's data/assets can be ported.",
                        self.bridge.describe(),
                        missing
                    ))
                };
            }
            ModOrigin::NativeRust => {
                let missing: Vec<&str> = m
                    .hooks
                    .iter()
                    .map(String::as_str)
                    .filter(|h| !self.bridge.supports(h))
                    .collect();
                if !missing.is_empty() {
                    return reject(format!(
                        "native mod needs hooks this build does not expose: {missing:?}"
                    ));
                }
                return ModVerdict::Activate {
                    reload: Reload::BuildRequired,
                };
            }
            ModOrigin::HorizonContent | ModOrigin::JavaJarContent => {}
        }
        // Namespace clash: two enabled mods may never own the same namespace.
        for (other_id, other) in &self.active {
            if other_id == &m.id {
                continue;
            }
            let mine: BTreeSet<&str> = m.namespaces.iter().map(String::as_str).collect();
            let theirs: BTreeSet<&str> = other.namespaces.iter().map(String::as_str).collect();
            if !mine.is_disjoint(&theirs) {
                return reject(format!(
                    "namespace clash with already-active mod {other_id}; the first one wins"
                ));
            }
        }
        let reload = if m.changes_registries() {
            Reload::Restart
        } else {
            Reload::Hot
        };
        ModVerdict::Activate { reload }
    }

    pub fn remove(&mut self, id: &str) {
        if self.active.remove(id).is_some() {
            self.order.retain(|x| x != id);
            self.revision = self.revision.wrapping_add(1);
        }
        self.rejected.remove(id);
    }

    /// Deterministic activation order (the engine's `catalog::read_at` requires this).
    pub fn order(&self) -> &[String] {
        &self.order
    }
    pub fn active(&self) -> impl Iterator<Item = &ModDescriptor> {
        self.order.iter().filter_map(|id| self.active.get(id))
    }
    pub fn is_active(&self, id: &str) -> bool {
        self.active.contains_key(id)
    }
    /// Digest of everything actually loaded: the engine compares it to its content root.
    pub fn content_digest(&self) -> u64 {
        let mut bytes = Vec::new();
        for m in self.active() {
            bytes.extend_from_slice(m.id.as_bytes());
            bytes.extend_from_slice(&m.content_digest.to_be_bytes());
        }
        digest(&bytes)
    }
    pub fn restart_required(&self) -> bool {
        self.active()
            .any(|m| m.origin == ModOrigin::NativeRust || m.changes_registries())
    }
    /// Mods that a real version binding may load, in activation order.
    pub fn compatible_with(&self, binding: &VersionBinding) -> Vec<String> {
        self.active()
            .filter(|m| m.engine_version == "*" || m.engine_version == binding.id)
            .map(|m| m.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::{ENGINE_DATA_VERSION, ENGINE_PROTOCOL, SimulationPack, VersionPackParts};

    fn runtime_with_version(id: &str, dv: u32, proto: i32) -> VersionRuntime {
        let mut v = VersionRuntime::new();
        if id != crate::version::ENGINE_PIN {
            let artifact = format!("{id}/jars").into_bytes();
            let registries = format!("{id}/registries");
            let items = format!("{id}/items");
            let worldgen = format!("{id}/worldgen");
            let parts = VersionPackParts {
                registries: registries.as_bytes(),
                items: items.as_bytes(),
                worldgen: worldgen.as_bytes(),
                artifact: &artifact,
            };
            let pack = SimulationPack::from_parts(id, dv, proto, &parts);
            v.register(id, pack, format!("versions/{id}/resources"), &artifact)
                .unwrap();
            assert!(v.activate(id).is_playable());
        }
        v
    }

    #[test]
    fn java_code_mods_are_rejected_with_an_actionable_reason_not_silently_installed() {
        let mut mods = ModRuntime::new("26.3");
        let versions = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        let mut m = ModDescriptor::content("sodium", "26.3", &[Capability::Assets], b"assets");
        m.origin = ModOrigin::JavaCode;
        m.hooks = vec!["onInitialize".into(), "mixin:WorldRenderer".into()];
        let v = mods.submit(m, &versions);
        assert!(!v.accepted());
        assert!(mods.rejected["sodium"].contains("Java bytecode cannot run"));
        assert!(!mods.is_active("sodium"));
    }

    #[test]
    fn a_ported_jars_data_and_assets_do_activate() {
        let mut mods = ModRuntime::new("26.3");
        let versions = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        let mut m = ModDescriptor::content(
            "biomes_content",
            "26.3",
            &[Capability::Assets, Capability::Data],
            b"textures+recipes",
        );
        m.origin = ModOrigin::JavaJarContent;
        let v = mods.submit(m, &versions);
        assert_eq!(
            v,
            ModVerdict::Activate {
                reload: Reload::Hot
            }
        );
        assert!(mods.is_active("biomes_content"));
    }

    #[test]
    fn a_registry_mod_requires_a_real_engine_restart() {
        let mut mods = ModRuntime::new("26.3");
        let versions = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        let v = mods.submit(
            ModDescriptor::content(
                "copper_blocks",
                "26.3",
                &[Capability::Registries],
                b"blocks",
            ),
            &versions,
        );
        assert_eq!(
            v,
            ModVerdict::Activate {
                reload: Reload::Restart
            }
        );
        assert!(mods.restart_required());
    }

    #[test]
    fn a_mod_for_another_version_is_refused_until_that_real_version_is_active() {
        let mut mods = ModRuntime::new("26.3");
        let pinned = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        let m = ModDescriptor::content("old_mod", "1.20.4", &[Capability::Data], b"recipes");
        assert!(!mods.submit(m.clone(), &pinned).accepted());
        assert!(mods.rejected["old_mod"].contains("targets Minecraft 1.20.4"));
        // Now really switch to 1.20.4: the same mod becomes loadable.
        let real = runtime_with_version("1.20.4", 3700, 765);
        assert!(mods.submit(m, &real).accepted());
        assert!(mods.is_active("old_mod"));
    }

    #[test]
    fn revision_changes_on_every_mutation_so_the_engine_can_rebuild_content() {
        let mut mods = ModRuntime::new("26.3");
        let v = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        let r0 = mods.revision;
        mods.submit(
            ModDescriptor::content("a", "26.3", &[Capability::Assets], b"a"),
            &v,
        );
        assert_eq!(mods.revision, r0 + 1);
        let d0 = mods.content_digest();
        mods.remove("a");
        assert_eq!(mods.revision, r0 + 2);
        assert_ne!(mods.content_digest(), d0);
    }

    #[test]
    fn namespace_clashes_are_deterministic_and_order_is_stable() {
        let mut mods = ModRuntime::new("26.3");
        let v = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        mods.submit(ModDescriptor::content("first", "26.3", &[], b"1"), &v);
        let mut clash = ModDescriptor::content("second", "26.3", &[], b"2");
        clash.namespaces = vec!["first".into()];
        assert!(!mods.submit(clash, &v).accepted());
        assert_eq!(mods.order(), ["first"]);
        assert_eq!(mods.active().count(), 1);
    }

    #[test]
    fn native_hooks_only_run_with_the_bridge_that_exposes_them() {
        let versions = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        let mut m = ModDescriptor::content("goals", "26.3", &[Capability::NativeHook], b"rust");
        m.origin = ModOrigin::NativeRust;
        m.hooks = vec!["register_goals".into()];
        let mut no_bridge = ModRuntime::new("26.3");
        assert!(!no_bridge.submit(m.clone(), &versions).accepted());
        let mut with_bridge =
            ModRuntime::with_bridge("26.3", Box::new(NativeHookBridge::new(&["register_goals"])));
        assert_eq!(
            with_bridge.submit(m, &versions),
            ModVerdict::Activate {
                reload: Reload::BuildRequired
            }
        );
    }

    #[test]
    fn wasm_legacy_mods_are_refused() {
        let mut mods = ModRuntime::new("26.3");
        let v = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        let mut m = ModDescriptor::content("old_wasm", "26.3", &[], b"wasm");
        m.origin = ModOrigin::WasmLegacy;
        assert!(!mods.submit(m, &v).accepted());
    }

    #[test]
    fn api_level_gate_refuses_future_mods() {
        let mut mods = ModRuntime::new("26.3");
        let v = runtime_with_version("26.3", ENGINE_DATA_VERSION, ENGINE_PROTOCOL);
        let mut m = ModDescriptor::content("future", "26.3", &[], b"x");
        m.api_level = 99;
        assert!(!mods.submit(m, &v).accepted());
    }
}
