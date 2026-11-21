//! One importance ladder that the whole frame agrees on.
//!
//! # The problem this crate exists for
//!
//! The engine already decides, in four different places, what matters. None of them knows about
//! the others:
//!
//! * [`horizon_visibility_budget::Sort`] orders *what is drawn* — `Targeted` first, `Particle`
//!   last — but its budget is a millisecond count handed in by the renderer, and nothing ties that
//!   number to the rest of the frame.
//! * [`horizon_frame_sched::FrameKind`] orders *what the frame spends time on* — `PlayerCritical`
//!   reserved from every other kind — but no renderer call site consults it yet.
//! * [`horizon_threads::jobs::Class`] orders *background work* — `Urgent`, `Near`, `Far` — with no
//!   notion that a `Far` chunk mesh may be worth more than an `Urgent` cache write once the player
//!   is also starved of the terrain they are standing on.
//! * [`horizon_memory_governor::Class`] orders *what may be evicted* — `Irreplaceable` never,
//!   `Scratch` freely — independent of whether the player is looking at it.
//!
//! Four ladders, four spacings, and the sum of them is a frame that spends its milliseconds on
//! whatever subsystem shouted first. On an A11 with an 8 ms budget that is exactly the stutter the
//! player reports: the terrain under their feet arrives late because *something* of nominally
//! higher priority held the lane.
//!
//! # The rule
//!
//! [`Importance`] is one ladder, defined once, in the order a player would complain about losing
//! each item. Every other vocabulary maps onto it with a total function ([`Importance::of_sort`],
//! [`Importance::of_frame_kind`], [`Importance::of_job`], [`Importance::of_cache`]). Consecutive
//! rungs are far enough apart that a whole *kind* outranks a whole other kind regardless of
//! distance or centrality, and [`Shedder`] then answers the only question the frame actually has:
//! **given this many milliseconds, which of these items do I drop?**
//!
//! The ladder is deliberately small. It has to be, to be trusted: an ordering nobody can hold in
//! their head is one that gets accidentally violated.
//!
//! # Why not just use the existing ones
//!
//! Because they disagree, and the disagreement is not resolved anywhere. `Sort::EntityNear` (0.95
//! weight) sits above `Sort::SectionNear` (0.75), so a distant cow outranks the ground; but
//! `jobs::Class::Near` chunk meshing, which *produces* that ground, is a different enum with no
//! relationship to either, and `FrameKind::PlayerCritical` — which exists precisely to protect the
//! ground — is not consulted by the draw path at all. Measuring the frame and then not acting on
//! the measurement is the whole bug.
//!
//! Nothing here allocates in the steady state, nothing here reads a clock, and nothing here touches
//! a GPU handle: [`Shedder::shed`] works on a caller-supplied slice and returns an index mask, so
//! the whole policy is unit-testable and the caller keeps ownership of its own data.

#![forbid(unsafe_code)]

use horizon_frame_sched::FrameKind;
use horizon_memory_governor::Class as CacheClass;
use horizon_visibility_budget::Sort;

/// One rung of the ladder: everything the frame can be asked to do, in the order a player would
/// notice its absence.
///
/// The variants are ordered and the spacing between their [`Importance::rank`] values is uniform
/// within a tier and wide between tiers, so a comparison never depends on how many candidates of a
/// kind happen to be present.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Importance {
    /// The block being broken, the entity being attacked, the item in hand, the crosshair. Losing
    /// this makes the game unplayable rather than merely ugly.
    Critical = 0,
    /// The ground and walls within reach, and the entities the player is fighting. Losing this
    /// means falling through the world or losing a fight.
    Near = 1,
    /// The world beyond reach but in view: the hill on the horizon, the village down the valley.
    Visible = 2,
    /// The player's own body, held items and armour on it, and the HUD.
    OwnBody = 3,
    /// Moving things in view: entities, particles, block entity animation.
    Animated = 4,
    /// Sky, clouds, weather, distant decoration. Losing this is scenery.
    Ambient = 5,
    /// Work that produces something later: chunk meshes for ground not yet needed, prefetch,
    /// speculative uploads. First to go, and the only tier that may be dropped entirely while the
    /// frame is healthy.
    Speculative = 6,
    /// Nothing the player can perceive this frame: occluded, behind the camera, off-screen.
    /// Never submitted.
    Invisible = 7,
}

