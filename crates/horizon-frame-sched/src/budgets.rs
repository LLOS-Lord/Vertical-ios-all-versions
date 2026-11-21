//! The frame's work, ranked and budgeted.
//!
//! The renderer has more work available each frame than it can do: every meshed section, every
//! particle, every entity, every block entity, the far tiles, the sky, the clouds, the HUD. Today
//! that work is done in whatever order it happens to be in the code, and when the frame runs long
//! the whole frame is long — the player sees a hitch *and* still gets the unimportant updates.
//!
//! [`FrameLedger`] turns that into an explicit contract: the frame has a budget in milliseconds
//! derived from the display, work is ranked by [`FrameKind`], and a caller takes a [`Ticket`]
//! before doing the work. If the frame's remaining budget cannot pay for the ticket's group, the
//! ticket is refused — and the scheduler drops the *least important* work first, keeps a small
//! reserve for the things the player cannot play without, and only then lets the frame run over.
//!
//! Nothing here reads a clock or a GPU: the caller reports elapsed time. That keeps the policy
//! testable and makes the cost of the bookkeeping obvious (two float adds per ticket).

/// How important a piece of frame work is. Ordered: `PlayerCritical` first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FrameKind {
    /// Without it the player cannot play: the hand, the crosshair, the block they are breaking,
    /// the entity they are attacking, the input's own feedback.
    PlayerCritical = 0,
    /// The world the player is looking at, near enough to be unmistakable.
    VisibleNear = 1,
    /// The world further out, and the entities that are moving.
    VisibleFar = 2,
    /// Sky, clouds, weather, block entity animations, particles.
    Ambient = 3,
    /// Off-screen, occluded, and speculative work: pre-meshing a chunk on the other side of a hill,
    /// an entity behind a wall. The first thing to skip.
    Speculative = 4,
}

impl FrameKind {
    /// The share of the frame's budget this kind may use, of what is left. The reserve for
    /// `PlayerCritical` is held back from every other kind.
    fn share(self) -> f32 {
        match self {
            Self::PlayerCritical => 1.0,
            Self::VisibleNear => 0.75,
            Self::VisibleFar => 0.5,
            Self::Ambient => 0.25,
            Self::Speculative => 0.1,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlayerCritical => "player",
            Self::VisibleNear => "near",
            Self::VisibleFar => "far",
            Self::Ambient => "ambient",
            Self::Speculative => "speculative",
        }
    }
}

/// A budget for one kind of work inside one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    pub kind: FrameKind,
    pub ms: f32,
    pub spent_ms: f32,
    pub skipped: u32,
}

impl Budget {
    pub fn remaining_ms(self) -> f32 {
        (self.ms - self.spent_ms).max(0.0)
    }
    pub fn exhausted(self) -> bool {
        self.spent_ms >= self.ms
    }
}

/// The frame's ledger: a budget per kind, and the numbers the performance overlay can show.
#[derive(Clone, Debug)]
pub struct FrameLedger {
    /// The frame's total budget, ms.
    total_ms: f32,
    budgets: [Budget; 5],
    /// Frames in a row that ran over budget.
    over: u32,
    /// The current quality step, 0 = full, higher = degraded. The caller reads this to pick a
    /// cheaper path (fewer particles, a shorter entity cull distance).
    pub degrade_level: u8,
    /// Frames left before the degrade level relaxes by one step.
    relax_in: u32,
}

/// After this many consecutive over-budget frames, the ledger degrades one level.
const DEGRADE_AFTER: u32 = 30;
/// Levels of degradation available.
pub const MAX_DEGRADE: u8 = 4;
/// Frames at a degraded level that fit the budget before it relaxes.
const RELAX_AFTER: u32 = 600;

impl FrameLedger {
    pub fn new(total_ms: f32) -> Self {
        // The shares are of the *total*: player-critical work is never refused, so its budget is
        // the whole frame and the others are cut from what is left.
        let total_ms = total_ms.max(1.0);
        let zero = |kind| Budget { kind, ms: 0.0, spent_ms: 0.0, skipped: 0 };
        let mut ledger = Self {
            total_ms,
            budgets: [
                zero(FrameKind::PlayerCritical),
                zero(FrameKind::VisibleNear),
                zero(FrameKind::VisibleFar),
                zero(FrameKind::Ambient),
                zero(FrameKind::Speculative),
            ],
            over: 0,
            degrade_level: 0,
            relax_in: RELAX_AFTER,
        };
        ledger.set_total(total_ms);
        ledger
    }

    /// A new frame budget (a resolution change, a thermal step, a different display).
    pub fn set_total(&mut self, total_ms: f32) {
        self.total_ms = total_ms.max(1.0);
        // A degraded ledger gets proportionally more room: the whole point of degrading is that
        // the cheaper path fits.
        let scale = 1.0 + f32::from(self.degrade_level) * 0.08;
        for budget in &mut self.budgets {
            budget.ms = self.total_ms * budget.kind.share() * scale;
            budget.spent_ms = 0.0;
            budget.skipped = 0;
        }
    }

    pub fn total_ms(&self) -> f32 {
        self.total_ms
    }

    pub fn budget(&self, kind: FrameKind) -> Budget {
        self.budgets[kind as usize]
    }

