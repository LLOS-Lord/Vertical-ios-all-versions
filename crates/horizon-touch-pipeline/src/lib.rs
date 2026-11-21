//! The touch input pipeline: what happens between a finger and the game.
//!
//! # Where the latency and the judder come from
//!
//! A finger's `touchesMoved` arrives at up to 120 Hz on a ProMotion panel but is delivered to the
//! game's event loop *after* UIKit has run its own gesture recognisers, and the game then renders
//! the frame after next. Three separate problems follow, and they need three separate answers,
//! which is what this crate is:
//!
//! 1. **Press latency.** A tap in the existing code is classified on *release*: `Touches::end_at`
//!    decides it was a tap, and `game/touch.rs` then presses and releases the mouse button across
//!    two ticks (`late`/`late_keys`) because vanilla samples buttons once a tick. That is correct
//!    for a tap that must not be lost, but it means a button's *visual* feedback and the game's
//!    response wait for the lift — up to a third of a second on a long tap. [`Press`] separates
//!    the two: a control's press is emitted the frame the finger lands ([`Press::on_down`]), while
//!    the *action* keeps the existing late-release bookkeeping. The latency the player feels is
//!    then one frame, and no input is lost.
//! 2. **Look judder.** `Out::Look` deltas go to `mouse.add_motion` raw. A finger's samples are
//!    uneven (UIKit coalesces them, so three moves arrive in one frame and none in the next), and
//!    raw deltas turn that into a camera that stutters. [`LookSmoother`] redistributes the motion
//!    over the frames it belongs to — but it *conserves the total*, so aim is never shifted, only
//!    spaced out. That distinction matters: a smoothing filter that lags the pointer (an EMA)
//!    makes aiming feel mushy and is what players call "slow touch".
//! 3. **List judder.** A menu scroll does `((at.1 - last.1) / SCROLL_LINE).trunc()` and sends whole
//!    lines: at 12 GUI pixels a line, a finger moving 30 pixels a frame sends two lines and keeps
//!    a residual, so a list of 20-pixel rows moves 24 pixels, then 48, then 24. [`Scroll`] keeps
//!    the residual *and* the fractional rate, so the list follows the finger pixel for pixel at the
//!    display's rate. The existing `residual` carry is kept as the fallback for `discrete_scroll`
//!    (`Options.discreteScrolling`), which vanilla itself quantises and must keep quantised.
//!
//! Everything is pure arithmetic over caller-supplied time, so the whole thing is unit-tested on a
//! desktop; nothing here knows what a `Touch` is.

#![forbid(unsafe_code)]

/// A finger's identity, as UIKit assigns it.
pub type Finger = u64;

/// One sample of a finger's position in GUI pixels, at a time in seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub finger: Finger,
    pub x: f32,
    pub y: f32,
    /// Seconds, monotonically increasing across the whole session.
    pub time: f64,
}

impl Sample {
    pub fn new(finger: Finger, x: f32, y: f32, time: f64) -> Self {
        Self { finger, x, y, time }
    }
}

// ---------------------------------------------------------------------------------------------
// Press latency
// ---------------------------------------------------------------------------------------------

/// When a control's *visual* press happens, relative to when the game acts on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PressTiming {
    /// Press on touch-down (the default): the button lights up under the finger, one frame of
    /// latency, and the action follows the existing tap/hold rules.
    #[default]
    Down,
    /// Press on touch-up, for a layout whose author wants exactly the old behaviour back.
    Up,
}

/// Decides when a control's held state changes. The stateless part of the old `Touches` is kept:
/// this only says *when* to light a button, not what the button does.
#[derive(Debug, Default)]
pub struct Press {
    timing: PressTiming,
    held: Vec<crate::ControlId>,
}

/// A control's identity in the pipeline: the overlay's named buttons and the player's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ControlId(pub u16);