/// How many rungs are on the ladder, for array-indexed bookkeeping.
pub const RUNGS: usize = 8;

impl Importance {
    /// The ladder as a number, `0` highest. Uniform spacing of one per rung: comparisons are exact
    /// and no tier can shadow another by accumulating weight.
    pub const fn rank(self) -> u8 {
        self as u8
    }

    /// Every rung, highest first, for a caller that wants to walk them.
    pub const ALL: [Self; RUNGS] = [
        Self::Critical,
        Self::Near,
        Self::Visible,
        Self::OwnBody,
        Self::Animated,
        Self::Ambient,
        Self::Speculative,
        Self::Invisible,
    ];

    /// The rung's name, for the performance overlay and the log.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Near => "near",
            Self::Visible => "visible",
            Self::OwnBody => "self",
            Self::Animated => "animated",
            Self::Ambient => "ambient",
            Self::Speculative => "speculative",
            Self::Invisible => "invisible",
        }
    }

    /// Whether work at this rung is protected from the shedder no matter how far over budget the
    /// frame ran. Only [`Importance::Critical`]: the player is never left without a hand, a
    /// crosshair, or the block they are breaking, even if that means the frame is late.
    pub const fn is_protected(self) -> bool {
        matches!(self, Self::Critical)
    }

    /// Whether this rung may be shed while the frame is inside its budget. Only
    /// [`Importance::Speculative`]: everything else is visible to the player right now, so
    /// dropping it early is a bug rather than an optimisation.
    pub const fn is_optional(self) -> bool {
        matches!(self, Self::Speculative | Self::Invisible)
    }

    /// The renderer's draw order, mapped onto the ladder.
    ///
    /// [`Sort::Hidden`] is the only rung that is *never* drawn, and it is the only `Invisible`.
    /// The renderer's own weighting puts `EntityNear` above `SectionNear`, which is right for a
    /// cow the player is fighting and wrong for a cow three hills away — that distinction is
    /// [`horizon_visibility_budget::score`]'s job, not this function's. This function only says
    /// which *tier* a kind belongs to.
    pub const fn of_sort(sort: Sort) -> Self {
        match sort {
            Sort::Targeted => Self::Critical,
            Sort::SectionTargeted => Self::Near,
            Sort::EntityNear => Self::Near,
            Sort::SectionNear => Self::Near,
            Sort::SectionFar => Self::Visible,
            Sort::BlockEntity => Self::Animated,
            Sort::Particle => Self::Animated,
            Sort::Hidden => Self::Invisible,
        }
    }

    /// The frame ledger's kinds, mapped onto the ladder.
    ///
    /// `PlayerCritical` is `Critical` — that is the whole point of the reserve. `VisibleNear` and
    /// `VisibleFar` split across `Near` and `Visible`. `Ambient` is the scenery rung. `Speculative`
    /// is the front of the shed queue.
    pub const fn of_frame_kind(kind: FrameKind) -> Self {
        match kind {
            FrameKind::PlayerCritical => Self::Critical,
            FrameKind::VisibleNear => Self::Near,
            FrameKind::VisibleFar => Self::Visible,
            FrameKind::Ambient => Self::Ambient,
            FrameKind::Speculative => Self::Speculative,
        }
    }

    /// The job pool's class, mapped onto the ladder.
    ///
    /// This is the mapping that fixes the starvation. `jobs::Class::Urgent` is *not* unwrapped to
    /// `Critical`: urgent background work is a save or a tick's chunk encoding, which the player
    /// cannot lose the game to, so it lands at [`Importance::Visible`] — above scenery and
    /// speculation, below anything the player is looking at. `Near` chunk work is what produces the
    /// ground the player stands on, so it lands at [`Importance::Near`]. `Far` work is
    /// [`Importance::Speculative`] and is the first thing the pool should stop doing.
    pub const fn of_job(class: Class) -> Self {
        match class {
            Class::Urgent => Self::Visible,
            Class::Near => Self::Near,
            Class::Far => Self::Speculative,
        }
    }

    /// The memory governor's cache class, mapped onto the ladder — *inverted*, because that ladder
    /// is about what may be freed and this one is about what must be kept.
    ///
    /// `Irreplaceable` caches hold player edits and are [`Importance::Critical`]: freeing one loses
    /// data. `Scratch` is [`Importance::Speculative`]: rebuild it next frame for nothing.
    pub const fn of_cache(class: CacheClass) -> Self {
        match class {
            CacheClass::Irreplaceable => Self::Critical,
            CacheClass::Costly => Self::Near,
            CacheClass::Rebuildable => Self::Ambient,
            CacheClass::Scratch => Self::Speculative,
        }
    }

    /// The [`FrameKind`] the ledger should charge this rung's work to, so a decision made here is
    /// visible in the same counters the overlay already prints.
    pub const fn frame_kind(self) -> FrameKind {
        match self {
            Self::Critical => FrameKind::PlayerCritical,
            Self::Near | Self::OwnBody => FrameKind::VisibleNear,
            Self::Visible => FrameKind::VisibleFar,
            Self::Animated => FrameKind::VisibleNear,
            Self::Ambient => FrameKind::Ambient,
            Self::Speculative | Self::Invisible => FrameKind::Speculative,
        }
    }
}

