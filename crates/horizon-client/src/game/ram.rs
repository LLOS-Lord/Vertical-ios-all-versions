//! The game's RAM budget: the device's real Jetsam ceiling, the caches that feed it, and the
//! evictions that follow when the process nears it.
//!
//! # Why the ceiling cannot be a table
//!
//! iOS does not publish a memory quota. The kernel's jetsam policy kills a process when its
//! **footprint** — not its resident size — approaches a per-device limit, and that limit is not
//! derived from installed RAM: an iPhone SE (3rd gen) with 4 GB is held to roughly 1.4 GB, an
//! iPhone 13 with 6 GB to roughly 2.1 GB, an iPad (9th gen) to roughly 1.0 GB. Two phones with the
//! same RAM can be held to limits 50 % apart.
//!
//! So there are two numbers, and this module reads both at runtime:
//!
//! * `os_proc_available_memory()` (iOS 13+) — how much more this process may allocate before it is
//!   killed. It returns 0 once the process is already over, which is why it is never used alone.
//! * `task_vm_info.phys_footprint` — the exact metric jetsam compares against, which counts
//!   compressed and IOKit-backed pages that `resident_size` misses.
//!
//! The ceiling is their sum: `footprint + available = the limit`. Everything else here is a policy
//! on top of that one measurement, so the game adapts to a limit Apple changes without an update.
//!
//! Before this module the reserve was a four-row table (`Legacy => 700`, `Mobile => 900`, …) keyed
//! on chip tier. That table is still the *fallback* — a `None` from the probe, or a desktop build —
//! but it is no longer the answer on a phone.
//!
//! # What gets evicted, and in what order
//!
//! The caches are the four the renderer and streamer actually hold, each with a byte size and a
//! visibility share the caller supplies: the part of that cache the player can currently see. The
//! order is (class, visibility, least-recently-used), so a chunk section behind the player's head
//! goes before one under the crosshair — and a section the player is *standing on* goes last of
//! all, because losing it is a hole in the world rather than a re-mesh.
//!
//! Nothing here touches a `wgpu` handle: [`Budget::frame`] returns a [`Plan`], and `game.rs`
//! executes it against the renderer and the streamer. That keeps the policy testable on a machine
//! with no GPU, which is where this code actually runs its tests.

use std::time::{Duration, Instant};

use horizon_frame_sched::DeviceProfile;
use horizon_memory_governor::{CacheRegistry, Cause, Class, EvictionPlan, Governor, WaterMarks};

/// The share of the jetsam ceiling the game designs to. Leaving a quarter in hand absorbs the
/// bursts no policy can schedule — a texture atlas decode, a shader compile, an iOS system
/// framework allocating on its own thread — and keeps the process out of the band where the
/// kernel starts compressing pages (which costs CPU on every access) well before it kills.
const DESIGN_SHARE: u64 = 65;

/// The share the game designs to when a memory warning has already arrived: the warning means the
/// device is under pressure from *other* processes too, so the burst allowance is smaller.
const DESIGN_SHARE_WARNED: u64 = 50;

/// How long a memory warning keeps the lower design share. iOS does not send an "all clear", so
/// the game infers one from quiet.
const WARNING_HOLD: Duration = Duration::from_secs(10);

/// The smallest ceiling worth honouring. Below this the game cannot hold a world at any render
/// distance, and pretending otherwise only makes the crash later.
const FLOOR_MIB: u64 = 256;

/// At or under this much jetsam headroom the process acts on the frame it sees it, whatever the
/// watermarks say: this is the margin the kernel kills from, and a photo, a dimension change or a
/// chunk burst in the next second would spend it.
pub const OUT_OF_HEADROOM_BYTES: u64 = 32 << 20;