    /// A frame begins: the spent counters reset, the budgets stay.
    pub fn begin(&mut self) {
        for budget in &mut self.budgets {
            budget.spent_ms = 0.0;
            budget.skipped = 0;
        }
    }

    /// The milliseconds the frame may still spend on `kind`. `PlayerCritical` always gets what is
    /// left, which may exceed the budget: the player is never left without a hand.
    pub fn allowance(&self, kind: FrameKind) -> f32 {
        self.budgets[kind as usize].remaining_ms()
    }

    /// Whether work of `kind` that the caller estimates at `estimate_ms` may run now. A refused
    /// call counts as a skip, which the overlay reports: that is how the code finds the work that
    /// never fits.
    pub fn request(&mut self, kind: FrameKind, estimate_ms: f32) -> bool {
        if kind == FrameKind::PlayerCritical {
            return true;
        }
        let budget = &mut self.budgets[kind as usize];
        if budget.spent_ms + estimate_ms <= budget.ms {
            budget.spent_ms += estimate_ms;
            true
        } else {
            budget.skipped += 1;
            false
        }
    }

    /// Work was done whose real cost differed from the estimate.
    pub fn report(&mut self, kind: FrameKind, estimate_ms: f32, actual_ms: f32) {
        let budget = &mut self.budgets[kind as usize];
        budget.spent_ms = (budget.spent_ms - estimate_ms + actual_ms).max(0.0);
    }

    /// Whether a speculative unit of work should even be attempted: only while the frame is
    /// comfortably inside its budget and the ledger is not degraded.
    pub fn may_speculate(&self) -> bool {
        self.degrade_level == 0
            && !self.budgets[FrameKind::Speculative as usize].exhausted()
            && self.used_ms() < self.total_ms * 0.85
    }

    /// Everything spent this frame, ms.
    pub fn used_ms(&self) -> f32 {
        self.budgets.iter().map(|b| b.spent_ms).sum()
    }

    /// The frame ends: decided whether it fit. Returns the degrade level after the decision, which
    /// is what the caller applies next frame.
    pub fn end(&mut self, frame_ms: f32) -> u8 {
        self.over = if frame_ms > self.total_ms {
            self.over.saturating_add(1)
        } else {
            0
        };
        if self.over >= DEGRADE_AFTER && self.degrade_level < MAX_DEGRADE {
            self.degrade_level += 1;
            self.over = 0;
            self.relax_in = RELAX_AFTER;
            self.set_total(self.total_ms);
        } else if self.degrade_level > 0 {
            self.relax_in = self.relax_in.saturating_sub(1);
            if self.relax_in == 0 {
                self.degrade_level -= 1;
                self.relax_in = RELAX_AFTER;
                self.set_total(self.total_ms);
            }
        }
        self.degrade_level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_critical_work_is_never_refused() {
        let mut l = FrameLedger::new(4.0);
        l.begin();
        // Blow every other budget first.
        l.request(FrameKind::VisibleNear, 3.0);
        l.request(FrameKind::VisibleFar, 2.0);
        l.request(FrameKind::Ambient, 1.0);
        l.request(FrameKind::Speculative, 0.4);
        assert!(l.budget(FrameKind::Ambient).exhausted());
        assert!(l.request(FrameKind::Speculative, 1.0) == false);
        assert!(l.request(FrameKind::PlayerCritical, 50.0));
    }

    #[test]
    fn the_least_important_work_is_refused_first() {
        let mut l = FrameLedger::new(10.0);
        l.begin();
        // Ambient may use a quarter of the frame; speculative a tenth.
        assert_eq!(l.budget(FrameKind::Ambient).ms, 2.5);
        assert_eq!(l.budget(FrameKind::Speculative).ms, 1.0);
        assert!(l.request(FrameKind::Ambient, 2.0));
        assert!(!l.request(FrameKind::Ambient, 1.0));
        assert_eq!(l.budget(FrameKind::Ambient).skipped, 1);
        assert!(!l.request(FrameKind::Speculative, 1.5));
    }

    #[test]
    fn sustained_over_budget_frames_degrade_then_relax() {
        let mut l = FrameLedger::new(8.0);
        for _ in 0..DEGRADE_AFTER {
            l.begin();
            l.end(12.0);
        }
        assert_eq!(l.degrade_level, 1);
        for _ in 0..RELAX_AFTER {
            l.begin();
            l.end(4.0);
        }
        assert_eq!(l.degrade_level, 0);
    }

    #[test]
    fn speculative_work_stops_when_the_frame_is_already_expensive() {
        let mut l = FrameLedger::new(10.0);
        l.begin();
        assert!(l.may_speculate());
        l.request(FrameKind::VisibleNear, 7.0);
        l.request(FrameKind::VisibleFar, 4.0);
        assert!(!l.may_speculate());
    }

    #[test]
    fn a_report_corrects_the_estimate() {
        let mut l = FrameLedger::new(10.0);
        l.begin();
        l.request(FrameKind::Ambient, 1.0);
        l.report(FrameKind::Ambient, 1.0, 3.0);
        assert_eq!(l.used_ms(), 3.0);
    }
}
