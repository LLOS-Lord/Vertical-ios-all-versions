//! How many of the frame's section draws the GPU is actually given.
//!
//! # What this decides, and what it deliberately does not
//!
//! The renderer already submits in the order Apple's tile-based GPUs want: solid terrain in its own
//! pass, translucent in another, the mirror and the shadow camera each in theirs. A translucent
//! draw is never interleaved with an opaque one, so the hidden-surface pass is never flushed early
//! by the draw order. There is therefore nothing here for a *bucket* sort to fix — which is why this
//! module does not add one.
//!
//! What the renderer had no answer for is the frame where the GPU is simply given too much: a long
//! render distance, a player who has just teleported, a phone that has been in the sun. The draw
//! list was whatever the visibility walk produced, and the only response to a late frame was
//! Dynamic Resolution, which lowers the *pixel* count and leaves the number of draw calls, bind
//! groups and uniform writes untouched — precisely the things a tiler is short of.
//!
//! [`horizon_gpu_governor::Governor`] measures the GPU's own frame time and answers with a scale.
//! This module is the editor that applies it to the draw list: it **cuts the tail** of each range,
//! never holes in the middle.
//!
//! # Why a tail cut rather than a priority pick
//!
//! `Renderer::draws` is not a bag of independent draws. It is a sequence of contiguous *runs*, each
//! run sharing an arena page and an index page, because `draw_sections_from` binds one pipeline and
//! one pair of buffers per page and then issues a whole run. The solid range is a prefix
//! (`0..solid_draws`), the translucent range follows it, and within the solid range
//! `phase1_draws..phase2_cutout` is opaque while the rest is cutout.
//!
//! Two consequences follow, and both are load-bearing:
//!
//! 1. **Removing a draw from the middle is free but not worth it.** It would leave the page bindings
//!    untouched and save one `draw_indexed`, which is what the GPU was going to skip anyway once its
//!    bins proved empty.
//! 2. **Cutting the tail is worth a great deal.** A run of 200 draws on one page becomes one
//!    `multi_draw_indexed_indirect` call. Halving the list halves that call, and on Apple hardware
//!    the geometry/bin-list cost is the bottleneck rather than the fragment cost.
//!
//! So the honest lever is a prefix length, and that is what [`Plan`] carries.
//!
//! # Why the far end
//!
//! The list is already ordered nearest-first by the visibility walk, so the tail is the far field:
//! the sections at the render distance's edge, whose loss is a shorter view rather than a hole
//! around the player. Cutting the tail is the same trade Dynamic Resolution makes, applied to
//! geometry instead of pixels — and unlike resolution it is invisible on a device that was already
//! going to drop the frame.
//!
//! # When it does nothing
//!
//! A frame the governor is happy with keeps its whole draw list: [`Plan::solid`] is the full range
//! and the call site's range arithmetic reduces to what it was before this module existed. The
//! measured path is the exception, not the default — a renderer that shortens every frame would be
//! drawing a permanently shorter view to solve a problem it did not have.

use horizon_gpu_governor::Governor;

/// The fewest solid draws worth submitting. Below this the near field itself is being cut, which
/// is the point at which a shorter view stops being a graceful degradation, so the plan clamps
/// here rather than following the scale to nothing.
const MIN_SOLID_DRAWS: u32 = 8;

/// What the GPU is given this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// How many of the frame's solid terrain draws to submit: `0..solid`.
    pub solid: u32,
    /// How many of the frame's translucent terrain draws to submit: `0..translucent`.
    pub translucent: u32,
    /// The resolution scale the governor asked for, which Dynamic Resolution composes with its own
    /// measurement. Reported whether or not this plan shortened anything.
    pub scale: f32,
}

impl Plan {
    /// The plan that shortens nothing: every draw the walk produced.
    pub fn full(solid: u32, translucent: u32, scale: f32) -> Self {
        Self { solid, translucent, scale }
    }

    /// The plan for one frame.
    ///
    /// `solid` and `translucent` are the range lengths the walk produced. `scale` is the
    /// governor's answer, `0.25..=1.0`, applied to the solid range only: translucent draws are
    /// water and glass the near field is judged by, they are a handful compared with the solid
    /// list, and dropping them first would empty the lakes around the player before it shortened
    /// the horizon.
    pub fn of(solid: u32, translucent: u32, scale: f32) -> Self {
        let scale = if scale.is_finite() { scale.clamp(0.25, 1.0) } else { 1.0 };
        if scale >= 1.0 || solid <= MIN_SOLID_DRAWS {
            return Self::full(solid, translucent, scale);
        }
        // The scaled count, never below the near-field floor and never above what there is.
        let wanted = (solid as f32 * scale) as u32;
        let kept = wanted.max(MIN_SOLID_DRAWS).min(solid);
        Self { solid: kept, translucent, scale }
    }