/// What the probe learned about the device this run.
///
/// Every field is `Option` because every one of them comes from a platform call that a non-Apple
/// target does not have. `None` everywhere means "use the tier table", which is what a desktop
/// build and a test binary do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Device {
    /// `ProcessInfo.physicalMemory`, MiB.
    pub physical_mib: Option<u64>,
    /// `os_proc_available_memory()`, MiB. The headroom left before jetsam.
    pub available_mib: Option<u64>,
    /// `task_vm_info.phys_footprint`, MiB. What jetsam actually measures.
    pub footprint_mib: Option<u64>,
    /// The jetsam limit the two imply: `footprint + available`. When this is `Some`, it is the
    /// ceiling and the tier table is ignored.
    pub limit_mib: Option<u64>,
}

impl Device {
    /// Derive the limit from the two runtime counters. Public because it is the one piece of
    /// arithmetic worth testing directly: `available` is 0 exactly when the process is already
    /// over, and a limit of "footprint + 0" would then ratchet the ceiling down to nothing.
    pub fn limit_from(footprint_mib: u64, available_mib: u64) -> Option<u64> {
        // `os_proc_available_memory` returns 0 both for "no headroom" and for "already over". In
        // the second case the honest limit is at least what is already held, so never report a
        // limit below the footprint.
        let limit = footprint_mib.saturating_add(available_mib);
        (limit >= FLOOR_MIB).then_some(limit)
    }

    /// The ceiling this device imposes, MiB, and whether it came from a measurement.
    ///
    /// A measured limit wins outright: it is the number the kernel will act on. The tier table is
    /// only consulted when the platform gave no limit at all.
    pub fn ceiling_mib(&self, profile: &DeviceProfile) -> (u64, bool) {
        match self.limit_mib {
            Some(limit) => (limit.max(FLOOR_MIB), true),
            None => (profile.memory_ceiling_mib(), false),
        }
    }
}

/// What the game caches, as the caller measures it each frame. Cheap: every field is arithmetic
/// over counters the renderer and streamer already keep, except `sections`, which is the world's
/// own length.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Census {
    /// Vertex bytes of the resident chunk meshes. Split into near and far by `visible_share`.
    pub mesh_bytes: u64,
    /// The part of `mesh_bytes` the player can currently see, 0..=1. Supplied by
    /// [`visible_share`], which derives it from the renderer's own residency counters rather than
    /// guessing.
    pub visible_share: f32,
    /// Far-tile geometry: the LOD quads beyond the render distance.
    pub lod_bytes: u64,
    /// Block and item sprites: the texture atlas.
    pub texture_bytes: u64,
    /// Streaming scratch: buffers the mesher workers hold.
    pub scratch_bytes: u64,
    /// Chunks the world currently holds, and their bytes.
    pub world_bytes: u64,
    /// Resident sections, for the per-section cost in the log.
    pub sections: u64,
    /// The process's `phys_footprint`, bytes, read from the kernel. This is the number jetsam
    /// compares against, and the only figure here that is a measurement rather than a guess.
    ///
    /// The census alone was the budget's whole input, and on a device it read **8% of a 1363 MiB
    /// ceiling** while the game was running a world at render distance 32 — because `mesh_bytes`
    /// counted only the vertices resident *sections* hold, not the arena pages, the index buffer or
    /// the block sprites' mips. A budget that under-reads by an order of magnitude never evicts;
    /// that is why nine hours of device logs contain `freed 0 MiB` and nothing else.
    pub measured_bytes: Option<u64>,
    /// The process's jetsam headroom, bytes, from `os_proc_available_memory`. Zero means already
    /// over the limit, which is the one state that must act immediately.
    pub available_bytes: Option<u64>,
}

impl Census {
    /// What the budget should compare against the ceiling: the kernel's `phys_footprint` when it
    /// was read, and the accounted total otherwise.
    ///
    /// Taking the larger of the two is deliberate. A census that over-reports would evict memory
    /// the process is really using; a census that under-reports — which is what the device log
    /// showed — leaves the budget blind. `max` cannot be fooled by the second, and the first only
    /// errs on the side of freeing caches that can be rebuilt.
    pub fn in_use_bytes(&self) -> u64 {
        self.measured_bytes
            .unwrap_or(0)
            .max(self.total_bytes())
    }