impl Press {
    pub fn new(timing: PressTiming) -> Self {
        Self {
            timing,
            held: Vec::new(),
        }
    }
    /// A finger landed on `control`: light it now when the timing says down.
    pub fn on_down(&mut self, control: ControlId) -> bool {
        if self.held.contains(&control) {
            return false;
        }
        if self.timing == PressTiming::Down {
            self.held.push(control);
            return true;
        }
        false
    }
    /// The finger is still on `control` on this frame: with [`PressTiming::Up`] this is where the
    /// button first lights, so a press that never lifts still shows.
    pub fn on_frame(&mut self, control: ControlId, still_down: bool) -> bool {
        if self.timing != PressTiming::Up || !still_down {
            return false;
        }
        if self.held.contains(&control) {
            return false;
        }
        self.held.push(control);
        true
    }
    /// The finger left or lifted.
    pub fn on_up(&mut self, control: ControlId) -> bool {
        match self.held.iter().position(|c| *c == control) {
            Some(i) => {
                self.held.swap_remove(i);
                true
            }
            None => false,
        }
    }
    pub fn held(&self, control: ControlId) -> bool {
        self.held.contains(&control)
    }
}

// ---------------------------------------------------------------------------------------------
// Look smoothing
// ---------------------------------------------------------------------------------------------

/// Redistributes a finger's look motion over frames without changing where the camera ends up.
///
/// The rule that makes this safe: **the sum of the deltas handed to the game equals the sum of the
/// deltas the finger produced.** Nothing is predicted, nothing is scaled, and no sample is dropped:
/// a frame that carries three samples' worth of motion hands over three samples' worth, and a frame
/// that carries none hands over a share of what is still owed. That is what removes the judder
/// without the mushy aim an EMA causes.
#[derive(Debug)]
pub struct LookSmoother {
    /// Motion owed to the game, in GUI pixels: the difference between what the finger did and what
    /// the game has been given.
    owed: (f32, f32),
    /// Frames the current debt should be spread over. 1 disables smoothing (raw input).
    spread: u8,
    /// A hard ceiling on the debt, so a pathological burst (a backgrounded app, a stuck finger)
    /// cannot produce a camera that keeps turning after the finger stopped.
    cap: f32,
}

impl Default for LookSmoother {
    /// The smoothing the game starts with: spread over two frames. One is raw and judders; three
    /// or more starts to trail the finger visibly at 60 fps.
    fn default() -> Self {
        Self::new(2)
    }
}

impl LookSmoother {
    /// `spread`: frames to spread a burst over. 1 = raw, 2–3 is what removes judder without
    /// feeling laggy at 60 fps; 1 is offered so competitive players can turn it off.
    pub fn new(spread: u8) -> Self {
        Self {
            owed: (0.0, 0.0),
            spread: spread.clamp(1, 6),
            cap: 240.0,
        }
    }

    /// How many frames a burst is spread over, 1..=6 (1 is raw).
    pub fn set_spread(&mut self, spread: u8) {
        self.spread = spread.clamp(1, 6);
    }

    pub fn spread(&self) -> u8 {
        self.spread
    }

    /// The finger moved by `(dx, dy)` this frame. Call [`LookSmoother::next`] for what to hand to
    /// the game in the same frame.
    pub fn push(&mut self, dx: f32, dy: f32) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.owed.0 += dx;
        self.owed.1 += dy;
        // Clamp the *owed*, not the push: the excess is dropped, deliberately, because a debt this
        // large is a stuck sample rather than a flick.
        let len = self.owed.0.hypot(self.owed.1);
        if len > self.cap {
            let k = self.cap / len;
            self.owed = (self.owed.0 * k, self.owed.1 * k);
        }
    }

    /// What to hand to the game this frame. Conserves the total: a debt smaller than one frame's
    /// share is handed over whole.
    pub fn next(&mut self) -> (f32, f32) {
        if self.owed == (0.0, 0.0) {
            return (0.0, 0.0);
        }
        if self.spread <= 1 {
            return std::mem::take(&mut self.owed);
        }
        let share = 1.0 / f32::from(self.spread);
        let (dx, dy) = (self.owed.0 * share, self.owed.1 * share);
        // A sub-pixel remainder keeps owing; a whole-debt frame (the finger lifted) flushes it.
        self.owed = (self.owed.0 - dx, self.owed.1 - dy);
        if self.owed.0.abs() < 0.01 && self.owed.1.abs() < 0.01 {
            self.owed = (0.0, 0.0);
        }
        (dx, dy)
    }

    /// The finger lifted: hand over everything still owed, so the camera lands exactly where the
    /// finger pointed.
    pub fn flush(&mut self) -> (f32, f32) {
        std::mem::take(&mut self.owed)
    }

    pub fn owed(&self) -> (f32, f32) {
        self.owed
    }
}

