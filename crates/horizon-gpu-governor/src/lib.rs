//! Keeps the GPU from being over-driven.
//!
//! # Two different jobs, often confused
//!
//! *Not drawing what nobody can see* is `horizon-visibility-budget`'s and the renderer's `cull`
//! module's job: they remove work. This crate does the other half — deciding **how much work the
//! GPU may be given at all**, and in what order, so that the work which does arrive finishes inside
//! the frame. A frame that removes 90 % of its geometry and then overdraws the remaining 10 % eight
//! times deep is still late.
//!
//! # What Apple's GPUs actually punish
//!
//! Apple silicon is a tile-based deferred renderer. Two consequences drive everything here, and
//! both are the opposite of the intuition from an immediate-mode renderer:
//!
//! 1. **Hidden surface removal is free and pixel-perfect.** The tile memory already removes opaque
//!    overdraw, so a depth prepass buys nothing — Xcode 12's Metal insights flags one as
//!    unnecessary on Apple GPUs, and measurement agrees: a two-phase occlusion scheme on an Apple
//!    GPU came out 0–1.9 ms *slower* per frame because the hidden instances were cheap far LODs
//!    whose pixels the tiler discarded anyway.
//! 2. **The tiler flushes when the pipeline changes in a way that ends the hidden-surface pass.**
//!    The canonical killer is a translucent command interleaved with opaque ones, or any
//!    depth-writing `discard`. Every flush reloads the whole frame's tile memory. So the *order* of
//!    submission is worth more than the count of pixels: opaque first, alpha-tested second,
//!    translucent last, never mixed.
//!
//! What a tiler *is* short of is bin-list capacity and geometry throughput, so draw calls and
//! triangles are the budgets that matter, not fragment count. [`Budget`] therefore charges all
//! three, and [`Batches`] is what the renderer walks before it submits.
//!
//! # The control loop
//!
//! [`Governor`] watches the GPU's own frame time and answers with a resolution scale, in the same
//! shape as the frame ledger's degrade level: raise load only after sustained headroom, lower it
//! immediately when the frame is late. The asymmetry is deliberate — a late frame is felt now, a
//! slightly soft one is not — and the hysteresis (`RAISE_AFTER`, `LOWER_AFTER`) is what stops the
//! scale pumping, which is far more visible than either end state.
//!
//! Nothing here owns a GPU handle or reads a clock: the caller passes the measurements in, so the
//! whole policy is unit-testable.

#![forbid(unsafe_code)]

use horizon_importance::Importance;

/// Which pass of the frame a draw belongs to. **The order is the submission order** and it is the
/// whole reason this enum exists: see the module docs on tiler flushes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bucket {
    /// Writes depth, discards nothing. Apple's HSR removes the overdraw inside this bucket for
    /// free, so it is cheapest first and widest.
    Opaque = 0,
    /// Writes depth and discards: leaves, cutout foliage, fences, panes, and entities that use an
    /// alpha cutout. **Must** be its own bucket — a `discard` ends the hidden-surface pass.
    AlphaTest = 1,
    /// Does not write depth, blends. Drawn last, in back-to-front order, and never allowed to
    /// interleave with the two above.
    Translucent = 2,
}

/// How many buckets there are, for array-indexed counters.
pub const BUCKETS: usize = 3;

impl Bucket {
    pub const ALL: [Self; BUCKETS] = [Self::Opaque, Self::AlphaTest, Self::Translucent];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opaque => "opaque",
            Self::AlphaTest => "alpha-test",
            Self::Translucent => "translucent",
        }
    }

    /// Whether this bucket's draws write depth. Both opaque and alpha-tested do; translucent does
    /// not, which is why it cannot go first even if it were cheaper.
    pub const fn writes_depth(self) -> bool {
        !matches!(self, Self::Translucent)
    }
}

/// What one draw costs, as the budget charges it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    /// Triangles the draw rasterises (post-cull, so this is the real number).
    pub triangles: u32,
    /// Vertices it reads.
    pub vertices: u32,
    /// Its share of the frame's milliseconds. The renderer measures this or estimates it from the
    /// triangle count; the governor only needs it to be consistent between draws.
    pub ms: f32,
}

