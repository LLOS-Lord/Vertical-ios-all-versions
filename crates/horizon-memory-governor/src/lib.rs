//! The game's memory governor.
//!
//! # The problem on iOS
//!
//! iOS has no memory quota an app can set. The kernel's jetsam policy kills the app when its
//! footprint approaches a device-specific limit, and the limit for a foreground game is roughly
//! 1.2–1.8 GB on an A11 phone, 2–3 GB on an A14, 4 GB+ on an M-series iPad — none of it visible to
//! the app except through `os_proc_available_memory` and the `didReceiveMemoryWarning` callback,
//! which arrives when it is already too late to be gentle. The renderer already reacts to warnings
//! (`horizon-client/src/game/memory_warning.rs` lowers the render distance after the second one),
//! but a reaction is not a policy: the game still grows until the warning comes.
//!
//! # What this crate does
//!
//! * [`CacheRegistry`] holds named caches with a byte size, a priority class, and — the part that
//!   makes this a game governor rather than a generic LRU — a *visibility* score supplied by the
//!   renderer each frame: how much of the cache the player can currently see. A chunk 200 blocks
//!   behind the camera is worth less than one under the crosshair, even if it was touched more
//!   recently, and it is the first thing to give up.
//! * [`Governor`] compares the total against a ceiling derived from the device
//!   ([`horizon_frame_sched::DeviceProfile::memory_ceiling_mib`]) and a headroom band, and asks for
//!   [eviction](EvictionPlan) when the total crosses the high-water mark. It is hysteretic: it
//!   evicts down to a *low*-water mark, so it does not evict a little every frame.
//! * [`Governor::pressure`] turns an iOS memory warning (or the absence of one) into a target:
//!   every warning lowers the low-water mark for a while, and a quiet minute raises it back.
//!
//! The governor never touches a `wgpu::Texture` or a chunk: it produces a plan (a list of
//! `(cache, bytes)` in the order they should be given up) and the renderer executes it, which
//! keeps the policy free of GPU handles and therefore testable.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

/// What a cache is worth keeping relative to the others. The order is also the eviction order when
/// visibility is equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Cannot be rebuilt from disk without a download or a generation run: player edits, edited
    /// chunk meshes, screenshots in flight. Never evicted by the governor; only reported.
    Irreplaceable = 0,
    /// Expensive to rebuild: generated chunk meshes, far-tile geometry, compiled shaders.
    Costly = 1,
    /// Cheap to rebuild: decoded textures, light arrays, particle atlases.
    Rebuildable = 2,
    /// Free to drop: scratch buffers, one-frame uploads, prefetch.
    Scratch = 3,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Irreplaceable => "irreplaceable",
            Self::Costly => "costly",
            Self::Rebuildable => "rebuildable",
            Self::Scratch => "scratch",
        }
    }
}

/// One registered cache, as the owner of it reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheEntry {
    pub name: String,
    pub class: Class,
    /// The bytes this cache currently holds.
    pub bytes: u64,
    /// How much of it the player can see right now, 0..=1. The renderer updates it each frame; a
    /// cache that cannot say leaves it at 1.0 and is evicted by class and age only.
    pub visible: f32,
    /// Frames since the cache was last used, for the age tie-break.
    pub idle_frames: u64,
    /// False once the owner has nothing left to give: it is not asked again until it grows.
    pub evictable: bool,
}

/// What the caller must free: one cache at a time, in order, until the total fits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EvictionPlan {
    /// `(cache name, bytes to free)`, most expendable first.
    pub free: Vec<(String, u64)>,
    /// The total the plan is trying to reach.
    pub target_bytes: u64,
    /// The total before the plan.
    pub current_bytes: u64,
    /// Why the plan was made: the performance overlay and the log show this.
    pub cause: Cause,
}

impl EvictionPlan {
    pub fn freed_bytes(&self) -> u64 {
        self.free.iter().map(|(_, b)| *b).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.free.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cause {
    #[default]
    HighWater,
    /// iOS sent `didReceiveMemoryWarning`.
    Warning,
    /// The device got hot and streaming was asked to slow down: caches shrink with it.
    Thermal,
    /// A world was closed: everything belonging to it goes.
    WorldClosed,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HighWater => "high-water",
            Self::Warning => "warning",
            Self::Thermal => "thermal",
            Self::WorldClosed => "world-closed",
        }
    }
}