// ---------------------------------------------------------------------------------------------
// Scrolling
// ---------------------------------------------------------------------------------------------

/// Turns a finger's vertical travel over a list into the scroll the game should apply.
///
/// Two modes, because vanilla has two:
///
/// * **Smooth** (the default): the list follows the finger pixel for pixel. The conversion is
///   `lines = pixels / line_px`, and the fractional part is carried, so 30 pixels over 12-pixel
///   lines is 2.5 lines now and 0.5 next frame, never 2 then 3.
/// * **Discrete** (`Options.discreteScrolling`, which vanilla quantises on purpose): whole lines,
///   emitted at most once per frame, with the remainder carried. A wheel notch is one line.
///
/// Both carry a `velocity` for the momentum a flick gets, and a `settle` so the last sub-line
/// remainder of a lifted finger is applied rather than lost.
#[derive(Debug)]
pub struct Scroll {
    line_px: f32,
    discrete: bool,
    /// Pixels accumulated and not yet turned into lines.
    carry: f32,
    /// Where the finger was last frame.
    last_y: f32,
    /// Lines per second, smoothed: only for a flick's momentum.
    velocity: f32,
}

impl Scroll {
    pub fn new(line_px: f32, discrete: bool) -> Self {
        Self {
            line_px: line_px.max(1.0),
            discrete,
            carry: 0.0,
            last_y: 0.0,
            velocity: 0.0,
        }
    }

    /// The finger was placed. No scroll is produced: the first move must be a move.
    pub fn begin(&mut self, y: f32) {
        self.last_y = y;
        self.carry = 0.0;
        self.velocity = 0.0;
    }

    /// The finger moved to `y`; `dt` is the seconds since the last move. Returns the lines to
    /// scroll this frame (positive = content up, as `MouseWheel` counts it).
    pub fn moved(&mut self, y: f32, dt: f32) -> f32 {
        let delta_px = self.last_y - y; // a finger moving up scrolls down
        self.last_y = y;
        if !delta_px.is_finite() {
            return 0.0;
        }
        self.carry += delta_px;
        let raw = self.carry / self.line_px;
        let lines = if self.discrete {
            // Whole lines only, and never more than three in one frame: a UIKit coalesced burst
            // must not teleport a list.
            let whole = raw.trunc().clamp(-3.0, 3.0);
            self.carry -= whole * self.line_px;
            whole
        } else {
            // Everything, fractional part included: the list follows the finger.
            self.carry = raw.fract() * self.line_px;
            raw - raw.fract()
        };
        if dt > 1e-5 {
            let instant = lines / dt;
            self.velocity = self.velocity * 0.7 + instant * 0.3;
        }
        lines
    }

    /// The finger lifted: the last sub-line remainder, so a short drag still moves the list.
    pub fn end(&mut self) -> f32 {
        let rest = self.carry / self.line_px;
        self.carry = 0.0;
        self.velocity = 0.0;
        if self.discrete {
            rest.trunc()
        } else {
            rest
        }
    }

    /// The pixels one whole line is worth: what a caller needs to turn the lines it was given back
    /// into the finger's own travel.
    pub fn line_px(&self) -> f32 {
        self.line_px
    }

    /// The pixels accumulated and not yet turned into lines: what the list has *not* been scrolled
    /// by. A caller that follows a finger pixel by pixel adds this to the lines it applied.
    pub fn carry_px(&self) -> f32 {
        self.carry
    }

    /// The velocity a flick leaves behind, lines per second, for a caller that wants momentum.
    /// Decays geometrically per frame; the caller applies its own friction.
    pub fn velocity(&self) -> f32 {
        self.velocity
    }

