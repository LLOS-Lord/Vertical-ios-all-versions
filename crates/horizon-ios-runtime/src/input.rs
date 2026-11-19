//! Touch latency, scroll jitter, and the missing villager / mount controls.
//!
//! ## Root causes in the tree
//!
//! * `TouchControls.swift::scrolled()` emits `UIPanGestureRecognizer.velocity(in:)`
//!   directly as a menu scroll event. Touch delivery is not phase-locked to the display
//!   (120 Hz touch vs 60 Hz display): some frames receive 1 sample, others 2 or 3, so a
//!   steady drag steps 10/20/30 px — "menu UI dật dật". Apple's WWDC15 "Advanced Touch
//!   Input on iOS" and the GPUI iOS resampling work both prescribe: deliver **coalesced**
//!   touches with hardware timestamps, then resample **once per frame** at
//!   `targetTimestamp - latency`.
//! * `poll()` subtracts `ProcessInfo.systemUptime` at *dequeue* time rather than using
//!   each sample's `UITouch.timestamp`, adding one variable queue delay to every event.
//! * `actions.rs` has no `TradeVillager` / `RideEntity` / `Dismount` actions. In Java
//!   Edition those are all `key.use` (right-click on the entity) and `key.sneak`
//!   (dismount); the touch layer only exposes raw left/right mouse buttons and never
//!   routes a context-sensitive entity interaction, so players cannot trade or ride.
//!
//! ## What this module provides
//!
//! A deterministic, testable resampler + scroll smoother + context-sensitive action
//! mapping to the engine's *existing* `Action::Use` / `Action::Sneak` bindings. No new
//! key mapping is required for the gameplay to work; three `pad.*` actions are also
//! defined for the on-screen button layout.

use std::collections::VecDeque;

/// A touch sample with its hardware timestamp (ms, monotonic). `coalesced` samples
/// come from `event.coalescedTouches(for:)`, `predicted` from `predictedTouches(for:)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchSample {
    pub t_ms: f64,
    pub x: f64,
    pub y: f64,
    pub predicted: bool,
}
impl TouchSample {
    pub fn new(t_ms: f64, x: f64, y: f64) -> Self {
        Self {
            t_ms,
            x,
            y,
            predicted: false,
        }
    }
}

/// Resample a pan once per presented frame at a fixed latency behind the finger.
///
/// The step is interpolated between the two samples surrounding `present_ms - latency`.
/// When all real samples are older, a short prediction (Android's input resampling
/// rule: at most 8 ms and at most half the last sample interval) keeps the content
/// moving without adding a frame of latency.
#[derive(Clone, Debug)]
pub struct TouchResampler {
    samples: VecDeque<TouchSample>,
    latency_ms: f64,
    max_predict_ms: f64,
    last: Option<(f64, f64)>,
    pub emitted: u32,
}
impl TouchResampler {
    /// `refresh_ms` is CADisplayLink.duration (16.67 at 60 Hz, 8.33 at 120 Hz).
    pub fn new(refresh_ms: f64) -> Self {
        Self {
            samples: VecDeque::new(),
            latency_ms: refresh_ms + 5.0,
            max_predict_ms: 8.0,
            last: None,
            emitted: 0,
        }
    }
    pub fn with_latency(mut self, latency_ms: f64) -> Self {
        self.latency_ms = latency_ms.max(0.0);
        self
    }
    pub fn push(&mut self, s: TouchSample) {
        // Drop predicted samples as soon as a real one arrives; keep monotonic time.
        if !s.predicted {
            while self.samples.back().is_some_and(|b| b.predicted) {
                self.samples.pop_back();
            }
        }
        if self.samples.back().is_some_and(|b| s.t_ms < b.t_ms) {
            return;
        }
        self.samples.push_back(s);
        if self.samples.len() > 128 {
            self.samples.pop_front();
        }
    }
    pub fn push_coalesced(&mut self, samples: &[TouchSample]) {
        for s in samples {
            self.push(*s);
        }
    }
    /// The position to draw for the frame that will be presented at `present_ms`.
    pub fn sample_at(&mut self, present_ms: f64) -> Option<(f64, f64)> {
        let target = present_ms - self.latency_ms;
        if self.samples.is_empty() {
            return self.last;
        }
        let mut real: Vec<TouchSample> = self
            .samples
            .iter()
            .copied()
            .filter(|s| !s.predicted)
            .collect();
        if real.is_empty() {
            return self.last;
        }
        let pos = if target <= real[0].t_ms {
            (real[0].x, real[0].y)
        } else if target >= real[real.len() - 1].t_ms {
            // Predict at most 8 ms and at most half the newest sample interval.
            let n = real.len();
            let prev = n.saturating_sub(2);
            let (a, b) = (real[prev], real[n - 1]);
            let dt = (b.t_ms - a.t_ms).max(0.0);
            let ahead = (target - b.t_ms).min(self.max_predict_ms).min(dt / 2.0);
            let vx = if dt > 0.0 { (b.x - a.x) / dt } else { 0.0 };
            let vy = if dt > 0.0 { (b.y - a.y) / dt } else { 0.0 };
            (b.x + vx * ahead, b.y + vy * ahead)
        } else {
            let mut chosen = (real[0].x, real[0].y);
            for w in real.windows(2) {
                let (a, b) = (w[0], w[1]);
                if a.t_ms <= target && target <= b.t_ms {
                    let dt = b.t_ms - a.t_ms;
                    let f = if dt > 0.0 {
                        (target - a.t_ms) / dt
                    } else {
                        0.0
                    };
                    chosen = (a.x + (b.x - a.x) * f, a.y + (b.y - a.y) * f);
                    break;
                }
            }
            chosen
        };
        // Keep only samples still needed for the next interpolation.
        let keep_from = target - 32.0;
        while self.samples.len() > 2 && self.samples[1].t_ms < keep_from {
            self.samples.pop_front();
        }
        real.clear();
        self.last = Some(pos);
        self.emitted += 1;
        Some(pos)
    }
    pub fn reset(&mut self) {
        self.samples.clear();
        self.last = None;
    }
}

