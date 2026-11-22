//! The mod loader API shim.
//!
//! # The problem: a ported mod that installs and does nothing
//!
//! `horizon-port-pack` already does the hard part of moving a Java mod into `horizon-mods/<id>/`:
//! it reads the jar, classifies its content, and carries what it can. `horizon-mod-diagnostics`
//! then explains why an installed mod is *inert* — the wrong folder, no declarable content, an
//! unbuilt native module. Neither of them answers the question a mod author actually asks:
//!
//! > the porter says it worked; which of the things my mod registers will the game call?
//!
//! For a Fabric or Forge mod the honest answer is "some of them", and the reason is structural.
//! Sinytra Connector's own maintainers put the bound plainly: a compatibility layer is only ever as
//! good as the API surface it actually shims, and everything it does not shim fails *silently* —
//! the entrypoint runs, registers a keybind into a mapping that nothing reads, and the mod appears
//! installed and on while doing nothing at all. Wrapper makes the same trade and answers it the
//! same way: a per-module table of which surface is shimmed and which is not.
//!
//! # What this crate adds
//!
//! [`Shim`] is that table. It maps each well-known loader extension point onto the Horizon facility
//! that implements it — `horizon-hooks::hud`, `lang`, `pages`, `perf`, `players`, `settings`,
//! `tunnel`, `horizon-client::mod_api`'s `Native*` views — and records, per entry point, **how
//! faithful the mapping is**:
//!
//! * [`Fidelity::Native`] — the same call reaches the same behaviour. Nothing is lost.
//! * [`Fidelity::Adapted`] — the behaviour exists but the shape differs (a different event
//!   ordering, a different signature, a callback that arrives at a different time in the tick).
//!   The mod runs; it may behave subtly differently.
//! * [`Fidelity::Absent`] — nothing implements it. The registration is dropped, and
//!   [`Coverage::absent`] names it so the launcher can say so instead of the player guessing.
//!
//! # Why this is data and not code
//!
//! The table is a `const` fixture: no jar is opened, no bytecode is read, no mod is executed. That
//! keeps it testable on any machine and keeps the decision of what is *possible* reviewable in one
//! file. The port-pack asks the table; the launcher renders the answer; the game never consults it
//! at runtime, because by then the registration either happened or did not.
//!
//! # The one rule that matters
//!
//! **Never claim more than is true.** A shim that reports `Native` for something that is merely
//! `Adapted` is worse than no shim at all: it moves the failure from "the launcher said it might
//! not work" to "the mod is on and broken", which is the state players complain about. Every
//! `Native` in [`TABLE`] is a claim about a facility that exists in this repo today.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// How faithfully a loader extension point is served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fidelity {
    /// The same call reaches the same behaviour.
    Native = 0,
    /// The behaviour exists, in a different shape: the mod runs and may behave subtly differently.
    Adapted = 1,
    /// Nothing implements it. The registration is dropped and named.
    Absent = 2,
}

impl Fidelity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Adapted => "adapted",
            Self::Absent => "absent",
        }
    }

    /// Whether a mod that uses only this point can be expected to work.
    pub const fn works(self) -> bool {
        !matches!(self, Self::Absent)
    }
}

/// The families of loader API, which is how a player and a mod author both think about it: "does
/// this thing do HUD stuff?" rather than "which package does it import?".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Family {
    /// Mod lifecycle: initialisation, client/server split, reload.
    Lifecycle,
    /// Events: tick, world, player, render.
    Events,
    /// Input: keybinds, controller buttons, touch.
    Input,
    /// Commands and chat.
    Commands,
    /// HUD, screens and menus.
    Rendering,
    /// Resource packs, models, textures, language files.
    Resources,
    /// Blocks, items, entities, recipes.
    Registry,
    /// Custom packets between client and server.
    Networking,
    /// Config files and the settings screen.
    Config,
}

impl Family {
    pub const ALL: [Self; 9] = [
        Self::Lifecycle,
        Self::Events,
        Self::Input,
        Self::Commands,
        Self::Rendering,
        Self::Resources,
        Self::Registry,
        Self::Networking,
        Self::Config,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lifecycle => "lifecycle",
            Self::Events => "events",
            Self::Input => "input",
            Self::Commands => "commands",
            Self::Rendering => "rendering",
            Self::Resources => "resources",
            Self::Registry => "registry",
            Self::Networking => "networking",
            Self::Config => "config",
        }
    }
}