impl Cost {
    pub const fn new(triangles: u32, vertices: u32, ms: f32) -> Self {
        Self { triangles, vertices, ms }
    }
}

/// One draw, as the renderer has already decided to submit it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Draw {
    pub bucket: Bucket,
    pub cost: Cost,
    /// The draw's importance, from [`horizon_importance`]. The budget spends top-down.
    pub importance: Importance,
    /// The caller's identity for it, so the kept list can be matched back.
    pub id: u64,
}

impl Draw {
    pub const fn new(bucket: Bucket, cost: Cost, importance: Importance, id: u64) -> Self {
        Self { bucket, cost, importance, id }
    }
}

/// The per-frame ceilings. Every one of them is a *budget*, not a target: the frame aims to spend
/// less.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    /// Milliseconds the GPU may take for the world pass.
    pub ms: f32,
    /// Draw calls. The tiler's bin list and the CPU's encoding both scale with this, and it is the
    /// number that hurts first on a phone.
    pub draws: u32,
    /// Triangles submitted.
    pub triangles: u32,
}

impl Budget {
    /// A budget for a frame time and a device tier. The draw and triangle ceilings scale with the
    /// milliseconds, because on a fixed chip those three are proportional: half the time is half
    /// the geometry a given shader can chew.
    ///
    /// The constants are calibrated for an A11 at 60 Hz (16.7 ms), the strictest target the game
    /// supports; a desktop gets proportionally more by passing a larger `ms`.
    pub fn for_frame(ms: f32) -> Self {
        let ms = ms.max(0.5);
        // ~120 draws per millisecond of world pass and ~30 k triangles per millisecond are the
        // order of magnitude measured on the A11-class GPU the game targets; both are deliberately
        // conservative, because a budget that is occasionally generous is a budget that is
        // occasionally a dropped frame.
        Self {
            ms,
            draws: (ms * 120.0) as u32,
            triangles: (ms * 30_000.0) as u32,
        }
    }

    /// The share of the budget available to one importance rung, so a particle burst cannot spend
    /// the world's triangles. High rungs get the whole budget; speculation gets almost none.
    pub fn share_for(importance: Importance) -> f32 {
        match importance {
            Importance::Critical => 1.0,
            Importance::Near => 0.85,
            Importance::Visible => 0.7,
            Importance::OwnBody => 1.0,
            Importance::Animated => 0.45,
            Importance::Ambient => 0.25,
            Importance::Speculative => 0.1,
            Importance::Invisible => 0.0,
        }
    }
}

/// What the renderer walks: the draws that fit, grouped into the three submission buckets.
///
/// The grouping is the point. A caller that submits `bucket(0)`, then `1`, then `2` gets the tile
/// flushes out of its frame; a caller that submits in the order the world happened to be walked
/// gets one per interleave.
#[derive(Clone, Debug, PartialEq)]
pub struct Batches {
    /// Kept draws, one `Vec` per [`Bucket`], each in the caller's own order. Submit 0, then 1,
    /// then 2.
    pub buckets: [Vec<u64>; BUCKETS],
    /// What the kept draws cost together.
    pub spent: Cost,
    /// How many draws were dropped, by bucket.
    pub dropped: [u32; BUCKETS],
    /// The lowest importance rung that survived.
    pub floor: Importance,
}

impl Default for Batches {
    fn default() -> Self {
        Self {
            buckets: [Vec::new(), Vec::new(), Vec::new()],
            spent: Cost::default(),
            dropped: [0; BUCKETS],
            // Nothing kept yet, so the floor is the top of the ladder.
            floor: Importance::Critical,
        }
    }
}

impl Batches {
    /// The draws to submit, in bucket order, with each bucket's own list intact.
    pub fn iter(&self) -> impl Iterator<Item = (Bucket, &Vec<u64>)> {
        Bucket::ALL.into_iter().zip(self.buckets.iter())
    }

    /// Every kept draw, in submission order.
    pub fn flatten(&self) -> Vec<u64> {
        self.buckets.iter().flatten().copied().collect()
    }

