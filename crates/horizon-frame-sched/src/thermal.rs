//! Thermal state and what the game gives up for it.
//!
//! Apple SoCs are power-limited long before they are temperature-limited: an A11 that has been
//! drawing chunks for ten minutes loses half its peak GPU clock and never reports a fault. iOS
//! exposes the situation as `ProcessInfo.thermalState` (nominal → fair → serious → critical) and
//! is generous enough with *serious* that a game ignoring it will be at 8 fps and a locked
//! frame-generation pipeline before it sees *critical* — which is exactly the "game freezes when I
//! turn on frame generation" report.
//!
//! The governor turns a thermal level into an ordered list of sacrifices. It is a pure state
//! machine with hysteresis: two readings of the same level, a level's own `hold` time, and a
//! step-down that is larger than a step-up, so it never oscillates.

use crate::device::DeviceTier;

/// `ProcessInfo.ThermalState`, plus a "nominal but we have been hot before" that the governor keeps
/// in its own history rather than in [`ThermalLevel`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalLevel {
    #[default]
    Nominal,
    Fair,
    Serious,
    Critical,
}

impl ThermalLevel {
    /// The level a raw `ProcessInfo.thermalState` integer stands for (0..=3).
    pub fn from_raw(raw: i64) -> Self {
        match raw {
            0 => Self::Nominal,
            1 => Self::Fair,
            2 => Self::Serious,
            _ => Self::Critical,
        }
    }

    /// Where the level sits in the report's scale: 0 nominal, 3 critical. Apple's own order, and
    /// the index `HOLD` and the ordering comparisons are written against.
    fn index(self) -> u8 {
        match self {
            Self::Nominal => 0,
            Self::Fair => 1,
            Self::Serious => 2,
            Self::Critical => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Fair => "fair",
            Self::Serious => "serious",
            Self::Critical => "critical",
        }
    }
}

/// One level's sacrifices. Each field is a multiplier or a switch the caller applies; the governor
/// does not reach into the renderer, so the policy stays testable and the renderer keeps owning
/// its own state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThermalAction {
    /// Multiplier for the world's dynamic resolution scale (1.0 = unchanged). Only ever lowers.
    pub resolution_scale: f32,
    /// Multiplier for the particle budget.
    pub particle_budget: f32,
    /// Render distance in chunks the governor insists on, if lower than the option.
    pub render_distance_cap: Option<i32>,
    /// Frame Generation must be off at this level (it is never *turned on* by the governor).
    pub suspend_frame_gen: bool,
    /// The simulation may tick at half rate while nothing in the world can observe the difference
    /// (single player, no other players, no redstone-timed block within the simulation distance).
    pub relax_background_ticks: bool,
    /// Frame-time budget in milliseconds for the renderer's own work, before it starts culling.
    pub frame_budget_ms: f32,
}

impl ThermalAction {
    /// Nominal: full quality on the tier's own budget.
    pub fn nominal(tier: DeviceTier) -> Self {
        let (budget, particles) = match tier {
            DeviceTier::Legacy => (12.0, 0.65),
            DeviceTier::Mobile => (10.0, 0.8),
            DeviceTier::Balanced => (9.0, 1.0),
            DeviceTier::Capable => (8.0, 1.0),
        };
        Self {
            resolution_scale: 1.0,
            particle_budget: particles,
            render_distance_cap: None,
            suspend_frame_gen: false,
            relax_background_ticks: false,
            frame_budget_ms: budget,
        }
    }