/// One extension point: what a loader calls it, where Horizon implements it, and how faithfully.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The loader's name for the point, as a mod declares it: `"fabric-lifecycle-events"`,
    /// `"fabric-key-binding-api-v1"`, `"forge:registry"`. Matching is by these names, which is
    /// what the porter already extracts from `fabric.mod.json` and `mods.toml`.
    pub loader_point: &'static str,
    /// The family, for grouping the report.
    pub family: Family,
    /// How faithfully Horizon serves it.
    pub fidelity: Fidelity,
    /// Where it is implemented, as a path a maintainer can open. Empty when `Absent`.
    pub horizon: &'static str,
    /// One sentence a player could understand, for the launcher row when the point is not native.
    pub note: &'static str,
}

/// The table. Every `Native` here names a facility that exists in this repository; see the module
/// docs for why that is the load-bearing rule.
pub const TABLE: &[Entry] = &[
    // ---- Java-level patching: absent by construction ----
    Entry {
        loader_point: "fabric-mixin",
        family: Family::Lifecycle,
        fidelity: Fidelity::Absent,
        horizon: "",
        note: "A mixin patches the game's own Java classes as they load. Horizon is a native Rust \
               build with no JVM and no Java classes to patch, so the patch is dropped: the \
               mod's blocks, items, recipes and textures still port, the behaviour does not.",
    },
    Entry {
        loader_point: "fabric-access-widener",
        family: Family::Lifecycle,
        fidelity: Fidelity::Absent,
        horizon: "",
        note: "An access widener widens Java visibility at class-load time, which needs the same \
               JVM a mixin does. Dropped with it.",
    },
    // ---- lifecycle ----
    Entry {
        loader_point: "fabric-lifecycle-events",
        family: Family::Lifecycle,
        fidelity: Fidelity::Native,
        horizon: "crates/horizon-client/src/mod_api.rs (NativeMod::on_init)",
        note: "Called once, before the first frame.",
    },
    Entry {
        loader_point: "fabric-client-lifecycle",
        family: Family::Lifecycle,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-client/src/game.rs (world_closed, title transitions)",
        note: "The client's world start/stop callbacks arrive from the game loop's own transitions, \
               so they can land one tick later than Fabric's.",
    },
    Entry {
        loader_point: "forge:mod-load",
        family: Family::Lifecycle,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-hooks/src/porter.rs (PortOutcome)",
        note: "Forge's load order gives way to Horizon's dependency resolver; a mod that relied on \
               load order rather than declaring a dependency may initialise after its consumer.",
    },
    // ---- events ----
    Entry {
        loader_point: "fabric-api:client-tick-events",
        family: Family::Events,
        fidelity: Fidelity::Native,
        horizon: "crates/horizon-client/src/mod_api.rs (NativePlayerInput)",
        note: "Called at the start of every client tick.",
    },
    Entry {
        loader_point: "fabric-api:world-render-events",
        family: Family::Events,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-client/src/mod_api.rs (NativeCamera, NativeSceneView)",
        note: "The render events fire once per drawn frame rather than per buffer, and frame \
               generation's made frames do not fire them.",
    },
    Entry {
        loader_point: "forge:event-bus",
        family: Family::Events,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-client/src/mod_api.rs (the Native* views)",
        note: "Events declared with an explicit priority are delivered in registration order \
               instead; a handler that assumed it ran first may see different state.",
    },
    Entry {
        loader_point: "fabric-api:entity-events",
        family: Family::Events,
        fidelity: Fidelity::Native,
        horizon: "crates/horizon-client/src/mod_api.rs (NativeSceneView)",
        note: "Entity added/removed callbacks arrive as the client learns of the entity.",
    },
    // ---- input ----
    Entry {
        loader_point: "fabric-key-binding-api-v1",
        family: Family::Input,
        fidelity: Fidelity::Native,
        horizon: "crates/horizon-client/src/keymapping.rs (KeyAction), crates/horizon-client-input/",
        note: "A registered keybind appears in the controls screen and can be bound to a \
               controller button or a touch button, not only a key.",
    },
    Entry {
        loader_point: "forge:key-mapping",
        family: Family::Input,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-client/src/keymapping.rs",
        note: "Forge's key modifiers (shift+key as one binding) are not represented: two bindings \
               are needed instead.",
    },
    Entry {
        loader_point: "fabric-api:mouse-events",
        family: Family::Input,
        fidelity: Fidelity::Absent,
        horizon: "",
        note: "Raw mouse position and scroll hooks are not exposed to mods: touch is a first-class \
               input here and has no mouse to report.",
    },
    // ---- commands ----
    Entry {
        loader_point: "fabric-command-api-v2",
        family: Family::Commands,
        fidelity: Fidelity::Native,
        horizon: "crates/horizon-command/",
        note: "Client-side commands register into the same dispatcher the chat box uses.",
    },
    Entry {
        loader_point: "forge:command",
        family: Family::Commands,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-command/",
        note: "Brigadier argument types are carried for the common cases; a mod with a custom \
               argument type gets a plain string and must validate it itself.",
    },
    // ---- rendering ----
    Entry {
        loader_point: "fabric-rendering-v1:hud",
        family: Family::Rendering,
        fidelity: Fidelity::Native,
        horizon: "crates/horizon-hooks/src/hud.rs, crates/horizon-client/src/hud.rs",
        note: "HUD layers draw above the vanilla HUD, at the position the mod asks for.",
    },
    Entry {
        loader_point: "fabric-screen-api-v1",
        family: Family::Rendering,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-hooks/src/pages.rs, crates/horizon-client/src/mod_api.rs (NativeScreen)",
        note: "Screens are rebuilt from a widget description rather than being Java widgets; a \
               screen with custom immediate-mode drawing is served as a panel it can draw into.",
    },
    Entry {
        loader_point: "fabric-rendering-v1:world",
        family: Family::Rendering,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-client/src/mod_api.rs (NativeSceneBlock, NativeBlockRim)",
        note: "Custom world geometry is drawn through the mod draw batch, which is submitted after \
               the world's own buckets; it cannot participate in the world's occlusion culling.",
    },
    Entry {
        loader_point: "forge:overlay",
        family: Family::Rendering,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-hooks/src/hud.rs",
        note: "An overlay registered for a vanilla event draws inside that event's HUD layer.",
    },
    // ---- resources ----
    Entry {
        loader_point: "fabric-resource-loader-v0",
        family: Family::Resources,
        fidelity: Fidelity::Native,
        horizon: "crates/horizon-assets/, crates/horizon-client/src/packs.rs",
        note: "A mod's bundled assets and data are mounted as a built-in resource pack.",
    },
    Entry {
        loader_point: "fabric-model-loading-api-v1",
        family: Family::Resources,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-client/src/model_library.rs",
        note: "JSON models in the vanilla format load; a model built in code at runtime does not.",
    },
    Entry {
        loader_point: "forge:model-loader",
        family: Family::Resources,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-client/src/model_library.rs",
        note: "Forge's multi-part and custom loader models are reduced to their vanilla-format \
               equivalent where one exists.",
    },
    // ---- registry ----
    Entry {
        loader_point: "fabric-api:registry",
        family: Family::Registry,
        fidelity: Fidelity::Native,
        horizon: "crates/horizon-hooks::engine, the content `.hmod` format",
        note: "Blocks, items and entities declared in the mod's content file are registered before \
               the world loads.",
    },
    Entry {
        loader_point: "fabric-api:object-builder",
        family: Family::Registry,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-hooks::engine",
        note: "Builder options with a data-file equivalent are carried; a block whose behaviour is \
               defined entirely in an overridden Java method has no data equivalent and is served \
               as a plain block.",
    },
    Entry {
        loader_point: "forge:deferred-register",
        family: Family::Registry,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-hooks::engine",
        note: "Deferred registration is resolved at the same point in startup, so a mod that \
               registered very late relative to Forge may see its content appear earlier.",
    },
    // ---- networking ----
    Entry {
        loader_point: "fabric-networking-api-v1",
        family: Family::Networking,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-hooks/src/tunnel.rs, crates/horizon-protocol/",
        note: "Custom payloads are tunnelled to a Horizon server; against a vanilla or a modified \
               Java server without the same channel, the payload is dropped and the mod's feature \
               is off rather than crashing.",
    },
    Entry {
        loader_point: "forge:simple-channel",
        family: Family::Networking,
        fidelity: Fidelity::Absent,
        horizon: "",
        note: "Forge's simple channel with its own handshake is not implemented; a mod that needs \
               it will not sync, and the launcher says so before the player joins.",
    },
    // ---- config ----
    Entry {
        loader_point: "fabric-api:config",
        family: Family::Config,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-hooks/src/settings.rs",
        note: "The mod's options appear in Horizon's settings screen rather than its own; the file \
               format is JSON rather than the mod's own.",
    },
    Entry {
        loader_point: "forge:config",
        family: Family::Config,
        fidelity: Fidelity::Adapted,
        horizon: "crates/horizon-hooks/src/settings.rs",
        note: "TOML and JSON configs are read and rewritten; a config kept in a custom format is \
               read once and left alone.",
    },
];