/// Smooth a menu scroll: one frame step per display frame, with velocity clamping and
/// a low-pass filter so uneven touch sample counts cannot produce 10/20/30 px steps.
#[derive(Clone, Debug)]
pub struct ScrollSmoother {
    alpha: f64,
    velocity: f64,
    last_step: f64,
    max_step: f64,
}
impl ScrollSmoother {
    pub fn new(max_step: f64) -> Self {
        Self {
            alpha: 0.35,
            velocity: 0.0,
            last_step: 0.0,
            max_step: max_step.max(1.0),
        }
    }
    /// `raw_velocity` is the pan's instantaneous velocity; `dt_ms` is the frame time.
    pub fn step(&mut self, raw_velocity: f64, dt_ms: f64) -> f64 {
        self.velocity = self.velocity + self.alpha * (raw_velocity - self.velocity);
        let wanted = self.velocity * dt_ms / 1000.0;
        let clamped = wanted.clamp(-self.max_step, self.max_step);
        // Cap frame-to-frame acceleration as well, which kills the visible "dật".
        let accel = self.max_step * 0.35;
        let step = clamped.clamp(self.last_step - accel, self.last_step + accel);
        self.last_step = step;
        step
    }
    pub fn release(&mut self) {
        self.velocity = 0.0;
        self.last_step = 0.0;
    }
}

/// What the player is pointing at (from the engine's existing entity hit test).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityTarget {
    None,
    Villager,
    WanderingTrader,
    Rideable,
    /// Boat/minecart/horse the player is already on.
    CurrentVehicle,
    OtherEntity,
    Block,
}

/// On-screen actions. The three new ones are `pad.*` actions in `actions.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchAction {
    Use,
    Attack,
    Sneak,
    TradeVillager,
    RideEntity,
    Dismount,
}
impl TouchAction {
    pub fn action_name(self) -> &'static str {
        match self {
            Self::Use => "key.use",
            Self::Attack => "key.attack",
            Self::Sneak => "key.sneak",
            Self::TradeVillager => "pad.tradeVillager",
            Self::RideEntity => "pad.rideEntity",
            Self::Dismount => "pad.dismount",
        }
    }
    /// Choose the action a single context-sensitive button should perform.
    pub fn for_target(target: EntityTarget) -> Self {
        match target {
            EntityTarget::Villager | EntityTarget::WanderingTrader => Self::TradeVillager,
            EntityTarget::Rideable => Self::RideEntity,
            EntityTarget::CurrentVehicle => Self::Dismount,
            EntityTarget::None | EntityTarget::OtherEntity | EntityTarget::Block => Self::Use,
        }
    }
}

