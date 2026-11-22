//! Apple silicon's thermal state, and what a hot chip takes away.
//!
//! # Why the game must act before the OS does
//!
//! A hot Apple SoC loses clocks well before it reports a problem. By the time `ProcessInfo` says
//! `.serious`, the GPU may already be running at half its nominal frequency, and a game that waits
//! for that news spends the whole descent dropping frames. The rule this module implements is the
//! one in `WORKING_PRINCIPLES.md`: **reduce load before the OS throttles**, in the order that costs
//! the player least, and recover slowly so the device does not oscillate in and out of the
//! throttle band.
//!
//! # The order of sacrifices
//!
//! The policy itself lives in [`horizon_frame_sched::thermal`]: a `ThermalAction` per level, and
//! `ThermalGovernor`'s hysteresis. This module is the *integration* — it owns the clock, the
//! one-shot change notification the caller needs to apply the action once rather than every frame,
//! and the combination with the OS's own memory pressure, which lowers the render distance for a
//! different reason and must not be double-counted.
//!
//! What a hot device gives up, first to last: frame generation (a multiplier on GPU work, and the
//! first thing that should yield), the world's resolution scale, the particle budget, the render
//! distance, and finally background ticks. Everything above is a *quality* knob; nothing here
//! touches gameplay.

use std::time::Instant;

use horizon_frame_sched::{DeviceTier, ThermalAction, ThermalGovernor, ThermalLevel};

/// How often the thermal state is polled. `ProcessInfo.thermalStateDidChangeNotification` is
/// delivery-latency-bound and can miss a fast descent; a 2 s poll is cheap (`ProcessInfo` is a
/// property read, not a syscall) and bounded.
const POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// One thermal step applied and remembered, so the caller can tell "nothing changed" from "the
/// level changed and you must re-apply".
#[derive(Debug)]
pub struct Thermal {
    /// The policy.
    governor: ThermalGovernor,
    /// The tier this device is, which scales every action.
    tier: DeviceTier,
    /// When the level was last polled.
    polled: Option<Instant>,
    /// The level the caller has already been told about.
    announced: ThermalLevel,
    /// The level as last read, so a change can be detected without the governor's own latch.
    current: ThermalLevel,
    /// The last raw report, so a level that vanishes by the next poll is not acted on.
    pending: Option<ThermalLevel>,
    /// The renderer's resolution ceiling from the heat alone, 0..=1. Dynamic Resolution still
    /// decides *where* in the range the frame lands; this only lowers the top of the range, which
    /// is why it composes with the frame's own autotune instead of overriding it.
    ///
    /// A field rather than a method because `game.rs` reads it directly
    /// (`renderer.set_heat_scale_ceiling(self.heat.resolution_scale)`): the value handed to the
    /// renderer is then obviously the one the governor holds, with no call between them to drift.
    /// Kept in step by [`Thermal::sync`] after every observation.
    pub resolution_scale: f32,
}

impl Thermal {
    /// A governor for `tier`, nominal to begin with. Nothing is assumed about the device's warmth:
    /// an unmeasured chip is treated as cool, because acting on a guess costs quality for nothing.
    pub fn new(tier: DeviceTier) -> Self {
        Self {
            governor: ThermalGovernor::new(tier),
            tier,
            polled: None,
            announced: ThermalLevel::Nominal,
            current: ThermalLevel::Nominal,
            resolution_scale: 1.0,
            pending: None,
        }
    }

    /// The action in force: the multipliers the renderer applies.
    pub fn action(&self) -> ThermalAction {
        self.governor.action()
    }

    /// Re-read the governor's multipliers into the fields `game.rs` reads directly. Called after
    /// every `observe`, so the cached field can never disagree with the policy.
    fn sync(&mut self) {
        self.resolution_scale = self.governor.action().resolution_scale;
    }

    /// The level currently in force.
    pub fn level(&self) -> ThermalLevel {
        self.current
    }