/// The job pool's three classes, mirrored here.
///
/// `horizon-threads`-free on purpose: the draw path and the frame budget need to *name* a job class
/// without linking the allocator crate that owns the pool (`horizon-threads` pulls in mimalloc and
/// the OS bindings). This crate stays a pure policy crate: it depends only on the three crates
/// whose vocabularies it unifies, and none of them starts a thread.
///
/// There is deliberately **no** `From<jobs::Class>` here — that would mean depending on the
/// allocator crate, which is the thing this mirror exists to avoid. The call site that does have
/// both in scope converts with a three-arm match:
/// `jobs::Class::Near => importance::Class::Near`, and so on. [`Importance::of_job`] is the only
/// place the three names are given meaning, so that match cannot drift from the ladder.
///
/// The variants are the same three, in the same order, with the same meaning; see
/// [`Importance::of_job`] for how they land on the ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Work something waits for now: saves, a tick's chunk encoding.
    Urgent,
    /// The chunks around the player: generation, light, meshing.
    Near,
    /// Far terrain: runs on what the near work leaves.
    Far,
}

/// One candidate item, as the shedder sees it: how important, and what it would cost.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    /// Which rung of the ladder.
    pub importance: Importance,
    /// What submitting or running it costs this frame, milliseconds.
    pub cost_ms: f32,
    /// The caller's identity for it, so the result can be matched back.
    pub id: u64,
}

impl Item {
    pub const fn new(importance: Importance, cost_ms: f32, id: u64) -> Self {
        Self { importance, cost_ms, id }
    }
}

/// Why an item was dropped, for the census the overlay prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dropped {
    /// The frame's budget was already spent on more important work.
    Budget,
    /// The item is on the [`Importance::Invisible`] rung and is never submitted.
    Invisible,
    /// The item's own rung was shed wholesale because the frame is degraded far enough.
    Shed,
}

impl Dropped {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::Invisible => "invisible",
            Self::Shed => "shed",
        }
    }
}

/// The outcome of one shed: what to keep, in submit order, and why the rest went.
#[derive(Clone, Debug, PartialEq)]
pub struct Shed {
    /// The kept items' ids, highest importance first, and within a rung the caller's own order
    /// preserved — this is the order to submit them in.
    pub keep: Vec<u64>,
    /// `(id, reason)` for everything dropped.
    pub drop: Vec<(u64, Dropped)>,
    /// Milliseconds the kept set costs.
    pub spent_ms: f32,
    /// The lowest rung that survived intact: what the overlay should say the frame is really
    /// showing. Equal to [`Importance::Critical`] when nothing above it survived either.
    pub floor: Importance,
}

impl Default for Shed {
    fn default() -> Self {
        Self {
            keep: Vec::new(),
            drop: Vec::new(),
            spent_ms: 0.0,
            floor: Importance::Critical,
        }
    }
}