/// How a touch action reaches the engine. Vanilla's trading, mounting and dismounting
/// are all `Player.interactOn` (right-click) or sneaking — no new game logic is needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionBinding {
    /// Engine actions to press (down) this tick.
    pub press: Vec<&'static str>,
    /// Actions that must be held for `hold_ms` rather than tapped.
    pub hold: Vec<&'static str>,
    pub hold_ms: u32,
    /// Requires a crosshair entity hit; a bare tap on empty air must not open a trade UI.
    pub needs_entity_target: bool,
    pub target_filter: Option<EntityTarget>,
}
pub fn binding_for(action: TouchAction) -> ActionBinding {
    match action {
        TouchAction::Use => ActionBinding {
            press: vec!["key.use"],
            hold: vec![],
            hold_ms: 0,
            needs_entity_target: false,
            target_filter: None,
        },
        TouchAction::Attack => ActionBinding {
            press: vec!["key.attack"],
            hold: vec![],
            hold_ms: 0,
            needs_entity_target: false,
            target_filter: None,
        },
        TouchAction::Sneak => ActionBinding {
            press: vec![],
            hold: vec!["key.sneak"],
            hold_ms: 0,
            needs_entity_target: false,
            target_filter: None,
        },
        TouchAction::TradeVillager => ActionBinding {
            press: vec!["key.use"],
            hold: vec![],
            hold_ms: 0,
            needs_entity_target: true,
            target_filter: Some(EntityTarget::Villager),
        },
        TouchAction::RideEntity => ActionBinding {
            press: vec!["key.use"],
            hold: vec![],
            hold_ms: 0,
            needs_entity_target: true,
            target_filter: Some(EntityTarget::Rideable),
        },
        // Sneak while riding is vanilla's dismount (Entity.dismountTo).
        TouchAction::Dismount => ActionBinding {
            press: vec![],
            hold: vec!["key.sneak"],
            hold_ms: 0,
            needs_entity_target: true,
            target_filter: Some(EntityTarget::CurrentVehicle),
        },
    }
}