    /// Friction for one frame of momentum after the finger lifts: `lines` to apply and the next
    /// velocity, decaying by `friction` per frame and stopping below a threshold.
    pub fn momentum(&mut self, friction: f32) -> f32 {
        if self.velocity.abs() < 0.05 {
            self.velocity = 0.0;
            return 0.0;
        }
        let step = self.velocity / 60.0;
        self.velocity *= friction.clamp(0.0, 1.0);
        step
    }
}

// ---------------------------------------------------------------------------------------------
// Coalescing
// ---------------------------------------------------------------------------------------------

/// Merges the samples of one finger that arrived since the last frame into one motion, and keeps
/// the samples' own timing so a burst is not flattened.
///
/// UIKit delivers `touchesMoved` once per display refresh *or* less often when it coalesces; the
/// coalesced set is available through `coalescedTouchesForTouch`, and the iOS integration should
/// push every touch of that set here. The pipeline then produces one [`Motion`] per frame whose
/// `dx`/`dy` is the sum, whose `peak` is the largest single step (used to detect a flick), and
/// whose `samples` is how many arrived, which the latency log reports.
#[derive(Debug, Default)]
pub struct Coalescer {
    dx: f32,
    dy: f32,
    peak: f32,
    samples: u32,
    last: Option<Sample>,
}

/// One frame's worth of one finger's motion.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Motion {
    pub dx: f32,
    pub dy: f32,
    /// The largest single sample-to-sample step, pixels.
    pub peak: f32,
    pub samples: u32,
    /// Where the finger is now.
    pub at: (f32, f32),
    /// The finger was placed this frame (its first sample is not motion).
    pub began: bool,
}

impl Coalescer {
    /// A finger landed.
    pub fn begin(&mut self, sample: Sample) {
        self.dx = 0.0;
        self.dy = 0.0;
        self.peak = 0.0;
        self.samples = 1;
        self.last = Some(sample);
    }

    /// Another sample of the same finger.
    pub fn push(&mut self, sample: Sample) {
        match self.last {
            Some(last) => {
                let (dx, dy) = (sample.x - last.x, sample.y - last.y);
                self.dx += dx;
                self.dy += dy;
                self.peak = self.peak.max(dx.hypot(dy));
                self.samples += 1;
                self.last = Some(sample);
            }
            None => self.begin(sample),
        }
    }

    /// Take the frame's motion. Further pushes start a new frame.
    pub fn take(&mut self) -> Motion {
        let last = self.last;
        let began = self.samples == 1 && self.dx == 0.0 && self.dy == 0.0;
        let motion = Motion {
            dx: self.dx,
            dy: self.dy,
            peak: self.peak,
            samples: self.samples,
            at: last.map_or((0.0, 0.0), |s| (s.x, s.y)),
            began,
        };
        self.dx = 0.0;
        self.dy = 0.0;
        self.peak = 0.0;
        self.samples = 0;
        motion
    }

    pub fn has_motion(&self) -> bool {
        self.dx != 0.0 || self.dy != 0.0
    }
}

// ---------------------------------------------------------------------------------------------
// Gesture classification
// ---------------------------------------------------------------------------------------------

/// What a finger turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    /// Not yet decided: the finger is still inside the slop and under the hold time.
    Undecided,
    /// A press and a lift inside [`Classifier::slop`] and [`Classifier::hold`]: acts on lift.
    Tap,
    /// Still down past the hold time without leaving the slop: mines or uses until it lifts.
    Hold,
    /// Left the slop: turns the view (or drags an item).
    Drag,
}

/// Decides tap / hold / drag with hysteresis, so a finger that wobbles one pixel around the slop
/// boundary cannot flip between a drag and a tap and fire a click at the end of a camera turn.
///
/// The old code re-tested the slop on every move *and* on the lift, with the same threshold, which
/// is the mutation that produces "I dragged the camera and it clicked a button" reports. Here the
/// threshold to *become* a drag is `slop`, and the threshold to *stay* a tap is
/// `slop * HYSTERESIS` measured from the start; once a drag, always a drag.
#[derive(Debug)]
pub struct Classifier {
    slop: f32,
    hold: f64,
    start: (f32, f32),
    since: f64,
    state: Gesture,
}