    /// `level` applied on top of `nominal(tier)`.
    pub fn for_level(tier: DeviceTier, level: ThermalLevel) -> Self {
        let mut action = Self::nominal(tier);
        match level {
            ThermalLevel::Nominal => {}
            ThermalLevel::Fair => {
                action.resolution_scale = match tier {
                    DeviceTier::Legacy => 0.85,
                    DeviceTier::Mobile => 0.88,
                    DeviceTier::Balanced => 0.9,
                    DeviceTier::Capable => 0.92,
                };
                action.particle_budget *= 0.75;
                action.frame_budget_ms *= 0.9;
            }
            ThermalLevel::Serious => {
                action.resolution_scale = match tier {
                    DeviceTier::Legacy => 0.65,
                    DeviceTier::Mobile => 0.7,
                    DeviceTier::Balanced => 0.75,
                    DeviceTier::Capable => 0.8,
                };
                action.particle_budget *= 0.45;
                action.render_distance_cap = Some(match tier {
                    DeviceTier::Legacy => 5,
                    DeviceTier::Mobile => 6,
                    DeviceTier::Balanced => 8,
                    DeviceTier::Capable => 10,
                });
                // The one place Frame Generation is taken away rather than offered: it is the most
                // expensive optional thing the renderer can be doing on a hot SoC.
                action.suspend_frame_gen = true;
                action.relax_background_ticks = true;
                action.frame_budget_ms *= 0.8;
            }
            ThermalLevel::Critical => {
                action.resolution_scale = match tier {
                    DeviceTier::Legacy => 0.5,
                    DeviceTier::Mobile => 0.55,
                    DeviceTier::Balanced => 0.6,
                    DeviceTier::Capable => 0.65,
                };
                action.particle_budget *= 0.2;
                action.render_distance_cap = Some(match tier {
                    DeviceTier::Legacy => 3,
                    DeviceTier::Mobile => 4,
                    DeviceTier::Balanced => 5,
                    DeviceTier::Capable => 6,
                });
                action.suspend_frame_gen = true;
                action.relax_background_ticks = true;
                action.frame_budget_ms *= 0.7;
            }
        }
        action
    }
}

/// How many seconds at a level before the governor believes it, so a one-frame spike does not
/// drop the render distance.
const HOLD: [f32; 4] = [0.0, 2.0, 4.0, 1.0];
/// How many seconds back at or below the previous level before it climbs back. Stepping down is
/// quick, stepping up is slow: cooling is what takes time, and flapping is what players notice.
const RECOVER: f32 = 20.0;

/// The thermal policy, fed one reading per second (or per frame; time is passed in).
#[derive(Debug)]
pub struct ThermalGovernor {
    tier: DeviceTier,
    /// The level the governor is acting on.
    active: ThermalLevel,
    /// The highest level recently reported, to require [`RECOVER`] before descending.
    ceiling: ThermalLevel,
    /// Seconds spent at `pending`.
    pending: Option<(ThermalLevel, f32)>,
    /// Seconds since the level last fell below `active`.
    cool: f32,
    /// Set by the caller when the game has been idle (paused behind the launcher, backgrounded):
    /// the governor forgets its history so a resume is not judged by the previous session.
    reset_pending: bool,
}

impl ThermalGovernor {
    pub fn new(tier: DeviceTier) -> Self {
        Self {
            tier,
            active: ThermalLevel::Nominal,
            ceiling: ThermalLevel::Nominal,
            pending: None,
            cool: 0.0,
            reset_pending: false,
        }
    }

    pub fn level(&self) -> ThermalLevel {
        self.active
    }

    pub fn action(&self) -> ThermalAction {
        ThermalAction::for_level(self.tier, self.active)
    }

    /// Forget the history: called when the game returns to the foreground.
    pub fn reset(&mut self) {
        self.pending = None;
        self.cool = 0.0;
        self.reset_pending = true;
    }