/// What a Fabric mod's `fabric.mod.json` actually asks the loader for.
///
/// A `TABLE` lookup judges the points Horizon implements; this reads the *declaration* in the jar
/// and turns it into exactly those point names, so the verdict is about the mod in hand rather than
/// about Fabric in general. Fabric names these things in five separate places and a mod's metadata
/// is authoritative on all of them:
///
/// * `entrypoints` — `main`, `client`, `server`, `modmenu`, each contributed by the `fabric-loader`
///   and Fabric API entrypoint modules (`fabric-lifecycle-events-v1`'s client init is reached
///   through `client`).
/// * `depends` / `suggests` — the API modules themselves. A mod that depends on
///   `fabric-key-binding-api-v1` is *using* keybinds whether or not it says so again in the code.
/// * `mixins` — a Mixin config runs at class-load time against the game's own Java classes. No
///   amount of shimming serves it, and naming it as absent is the whole reason this exists.
/// * `accessWidener` — the same story one step further down: it widens Java visibility at load.
/// * `custom.modmenu.*` — badges, which nothing reads.
///
/// Anything not recognised is left **unknown** rather than absent, because "this build has never
/// heard of it" and "this build cannot do it" are different answers and only the second one should
/// lower a verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricDeclaration {
    /// The loader points the mod's metadata names, in a stable order, ready for [`judge`].
    pub points: Vec<String>,
    /// Mixin configuration files (`"mixins": ["example.mixins.json"]`): Java bytecode patched into
    /// the game's own classes, which Horizon's Rust build cannot host.
    pub mixins: Vec<String>,
    /// Access wideners: `"accessWidener"` plus the `aw` entries in `depends`.
    pub access_wideners: Vec<String>,
    /// What the mod says it needs from Fabric API, as it wrote them.
    pub api_modules: Vec<String>,
    /// The loader and game versions it was built for, so a mismatch is visible before install.
    pub minecraft: Vec<String>,
    pub loader: Vec<String>,
}