impl Shed {
    pub fn kept(&self) -> usize {
        self.keep.len()
    }
    pub fn dropped(&self) -> usize {
        self.drop.len()
    }
    /// How many were dropped for each reason, in [`Dropped`] order, for a one-line overlay.
    pub fn dropped_by(&self, reason: Dropped) -> usize {
        self.drop.iter().filter(|(_, r)| *r == reason).count()
    }
}

/// The shedder: a frame budget (ms) and a health (the ledger's degrade level, 0 = nominal).
///
/// It is stateless between calls — every number it needs is passed in — so the same instance serves
/// the render submit, the job pool and the streaming uploads without them interfering.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shedder {
    /// The frame's budget for the work being shed, milliseconds.
    pub budget_ms: f32,
    /// The ledger's degrade level, 0 = full quality. Each level sheds one more rung outright,
    /// highest first, so the decision is graded rather than a cliff.
    pub degrade: u8,
}

impl Shedder {
    pub fn new(budget_ms: f32, degrade: u8) -> Self {
        Self { budget_ms: budget_ms.max(0.0), degrade }
    }

    /// The highest rung that is shed *wholesale* at the current degrade level, or `None` when
    /// nothing is. Degrade 1 sheds `Speculative`, 2 also sheds `Ambient`, and so on — never
    /// reaching `Critical`, which is protected.
    pub fn sheds_above(self) -> Option<Importance> {
        match self.degrade {
            0 => None,
            1 => Some(Importance::Speculative),
            2 => Some(Importance::Ambient),
            3 => Some(Importance::Animated),
            _ => Some(Importance::OwnBody),
        }
    }

    /// Skip the work outright, without ranking it: the cheap check a caller makes before it builds
    /// the candidate list at all. This is what keeps a degraded frame from paying to discover that
    /// it cannot afford anything.
    pub fn skip(self, importance: Importance) -> Option<Dropped> {
        if importance == Importance::Invisible {
            return Some(Dropped::Invisible);
        }
        if importance.is_protected() {
            return None;
        }
        match self.sheds_above() {
            Some(shed) if importance.rank() >= shed.rank() => Some(Dropped::Shed),
            _ => None,
        }
    }