    /// Poll the platform's thermal state and step the governor. Returns whether the caller must
    /// re-apply the action — true only on a level change, because applying the same multipliers
    /// every frame would reset the renderer's own convergence.
    ///
    /// `now` is the frame's clock, so a test can drive a whole thermal descent without waiting.
    pub fn tick(&mut self, now: Instant) -> bool {
        // The platform's thermal state is only read on the poll interval: it is a property read
        // with a notification behind it, and reading it four hundred times a second to learn
        // nothing is waste on a phone.
        let dt = match self.polled {
            Some(at) => now.duration_since(at).as_secs_f32(),
            None => 0.0,
        };
        if self.polled.is_some() && dt < POLL.as_secs_f32() {
            return false;
        }
        self.polled = Some(now);
        let level = platform_level();
        // A level must survive one poll before it takes quality away. `thermalState` flaps between
        // nominal and fair on a warm phone, and acting on the first fair of a pair would spend a
        // resolution change (and the frame that pays for re-creating the target) on a spike that was
        // gone 2 seconds later. Descending is the expensive direction, so it is the one gated;
        // a cooler report is believed at once.
        let seen = self.pending.replace(level);
        let level = if level > self.current && seen != Some(level) {
            self.current
        } else {
            level
        };
        self.current = level;
        self.governor.observe(level, dt);
        self.sync();
        // The governor's action can change without the level changing (its own hysteresis), so the
        // change is measured against what the caller was last told. `announced` is **not** written
        // here: `take_change` owns it. Writing it here as well made `take_change` always answer
        // `None`, which is why a device's log carried the same "thermal nominal" line 131 times in
        // one session — the caller saw "no change" every time and its once-a-session guard never
        // tripped.
        self.governor.level() != self.announced
    }

    /// The change since the last call, if any: what the caller should announce to the player.
    pub fn take_change(&mut self) -> Option<ThermalLevel> {
        let level = self.governor.level();
        (level != self.announced).then_some(level)
    }

    /// The render distance the device insists on right now, given the player's setting and what the
    /// *memory* pressure already asked for.
    ///
    /// The two caps compose by taking the smaller: heat and memory are independent reasons to
    /// shorten the view, and honouring only one of them leaves the other unaddressed. `by_memory`
    /// is what `memory_warning` asked for, already applied.
    pub fn effective_distance(&self, setting: i32, by_memory: i32) -> i32 {
        let heat = self
            .governor
            .action()
            .render_distance_cap
            .unwrap_or(setting);
        setting.min(by_memory).min(heat).max(2)
    }

    /// Whether background ticks may relax (half rate) at this level. Single-player only, and the
    /// caller checks the other conditions: this is the thermal half of the decision.
    pub fn may_relax_ticks(&self) -> bool {
        self.governor.action().relax_background_ticks
    }

    /// The world closed: the renderer the action was applied to is gone, so the next one must be
    /// told again rather than inherit nothing.
    pub fn world_closed(&mut self) {
        self.announced = ThermalLevel::Nominal;
    }

    /// A one-line description for the log and the performance overlay.
    pub fn describe(&self) -> String {
        format!(
            "{} ({:.0}% resolution, {:.0}% particles, {} chunks{})",
            self.current.as_str(),
            self.resolution_scale * 100.0,
            self.governor.action().particle_budget * 100.0,
            self.effective_distance(32, 32),
            if self.governor.action().suspend_frame_gen {
                ", frame generation off"
            } else {
                ""
            }
        )
    }

    /// Forget everything: a new world, a fresh device probe.
    pub fn reset(&mut self) {
        self.governor.reset();
        self.polled = None;
        self.announced = ThermalLevel::Nominal;
        self.current = ThermalLevel::Nominal;
        self.pending = None;
        self.sync();
    }

    /// The tier the actions are scaled by.
    pub fn tier(&self) -> DeviceTier {
        self.tier
    }
}