/// The registered caches, by name.
#[derive(Debug, Default)]
pub struct CacheRegistry {
    entries: BTreeMap<String, CacheEntry>,
}

impl CacheRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or update a cache. A cache that shrinks below the mark becomes evictable again.
    pub fn upsert(
        &mut self,
        name: impl Into<String>,
        class: Class,
        bytes: u64,
        visible: f32,
        idle_frames: u64,
    ) {
        let name = name.into();
        let evictable = class != Class::Irreplaceable && bytes > 0;
        match self.entries.get_mut(&name) {
            Some(entry) => {
                entry.class = class;
                entry.bytes = bytes;
                entry.visible = visible.clamp(0.0, 1.0);
                entry.idle_frames = idle_frames;
                entry.evictable = evictable;
            }
            None => {
                self.entries.insert(
                    name.clone(),
                    CacheEntry {
                        name,
                        class,
                        bytes,
                        visible: visible.clamp(0.0, 1.0),
                        idle_frames,
                        evictable,
                    },
                );
            }
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<CacheEntry> {
        self.entries.remove(name)
    }

    pub fn get(&self, name: &str) -> Option<&CacheEntry> {
        self.entries.get(name)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every cache's bytes, summed. Saturating: a census over 16 EiB is a bug, not a panic.
    pub fn total_bytes(&self) -> u64 {
        self.entries.values().fold(0u64, |a, e| a.saturating_add(e.bytes))
    }

    /// The caches by eviction order: the least worth keeping first. Sort key, in order:
    /// class descending (scratch before rebuildable...), then visibility ascending, then idle
    /// frames descending. Irreplaceable caches are never in the list.
    pub fn eviction_order(&self) -> Vec<&CacheEntry> {
        let mut v: Vec<&CacheEntry> = self
            .entries
            .values()
            .filter(|e| e.evictable && e.bytes > 0 && e.class != Class::Irreplaceable)
            .collect();
        v.sort_by(|a, b| {
            b.class
                .cmp(&a.class)
                .then(
                    a.visible
                        .partial_cmp(&b.visible)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(b.idle_frames.cmp(&a.idle_frames))
                .then(a.name.cmp(&b.name))
        });
        v
    }

    /// A census for `horizon-ctl memory`-style output: `(name, class, bytes, visible)`.
    pub fn census(&self) -> Vec<(String, Class, u64, f32)> {
        self.entries
            .values()
            .map(|e| (e.name.clone(), e.class, e.bytes, e.visible))
            .collect()
    }
}

/// The ceilings, in bytes, and where the governor currently is between them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaterMarks {
    /// The hard ceiling: the device's, from the frame scheduler's device profile.
    pub ceiling: u64,
    /// Above this the governor asks for eviction.
    pub high: u64,
    /// ...down to this. The gap between the two is the hysteresis that stops per-frame eviction.
    pub low: u64,
}

impl WaterMarks {
    /// Marks for a ceiling in MiB. High is 85% of the ceiling, low is 70% — wide enough that a
    /// chunk load's burst does not oscillate, narrow enough that the ceiling is never reached.
    pub fn for_ceiling_mib(mib: u64) -> Self {
        let ceiling = mib.saturating_mul(1024 * 1024);
        Self {
            ceiling,
            high: ceiling / 100 * 85,
            low: ceiling / 100 * 70,
        }
    }
}

/// The governor's state.
#[derive(Debug)]
pub struct Governor {
    marks: WaterMarks,
    /// A warning tightens the marks for `tight_frames` frames; this counts them down.
    tight_frames: u64,
    /// Blocks a plan while the caller executes the last one.
    pending: bool,
    /// Statistics for the overlay.
    evictions: u64,
    evicted_bytes: u64,
    warnings: u32,
}

/// A warning holds the marks tight for this many frames (about ten seconds at 60 fps).
const WARNING_HOLD_FRAMES: u64 = 600;
/// A warning moves the low mark to this share of the ceiling: the game gives memory back
/// *before* the next warning, which is the only way a warning storm ends.
const WARNING_LOW_SHARE: u64 = 55;

impl Governor {
    pub fn new(marks: WaterMarks) -> Self {
        Self {
            marks,
            tight_frames: 0,
            pending: false,
            evictions: 0,
            evicted_bytes: 0,
            warnings: 0,
        }
    }

    /// A governor for a device profile's ceiling.
    pub fn for_device(profile: &horizon_frame_sched::DeviceProfile) -> Self {
        Self::new(WaterMarks::for_ceiling_mib(profile.memory_ceiling_mib()))
    }

    pub fn marks(&self) -> WaterMarks {
        self.marks
    }

    /// The ceiling changed: a different display mode, a background app giving memory back, a
    /// thermal step. The low/high marks follow, keeping the current tightening.
    pub fn set_ceiling_mib(&mut self, mib: u64) {
        let tight = self.tight_frames > 0;
        self.marks = WaterMarks::for_ceiling_mib(mib);
        if tight {
            self.marks.low = self.marks.ceiling / 100 * WARNING_LOW_SHARE;
        }
    }

    /// iOS said memory is short. `frames` is a hint of how long the tightening should last;
    /// [`WARNING_HOLD_FRAMES`] is used.
    pub fn memory_warning(&mut self) {
        self.warnings = self.warnings.saturating_add(1);
        self.tight_frames = WARNING_HOLD_FRAMES;
        self.marks.low = self.marks.ceiling / 100 * WARNING_LOW_SHARE;
        // A warning also raises the high mark's urgency: evict on the next frame regardless.
        self.pending = false;
    }

    /// One frame: age the tightening, and produce a plan when the registry is over the high mark.
    /// `frame_dt` in seconds is unused today but keeps the signature ready for a time-based
    /// tightening; the frame count is the clock.
    pub fn frame(&mut self, registry: &CacheRegistry, _frame_dt: f32) -> Option<EvictionPlan> {
        if self.tight_frames > 0 {
            self.tight_frames -= 1;
            if self.tight_frames == 0 {
                // Relax, but only half way: the marks return to their default over the next
                // warnings' worth of quiet, not at once.
                self.marks.low = (self.marks.low + self.marks.ceiling / 100 * 70) / 2;
            }
        }
        if self.pending {
            return None;
        }
        let current = registry.total_bytes();
        let over_warning = self.tight_frames > 0 && current > self.marks.low;
        if current <= self.marks.high && !over_warning {
            return None;
        }
        let target = self.marks.low;
        let mut need = current.saturating_sub(target);
        let mut free = Vec::new();
        for entry in registry.eviction_order() {
            if need == 0 {
                break;
            }
            let give = entry.bytes.min(need);
            free.push((entry.name.clone(), give));
            need = need.saturating_sub(give);
        }
        if free.is_empty() {
            return None;
        }
        self.pending = true;
        Some(EvictionPlan {
            free,
            target_bytes: target,
            current_bytes: current,
            cause: if self.tight_frames > 0 {
                Cause::Warning
            } else {
                Cause::HighWater
            },
        })
    }

    /// The caller has freed what the plan asked for (or as much as it could).
    pub fn plan_done(&mut self, freed: u64) {
        self.pending = false;
        self.evictions = self.evictions.saturating_add(1);
        self.evicted_bytes = self.evicted_bytes.saturating_add(freed);
    }

    /// A plan for a world closing: everything rebuildable and scratch, whatever the total.
    pub fn world_closed(&mut self, registry: &CacheRegistry) -> EvictionPlan {
        let free = registry
            .eviction_order()
            .into_iter()
            .map(|e| (e.name.clone(), e.bytes))
            .collect();
        EvictionPlan {
            free,
            target_bytes: 0,
            current_bytes: registry.total_bytes(),
            cause: Cause::WorldClosed,
        }
    }

    /// Counters for the performance overlay: `(evictions, bytes freed, warnings)`.
    pub fn stats(&self) -> (u64, u64, u32) {
        (self.evictions, self.evicted_bytes, self.warnings)
    }

    /// The share of the ceiling in use, 0..=1, for a warning line in the HUD.
    pub fn utilisation(&self, registry: &CacheRegistry) -> f32 {
        if self.marks.ceiling == 0 {
            return 0.0;
        }
        registry.total_bytes() as f32 / self.marks.ceiling as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn registry() -> CacheRegistry {
        let mut r = CacheRegistry::new();
        r.upsert("chunk_meshes_near", Class::Costly, 200 * MIB, 1.0, 0);
        r.upsert("chunk_meshes_behind", Class::Costly, 300 * MIB, 0.0, 2);
        r.upsert("decoded_textures", Class::Rebuildable, 150 * MIB, 0.6, 10);
        r.upsert("scratch", Class::Scratch, 50 * MIB, 0.0, 0);
        r.upsert("player_edits", Class::Irreplaceable, 100 * MIB, 0.0, 0);
        r
    }

    #[test]
    fn a_census_adds_up_and_ignores_no_cache() {
        assert_eq!(registry().total_bytes(), 800 * MIB);
    }

    #[test]
    fn the_least_visible_scratch_goes_first_and_edits_never_go() {
        let r = registry();
        let order = r.eviction_order();
        let names: Vec<&str> = order.iter().map(|e| e.name.as_str()).collect();
        // Rebuildable goes before costly, and among costly the invisible one before the seen one.
        assert_eq!(names, ["scratch", "decoded_textures", "chunk_meshes_behind", "chunk_meshes_near"]);
        assert!(!names.contains(&"player_edits"));
    }

    #[test]
    fn nothing_is_evicted_below_the_high_water_mark() {
        let mut g = Governor::new(WaterMarks::for_ceiling_mib(2048));
        let r = registry();
        assert!(g.frame(&r, 1.0 / 60.0).is_none(), "800 MiB is far under 2048 MiB");
    }

    #[test]
    fn crossing_the_high_mark_evicts_down_to_the_low_mark() {
        // A 900 MiB ceiling: high 765, low 630. 800 MiB is over.
        let mut g = Governor::new(WaterMarks::for_ceiling_mib(900));
        let r = registry();
        let plan = g.frame(&r, 1.0 / 60.0).expect("over the mark");
        assert!(plan.freed_bytes() >= 800 * MIB - 630 * MIB);
        assert_eq!(plan.cause, Cause::HighWater);
        assert_eq!(plan.free[0].0, "scratch");
        // While the plan is outstanding, no second one is made.
        assert!(g.frame(&r, 1.0 / 60.0).is_none());
        g.plan_done(plan.freed_bytes());
        let (evictions, freed, _) = g.stats();
        assert_eq!(evictions, 1);
        assert_eq!(freed, plan.freed_bytes());
    }

    #[test]
    fn a_warning_tightens_the_mark_and_holds_it() {
        // A 1200 MiB ceiling: 800 MiB of caches is under the high mark (1020) but the warning
        // pulls the low mark down to 660, so the next frame gives memory back *before* the next
        // warning arrives.
        let mut g = Governor::new(WaterMarks::for_ceiling_mib(1200));
        let r = registry();
        assert!(g.frame(&r, 0.016).is_none(), "800 MiB is under 1020");
        g.memory_warning();
        assert_eq!(g.marks().low, (1200 * MIB) / 100 * 55);
        let plan = g.frame(&r, 0.016).expect("a warning makes 800 MiB too much");
        assert_eq!(plan.cause, Cause::Warning);
        assert!(plan.freed_bytes() >= 800 * MIB - 660 * MIB);
    }

    #[test]
    fn utilisation_is_the_fraction_of_the_ceiling() {
        let g = Governor::new(WaterMarks::for_ceiling_mib(1600));
        let r = registry();
        assert!((g.utilisation(&r) - 0.5).abs() < 0.01);
    }

    #[test]
    fn closing_a_world_offers_everything_rebuildable() {
        let mut g = Governor::new(WaterMarks::for_ceiling_mib(4096));
        let plan = g.world_closed(&registry());
        assert_eq!(plan.cause, Cause::WorldClosed);
        assert_eq!(plan.freed_bytes(), 700 * MIB);
        assert!(plan.free.iter().all(|(name, _)| name != "player_edits"));
    }
}