    /// Whether the kernel says this process is out of headroom: the state jetsam kills from, and
    /// the only one that justifies evicting on the frame it is seen rather than at a watermark.
    pub fn out_of_headroom(&self) -> bool {
        self.available_bytes.is_some_and(|bytes| bytes < OUT_OF_HEADROOM_BYTES)
    }

    /// Everything the game accounts for, bytes.
    pub fn total_bytes(&self) -> u64 {
        self.mesh_bytes
            .saturating_add(self.lod_bytes)
            .saturating_add(self.texture_bytes)
            .saturating_add(self.scratch_bytes)
            .saturating_add(self.world_bytes)
    }
}

/// What one eviction pass asks the caller to do. Booleans rather than amounts, because the caller
/// knows how to free each thing and the policy does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Drop the far-tile geometry (`Renderer::clear_lods`).
    pub far_tiles: bool,
    /// Release the resident sections the camera cannot see (`Renderer::release_offscreen_sections`).
    pub offscreen_sections: bool,
    /// Return the streaming scratch to the allocator (`Streaming::release_memory`).
    pub scratch: bool,
    /// Why this plan was made.
    pub cause: Cause,
}

impl Plan {
    /// Whether the plan asks for nothing.
    pub fn is_empty(self) -> bool {
        !self.far_tiles && !self.offscreen_sections && !self.scratch
    }
}

/// The RAM budget's decision loop.
#[derive(Debug)]
pub struct Budget {
    /// The device, as probed once at startup.
    device: Device,
    /// The tier table's answer, kept for the fallback and for the log.
    profile: DeviceProfile,
    /// The governor: the marks and the hysteresis.
    governor: Governor,
    /// Whether the ceiling came from a measurement (true) or the tier table (false).
    measured: bool,
    /// The ceiling the governor is running against, MiB, after the design share.
    ceiling_mib: u64,
    /// When the last memory warning arrived, and how many came.
    warned: Option<Instant>,
    warnings: u32,
    /// The worst utilisation seen, so the log reports the peak rather than the frame it happened on.
    peak: f32,
    /// Evictions forced by jetsam headroom rather than by a watermark.
    emergencies: u32,
}

impl Budget {
    /// A budget for a device. The ceiling is the design share of whatever limit the probe implies.
    pub fn for_device(device: &Device) -> Self {
        let profile = probe_device();
        let (limit_mib, measured) = device.ceiling_mib(&profile);
        let ceiling_mib = (limit_mib * DESIGN_SHARE / 100).max(FLOOR_MIB);
        Self {
            device: *device,
            profile,
            governor: Governor::new(WaterMarks::for_ceiling_mib(ceiling_mib)),
            measured,
            ceiling_mib,
            warned: None,
            warnings: 0,
            peak: 0.0,
            emergencies: 0,
        }
    }

    /// The device profile the renderer's thermal and frame-generation decisions use.
    pub fn profile(&self) -> &DeviceProfile {
        &self.profile
    }

    /// The ceiling in force, MiB. This is the design share of the jetsam limit, not the limit.
    pub fn ceiling_mib(&self) -> u64 {
        self.ceiling_mib
    }

    /// Whether the ceiling came from `os_proc_available_memory` rather than the chip table.
    pub fn measured(&self) -> bool {
        self.measured
    }

    /// The jetsam limit the ceiling was derived from, MiB, when one was measured.
    pub fn limit_mib(&self) -> Option<u64> {
        self.device.limit_mib
    }

    /// What the budget looks like on the performance overlay: one line.
    pub fn describe(&self) -> String {
        let source = if self.measured { "measured" } else { "tier" };
        let limit = match self.device.limit_mib {
            Some(limit) => format!(", jetsam {limit} MiB"),
            None => String::new(),
        };
        format!(
            "RAM {} MiB ceiling ({source}{limit}), peak {:.0}%, {} warning(s), {} headroom eviction(s)",
            self.ceiling_mib,
            self.peak * 100.0,
            self.warnings,
            self.emergencies
        )
    }