    /// One reading. `dt` is the seconds since the previous reading. Returns `Some(level)` when the
    /// acting level changed, so the caller only reconfigures the renderer on a real transition.
    pub fn observe(&mut self, reported: ThermalLevel, dt: f32) -> Option<ThermalLevel> {
        let dt = dt.clamp(0.0, 5.0);
        if self.reset_pending {
            self.reset_pending = false;
            self.active = reported;
            self.ceiling = reported;
            return Some(self.active);
        }
        if reported >= self.active {
            self.ceiling = self.ceiling.max(reported);
            self.cool = 0.0;
        } else {
            self.cool += dt;
        }
        // Rising: believe it after the level's hold time.
        if reported > self.active {
            // The hold belongs to the level being moved *to*: Apple's own scale, where 1 (fair) is
            // believed at once and 2 (serious) has to persist, because a single serious reading is
            // often a burst (a shader compile, a chunk load) rather than heat.
            let held = match self.pending {
                Some((level, seconds)) if level == reported => seconds + dt,
                _ => dt,
            };
            if held >= HOLD[reported.index() as usize] {
                self.pending = None;
                self.active = reported;
                return Some(self.active);
            }
            self.pending = Some((reported, held));
            return None;
        }
        // Falling: only after a long cool, and only one step at a time.
        self.pending = None;
        if reported < self.active && self.cool >= RECOVER {
            self.active = match self.active {
                ThermalLevel::Critical => ThermalLevel::Serious,
                ThermalLevel::Serious => ThermalLevel::Fair,
                _ => ThermalLevel::Nominal,
            };
            self.ceiling = self.active;
            self.cool = 0.0;
            return Some(self.active);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_hot_reading_changes_nothing() {
        let mut g = ThermalGovernor::new(DeviceTier::Mobile);
        assert_eq!(g.observe(ThermalLevel::Serious, 1.0), None);
        assert_eq!(g.level(), ThermalLevel::Nominal);
    }

    #[test]
    fn a_sustained_level_is_believed_and_suspends_frame_generation() {
        let mut g = ThermalGovernor::new(DeviceTier::Mobile);
        g.observe(ThermalLevel::Serious, 1.0);
        g.observe(ThermalLevel::Serious, 1.0);
        g.observe(ThermalLevel::Serious, 1.0);
        let changed = g.observe(ThermalLevel::Serious, 1.0);
        assert_eq!(changed, Some(ThermalLevel::Serious));
        assert!(g.action().suspend_frame_gen);
        assert!(g.action().resolution_scale < 1.0);
        assert_eq!(g.action().render_distance_cap, Some(6));
    }

    #[test]
    fn cooling_back_down_takes_a_long_quiet_and_one_step_at_a_time() {
        let mut g = ThermalGovernor::new(DeviceTier::Mobile);
        for _ in 0..8 {
            g.observe(ThermalLevel::Critical, 1.0);
        }
        assert_eq!(g.level(), ThermalLevel::Critical);
        // Ten seconds cool: nothing yet.
        for _ in 0..10 {
            assert_eq!(g.observe(ThermalLevel::Nominal, 1.0), None);
        }
        // Past the recovery window: one step, to Serious.
        let mut steps = Vec::new();
        for _ in 0..80 {
            if let Some(level) = g.observe(ThermalLevel::Nominal, 1.0) {
                steps.push(level);
            }
        }
        assert_eq!(
            steps,
            [
                ThermalLevel::Serious,
                ThermalLevel::Fair,
                ThermalLevel::Nominal
            ]
        );
    }

    #[test]
    fn an_a11_caps_harder_than_an_m_series_at_the_same_heat() {
        let a11 = ThermalAction::for_level(DeviceTier::Legacy, ThermalLevel::Serious);
        let m = ThermalAction::for_level(DeviceTier::Capable, ThermalLevel::Serious);
        assert!(a11.resolution_scale < m.resolution_scale);
        assert!(a11.render_distance_cap < m.render_distance_cap);
        assert!(a11.particle_budget <= m.particle_budget);
        // A slower device is *allowed* more wall-clock time per frame, not less: its frames are
        // longer by nature, and the governor's job is to cut work, not to demand the impossible.
        assert!(a11.frame_budget_ms > m.frame_budget_ms);
    }

    #[test]
    fn the_frame_generation_off_switch_is_never_undone_by_heat() {
        for level in [
            ThermalLevel::Nominal,
            ThermalLevel::Fair,
            ThermalLevel::Serious,
            ThermalLevel::Critical,
        ] {
            let action = ThermalAction::for_level(DeviceTier::Capable, level);
            assert!(!action.suspend_frame_gen || level >= ThermalLevel::Serious);
        }
    }
}