/// The platform's thermal level.
///
/// `ProcessInfo.processInfo.thermalState` is the real source and lives in `horizon-ios`;
/// `HORIZON_THERMAL` overrides it so a QA run can drive the descent from a shell without heating a
/// phone. Everything else reports nominal — a desktop has no thermal state to report, and the
/// governor's other inputs (frame time) still act.
fn platform_level() -> ThermalLevel {
    // The phone's own report first: `horizon-ios-launcher` holds what Swift's
    // `ProcessInfo.thermalStateDidChangeNotification` last said. Reading only an environment
    // variable — which nothing sets on a device — is why the thermal governor never once acted on
    // heat in nine hours of play: it was told "nominal" on a phone that was hot.
    if let Ok(raw) = std::env::var("HORIZON_THERMAL") {
        return match raw.as_str() {
            "nominal" => ThermalLevel::Nominal,
            "fair" => ThermalLevel::Fair,
            "serious" => ThermalLevel::Serious,
            "critical" => ThermalLevel::Critical,
            other => other.parse::<i64>().map_or(ThermalLevel::Nominal, ThermalLevel::from_raw),
        };
    }
    horizon_ios_launcher::thermal_state().map_or(ThermalLevel::Nominal, ThermalLevel::from_raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_cool_device_takes_away_nothing() {
        let thermal = Thermal::new(DeviceTier::Mobile);
        let action = thermal.action();
        assert_eq!(action.resolution_scale, 1.0);
        assert!(!action.suspend_frame_gen);
        assert_eq!(thermal.effective_distance(12, 12), 12);
    }

    #[test]
    fn the_poll_interval_is_honoured() {
        let mut thermal = Thermal::new(DeviceTier::Mobile);
        let now = Instant::now();
        assert!(!thermal.tick(now), "nothing to apply on the first frame");
        // A second call inside the interval must not re-apply.
        assert!(!thermal.tick(now + Duration::from_millis(50)));
        // Past it, it polls again but the level is unchanged, so still nothing to apply.
        assert!(!thermal.tick(now + POLL + Duration::from_millis(1)));
    }

    /// The heat and the memory pressure compose: the smaller cap wins, and neither can drive the
    /// distance below the floor.
    #[test]
    fn distance_caps_compose_by_the_smaller() {
        let thermal = Thermal::new(DeviceTier::Legacy);
        assert_eq!(thermal.effective_distance(32, 12), 12);
        assert_eq!(thermal.effective_distance(8, 32), 8);
        // The floor of 2 chunks: a distance of 1 is not playable and the game refuses it.
        assert_eq!(thermal.effective_distance(1, 1), 2);
    }

    #[test]
    fn a_new_world_is_told_the_action_again() {
        let mut thermal = Thermal::new(DeviceTier::Mobile);
        thermal.tick(Instant::now());
        thermal.world_closed();
        // The next tick after the interval must report a change to re-apply.
        let before = thermal.announced;
        thermal.tick(Instant::now() + POLL + Duration::from_millis(1));
        assert_eq!(before, ThermalLevel::Nominal);
        assert_eq!(thermal.level(), ThermalLevel::Nominal);
    }

    /// The defect, as a test: `tick` must report the change from the level the caller was last
    /// told, and must not keep reporting it. On a device this branch logged the same
    /// "thermal nominal" line 131 times in 40 seconds because `tick` wrote `announced` itself and
    /// `take_change` then always answered `None`, so the caller's guard never held.
    #[test]
    fn a_level_is_announced_once_and_then_left_alone() {
        let mut thermal = Thermal::new(DeviceTier::Mobile);
        let start = Instant::now();
        // The first poll: nothing to compare against, so nothing is announced.
        assert!(!thermal.tick(start));
        // The next poll after the interval, same level: still nothing.
        assert!(!thermal.tick(start + POLL + Duration::from_millis(1)));
        assert_eq!(thermal.take_change(), None);
    }

    #[test]
    fn the_description_is_one_line() {
        let thermal = Thermal::new(DeviceTier::Legacy);
        let text = thermal.describe();
        assert!(text.contains("nominal"));
        assert!(text.contains('%'));
        assert!(!text.contains('\n'));
    }

    #[test]
    fn relax_ticks_is_off_when_nominal() {
        let thermal = Thermal::new(DeviceTier::Mobile);
        assert!(!thermal.may_relax_ticks());
    }

    #[test]
    fn the_tier_scales_every_action() {
        let legacy = Thermal::new(DeviceTier::Legacy);
        let capable = Thermal::new(DeviceTier::Capable);
        // A legacy device's own budget is larger in milliseconds (it needs more slack to hit the
        // same rate), which is the frame scheduler's whole premise.
        assert!(legacy.action().frame_budget_ms >= capable.action().frame_budget_ms);
    }
}