    /// Whether this plan submits less than the walk produced.
    pub fn shortened(&self, solid: u32) -> bool {
        self.solid < solid
    }
}

/// The frame-time governor, as the renderer's own.
///
/// It owns no GPU handle and reads no clock: `observe` is handed the GPU time the renderer already
/// collects into `gpu_times`, and the answer is a scale in `Plan`.
#[derive(Debug)]
pub struct DrawBudget {
    /// The policy.
    governor: Governor,
    /// The scale last handed to the caller, so a change is only acted on once.
    applied: f32,
    /// Frames in a row the GPU traced longer than the display's period. This — not "the plan
    /// shortened something" — is what "the device is struggling" means: the plan keeps shortening
    /// for as long as the scale is below one, which is hundreds of frames after the device has
    /// recovered, and reporting that as struggling would be saying something untrue.
    over: u32,
    /// Frames observed, and how many of them carried a GPU time. `observed == 0` after hundreds of
    /// frames means the platform never gave a measurement, which is a different report from a fast
    /// device and must not be shown as one.
    frames: u64,
    observed: u64,
}

impl DrawBudget {
    /// A governor for a target frame time (the display period, in milliseconds).
    pub fn new(target_ms: f32) -> Self {
        Self {
            governor: Governor::new(target_ms),
            frames: 0,
            observed: 0,
            applied: 1.0,
            over: 0,
        }
    }

    /// The display changed, or the frame ledger set a new total: retarget without forgetting what
    /// the device has sustained.
    pub fn set_target(&mut self, target_ms: f32) {
        self.governor.set_target(target_ms);
    }

    /// A new world or a rebuilt swapchain: forget this frame's history but keep the ceiling.
    pub fn scene_reset(&mut self) {
        self.governor.scene_reset();
    }

    /// The scale in force, `0.25..=1.0`.
    pub fn scale(&self) -> f32 {
        self.governor.scale()
    }

    /// One frame's measurement. `gpu_ms` is the GPU's own time for the frame, when the renderer
    /// could read it; a frame with no measurement changes nothing, because acting on a guess costs
    /// quality for nothing.
    ///
    /// Returns whether the plan for this frame is different from the last one, so the caller only
    /// re-plans when the answer actually moved.
    pub fn observe(&mut self, gpu_ms: Option<f32>, solid: u32, translucent: u32) -> (Plan, bool) {
        self.frames = self.frames.saturating_add(1);
        if let Some(ms) = gpu_ms {
            self.observed = self.observed.saturating_add(1);
            self.governor.observe(ms);
        }
        let plan = Plan::of(solid, translucent, self.governor.scale());
        let changed = (plan.scale - self.applied).abs() > f32::EPSILON;
        if changed {
            self.applied = plan.scale;
        }
        // Measured against the governor's own smoothed time, so a frame with no reading never
        // counts either way.
        if self.governor.frame_ms() > self.governor.target_ms() {
            self.over = self.over.saturating_add(1);
        } else {
            self.over = 0;
        }
        (plan, changed)
    }

    /// Whether the GPU has traced longer than the display's period for thirty frames running: the
    /// condition the log and the performance overlay report, as opposed to one busy frame.
    pub fn struggling(&self) -> bool {
        self.over >= 30
    }

