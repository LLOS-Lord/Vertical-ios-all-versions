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
}