    pub fn kept(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    pub fn dropped_total(&self) -> u32 {
        self.dropped.iter().sum()
    }

    /// The number of times the renderer must change pipeline state in a way that flushes a tiler,
    /// which is the number of non-empty buckets minus one. Reported so the overlay can show when a
    /// content change has made batching worse.
    pub fn flushes(&self) -> usize {
        self.buckets.iter().filter(|b| !b.is_empty()).count().saturating_sub(1)
    }
}

/// Spends a [`Budget`] over a frame's draws, in importance order, and sorts what survives into
/// buckets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scheduler {
    pub budget: Budget,
}

impl Scheduler {
    pub fn new(budget: Budget) -> Self {
        Self { budget }
    }

    /// Choose what the frame submits.
    ///
    /// The walk is by importance rung, highest first — the same ladder `horizon-importance` defines
    /// — and within a rung the caller's order is preserved. A rung that cannot fit closes the
    /// ladder for everything below it, exactly as the shedder does, so the frame is always a prefix
    /// of the priority order rather than a cheap scattered subset.
    ///
    /// Crucially the *output* is then regrouped: importance decided **what**, the bucket decides
    /// **when**.
    pub fn schedule(&self, draws: &[Draw]) -> Batches {
        let mut out = Batches {
            floor: Importance::Critical,
            ..Batches::default()
        };
        let mut spent = Cost::default();
        let mut closed = false;

        for rung in Importance::ALL {
            if rung == Importance::Invisible {
                // Never submitted, and it never keeps a lower rung alive either.
                closed = closed || draws.iter().any(|d| d.importance == rung);
                continue;
            }
            let share = Budget::share_for(rung);
            let rung_ms = self.budget.ms * share;
            let rung_draws = (self.budget.draws as f32 * share) as u32;
            let rung_tris = (self.budget.triangles as f32 * share) as u32;
            let mut kept_any = false;

            for draw in draws.iter().filter(|d| d.importance == rung) {
                let fits = !closed
                    && spent.ms + draw.cost.ms <= rung_ms
                    && (out.kept() as u32) < rung_draws
                    && spent.triangles + draw.cost.triangles <= rung_tris;
                if fits {
                    out.buckets[draw.bucket as usize].push(draw.id);
                    spent.ms += draw.cost.ms;
                    spent.triangles += draw.cost.triangles;
                    spent.vertices += draw.cost.vertices;
                    kept_any = true;
                } else {
                    out.dropped[draw.bucket as usize] += 1;
                    closed = true;
                }
            }
            if kept_any {
                out.floor = rung;
            }
        }
        out.spent = spent;
        out
    }
}

/// The GPU frame-time governor: resolution scale from measured frame time.
///
/// Same shape as the frame ledger's `degrade_level`, and composed with it rather than replacing it:
/// the ledger decides what the *frame* may spend on each kind of work, this decides how many pixels
/// the GPU may spend them on.
#[derive(Clone, Debug, PartialEq)]
pub struct Governor {
    /// The frame's target, milliseconds (the display period, or the ledger's total).
    target_ms: f32,
    /// The scale currently applied, 1.0 = native.
    scale: f32,
    /// The lowest scale allowed: below this the picture is too soft to read the HUD, and a blurry
    /// game at 60 fps reads worse than a sharp one at 50.
    floor: f32,
    /// Smoothed GPU frame time, the input to every decision.
    smoothed: f32,
    /// Consecutive frames under target, and over target.
    under: u32,
    over: u32,
    /// The best scale the device has sustained, so the governor does not crawl back to a level it
    /// already knows is too much. It decays back up under sustained evidence, because a failure
    /// recorded during one bad patch — a chunk load burst, a phone that was in the sun — must not
    /// pin the game to half resolution for the rest of the run.
    ceiling: f32,
    /// Frames of sustained headroom at the ceiling: what buys the ceiling its next step back up.
    clean: u32,
    /// How fast the smoothed time follows the measurement. A field rather than a constant so a
    /// headless soak test can drive the loop at a realistic rate instead of at 60 Hz of simulated
    /// frames.
    smoothing: f32,
}

/// Frames under the target before the scale rises one step. Twenty at 60 Hz is a third of a second
/// of sustained headroom, long enough that a single easy frame (the player looking at the sky) does
/// not raise it.
const RAISE_AFTER: u32 = 20;

/// Frames over the target before the scale drops. Two, because a late frame is felt immediately and
/// the cost of dropping is a slightly softer picture, not a stall.
const LOWER_AFTER: u32 = 2;