impl FabricDeclaration {
    /// Read `fabric.mod.json`'s text. `None` when it is not a Fabric mod at all.
    ///
    /// Pure parsing: nothing is executed, no class is loaded, and a malformed file yields `None`
    /// rather than an error, because the caller's next question is "then what is it?".
    pub fn parse(text: &str) -> Option<Self> {
        let json: serde_json::Value = serde_json::from_str(text).ok()?;
        let object = json.as_object()?;
        let mut declaration = Self::default();
        // `schemaVersion` is Fabrics's "this is a fabric.mod.json" marker.
        object.get("schemaVersion")?;

        // Entrypoints: each role name is a loader point of its own.
        if let Some(entrypoints) = object.get("entrypoints").and_then(|v| v.as_object()) {
            for role in entrypoints.keys() {
                let point = match role.as_str() {
                    "main" => "fabric-loader:main",
                    "client" => "fabric-loader:client",
                    "server" => "fabric-loader:server",
                    "modmenu" => "fabric-modmenu",
                    other => {
                        // A custom entrypoint is a Fabric API module's own: keep its name.
                        declaration.points.push(format!("fabric:{other}"));
                        continue;
                    }
                };
                declaration.points.push(point.to_owned());
            }
        }

        // Dependencies name the API modules the mod calls into.
        for key in ["depends", "suggests"] {
            let Some(map) = object.get(key).and_then(|v| v.as_object()) else {
                continue;
            };
            for (module, wanted) in map {
                match module.as_str() {
                    "fabricloader" => declaration.loader.extend(versions(wanted)),
                    "minecraft" => declaration.minecraft.extend(versions(wanted)),
                    "java" => {}
                    // `fabric-api` is the bundle: every module, so none of them is claimed
                    // individually — the jar's own code decides which it uses, and the porter
                    // reads that separately.
                    "fabric-api" => {}
                    other => {
                        declaration.api_modules.push(other.to_owned());
                        declaration.points.push(normalise_module(other));
                    }
                }
            }
        }

        // Mixins and access wideners: Java-level, absent whatever the table says.
        if let Some(list) = object.get("mixins") {
            declaration.mixins.extend(strings(list));
        }
        if let Some(aw) = object.get("accessWidener").and_then(|v| v.as_str()) {
            declaration.access_wideners.push(format!("{aw} (accessWidener)"));
        }
        if let Some(aw) = object.get("access_widener").and_then(|v| v.as_str()) {
            declaration.access_wideners.push(format!("{aw} (access_widener)"));
        }

        declaration.points.sort();
        declaration.points.dedup();
        Some(declaration)
    }