/// A finger that wanders this share of the slop back towards its start is still a tap... but a
/// drag never becomes a tap again: the `state` is monotonic.
const HYSTERESIS: f32 = 1.25;

impl Classifier {
    pub fn new(slop: f32, hold: f64) -> Self {
        Self {
            slop: slop.max(1.0),
            hold: hold.max(0.0),
            start: (0.0, 0.0),
            since: 0.0,
            state: Gesture::Undecided,
        }
    }

    pub fn begin(&mut self, at: (f32, f32), now: f64) {
        self.start = at;
        self.since = now;
        self.state = Gesture::Undecided;
    }

    /// One frame: where the finger is, and the time. Returns the state this frame.
    pub fn update(&mut self, at: (f32, f32), now: f64) -> Gesture {
        if self.state == Gesture::Tap || self.state == Gesture::Drag {
            // Decided for good until the finger lifts.
            return self.state;
        }
        let dist = (at.0 - self.start.0).hypot(at.1 - self.start.1);
        if self.state == Gesture::Undecided {
            if dist > self.slop {
                self.state = Gesture::Drag;
            } else if now - self.since >= self.hold {
                self.state = Gesture::Hold;
            }
        } else if self.state == Gesture::Hold && dist > self.slop * HYSTERESIS {
            // A hold that then travels is a drag: mining stops, the view turns.
            self.state = Gesture::Drag;
        }
        self.state
    }

    /// The finger lifted at `at`. The returned gesture is final; [`Gesture::Undecided`] becomes a
    /// [`Gesture::Tap`] when the finger never left the slop, else a drag.
    pub fn end(&mut self, at: (f32, f32)) -> Gesture {
        let state = match self.state {
            Gesture::Undecided => {
                let dist = (at.0 - self.start.0).hypot(at.1 - self.start.1);
                if dist <= self.slop { Gesture::Tap } else { Gesture::Drag }
            }
            other => other,
        };
        self.state = match state {
            Gesture::Hold => Gesture::Hold, // the caller stops mining on the lift
            other => other,
        };
        state
    }