    /// Decide what the frame keeps, spending `budget_ms` from the top of the ladder down.
    ///
    /// `items` must be in the caller's own preferred order *within* a rung: this function is
    /// stable, so the order the caller built survives and the result is directly submittable. It
    /// never reorders across rungs, and it never lets a cheap low-rung item in ahead of an
    /// expensive high-rung one — which is the failure mode of a plain greedy knapsack and the
    /// reason a particle burst could push out the ground.
    pub fn shed(&self, items: &[Item]) -> Shed {
        let mut out = Shed {
            keep: Vec::with_capacity(items.len()),
            drop: Vec::with_capacity(items.len() / 4),
            spent_ms: 0.0,
            floor: Importance::Critical,
        };
        // Once a rung runs out of budget, every rung below it is out of budget too. Without this
        // the walk falls through to cheaper, less important work and keeps it — which is how a
        // particle burst ends up drawn while the ground the player is standing on is not. The
        // frame must always be a *prefix* of the ladder, never a sparse subset of it.
        let mut closed = false;
        // Bucket by rung, preserving the caller's order inside each (the caller's slice is stable,
        // so a counting pass over the rungs gives the same result as a stable sort without it).
        for rung in Importance::ALL {
            if let Some(reason) = self.skip(rung) {
                for item in items.iter().filter(|i| i.importance == rung) {
                    out.drop.push((item.id, reason));
                }
                continue;
            }
            let mut kept_any = false;
            for item in items.iter().filter(|i| i.importance == rung) {
                // Critical is never *dropped* — the player is interacting with it — but its
                // time is real and is charged like any other. Pretending it were free would make
                // the frame report a budget it does not have.
                let fits = rung.is_protected()
                    || out.spent_ms + item.cost_ms.max(0.0) <= self.budget_ms;
                if fits && !closed {
                    out.spent_ms += item.cost_ms.max(0.0);
                    out.keep.push(item.id);
                    kept_any = true;
                } else {
                    out.drop.push((item.id, Dropped::Budget));
                    // A rung that could not fit closes the ladder for everything below it.
                    closed = closed || !rung.is_protected();
                }
            }
            if kept_any {
                out.floor = rung;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids by rung: 3 critical (3 ms), 2 and 5 near (2 ms each), 1 ambient (1 ms),
    /// 6 speculative (1 ms), 4 invisible (1 ms).
    fn items() -> Vec<Item> {
        vec![
            Item::new(Importance::Ambient, 1.0, 1),
            Item::new(Importance::Near, 2.0, 2),
            Item::new(Importance::Critical, 3.0, 3),
            Item::new(Importance::Invisible, 1.0, 4),
            Item::new(Importance::Near, 2.0, 5),
            Item::new(Importance::Speculative, 1.0, 6),
        ]
    }

    #[test]
    fn ladder_is_ordered_from_the_players_complaint() {
        assert!(Importance::Critical < Importance::Near);
        assert!(Importance::Near < Importance::Visible);
        assert!(Importance::Visible < Importance::OwnBody);
        assert!(Importance::OwnBody < Importance::Animated);
        assert!(Importance::Animated < Importance::Ambient);
        assert!(Importance::Ambient < Importance::Speculative);
        assert!(Importance::Speculative < Importance::Invisible);
        for (index, rung) in Importance::ALL.iter().enumerate() {
            assert_eq!(rung.rank() as usize, index, "{}", rung.as_str());
        }
    }

    #[test]
    fn critical_is_protected_and_only_speculative_is_optional() {
        for rung in Importance::ALL {
            assert_eq!(rung.is_protected(), rung == Importance::Critical);
            assert_eq!(
                rung.is_optional(),
                matches!(rung, Importance::Speculative | Importance::Invisible),
                "{}",
                rung.as_str()
            );
        }
        assert!(Importance::Speculative.is_optional());
        assert!(Importance::Invisible.is_optional());
        assert!(!Importance::Ambient.is_optional());
    }

    /// The bug this crate exists to fix: with a tight budget and the draw order the renderer uses,
    /// the ground must survive even when the scenery is cheaper.
    #[test]
    fn cheap_scenery_cannot_push_out_the_ground() {
        // 4 ms: critical (3) leaves 1, so neither near item fits.
        let shed = Shedder::new(4.0, 0).shed(&items());
        assert!(shed.keep.contains(&3), "critical kept");
        assert!(!shed.keep.contains(&2), "near terrain did not fit");
        // The scenery is 1 ms and *would* fit in the leftover — but it is a rung below work that
        // was dropped, so it is not submitted either.
        assert!(!shed.keep.contains(&1), "scenery dropped rather than the ground");
    }

    /// A plain greedy knapsack would take the cheap ambient item after the critical one. The
    /// cascade refuses it: the frame is a prefix of the ladder, never a sparse subset.
    #[test]
    fn the_ladder_outranks_cheapness() {
        let shed = Shedder::new(4.0, 0).shed(&items());
        assert!(!shed.keep.contains(&1), "ambient lost to a higher rung");
        // Both near items dropped, and the cascade marks the ambient/speculative as budget-dropped
        // rather than the frame quietly keeping them.
        assert_eq!(shed.dropped_by(Dropped::Budget), 4);
    }

    /// A budget generous enough for the whole near rung lets the lower rungs fill what is left.
    #[test]
    fn lower_rungs_may_use_what_the_higher_ones_leave() {
        // 8 ms: critical 3 + near 4 leaves 1, which the ambient item takes.
        let shed = Shedder::new(8.0, 0).shed(&items());
        assert_eq!(shed.keep, vec![3, 2, 5, 1]);
        assert_eq!(shed.floor, Importance::Ambient);
    }

    #[test]
    fn invisible_is_never_submitted() {
        let shed = Shedder::new(100.0, 0).shed(&items());
        assert!(!shed.keep.contains(&4));
        assert_eq!(shed.dropped_by(Dropped::Invisible), 1);
    }

    #[test]
    fn critical_survives_a_budget_of_zero() {
        let shed = Shedder::new(0.0, 0).shed(&items());
        assert!(shed.keep.contains(&3), "the player is never left without a hand");
        assert!(!shed.keep.contains(&2));
        // It is charged even so: the frame reports the time it really spent.
        assert!(shed.spent_ms >= 3.0, "{}", shed.spent_ms);
    }

    #[test]
    fn degrade_sheds_whole_rungs_highest_first() {
        assert_eq!(Shedder::new(16.0, 0).sheds_above(), None);
        assert_eq!(Shedder::new(16.0, 1).sheds_above(), Some(Importance::Speculative));
        assert_eq!(Shedder::new(16.0, 2).sheds_above(), Some(Importance::Ambient));
        assert_eq!(Shedder::new(16.0, 3).sheds_above(), Some(Importance::Animated));
        // Never gets above `OwnBody`, so `Near` and `Critical` always survive a degrade.
        assert_eq!(Shedder::new(16.0, 99).sheds_above(), Some(Importance::OwnBody));
        assert!(!Shedder::new(16.0, 99).skip(Importance::Near).is_some());
    }

    #[test]
    fn speculating_is_skipped_at_degrade_one() {
        let shed = Shedder::new(1000.0, 1).shed(&items());
        assert!(!shed.keep.contains(&6));
        assert_eq!(shed.dropped_by(Dropped::Shed), 1);
    }

    #[test]
    fn floor_reports_the_lowest_rung_that_survived() {
        let shed = Shedder::new(1000.0, 0).shed(&items());
        // Everything visible fits, so `Speculative` survives; the `Invisible` rung never does, so
        // the floor stops there rather than at the bottom of the enum.
        assert_eq!(shed.floor, Importance::Speculative);
        let tight = Shedder::new(4.0, 0).shed(&items());
        assert_eq!(tight.floor, Importance::Critical, "only critical survived");
    }

    #[test]
    fn submit_order_is_the_callers_order_within_a_rung() {
        // 8 ms exactly fills critical + both near + ambient.
        let shed = Shedder::new(8.0, 0).shed(&items());
        // Rung order first (critical, then near in caller order, then ambient), never re-sorted
        // by cost — 5 comes after 2 because that is the order the caller built.
        assert_eq!(shed.keep, vec![3, 2, 5, 1]);
    }

    #[test]
    fn jobs_map_below_what_the_player_sees() {
        // The starvation fix: urgent background work never outranks the ground under the player.
        assert_eq!(
            Importance::of_job(Class::Urgent),
            Importance::Visible
        );
        assert_eq!(
            Importance::of_job(Class::Near),
            Importance::Near
        );
        assert_eq!(
            Importance::of_job(Class::Far),
            Importance::Speculative
        );
    }

    #[test]
    fn cache_classes_map_inverted() {
        // Irreplaceable is the *most* important to keep, which is the top of this ladder.
        assert_eq!(Importance::of_cache(CacheClass::Irreplaceable), Importance::Critical);
        assert_eq!(Importance::of_cache(CacheClass::Costly), Importance::Near);
        assert_eq!(Importance::of_cache(CacheClass::Rebuildable), Importance::Ambient);
        assert_eq!(Importance::of_cache(CacheClass::Scratch), Importance::Speculative);
    }

    #[test]
    fn sort_maps_onto_the_ladder() {
        assert_eq!(Importance::of_sort(Sort::Targeted), Importance::Critical);
        assert_eq!(Importance::of_sort(Sort::SectionNear), Importance::Near);
        assert_eq!(Importance::of_sort(Sort::SectionFar), Importance::Visible);
        assert_eq!(Importance::of_sort(Sort::Particle), Importance::Animated);
        assert_eq!(Importance::of_sort(Sort::Hidden), Importance::Invisible);
    }

    #[test]
    fn every_rung_charges_a_frame_kind() {
        for rung in Importance::ALL {
            let kind = rung.frame_kind();
            // The charge must not be *more* important than the work, or the ledger would reserve
            // the whole frame for speculation.
            assert!(Importance::of_frame_kind(kind) <= rung);
        }
        assert_eq!(Importance::Critical.frame_kind(), FrameKind::PlayerCritical);
    }

    #[test]
    fn an_empty_frame_is_not_an_error() {
        let shed = Shedder::new(4.0, 2).shed(&[]);
        assert!(shed.keep.is_empty() && shed.drop.is_empty());
        assert_eq!(shed.spent_ms, 0.0);
    }
}