/// Validate a context-sensitive press: a trade/ride/dismount without a valid target is
/// dropped instead of being misrouted into a block use or an item use.
pub fn resolve_press(action: TouchAction, target: EntityTarget) -> Option<ActionBinding> {
    let b = binding_for(action);
    if !b.needs_entity_target {
        return Some(b);
    }
    let ok = match action {
        TouchAction::TradeVillager => matches!(
            target,
            EntityTarget::Villager | EntityTarget::WanderingTrader
        ),
        TouchAction::RideEntity => target == EntityTarget::Rideable,
        TouchAction::Dismount => target == EntityTarget::CurrentVehicle,
        _ => true,
    };
    ok.then_some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uneven_touch_delivery_produces_even_frame_steps() {
        // 120 Hz touch samples, 60 Hz display, with a drifting phase: some frames get 1,
        // some get 2 or 3 samples. Straight per-sample application would step 10/20/30 px.
        let mut r = TouchResampler::new(16.67);
        let mut t = 0.0;
        let mut pushed = 0;
        let mut steps = Vec::new();
        let mut prev = 0.0;
        for frame in 0..12 {
            let present = (frame + 1) as f64 * 16.67;
            // Deliver every sample whose timestamp is <= this frame's deadline, in
            // uneven batches (2, 1, 3, 2, ...).
            let batch = [2, 1, 3, 2][frame % 4];
            for _ in 0..batch {
                r.push(TouchSample::new(t, t * 0.6, 0.0));
                t += 8.33;
                pushed += 1;
            }
            if let Some((x, _)) = r.sample_at(present) {
                if frame > 0 {
                    steps.push(x - prev);
                }
                prev = x;
            }
        }
        assert!(pushed >= 20);
        // The first two frames are the cold-start ramp (no history to interpolate from).
        // Steady-state frames must be even even though sample batches stay uneven.
        let steady: Vec<f64> = steps.iter().skip(2).copied().collect();
        let min = steady.iter().copied().fold(f64::INFINITY, f64::min);
        let max = steady.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(max - min < 2.0, "frame steps jittered: {steps:?}");
    }

    #[test]
    fn resampling_has_a_fixed_latency_behind_the_finger() {
        let mut r = TouchResampler::new(16.67).with_latency(21.67);
        for i in 0..20 {
            r.push(TouchSample::new(i as f64 * 8.33, i as f64 * 10.0, 0.0));
        }
        let (x, _) = r.sample_at(100.0).unwrap();
        // target = 78.33 ms -> between sample 9 (75ms,90px) and 10 (83.3ms,100px).
        assert!((x - 94.0).abs() < 1.0, "x={x}");
    }

    #[test]
    fn prediction_is_capped_and_predicted_samples_are_replaced() {
        let mut r = TouchResampler::new(16.67).with_latency(0.0);
        r.push(TouchSample::new(0.0, 0.0, 0.0));
        r.push(TouchSample::new(10.0, 10.0, 0.0));
        let mut p = TouchSample::new(18.0, 18.0, 0.0);
        p.predicted = true;
        r.push(p);
        let (x, _) = r.sample_at(1000.0).unwrap();
        assert!(x <= 18.0, "prediction overshot: {x}");
        r.push(TouchSample::new(20.0, 20.0, 0.0));
        let (x2, _) = r.sample_at(1010.0).unwrap();
        assert!(
            (20.0..=28.0).contains(&x2),
            "real sample must replace the prediction: {x2}"
        );
    }

    #[test]
    fn scroll_smoother_kills_the_10_20_30_px_jitter() {
        let mut s = ScrollSmoother::new(20.0);
        let mut out = Vec::new();
        for v in [600.0, 300.0, 900.0, 300.0, 600.0, 900.0, 300.0] {
            out.push(s.step(v, 16.67));
        }
        let deltas: Vec<f64> = out.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        assert!(
            deltas.iter().all(|d| *d < 5.0),
            "smoothed steps still jump: {out:?}"
        );
        s.release();
        assert_eq!(s.step(0.0, 16.67), 0.0);
    }

    #[test]
    fn a_villager_target_opens_trade_and_a_rideable_mounts() {
        assert_eq!(
            TouchAction::for_target(EntityTarget::Villager),
            TouchAction::TradeVillager
        );
        assert_eq!(
            TouchAction::for_target(EntityTarget::WanderingTrader),
            TouchAction::TradeVillager
        );
        assert_eq!(
            TouchAction::for_target(EntityTarget::Rideable),
            TouchAction::RideEntity
        );
        assert_eq!(
            TouchAction::for_target(EntityTarget::CurrentVehicle),
            TouchAction::Dismount
        );
        assert_eq!(
            binding_for(TouchAction::TradeVillager).press,
            vec!["key.use"]
        );
        assert_eq!(binding_for(TouchAction::RideEntity).press, vec!["key.use"]);
        assert_eq!(binding_for(TouchAction::Dismount).hold, vec!["key.sneak"]);
    }

    #[test]
    fn a_trade_without_a_villager_is_dropped_not_misrouted() {
        assert!(resolve_press(TouchAction::TradeVillager, EntityTarget::None).is_none());
        assert!(resolve_press(TouchAction::TradeVillager, EntityTarget::Block).is_none());
        assert!(resolve_press(TouchAction::RideEntity, EntityTarget::OtherEntity).is_none());
        assert!(resolve_press(TouchAction::Dismount, EntityTarget::Rideable).is_none());
        assert!(resolve_press(TouchAction::TradeVillager, EntityTarget::Villager).is_some());
        assert!(resolve_press(TouchAction::Use, EntityTarget::None).is_some());
    }

    #[test]
    fn latency_is_never_more_than_one_refresh_plus_five_ms() {
        let r = TouchResampler::new(8.33);
        assert!((r.latency_ms - 13.33).abs() < 1e-9);
        let r = TouchResampler::new(16.67);
        assert!((r.latency_ms - 21.67).abs() < 1e-9);
    }
}