/// How far one step moves the scale. Small enough that a step is not visible as a pop.
const STEP: f32 = 0.05;

/// Frames of sustained headroom *at the ceiling* before the ceiling itself rises one step. Much
/// longer than `RAISE_AFTER`, because it undoes a recorded failure: two seconds at 60 Hz of
/// genuinely clean frames before the governor believes a level it already lost is available again.
///
/// Two seconds rather than five: the scale is already pinned at the ceiling by the time this
/// counter runs, so a device with headroom is visibly soft and the player is waiting on it. The
/// evidence is still much stronger than a single dropped step — 120 frames, not 20.
const RECOVER_AFTER: u32 = 120;

/// How quickly the smoothed frame time follows the measurement. Slow enough to ignore one spike
/// (a chunk mesh landing) and fast enough to catch a thermal step within a few frames.
const SMOOTHING: f32 = 0.15;

impl Governor {
    /// A governor for a target frame time, native scale.
    pub fn new(target_ms: f32) -> Self {
        let target_ms = target_ms.max(1.0);
        Self {
            target_ms,
            scale: 1.0,
            floor: 0.5,
            smoothed: target_ms,
            under: 0,
            over: 0,
            ceiling: 1.0,
            clean: 0,
            smoothing: SMOOTHING,
        }
    }

    /// A governor whose smoothing is `smoothing` per frame. Only for tests and soak runs: the
    /// renderer uses [`Governor::new`], whose rate is tuned for 60 Hz.
    pub fn with_smoothing(mut self, smoothing: f32) -> Self {
        self.smoothing = smoothing.clamp(0.001, 1.0);
        self
    }

    /// The lowest scale this governor will apply. Dynamic Resolution and the thermal governor both
    /// want a say; the caller composes them by taking the smallest.
    pub fn with_floor(mut self, floor: f32) -> Self {
        self.floor = floor.clamp(0.25, 1.0);
        self.scale = self.scale.max(self.floor);
        self
    }

    /// The scale to render at, 0.25..=1.0.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The smoothed GPU frame time the last decision used, ms.
    pub fn frame_ms(&self) -> f32 {
        self.smoothed
    }

    /// The frame's target, ms.
    pub fn target_ms(&self) -> f32 {
        self.target_ms
    }

    /// A new target (a different display, a thermal step, a resolution change).
    pub fn set_target(&mut self, target_ms: f32) {
        self.target_ms = target_ms.max(1.0);
    }

    /// The best scale this device has sustained. Once the governor has had to drop below a level,
    /// it does not climb back past it without a long, clean run — the alternative is a device that
    /// finds the edge every few seconds and visibly oscillates there.
    pub fn ceiling(&self) -> f32 {
        self.ceiling
    }

    /// One frame's GPU time. Returns whether the scale changed, so the caller only reconfigures the
    /// swapchain and its targets on a real transition.
    pub fn observe(&mut self, gpu_ms: f32) -> bool {
        if !gpu_ms.is_finite() || gpu_ms <= 0.0 {
            return false;
        }
        self.smoothed += (gpu_ms - self.smoothed) * self.smoothing;
        let before = self.scale;

        if self.smoothed > self.target_ms {
            self.under = 0;
            self.over = self.over.saturating_add(1);
            if self.over >= LOWER_AFTER {
                self.over = 0;
                // Drop proportionally to how far over: a frame 2x over needs more than one step,
                // or the governor spends ten frames catching up while the player watches it
                // stutter.
                let ratio = (self.smoothed / self.target_ms).clamp(1.0, 4.0);
                let step = STEP * ratio;
                self.scale = (self.scale - step).max(self.floor);
                // Remember the level it could not hold — but never *because* it is already at the
                // floor. A device pinned at its floor has no information left to give, and
                // recording the floor as the ceiling would make the state permanent: the game
                // would stay at half resolution for the rest of the run after one bad second.
                if self.scale > self.floor {
                    self.ceiling = self.ceiling.min(self.scale);
                }
                self.clean = 0;
            }
        } else {
            self.over = 0;
            self.under = self.under.saturating_add(1);
            // Only raise while there is real headroom, and only up to the ceiling this device is
            // known to sustain.
            // Sustained headroom twice over is what buys the *ceiling* back, independent of the
            // scale's own step: the evidence that a level is available again has to be stronger
            // than the evidence that raised the scale to it.
            if self.smoothed < self.target_ms * 0.9 {
                self.clean = self.clean.saturating_add(1);
                if self.clean >= RECOVER_AFTER && self.ceiling < 1.0 {
                    self.clean = 0;
                    self.ceiling = (self.ceiling + STEP).min(1.0);
                }
            } else {
                self.clean = 0;
            }
            if self.under >= RAISE_AFTER && self.smoothed < self.target_ms * 0.9 {
                self.under = 0;
                self.scale = (self.scale + STEP).min(self.ceiling).min(1.0);
            }
        }
        (self.scale - before).abs() > f32::EPSILON
    }