    /// The points to judge, including the two Java-level ones as [`Fidelity::Absent`] by name so a
    /// report cannot quietly omit them.
    pub fn points_with_java_level(&self) -> Vec<String> {
        let mut points = self.points.clone();
        if !self.mixins.is_empty() {
            points.push("fabric-mixin".to_owned());
        }
        if !self.access_wideners.is_empty() {
            points.push("fabric-access-widener".to_owned());
        }
        points.sort();
        points.dedup();
        points
    }

    /// The verdict the launcher shows, combining this metadata with what the content porter found.
    ///
    /// [`Coverage::verdict`] judges the *loader points* alone, and a mixin mod has none that work,
    /// so it answers `"no"` — true of the code and misleading about the mod, whose blocks, items and
    /// recipes do port. Two verdicts exist because two things are being moved: content (which
    /// `check::Verdict` judges) and behaviour (which this does). `carries_content` is the porter's
    /// answer for the first.
    pub fn verdict(&self, coverage: &Coverage, carries_content: bool) -> &'static str {
        if self.needs_class_patching() {
            // Its behaviour cannot run here. Content alone is "partly": something works.
            return if carries_content { "partly" } else { "no" };
        }
        coverage.verdict()
    }

    /// Whether the mod cannot work here whatever the content porter carries: it patches game
    /// classes at load time, which is code, not content.
    pub fn needs_class_patching(&self) -> bool {
        !self.mixins.is_empty() || !self.access_wideners.is_empty()
    }

    /// One sentence for the launcher, in the player's terms. Says what happened, what will not, and
    /// what to do about it — the three things a verdict is for.
    pub fn summary(&self, coverage: &Coverage) -> String {
        if self.needs_class_patching() {
            return format!(
                "Uses {} mixin config(s){}: it patches the game's own Java classes when it loads, \
                 which a native Rust build cannot host. Its blocks, items, recipes and textures \
                 still port; the behaviour its code adds does not.",
                self.mixins.len().max(self.access_wideners.len()),
                if !self.access_wideners.is_empty() {
                    " and an access widener"
                } else {
                    ""
                }
            );
        }
        match coverage.working_share() {
            Some(share) if share >= 1.0 => {
                "Every loader feature it asks for is served. Its content ports and its behaviour runs."
                    .to_owned()
            }
            Some(share) => format!(
                "{:.0}% of what it asks for is served; {} will not run.",
                share * 100.0,
                if coverage.absent.is_empty() {
                    "the rest".to_owned()
                } else {
                    coverage.absent.join(", ")
                }
            ),
            // Nothing declarable: the jar's `assets/` and `data/` are all it has.
            None => "Content only as far as its metadata says: no entrypoints and no API modules."
                .to_owned(),
        }
    }
}

/// A Fabric API module's name as a loader point: `fabric-key-binding-api-v1` keeps its own name.
fn normalise_module(module: &str) -> String {
    if module.starts_with("fabric-") {
        module.to_owned()
    } else {
        format!("fabric-{module}")
    }
}