    /// The share of the ceiling the process has come closest to this run, 0..=1. `1.0` means the
    /// governor's ceiling was reached: the design share of the jetsam limit, not the limit itself.
    ///
    /// It is a peak rather than an instantaneous reading on purpose — the frame the peak happened
    /// on is long gone by the time anyone reads the overlay, and an average would hide the spikes
    /// that actually end a run.
    pub fn utilisation(&self) -> f32 {
        self.peak
    }

    /// An iOS memory warning. The ceiling drops to [`DESIGN_SHARE_WARNED`] for [`WARNING_HOLD`],
    /// and the governor's own marks tighten with it.
    ///
    /// The warning is *halved*, not purged: dropping every cache at once means the next second
    /// re-decodes and re-meshes everything the player is standing in, which is both a longer stall
    /// than the pressure caused and a larger allocation than the one before. The governor already
    /// evicts down to the low-water mark on the next frame; this only makes that mark lower.
    pub fn warning(&mut self) {
        self.warned = Some(Instant::now());
        self.warnings = self.warnings.saturating_add(1);
        self.governor.memory_warning();
        let limit = self.device.limit_mib.unwrap_or(0);
        if limit > 0 {
            let warned = (limit * DESIGN_SHARE_WARNED / 100).max(FLOOR_MIB);
            if warned < self.ceiling_mib {
                self.ceiling_mib = warned;
                self.governor.set_ceiling_mib(warned);
            }
        }
    }

    /// The world closed: everything that belongs to it goes, in one pass, with no hysteresis —
    /// there is no next frame to spread it over.
    pub fn world_closed(&mut self) -> Plan {
        Plan {
            far_tiles: true,
            offscreen_sections: true,
            scratch: true,
            cause: Cause::WorldClosed,
        }
    }

    /// The per-frame decision. Returns a plan when the process has crossed the high-water mark, or
    /// when a warning is pending; `None` on the overwhelming majority of frames.
    ///
    /// `dt` paces the governor's own decay (how fast a warning's effect fades), and `now` decides
    /// when the warning hold has elapsed.
    pub fn frame(&mut self, census: Census, now: Instant, dt: f32) -> Option<Plan> {
        // The hold expiring is the only way the ceiling rises again.
        if let Some(at) = self.warned
            && now.duration_since(at) >= WARNING_HOLD
        {
            self.warned = None;
            let (limit_mib, _) = self.device.ceiling_mib(&self.profile);
            let restored = (limit_mib * DESIGN_SHARE / 100).max(FLOOR_MIB);
            if restored > self.ceiling_mib {
                self.ceiling_mib = restored;
                self.governor.set_ceiling_mib(restored);
            }
        }

        let registry = self.registry(&census);
        // The peak is against the ceiling, so a run that never crosses the high mark never logs.
        self.peak = self.peak.max(self.governor.utilisation(&registry));

        // Out of headroom: evict now, in the order the registry ranks, without waiting for a
        // watermark. The governor's marks are a share of a ceiling it only reaches on the way down;
        // `os_proc_available_memory` under 32 MiB is the kernel saying the kill is close, and that
        // is not a moment to be prudent about hysteresis.
        if census.out_of_headroom() {
            self.emergencies = self.emergencies.saturating_add(1);
            let plan = Plan {
                far_tiles: true,
                offscreen_sections: true,
                scratch: true,
                cause: Cause::Headroom,
            };
            self.governor.plan_done(0);
            return Some(plan);
        }

        let plan = self.governor.frame(&registry, dt)?;
        Some(self.translate(plan, &census))
    }

    /// The eviction plan finished and freed `bytes`. The governor needs it to know whether to keep
    /// evicting or to stop; withholding it is how a governor evicts every frame forever.
    pub fn done(&mut self, bytes: u64) {
        self.governor.plan_done(bytes);
    }

    /// The report counters: `(ceiling, plan bytes, plans made)`.
    pub fn stats(&self) -> (u64, u64, u32) {
        self.governor.stats()
    }