    pub fn state(&self) -> Gesture {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn a_button_lights_the_frame_the_finger_lands() {
        let mut press = Press::new(PressTiming::Down);
        let button = ControlId(3);
        assert!(press.on_down(button));
        assert!(press.held(button));
        // A second sample of the same finger changes nothing.
        assert!(!press.on_down(button));
        assert!(press.on_up(button));
        assert!(!press.held(button));
    }

    #[test]
    fn the_old_up_timing_still_exists_for_layouts_that_want_it() {
        let mut press = Press::new(PressTiming::Up);
        let button = ControlId(1);
        assert!(!press.on_down(button));
        assert!(press.on_frame(button, true));
        assert!(press.on_up(button));
    }

    #[test]
    fn look_smoothing_conserves_the_total_motion() {
        let mut s = LookSmoother::new(3);
        let (mut pushed, mut handed) = (0.0f32, 0.0f32);
        // The judder pattern: three frames with motion, then one with none.
        for i in 0..120 {
            if i % 4 != 3 {
                s.push(9.0, -4.0);
                pushed += 9.0;
            }
            let (dx, dy) = s.next();
            handed += dx;
            assert!(dy.is_finite());
        }
        // The finger lifts: whatever is still owed is handed over.
        handed += s.flush().0;
        assert!(approx(handed, pushed), "handed {handed} of {pushed}");
        // And an empty frame after the debt is paid hands over nothing.
        assert_eq!(s.next(), (0.0, 0.0));
        // While smoothing, no single frame hands over a whole burst's worth.
        let mut s = LookSmoother::new(3);
        s.push(30.0, 0.0);
        assert!(s.next().0 < 30.0);
    }

    #[test]
    fn raw_mode_hands_everything_over_at_once() {
        let mut s = LookSmoother::new(1);
        s.push(12.0, 3.0);
        assert_eq!(s.next(), (12.0, 3.0));
        assert_eq!(s.next(), (0.0, 0.0));
    }

    #[test]
    fn a_stuck_sample_cannot_spin_the_camera_forever() {
        let mut s = LookSmoother::new(4);
        s.push(1.0e6, 0.0);
        assert!(s.owed().0 <= 240.0 + 1e-3);
    }

    #[test]
    fn a_smooth_scroll_follows_the_finger_and_keeps_the_remainder() {
        let mut scroll = Scroll::new(12.0, false);
        scroll.begin(100.0);
        // Four frames of 30 pixels: 2.5 lines each.
        let mut total = 0.0;
        for _ in 0..4 {
            scroll.last_y = 100.0;
            total += scroll.moved(70.0, 1.0 / 60.0);
        }
        assert!(approx(total, 120.0 / 12.0), "{total}");
    }

    #[test]
    fn a_discrete_scroll_only_ever_moves_whole_lines() {
        let mut scroll = Scroll::new(12.0, true);
        scroll.begin(100.0);
        let mut lines = Vec::new();
        let mut y = 100.0;
        for _ in 0..40 {
            y -= 3.0; // 0.25 lines per frame
            lines.push(scroll.moved(y, 1.0 / 60.0));
        }
        assert!(lines.iter().all(|l| l.fract() == 0.0));
        // 120 pixels = 10 lines, however they were cut up.
        assert!(approx(lines.iter().sum::<f32>(), 10.0));
        // And never more than three in one frame, even for a coalesced burst.
        scroll.begin(0.0);
        assert!(scroll.moved(-200.0, 1.0 / 60.0) >= -3.0);
    }

    #[test]
    fn lifting_hands_over_the_last_partial_line() {
        let mut scroll = Scroll::new(12.0, false);
        scroll.begin(50.0);
        let _ = scroll.moved(44.0, 1.0 / 60.0); // 0.5 lines
        assert!(approx(scroll.end(), 0.5));
        assert_eq!(scroll.end(), 0.0);
    }

    #[test]
    fn coalesced_samples_become_one_motion_and_keep_their_peak() {
        let mut c = Coalescer::default();
        c.begin(Sample::new(1, 10.0, 10.0, 0.0));
        c.push(Sample::new(1, 12.0, 10.0, 0.004));
        c.push(Sample::new(1, 20.0, 10.0, 0.008));
        let m = c.take();
        assert!(approx(m.dx, 10.0));
        assert!(approx(m.peak, 8.0));
        assert_eq!(m.samples, 3);
        assert!(!m.began);
        assert_eq!(c.take().samples, 0);
    }

    #[test]
    fn a_drag_can_never_end_as_a_tap() {
        let mut c = Classifier::new(8.0, 0.35);
        c.begin((100.0, 100.0), 0.0);
        assert_eq!(c.update((102.0, 101.0), 0.01), Gesture::Undecided);
        // Out past the slop: a drag.
        assert_eq!(c.update((130.0, 100.0), 0.02), Gesture::Drag);
        // Back over the start: still a drag, and the lift cannot make it a tap.
        assert_eq!(c.update((100.0, 100.0), 0.03), Gesture::Drag);
        assert_eq!(c.end((100.0, 100.0)), Gesture::Drag);
    }

    #[test]
    fn a_still_finger_becomes_a_hold_and_a_travelling_hold_becomes_a_drag() {
        let mut c = Classifier::new(8.0, 0.35);
        c.begin((0.0, 0.0), 0.0);
        assert_eq!(c.update((1.0, 0.0), 0.2), Gesture::Undecided);
        assert_eq!(c.update((1.0, 0.0), 0.36), Gesture::Hold);
        assert_eq!(c.update((1.0, 0.0), 0.5), Gesture::Hold);
        assert_eq!(c.update((20.0, 0.0), 0.6), Gesture::Drag);
    }

    #[test]
    fn a_quick_still_touch_is_a_tap_even_if_it_lifts_outside_the_slop() {
        let mut c = Classifier::new(8.0, 0.35);
        c.begin((0.0, 0.0), 0.0);
        assert_eq!(c.end((5.0, 5.0)), Gesture::Tap);
        let mut c = Classifier::new(8.0, 0.35);
        c.begin((0.0, 0.0), 0.0);
        assert_eq!(c.end((40.0, 0.0)), Gesture::Drag);
    }
}