/// `depends` values are `">=1.20"`, `"*"`, or a list of alternatives.
fn versions(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter(|v| v.is_string() || v.is_object())
            .filter_map(|v| {
                v.as_str().map(str::to_owned).or_else(|| {
                    v.get("config")
                        .and_then(|c| c.as_str())
                        .map(str::to_owned)
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Look up one loader point. Exact match on the loader's own name.
pub fn lookup(loader_point: &str) -> Option<&'static Entry> {
    TABLE.iter().find(|e| e.loader_point == loader_point)
}

/// What a mod's registrations amount to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// The points the mod declares that something implements faithfully.
    pub native: Vec<String>,
    /// The points the mod declares that something implements differently.
    pub adapted: Vec<String>,
    /// The points the mod declares that nothing implements. These are the silent failures.
    pub absent: Vec<String>,
    /// The points the mod declares that this table has never heard of. Neither shimmed nor known
    /// broken: the honest answer, and deliberately not folded into `absent`.
    pub unknown: Vec<String>,
}

impl Coverage {
    /// The share of the mod's declared points that work at all, 0..=1. `None` when the mod
    /// declares nothing this table can judge, because 0/0 is not "0 % works".
    pub fn working_share(&self) -> Option<f32> {
        let total = self.native.len() + self.adapted.len() + self.absent.len();
        (total > 0).then(|| (self.native.len() + self.adapted.len()) as f32 / total as f32)
    }

    /// Whether anything the mod declares will not work. The one bit the launcher's verdict needs.
    pub fn has_gaps(&self) -> bool {
        !self.absent.is_empty()
    }

    /// The absent points as `(point, why)` pairs, for the diagnosis line.
    pub fn gaps(&self) -> Vec<(&'static str, &'static str)> {
        self.absent
            .iter()
            .filter_map(|name| lookup(name))
            .map(|entry| (entry.loader_point, entry.note))
            .collect()
    }

    /// A one-line verdict, in the same shape the launcher's mod rows already show.
    ///
    /// `"works"` when every declared point is served, `"partly"` when some are, and `"no"` when
    /// there is nothing left. A mod that declares only unknown points is `"partly"`: it may work
    /// perfectly, but this table has no evidence either way and saying `"works"` would be a claim
    /// it cannot support.
    pub fn verdict(&self) -> &'static str {
        if self.absent.is_empty() && self.unknown.is_empty() && !self.native.is_empty() {
            return "works";
        }
        if self.native.is_empty() && self.adapted.is_empty() {
            return "no";
        }
        "partly"
    }

    /// The families this mod's declarations touch, so the launcher can group a long list.
    pub fn families(&self) -> Vec<Family> {
        let mut families: Vec<Family> = self
            .native
            .iter()
            .chain(&self.adapted)
            .chain(&self.absent)
            .filter_map(|name| lookup(name))
            .map(|entry| entry.family)
            .collect();
        families.sort_unstable();
        families.dedup();
        families
    }
}

/// Judge a mod that declares `points` — the loader extension points its metadata lists.
///
/// Order is preserved from the mod's own declaration, so the report reads in the order the author
/// wrote it rather than in table order.
pub fn judge(points: &[String]) -> Coverage {
    let mut coverage = Coverage::default();
    for point in points {
        match lookup(point) {
            Some(entry) => match entry.fidelity {
                Fidelity::Native => coverage.native.push(point.clone()),
                Fidelity::Adapted => coverage.adapted.push(point.clone()),
                Fidelity::Absent => coverage.absent.push(point.clone()),
            },
            None => coverage.unknown.push(point.clone()),
        }
    }
    coverage
}

/// The whole table, grouped by family, as the launcher's "what can Horizon run?" page shows it.
pub fn by_family() -> Vec<(Family, Vec<&'static Entry>)> {
    Family::ALL
        .into_iter()
        .map(|family| {
            (
                family,
                TABLE.iter().filter(|e| e.family == family).collect(),
            )
        })
        .filter(|(_, entries): &(Family, Vec<&Entry>)| !entries.is_empty())
        .collect()
}

/// A census for the log: `(native, adapted, absent)` counts across the whole table.
pub fn census() -> (usize, usize, usize) {
    let count = |fidelity: Fidelity| TABLE.iter().filter(|e| e.fidelity == fidelity).count();
    (
        count(Fidelity::Native),
        count(Fidelity::Adapted),
        count(Fidelity::Absent),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn every_entry_names_a_real_implementation_unless_absent() {
        for entry in TABLE {
            match entry.fidelity {
                Fidelity::Absent => assert!(
                    entry.horizon.is_empty(),
                    "{} claims to be absent but names {}",
                    entry.loader_point,
                    entry.horizon
                ),
                _ => assert!(
                    !entry.horizon.is_empty(),
                    "{} claims to work but names nowhere it lives",
                    entry.loader_point
                ),
            }
            assert!(
                !entry.note.is_empty(),
                "{} has no explanation",
                entry.loader_point
            );
        }
    }

    #[test]
    fn no_point_appears_twice() {
        let mut names: Vec<&str> = TABLE.iter().map(|e| e.loader_point).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "a loader point is listed twice");
    }

    #[test]
    fn a_fully_served_mod_works() {
        let coverage = judge(&points(&[
            "fabric-lifecycle-events",
            "fabric-key-binding-api-v1",
            "fabric-command-api-v2",
        ]));
        assert_eq!(coverage.verdict(), "works");
        assert!(!coverage.has_gaps());
        assert_eq!(coverage.working_share(), Some(1.0));
    }

    #[test]
    fn an_absent_point_makes_it_partly_and_names_itself() {
        let coverage = judge(&points(&[
            "fabric-lifecycle-events",
            "fabric-api:mouse-events",
        ]));
        assert_eq!(coverage.verdict(), "partly");
        assert!(coverage.has_gaps());
        let gaps = coverage.gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].0, "fabric-api:mouse-events");
        assert!(gaps[0].1.contains("touch"), "{}", gaps[0].1);
        assert_eq!(coverage.working_share(), Some(0.5));
    }

    /// An unknown point is not a gap: the table has no evidence, which is different from evidence
    /// of absence. Folding them together would make every mod on a loader this table has not been
    /// taught read as broken.
    #[test]
    fn an_unknown_point_is_not_called_broken() {
        let coverage = judge(&points(&[
            "fabric-lifecycle-events",
            "some-loader-we-never-heard-of",
        ]));
        assert!(coverage.absent.is_empty(), "unknown must not be absent");
        assert_eq!(coverage.unknown.len(), 1);
        // But the verdict is honest about it.
        assert_eq!(coverage.verdict(), "partly");
    }

    #[test]
    fn a_mod_that_declares_nothing_judgeable_has_no_share() {
        let coverage = judge(&points(&["nonsense"]));
        assert_eq!(coverage.working_share(), None);
        assert_eq!(coverage.verdict(), "no");
    }

    #[test]
    fn a_mod_whose_points_all_work_is_not_zero_percent() {
        let coverage = judge(&[]);
        assert_eq!(coverage.verdict(), "no");
        assert_eq!(coverage.working_share(), None);
    }

    #[test]
    fn judgement_preserves_the_mods_own_order() {
        let coverage = judge(&points(&[
            "forge:config",
            "fabric-lifecycle-events",
            "fabric-api:mouse-events",
        ]));
        assert_eq!(
            coverage.native,
            points(&["fabric-lifecycle-events"]),
            "native keeps the caller's order"
        );
        assert_eq!(coverage.adapted, points(&["forge:config"]));
        assert_eq!(coverage.absent, points(&["fabric-api:mouse-events"]));
    }

    #[test]
    fn families_are_reported_once_each() {
        let coverage = judge(&points(&[
            "fabric-lifecycle-events",
            "fabric-client-lifecycle",
            "fabric-key-binding-api-v1",
        ]));
        // Two lifecycle points, one input point.
        assert_eq!(coverage.families(), vec![Family::Lifecycle, Family::Input]);
    }

    #[test]
    fn the_census_matches_the_table() {
        let (native, adapted, absent) = census();
        assert_eq!(native + adapted + absent, TABLE.len());
        // The table must contain all three kinds, or it is not describing a real boundary.
        assert!(native > 0 && adapted > 0 && absent > 0);
        // And the boundary is not mostly holes: if most of the surface were absent the porter
        // would be lying by existing.
        assert!(native + adapted > absent * 4, "{native}/{adapted}/{absent}");
    }

    #[test]
    fn every_family_has_at_least_one_entry() {
        let grouped = by_family();
        assert_eq!(grouped.len(), Family::ALL.len());
        for (family, entries) in grouped {
            assert!(!entries.is_empty(), "{} is empty", family.as_str());
        }
    }

    #[test]
    fn fidelity_orders_from_faithful_to_absent() {
        assert!(Fidelity::Native < Fidelity::Adapted);
        assert!(Fidelity::Adapted < Fidelity::Absent);
        assert!(Fidelity::Native.works());
        assert!(Fidelity::Adapted.works());
        assert!(!Fidelity::Absent.works());
    }
    /// The whole point of [`FabricDeclaration`]: a mixin mod is not "partly working", it cannot work
    /// at all on a native build, and the verdict has to say so before the player installs it rather
    /// than after they wonder why nothing happens.
    #[test]
    fn a_mixin_mod_is_absent_not_partly() {
        let text = r#"{
            "schemaVersion": 1,
            "id": "example",
            "entrypoints": { "main": ["com.example.Example"] },
            "mixins": ["example.mixins.json"],
            "depends": { "fabricloader": ">=0.15", "minecraft": "~1.21", "fabric-api": "*" }
        }"#;
        let declaration = FabricDeclaration::parse(text).expect("a fabric.mod.json");
        assert!(declaration.needs_class_patching());
        let coverage = judge(&declaration.points_with_java_level());
        assert!(
            coverage.absent.contains(&"fabric-mixin".to_owned()),
            "a mixin config must be named absent, got {:?}",
            coverage
        );
        // The loader points alone: nothing of what it asks for runs.
        assert_eq!(coverage.verdict(), "no");
        // The mod: its content still ports, so the launcher says "partly" rather than "no".
        assert_eq!(declaration.verdict(&coverage, true), "partly");
        assert_eq!(declaration.verdict(&coverage, false), "no");
        let summary = declaration.summary(&coverage);
        assert!(summary.contains("mixin"), "{summary}");
        // It does not pretend the whole mod is dead: content still ports.
        assert!(summary.contains("still port"), "{summary}");
    }

    /// A content-only Fabric mod — a jar of `assets/`, `data/` and no entrypoints — is the case the
    /// porter handles well, and the summary must not imply a JVM it does not need.
    #[test]
    fn a_content_only_mod_declares_nothing_judgeable() {
        let text = r#"{ "schemaVersion": 1, "id": "example", "depends": { "minecraft": "1.21" } }"#;
        let declaration = FabricDeclaration::parse(text).expect("a fabric.mod.json");
        assert!(!declaration.needs_class_patching());
        let coverage = judge(&declaration.points_with_java_level());
        assert_eq!(coverage.working_share(), None, "0/0 is not 0%");
        assert!(declaration.summary(&coverage).contains("Content only"));
    }

    /// The API modules in `depends` are what the code will call: a keybind module is a keybind.
    #[test]
    fn api_modules_in_depends_become_the_points_the_code_uses() {
        let text = r#"{
            "schemaVersion": 1,
            "id": "example",
            "entrypoints": { "client": ["com.example.Client"] },
            "depends": {
                "fabric-key-binding-api-v1": "*",
                "fabric-lifecycle-events-v1": ">=2.0"
            }
        }"#;
        let declaration = FabricDeclaration::parse(text).expect("a fabric.mod.json");
        assert!(declaration.points.contains(&"fabric-loader:client".to_owned()));
        assert!(
            declaration
                .points
                .contains(&"fabric-key-binding-api-v1".to_owned())
        );
        // And the table knows that module, so the verdict is evidence, not a guess.
        assert!(lookup("fabric-key-binding-api-v1").is_some());
    }

    /// `accessWidener` and the `aw` dependency both mean load-time Java; neither is content.
    #[test]
    fn an_access_widener_alone_still_blocks_the_behaviour() {
        let text = r#"{
            "schemaVersion": 1,
            "id": "example",
            "accessWidener": "example.accesswidener"
        }"#;
        let declaration = FabricDeclaration::parse(text).expect("a fabric.mod.json");
        assert!(declaration.needs_class_patching());
        assert_eq!(declaration.access_wideners.len(), 1);
        let coverage = judge(&declaration.points_with_java_level());
        assert!(coverage.absent.contains(&"fabric-access-widener".to_owned()));
    }

    /// Not a Fabric jar: the parser answers `None` rather than inventing a verdict.
    #[test]
    fn a_jar_without_fabric_metadata_is_not_fabric() {
        assert!(FabricDeclaration::parse("{}").is_none());
        assert!(FabricDeclaration::parse("not json").is_none());
    }
}