    /// A one-line description for the performance overlay.
    pub fn describe(&self) -> String {
        // Say so when there is no measurement at all. Without this the overlay reads "GPU 0.0/16.7
        // ms at 100% scale", which looks like a device with room to spare rather than one whose
        // frame time was never read — and on a platform where the timestamp query is not granted
        // (the Metal path being the one that matters here) that is exactly what happens: the
        // governor is never observed and never shortens anything.
        if self.observed == 0 {
            return format!(
                "GPU time not read ({} frame(s), no timestamp query): the draw budget is not \
                 shortening anything",
                self.frames
            );
        }
        self.governor.describe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_happy_governor_keeps_the_whole_list() {
        let mut budget = DrawBudget::new(16.7);
        // Frames comfortably inside the target.
        let mut plan = Plan::full(0, 0, 1.0);
        for _ in 0..10 {
            plan = budget.observe(Some(8.0), 400, 20).0;
        }
        assert_eq!(plan, Plan::full(400, 20, 1.0));
    }

    #[test]
    fn a_late_frame_shortens_the_solid_range_only() {
        let mut budget = DrawBudget::new(16.7);
        let mut plan = Plan::full(400, 20, 1.0);
        for _ in 0..20 {
            plan = budget.observe(Some(60.0), 400, 20).0;
        }
        assert!(plan.solid < 400, "solid {}", plan.solid);
        // Water and glass are not the thing to drop: the lakes around the player stay.
        assert_eq!(plan.translucent, 20);
        assert!(plan.scale < 1.0);
    }

    /// The clamp: a device at the floor of the governor's range still gets its near field.
    #[test]
    fn the_near_field_is_never_cut_away() {
        let plan = Plan::of(400, 20, 0.25);
        assert!(plan.solid >= MIN_SOLID_DRAWS);
        // And a list shorter than the floor is left alone entirely.
        assert_eq!(Plan::of(4, 2, 0.25), Plan::full(4, 2, 0.25));
    }

    #[test]
    fn the_plan_never_asks_for_more_than_there_is() {
        let plan = Plan::of(10, 0, 1.0);
        assert_eq!(plan.solid, 10);
        // A scale above one is clamped rather than inflating the list.
        assert_eq!(Plan::of(400, 0, 2.5).solid, 400);
    }

    #[test]
    fn a_bogus_scale_is_treated_as_no_shortening() {
        assert_eq!(Plan::of(400, 5, f32::NAN), Plan::full(400, 5, 1.0));
    }

    #[test]
    fn a_frame_with_no_measurement_changes_nothing() {
        let mut budget = DrawBudget::new(16.7);
        for _ in 0..20 {
            let (plan, changed) = budget.observe(None, 400, 20);
            assert_eq!(plan, Plan::full(400, 20, 1.0));
            assert!(!changed);
        }
    }

    #[test]
    fn a_change_is_reported_once() {
        let mut budget = DrawBudget::new(16.7);
        let mut changes = 0;
        for _ in 0..40 {
            if budget.observe(Some(80.0), 400, 20).1 {
                changes += 1;
            }
        }
        assert!(changes > 0, "the scale moved but nothing was reported");
        // It settles: the same scale is not reported frame after frame.
        let settled = (0..40)
            .filter(|_| budget.observe(Some(80.0), 400, 20).1)
            .count();
        assert_eq!(settled, 0, "the scale was already at its floor");
    }

    #[test]
    fn struggling_needs_more_than_one_busy_frame() {
        let mut budget = DrawBudget::new(16.7);
        for _ in 0..5 {
            budget.observe(Some(80.0), 400, 20);
        }
        assert!(!budget.struggling());
        for _ in 0..40 {
            budget.observe(Some(80.0), 400, 20);
        }
        assert!(budget.struggling());
        // And a healthy run clears it — within a few frames, because the smoothed time follows a
        // measurement at 0.15 per frame: a 5 ms frame does not immediately erase an 80 ms one.
        for _ in 0..40 {
            budget.observe(Some(5.0), 400, 20);
        }
        assert!(!budget.struggling());
    }

    #[test]
    fn the_description_is_one_line() {
        let mut budget = DrawBudget::new(16.7);
        budget.observe(Some(8.0), 400, 20);
        let text = budget.describe();
        assert!(text.contains("ms") && !text.contains('\n'));
    }

    /// The defect this fixed: with no GPU time ever reported, the overlay said
    /// "GPU 0.0/16.7 ms at 100% scale", which reads as a device with plenty of room. It was a
    /// device whose frame time was never measured — and a budget that never shortens anything.
    #[test]
    fn no_measurement_is_reported_as_no_measurement() {
        let mut budget = DrawBudget::new(16.7);
        for _ in 0..30 {
            budget.observe(None, 400, 20);
        }
        let text = budget.describe();
        assert!(text.contains("not read"), "{text}");
        assert!(!text.contains("0.0/16.7"), "a missing reading is not a fast frame: {text}");
        // And it still does not pretend to have shortened anything.
        assert_eq!(budget.scale(), 1.0);
    }
}