    /// Build the registry from the census. Each cache is registered with its class and its
    /// visibility, which is what orders the eviction: a near section is worth more than a far one
    /// at the same class and the same age.
    fn registry(&self, census: &Census) -> CacheRegistry {
        let mut registry = CacheRegistry::new();
        let share = census.visible_share.clamp(0.0, 1.0);
        // The mesh cache is the one that can be split: the visible part is `Costly` (re-meshing it
        // is a worker job the player will notice) and the invisible part is `Rebuildable` (it can
        // be re-meshed off the frame's critical path). This is the split `visible_share` exists
        // for.
        let visible = (census.mesh_bytes as f32 * share) as u64;
        let hidden = census.mesh_bytes.saturating_sub(visible);
        registry.upsert(
            "chunk_meshes_near",
            Class::Costly,
            visible,
            share.max(0.5),
            0,
        );
        // What the player cannot see: re-meshing it happens off the critical path.
        registry.upsert(
            "chunk_meshes_far",
            Class::Rebuildable,
            hidden,
            1.0 - share,
            0,
        );
        // Far tiles are pure decoration beyond the render distance: always rebuildable.
        registry.upsert("lod_tiles", Class::Rebuildable, census.lod_bytes, 0.0, 0);
        // The atlas is visible whenever the HUD is: `Rebuildable`, but never the first choice,
        // because re-decoding it is a stall the player sees.
        registry.upsert("block_sprites", Class::Rebuildable, census.texture_bytes, 0.25, 0);
        // Scratch is free: it is a buffer the workers reallocate on their next job.
        registry.upsert("streaming_scratch", Class::Scratch, census.scratch_bytes, 0.0, 0);
        // The world's own chunks are player data: never evicted, reported so the total is honest.
        registry.upsert("world_chunks", Class::Irreplaceable, census.world_bytes, 0.0, 0);
        // What the census cannot name — the arena pages, the index buffer, the sprites' mips, the
        // GPU driver's host-side copies, the JVM-free heap of everything else the game holds — is
        // the difference between what the kernel measures and what the caches above account for.
        //
        // Without this row the governor's utilisation was `census / ceiling`, which on the device
        // read 8% while a world ran at render distance 32: the watermarks were never crossed and no
        // cache was ever a candidate. The row is `Irreplaceable` and untouchable — it is not a
        // cache — but it makes the total the number jetsam actually sees, so the marks mean what
        // they say.
        let named = registry.total_bytes();
        let unnamed = census.in_use_bytes().saturating_sub(named);
        registry.upsert("process_unaccounted", Class::Irreplaceable, unnamed, 0.0, 0);
        registry
    }

    /// Turn the governor's ordered plan into the booleans `game.rs` executes.
    ///
    /// The governor names caches; the caller acts on subsystems. This is the one place the two
    /// vocabularies meet, and it is deliberately total: an unrecognised cache name frees nothing
    /// rather than being guessed at.
    fn translate(&self, plan: EvictionPlan, _census: &Census) -> Plan {
        let names = |wanted: &str| plan.free.iter().any(|(name, _)| name == wanted);
        Plan {
            far_tiles: names("lod_tiles"),
            offscreen_sections: names("chunk_meshes_far"),
            scratch: names("streaming_scratch"),
            cause: plan.cause,
        }
    }
}

/// The part of the renderer's mesh bytes the player can see, 0..=1.
///
/// Derived from the renderer's own residency counters rather than estimated from the render
/// distance: the census already knows how many sections are resident and how many were drawn, and
/// their ratio is the share that eviction must protect. When no section is resident (a loading
/// screen, the title) the share is 1.0 — nothing is evictable, and reporting 0.0 there would let a
/// warning purge the world the player is about to spawn into.
pub fn visible_share(renderer: &horizon_client_render::gfx::Renderer) -> f32 {
    let (drawn, resident) = renderer.drawn_and_resident();
    if resident == 0 {
        return 1.0;
    }
    (drawn as f32 / resident as f32).clamp(0.0, 1.0)
}