    /// The GPU was reset, the swapchain was recreated, or the world changed: forget the history but
    /// keep the ceiling, which is a fact about the device rather than about the frame.
    pub fn scene_reset(&mut self) {
        self.smoothed = self.target_ms;
        self.under = 0;
        self.over = 0;
        self.clean = 0;
    }

    /// A thermal step or a different device: forget the ceiling too.
    pub fn reset(&mut self) {
        self.scale = 1.0;
        self.ceiling = 1.0;
        self.scene_reset();
    }

    /// A one-line description for the performance overlay.
    pub fn describe(&self) -> String {
        format!(
            "GPU {:.1}/{:.1} ms at {:.0}% scale (ceiling {:.0}%)",
            self.smoothed,
            self.target_ms,
            self.scale * 100.0,
            self.ceiling * 100.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(bucket: Bucket, tris: u32, ms: f32, importance: Importance, id: u64) -> Draw {
        Draw::new(bucket, Cost::new(tris, tris * 2, ms), importance, id)
    }

    #[test]
    fn buckets_are_in_submission_order() {
        assert!(Bucket::Opaque < Bucket::AlphaTest);
        assert!(Bucket::AlphaTest < Bucket::Translucent);
        assert!(Bucket::Opaque.writes_depth());
        assert!(Bucket::AlphaTest.writes_depth());
        assert!(!Bucket::Translucent.writes_depth());
    }

    /// The whole reason for the buckets: whatever order the world was walked in, the submission is
    /// opaque → alpha-tested → translucent.
    #[test]
    fn submission_is_regrouped_even_when_the_walk_interleaved() {
        let draws = vec![
            draw(Bucket::Translucent, 100, 0.1, Importance::Near, 1),
            draw(Bucket::Opaque, 100, 0.1, Importance::Near, 2),
            draw(Bucket::AlphaTest, 100, 0.1, Importance::Near, 3),
            draw(Bucket::Opaque, 100, 0.1, Importance::Near, 4),
        ];
        let batches = Scheduler::new(Budget::for_frame(16.0)).schedule(&draws);
        assert_eq!(batches.buckets[Bucket::Opaque as usize], vec![2, 4]);
        assert_eq!(batches.buckets[Bucket::AlphaTest as usize], vec![3]);
        assert_eq!(batches.buckets[Bucket::Translucent as usize], vec![1]);
        // Flattened, that is opaque first regardless of what came first in the input.
        assert_eq!(batches.flatten(), vec![2, 4, 3, 1]);
    }

    #[test]
    fn a_well_batched_frame_reports_no_extra_flushes() {
        let draws = vec![
            draw(Bucket::Opaque, 100, 0.1, Importance::Near, 1),
            draw(Bucket::Opaque, 100, 0.1, Importance::Near, 2),
        ];
        let batches = Scheduler::new(Budget::for_frame(16.0)).schedule(&draws);
        assert_eq!(batches.flushes(), 0, "one bucket, nothing to flush between");
    }

    #[test]
    fn all_three_buckets_cost_two_flushes() {
        let draws = vec![
            draw(Bucket::Opaque, 10, 0.1, Importance::Near, 1),
            draw(Bucket::AlphaTest, 10, 0.1, Importance::Near, 2),
            draw(Bucket::Translucent, 10, 0.1, Importance::Near, 3),
        ];
        let batches = Scheduler::new(Budget::for_frame(16.0)).schedule(&draws);
        assert_eq!(batches.flushes(), 2);
    }

    /// A budget that cannot fit everything must drop from the bottom of the ladder, never the top.
    #[test]
    fn the_budget_drops_the_least_important_first() {
        let draws = vec![
            draw(Bucket::Opaque, 1000, 1.0, Importance::Critical, 1),
            draw(Bucket::Opaque, 1000, 1.0, Importance::Animated, 2),
            draw(Bucket::Translucent, 1000, 1.0, Importance::Ambient, 3),
        ];
        // 2 ms: critical fits (1 ms, and critical gets the whole budget), animated does not.
        let batches = Scheduler::new(Budget::for_frame(2.0)).schedule(&draws);
        assert!(batches.flatten().contains(&1));
        assert!(!batches.flatten().contains(&2));
        assert!(!batches.flatten().contains(&3));
    }

    #[test]
    fn invisible_draws_are_never_submitted() {
        let draws = vec![draw(Bucket::Opaque, 10, 0.1, Importance::Invisible, 9)];
        let batches = Scheduler::new(Budget::for_frame(16.0)).schedule(&draws);
        assert!(batches.flatten().is_empty());
    }

    #[test]
    fn the_draw_ceiling_is_the_one_that_bites_first() {
        // A budget of 1 ms is 120 draws and 30 000 triangles. Give it tiny triangles and plenty of
        // importance: the draw count, not the triangle count, must stop it.
        let draws: Vec<Draw> = (0..300)
            .map(|id| draw(Bucket::Opaque, 1, 0.001, Importance::Critical, id))
            .collect();
        let batches = Scheduler::new(Budget::for_frame(1.0)).schedule(&draws);
        assert_eq!(batches.kept(), 120);
        assert_eq!(batches.dropped_total(), 180);
    }

    #[test]
    fn an_empty_frame_is_not_an_error() {
        let batches = Scheduler::new(Budget::for_frame(16.0)).schedule(&[]);
        assert_eq!(batches.kept(), 0);
        assert_eq!(batches.flushes(), 0);
    }

    // ----- the governor -----

    #[test]
    fn a_frame_inside_the_target_keeps_native_scale() {
        let mut governor = Governor::new(16.7);
        for _ in 0..60 {
            governor.observe(10.0);
        }
        // It has headroom, but it is already at 1.0: nothing to raise.
        assert_eq!(governor.scale(), 1.0);
    }

    #[test]
    fn a_late_frame_lowers_the_scale() {
        let mut governor = Governor::new(16.7);
        // Two frames over the target, which is the threshold.
        governor.observe(30.0);
        governor.observe(30.0);
        assert!(governor.scale() < 1.0, "{}", governor.scale());
    }

    /// A frame twice as late as the target must drop further than one barely over it: the governor
    /// scales its step with how badly it missed.
    #[test]
    fn the_step_scales_with_how_late_the_frame_is() {
        let mut mild = Governor::new(16.7);
        let mut bad = Governor::new(16.7);
        for _ in 0..2 {
            mild.observe(18.0);
            bad.observe(60.0);
        }
        assert!(bad.scale() < mild.scale(), "{} < {}", bad.scale(), mild.scale());
    }

    #[test]
    fn the_scale_never_goes_below_the_floor() {
        let mut governor = Governor::new(16.7);
        for _ in 0..500 {
            governor.observe(200.0);
        }
        assert_eq!(governor.scale(), governor.floor);
    }

    /// Sustained headroom raises the scale, but only after many frames — one easy frame (the
    /// player looking at the sky) must not raise it.
    #[test]
    fn one_easy_frame_does_not_raise_the_scale() {
        let mut governor = Governor::new(16.7);
        // Push it down first.
        for _ in 0..10 {
            governor.observe(40.0);
        }
        let low = governor.scale();
        governor.observe(5.0);
        assert_eq!(governor.scale(), low, "a single frame is not headroom");
    }

    #[test]
    fn sustained_headroom_raises_the_scale() {
        // A faster smoothing rate, so the loop converges inside a test: at the renderer's 0.15 the
        // smoothed time needs about fifty frames to reach a new level, which is the right tuning
        // for a phone and the wrong one for a unit test.
        let mut governor = Governor::new(16.7).with_smoothing(0.5);
        // Over the target but not far enough that the drop reaches the floor, so the ceiling
        // records a real level rather than the device giving up.
        for _ in 0..8 {
            governor.observe(22.0);
        }
        let low = governor.scale();
        assert!(low < 1.0 && low > governor.floor, "{low}");
        for _ in 0..(RECOVER_AFTER * 6) {
            governor.observe(6.0);
        }
        assert!(governor.scale() > low, "{} > {}", governor.scale(), low);
    }

    /// One bad second must not pin the game to half resolution for the rest of the run: the
    /// ceiling recovers under sustained evidence.
    #[test]
    fn the_ceiling_decays_back_up_after_a_bad_patch() {
        let mut governor = Governor::new(16.7);
        // Drive it hard to the floor: a chunk load burst, a phone in the sun.
        for _ in 0..200 {
            governor.observe(120.0);
        }
        assert_eq!(governor.scale(), governor.floor);
        // A long clean run brings it back, ceiling included.
        for _ in 0..(RECOVER_AFTER * 12) {
            governor.observe(3.0);
        }
        assert!(
            governor.scale() > governor.floor,
            "still pinned at {:?}",
            governor.scale()
        );
    }

    /// A device that is genuinely at its limit stays low: the recovery must not be a free pass.
    #[test]
    fn a_device_that_stays_hot_stays_low() {
        let mut governor = Governor::new(16.7);
        for _ in 0..400 {
            // Alternating: some headroom, never enough to be clean.
            governor.observe(if governor.scale() > 0.6 { 30.0 } else { 16.0 });
        }
        assert!(governor.scale() < 0.9, "{}", governor.scale());
    }

    /// The hysteresis that stops the pumping: once the device has failed to hold a level, it is not
    /// allowed straight back up to it.
    #[test]
    fn the_ceiling_remembers_what_the_device_could_not_hold() {
        let mut governor = Governor::new(16.7);
        for _ in 0..6 {
            governor.observe(35.0);
        }
        let after_drop = governor.scale();
        assert!(governor.ceiling() <= after_drop + f32::EPSILON);
        // A long clean run cannot climb back past what it failed to hold.
        for _ in 0..(RAISE_AFTER * 20) {
            governor.observe(4.0);
        }
        assert!(
            governor.scale() <= governor.ceiling() + f32::EPSILON,
            "{} <= {}",
            governor.scale(),
            governor.ceiling()
        );
    }

    #[test]
    fn a_bogus_measurement_is_ignored() {
        let mut governor = Governor::new(16.7);
        assert!(!governor.observe(f32::NAN));
        assert!(!governor.observe(0.0));
        assert!(!governor.observe(-5.0));
        assert_eq!(governor.scale(), 1.0);
    }

    #[test]
    fn a_scene_reset_keeps_the_ceiling_but_a_full_reset_does_not() {
        let mut governor = Governor::new(16.7);
        for _ in 0..10 {
            governor.observe(40.0);
        }
        let ceiling = governor.ceiling();
        assert!(ceiling < 1.0);
        governor.scene_reset();
        assert_eq!(governor.ceiling(), ceiling);
        governor.reset();
        assert_eq!(governor.ceiling(), 1.0);
        assert_eq!(governor.scale(), 1.0);
    }

    #[test]
    fn the_description_is_one_line() {
        let governor = Governor::new(16.7);
        let text = governor.describe();
        assert!(text.contains("ms"));
        assert!(text.contains('%'));
        assert!(!text.contains('\n'));
    }

    #[test]
    fn the_budget_scales_with_the_frame() {
        let sixty = Budget::for_frame(16.7);
        let half = Budget::for_frame(8.35);
        assert!(sixty.draws > half.draws);
        assert!(sixty.triangles > half.triangles);
    }

    /// Particles must not be able to spend the world's triangles.
    #[test]
    fn an_ambient_rung_cannot_spend_the_worlds_budget() {
        let draws = vec![
            draw(Bucket::Opaque, 1000, 1.0, Importance::Near, 1),
            draw(Bucket::Translucent, 100_000, 5.0, Importance::Ambient, 2),
        ];
        let batches = Scheduler::new(Budget::for_frame(4.0)).schedule(&draws);
        assert!(batches.flatten().contains(&1));
        assert!(!batches.flatten().contains(&2), "ambient over its triangle share");
    }
}