/// The device as the platform reports it, probed once.
///
/// On a non-Apple target — and in a test binary — every field is `None`, which makes
/// [`Budget::for_device`] fall back to the tier table. That is deliberate: a desktop has no jetsam,
/// so a desktop's ceiling is genuinely its chip tier's, and inventing a measurement there would be
/// worse than the table.
pub fn probe_device() -> DeviceProfile {
    DeviceProfile::from_hw_machine(&hw_machine())
}

/// The platform's machine identifier: `iPhone11,8` on the phone itself.
///
/// `sysctlbyname("hw.machine")` is the phone's own answer. It used to come only from an environment
/// variable, which nothing sets on a device — so `from_hw_machine("")` fell through to `A12` on
/// **every** phone: the wrong SoC tier, and with it the wrong thermal table and memory ceiling. The
/// variable is still honoured first, because a soak harness needs to pretend to be a given device.
fn hw_machine() -> String {
    match std::env::var("HORIZON_HW_MACHINE") {
        Ok(faked) if !faked.trim().is_empty() => faked,
        _ => horizon_threads::process_memory::hw_machine().unwrap_or_default(),
    }
}

/// Probe the process's memory counters.
///
/// `limit_mib` is the number this whole module is built on: `phys_footprint + available`, both read
/// from the kernel on an Apple platform. When the kernel cannot answer (a desktop, or an iOS before
/// 13), it stays `None` and [`Budget`] falls back to the tier table — but on a phone it now answers.
pub fn probe() -> Device {
    let physical_mib = horizon_threads::process_memory::usage()
        .map(|usage| usage[2] >> 20)
        .or_else(|| horizon_threads::process_memory::physical_memory_bytes().map(|b| b >> 20));
    // `phys_footprint`, the metric jetsam compares against; `resident_bytes` inside it is the older
    // counter and misses compressed pages.
    let footprint_mib = horizon_threads::process_memory::footprint_bytes().map(|b| b >> 20);
    let available_mib = horizon_threads::process_memory::available_memory_bytes().map(|b| b >> 20);
    Device {
        physical_mib,
        available_mib,
        footprint_mib,
        limit_mib: horizon_threads::process_memory::jetsam_limit_bytes().map(|b| b >> 20),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(limit: Option<u64>, footprint: u64, available: u64) -> Device {
        Device {
            physical_mib: Some(4096),
            available_mib: Some(available),
            footprint_mib: Some(footprint),
            limit_mib: limit,
        }
    }

    /// The research finding, as a test: the limit is `footprint + available`, and it is not derived
    /// from installed RAM. Two devices with the same 4 GB and different limits.
    #[test]
    fn the_limit_is_footprint_plus_headroom() {
        assert_eq!(Device::limit_from(900, 484), Some(1384));
        assert_eq!(Device::limit_from(1200, 899), Some(2099));
        // Same 4096 MiB of RAM in both cases; the limits differ by 50%.
        assert_ne!(Device::limit_from(900, 484), Device::limit_from(1200, 899));
    }

    /// `os_proc_available_memory` returns 0 when the process is already over the limit. The limit
    /// must then be at least the footprint, never zero.
    #[test]
    fn zero_headroom_does_not_ratchet_the_ceiling_to_nothing() {
        assert_eq!(Device::limit_from(1400, 0), Some(1400));
        // And a process that reports almost nothing is not a real device.
        assert_eq!(Device::limit_from(10, 0), None);
    }

    /// The defect this fixed, as a test: a device that reports a footprint and a headroom must
    /// produce a limit, so `Budget::frame` has a ceiling to compare against. Before, `probe()` hard
    /// coded `available_mib: None` and `limit_mib: None`, so the ceiling was always the tier
    /// table's — and the device log never once carried a budget line.
    #[test]
    fn a_device_that_reports_its_counters_gets_a_real_ceiling() {
        let probed = device(Device::limit_from(900, 484), 900, 484);
        assert_eq!(probed.limit_mib, Some(1384));
        assert!(probed.footprint_mib.is_some());
        assert!(probed.available_mib.is_some());
        let budget = Budget::for_device(&probed);
        assert!(budget.measured(), "a measured limit must be the one in force");
        assert_eq!(budget.limit_mib(), Some(1384));
    }

    /// The machine identifier is what picks the SoC tier, and with it the thermal table and the
    /// memory ceiling. It used to be read from an environment variable nothing sets on a phone, so
    /// every device was classified A12; a real identifier must now win.
    #[test]
    fn a_reported_machine_identifier_selects_its_own_tier() {
        // An A11 phone is `iPhone10,x`; the fallback for an empty identifier is A12, a different tier.
        let a11 = DeviceProfile::from_hw_machine("iPhone10,3");
        let fallback = DeviceProfile::from_hw_machine("");
        assert_ne!(a11, fallback, "hw.machine must change the profile, not be ignored");
    }

    #[test]
    fn a_measured_limit_wins_over_the_tier_table() {
        let profile = DeviceProfile::MOBILE;
        let measured = device(Some(1384), 900, 484);
        let (ceiling, from_measurement) = measured.ceiling_mib(&profile);
        assert_eq!(ceiling, 1384);
        assert!(from_measurement);

        let unmeasured = device(None, 0, 0);
        let (fallback, from_measurement) = unmeasured.ceiling_mib(&profile);
        assert_eq!(fallback, profile.memory_ceiling_mib());
        assert!(!from_measurement);
    }

    #[test]
    fn the_budget_designs_below_the_limit() {
        let budget = Budget::for_device(&device(Some(2000), 1300, 700));
        // 65% of 2000.
        assert_eq!(budget.ceiling_mib(), 1300);
        assert!(budget.measured());
        assert_eq!(budget.limit_mib(), Some(2000));
    }

    #[test]
    fn a_warning_lowers_the_ceiling_but_does_not_purge() {
        let mut budget = Budget::for_device(&device(Some(2000), 1300, 700));
        let before = budget.ceiling_mib();
        budget.warning();
        let after = budget.ceiling_mib();
        assert!(after < before, "{after} < {before}");
        assert_eq!(after, 1000, "50% of the limit");
        assert!(after > 0, "a warning never purges the whole budget");
    }

    #[test]
    fn the_ceiling_restores_after_the_hold() {
        let mut budget = Budget::for_device(&device(Some(2000), 1300, 700));
        let original = budget.ceiling_mib();
        budget.warning();
        assert!(budget.ceiling_mib() < original);
        // A quiet minute: the hold elapsed, and the ceiling goes back.
        let later = Instant::now() + WARNING_HOLD + Duration::from_secs(1);
        let census = Census::default();
        let _ = budget.frame(census, later, 0.016);
        assert_eq!(budget.ceiling_mib(), original);
    }

    #[test]
    fn a_frame_under_the_mark_plans_nothing() {
        let mut budget = Budget::for_device(&device(Some(2000), 1300, 700));
        let census = Census {
            mesh_bytes: 16 << 20,
            visible_share: 0.5,
            ..Census::default()
        };
        assert!(budget.frame(census, Instant::now(), 0.016).is_none());
    }

    #[test]
    fn the_world_closing_frees_everything_that_belongs_to_it() {
        let mut budget = Budget::for_device(&device(Some(2000), 1300, 700));
        let plan = budget.world_closed();
        assert!(plan.far_tiles && plan.offscreen_sections && plan.scratch);
        assert_eq!(plan.cause, Cause::WorldClosed);
        assert!(!plan.is_empty());
    }

    #[test]
    fn the_census_total_is_the_sum_of_its_parts() {
        let census = Census {
            mesh_bytes: 1 << 20,
            lod_bytes: 2 << 20,
            texture_bytes: 3 << 20,
            scratch_bytes: 4 << 20,
            world_bytes: 5 << 20,
            measured_bytes: None,
            available_bytes: None,
            visible_share: 0.5,
            sections: 100,
        };
        assert_eq!(census.total_bytes(), 15 << 20);
    }

    /// A device with no measurement at all — the test binary — must still produce a working
    /// budget rather than dividing by zero or claiming a zero ceiling.
    #[test]
    fn a_budget_works_with_no_measurement() {
        let budget = Budget::for_device(&Device::default());
        assert!(!budget.measured());
        assert!(budget.ceiling_mib() >= FLOOR_MIB);
        assert!(budget.describe().contains("tier"));
    }
    /// The device's own numbers, from the bundle that exposed the defect: an iPhone10,5 with a
    /// 2098 MiB jetsam limit, a 1363 MiB ceiling, and a census that summed to 8% of it — 109 MiB —
    /// while a world ran at render distance 32. Before this change the governor compared 109 MiB
    /// against 1363 and never crossed a watermark, which is exactly what the log showed: no plan,
    /// no eviction, `freed 0 MiB`, for nine hours.
    #[test]
    fn the_devices_own_numbers_now_reach_the_governor() {
        let census = Census {
            // What the renderer's counters accounted for on the device.
            mesh_bytes: 40 << 20,
            visible_share: 0.5,
            lod_bytes: 8 << 20,
            texture_bytes: 16 << 20,
            scratch_bytes: 1 << 20,
            world_bytes: 44 << 20,
            sections: 12_288,
            // What the kernel measured: the real footprint of a phone running a world.
            measured_bytes: Some(900 << 20),
            available_bytes: Some(1198 << 20),
        };
        assert_eq!(census.total_bytes(), 109 << 20, "the census alone under-reads");
        assert_eq!(census.in_use_bytes(), 900 << 20, "the kernel's figure is the one in use");

        let device = Device {
            physical_mib: Some(2973),
            available_mib: Some(1198),
            footprint_mib: Some(900),
            limit_mib: Some(2098),
        };
        let mut budget = Budget::for_device(&device);
        assert_eq!(budget.ceiling_mib(), 1363);

        // 900 MiB of 1363 is 66%: over the high-water mark, so a plan is made. The same census
        // without the kernel's figure is 8%, and made none — which is the difference between the
        // budget the log described and the budget it should have been.
        let now = Instant::now();
        let plan = budget.frame(census, now, 0.016).expect("over the high mark");
        assert!(!plan.is_empty());
    }

    /// `os_proc_available_memory` under the margin: evict on this frame, whatever the watermarks
    /// say. This is the path a Jetsam kill comes through, and no watermark can catch it — the
    /// ceiling is a design share, and the kill happens at the limit.
    #[test]
    fn no_jetsam_headroom_evicts_at_once() {
        let device = Device {
            physical_mib: Some(2973),
            available_mib: Some(8),
            footprint_mib: Some(2090),
            limit_mib: Some(2098),
        };
        let mut budget = Budget::for_device(&device);
        let census = Census {
            mesh_bytes: 40 << 20,
            visible_share: 1.0,
            measured_bytes: Some(2090 << 20),
            available_bytes: Some(8 << 20),
            ..Census::default()
        };
        let plan = budget.frame(census, Instant::now(), 0.016).expect("a plan");
        assert!(plan.far_tiles && plan.offscreen_sections && plan.scratch);
        assert_eq!(plan.cause, Cause::Headroom);
        assert!(budget.describe().contains("1 headroom eviction"));
    }

    /// A comfortable phone with the kernel's figures present and low: no plan, no eviction, and the
    /// reported share is against the real footprint rather than against a census that under-reads.
    #[test]
    fn a_comfortable_phone_is_left_alone() {
        let device = Device {
            physical_mib: Some(2973),
            available_mib: Some(1198),
            footprint_mib: Some(400),
            limit_mib: Some(2098),
        };
        let mut budget = Budget::for_device(&device);
        let census = Census {
            measured_bytes: Some(400 << 20),
            available_bytes: Some(1198 << 20),
            ..Census::default()
        };
        assert!(budget.frame(census, Instant::now(), 0.016).is_none());
        assert_eq!(budget.utilisation(), 0.0);
    }

}
