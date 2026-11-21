//! Whether the next frame may be generated, and how long the renderer may wait for it.
//!
//! See the crate docs for why this exists. The short version: Frame Generation is an option that
//! must prove it is helping, every frame, from measurements — never from the setting alone.
//!
//! The policy in one paragraph. A made frame only buys half a frame's time, so it can only ever be
//! worth its cost when a real frame already fits inside the display's refresh window with room to
//! spare. So: measure the real frame time and the display's period; generate only when the real
//! frame is faster than the period, and only at the multiplier the measurement supports. Never let
//! the render thread wait for the present thread for longer than a small share of a frame; a wait
//! longer than that is a stall the player feels, so it suspends generation for a while and reports
//! why. When made frames are being dropped (`made_share` short of what was asked for), step the
//! multiplier down before stepping it back up.

use crate::device::DeviceProfile;

/// The user's setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameGenSettings {
    /// Off (the default on every device).
    #[default]
    Off,
    /// On, paced by measurement: the mode that should be offered on a phone.
    Auto,
    /// On at the requested multiplier whatever the measurements say, for A/B comparison. The
    /// wait timeout still applies, because a freeze is not a diagnostic.
    Force,
}

impl FrameGenSettings {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "on" | "adaptive" => Self::Auto,
            "force" | "always" => Self::Force,
            _ => Self::Off,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Force => "force",
        }
    }
}

/// Why generation is on or off this frame. The renderer logs it once per transition and the
/// performance overlay can show it, which is what turns "frame generation freezes the game" into a
/// report with a cause in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The setting is off.
    Off,
    /// This device does not offer it (an A11, or a 60 Hz panel).
    Unsupported,
    /// Measuring the scene: no made frame until the real frame times settle.
    WarmUp,
    /// Real frames are slower than the display's period: a made frame would lengthen the queue.
    GpuBound,
    /// Generating, at the stated multiplier.
    Generating,
    /// The present thread made the render thread wait: generation is paused for a cool-down.
    PresentStall,
    /// The display is not absorbing the made frames (they are being dropped).
    Dropping,
    /// A shader pack or another pass owns the frame: nothing to interpolate.
    ExternalPost,
    /// The game is not in a world (a menu, the title screen, a loading screen).
    NotInWorld,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Unsupported => "unsupported",
            Self::WarmUp => "warm-up",
            Self::GpuBound => "gpu-bound",
            Self::Generating => "generating",
            Self::PresentStall => "present-stall",
            Self::Dropping => "dropping",
            Self::ExternalPost => "external-post",
            Self::NotInWorld => "not-in-world",
        }
    }
}

/// What the renderer measured on the last real frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurements {
    /// Smoothed time between the starts of two real frames, ms.
    pub real_ms: f32,
    /// The GPU time of the last real frame, ms, when the backend can report it.
    pub gpu_ms: Option<f32>,
    /// The share of real frames that actually had a made frame shown, 0..=1.
    pub made_share: f32,
    /// The display's refresh rate, Hz (60, 120, or whatever a ProMotion panel is doing).
    pub display_hz: f32,
    /// How long the render thread waited for a free frame pair last frame, ms.
    pub present_wait_ms: f32,
}

impl Default for Measurements {
    fn default() -> Self {
        Self {
            real_ms: 16.7,
            gpu_ms: None,
            made_share: 0.0,
            display_hz: 60.0,
            present_wait_ms: 0.0,
        }
    }
}

impl Measurements {
    /// The display's period, ms.
    pub fn period_ms(&self) -> f32 {
        if self.display_hz.is_finite() && self.display_hz >= 24.0 {
            1000.0 / self.display_hz
        } else {
            16.7
        }
    }
    /// The GPU's share of the frame when known, else the whole frame: the conservative choice.
    fn gpu(&self) -> f32 {
        self.gpu_ms.unwrap_or(self.real_ms).max(0.0)
    }
}

/// One frame's answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    /// Whether to make a frame between this one and the next.
    pub generate: bool,
    /// The multiplier asked for: 2.0 means one made frame per real frame.
    pub multiplier: f32,
    /// The longest the render thread may block waiting for the present thread, ms. A short timeout
    /// is what keeps a slow present thread from freezing the game.
    pub present_timeout_ms: f32,
    pub reason: Reason,
}

impl Decision {
    fn off(reason: Reason, timeout_ms: f32) -> Self {
        Self {
            generate: false,
            multiplier: 1.0,
            present_timeout_ms: timeout_ms,
            reason,
        }
    }
}

/// The state machine. One per renderer.
#[derive(Debug)]
pub struct Admission {
    settings: FrameGenSettings,
    device: DeviceProfile,
    /// Real frames seen since the last scene change: the warm-up.
    warm: u32,
    /// Frames left in a cool-down after a stall or a run of drops.
    cool_down: u32,
    /// The multiplier currently being attempted.
    multiplier: f32,
    /// Consecutive frames whose made frames were dropped.
    dropping: u32,
    /// Consecutive frames that were GPU bound.
    bound: u32,
    /// The last decision's reason, so a transition can be logged exactly once.
    last_reason: Reason,
}

/// Real frames measured before the policy is trusted with an opinion.
const WARM_FRAMES: u32 = 45;
/// A stall pauses generation for this many real frames (about two seconds at 60 Hz).
const STALL_COOL: u32 = 120;
/// A run of dropped made frames pauses generation for this many real frames.
const DROP_COOL: u32 = 60;
/// How many frames of dropping in a row before the multiplier steps down.
const DROP_STREAK: u32 = 30;
/// The present wait never exceeds this share of the real frame.
const WAIT_SHARE: f32 = 0.12;
/// ...nor this many milliseconds, on any device. Two frames of a 120 Hz panel.
const WAIT_CEILING_MS: f32 = 8.0;
/// The render thread needs this much headroom under the display period before a made frame is
/// worth it: the made frame's copy and present cost is not free.
const HEADROOM: f32 = 0.88;

impl Admission {
    pub fn new(device: DeviceProfile, settings: FrameGenSettings) -> Self {
        Self {
            settings,
            device,
            warm: 0,
            cool_down: 0,
            multiplier: 2.0,
            dropping: 0,
            bound: 0,
            last_reason: Reason::WarmUp,
        }
    }

    pub fn settings(&self) -> FrameGenSettings {
        self.settings
    }

    /// A new setting, a new scene (a world opened, a resolution change) or a thermal change:
    /// forget the measurements and start the warm-up again.
    pub fn reconfigure(&mut self, settings: FrameGenSettings) {
        if settings != self.settings {
            self.settings = settings;
            self.warm = 0;
            self.cool_down = 0;
            self.multiplier = 2.0;
            self.dropping = 0;
            self.bound = 0;
        }
    }

    /// The renderer knows something this frame that makes interpolation meaningless.
    pub fn scene_reset(&mut self) {
        self.warm = 0;
    }

    /// Whether the last call reported a different reason, for one log line per transition.
    pub fn take_reason_change(&mut self, reason: Reason) -> Option<Reason> {
        if reason == self.last_reason {
            return None;
        }
        self.last_reason = reason;
        Some(reason)
    }

    /// The present-thread timeout for this frame: a share of the frame, capped.
    pub fn present_timeout_ms(&self, m: &Measurements) -> f32 {
        let share = (m.real_ms * WAIT_SHARE).clamp(0.5, WAIT_CEILING_MS);
        if self.device.tier <= crate::device::DeviceTier::Mobile {
            share.min(3.0)
        } else {
            share
        }
    }

    /// One real frame's measurements in, one decision out. `in_world` and `external_post` are the
    /// renderer's own preconditions (a menu, a shader pack's post pass, HDR).
    pub fn observe(
        &mut self,
        m: &Measurements,
        in_world: bool,
        external_post: bool,
    ) -> Decision {
        let timeout = self.present_timeout_ms(m);
        if self.settings == FrameGenSettings::Off {
            return Decision::off(Reason::Off, timeout);
        }
        if !self.device.frame_gen_allowed() {
            return Decision::off(Reason::Unsupported, timeout);
        }
        if !in_world {
            self.scene_reset();
            return Decision::off(Reason::NotInWorld, timeout);
        }
        if external_post {
            self.scene_reset();
            return Decision::off(Reason::ExternalPost, timeout);
        }
        // A stall is never ignored, whatever the mode: it is the freeze the player feels.
        if m.present_wait_ms > self.present_timeout_ms(m) + 0.25 {
            self.cool_down = self.cool_down.max(STALL_COOL);
            self.dropping = 0;
            self.bound = 0;
            return Decision::off(Reason::PresentStall, timeout);
        }
        if self.cool_down > 0 {
            self.cool_down -= 1;
            return Decision::off(Reason::PresentStall, timeout);
        }
        if self.warm < WARM_FRAMES {
            self.warm += 1;
            return Decision::off(Reason::WarmUp, timeout);
        }
        let period = m.period_ms();
        // Two real frames must fit in one refresh window with headroom, or a made frame only makes
        // the queue longer. `gpu` is the conservative figure when the backend reports one.
        let fits = m.real_ms <= period * HEADROOM && m.gpu() <= period * 2.0 * HEADROOM;
        if !fits {
            self.bound = self.bound.saturating_add(1);
            // Force still generates: an A/B test needs the other side of the comparison. It keeps
            // the stall timeout, but it does not pretend the frame rate is going to improve.
            if self.settings == FrameGenSettings::Force {
                return Decision {
                    generate: true,
                    multiplier: 2.0,
                    present_timeout_ms: timeout,
                    reason: Reason::GpuBound,
                };
            }
            return Decision::off(Reason::GpuBound, timeout);
        }
        self.bound = 0;
        if self.settings == FrameGenSettings::Auto {
            // Did the display show the made frames that were asked for?
            let asked = self.multiplier - 1.0; // the share of real frames that should be made
            if m.made_share + 0.25 < asked {
                self.dropping += 1;
                if self.dropping >= DROP_STREAK {
                    self.dropping = 0;
                    if self.multiplier > 1.0 {
                        self.multiplier -= 0.5;
                        self.warm = 0;
                    }
                    if self.multiplier <= 1.0 {
                        self.cool_down = DROP_COOL;
                        self.multiplier = 2.0;
                        return Decision::off(Reason::Dropping, timeout);
                    }
                    self.cool_down = DROP_COOL / 4;
                    return Decision::off(Reason::Dropping, timeout);
                }
            } else if m.made_share + 0.1 >= asked {
                self.dropping = 0;
            }
            // Room for the asked-for multiplier, and the display has a window to fill?
            let want = if m.real_ms * 2.0 <= period * 2.0 * HEADROOM {
                2.0
            } else {
                1.5
            };
            if want > self.multiplier {
                self.warm = self.warm.saturating_sub(1);
            } else {
                self.multiplier = want;
            }
        }
        Decision {
            generate: true,
            multiplier: self.multiplier,
            present_timeout_ms: timeout,
            reason: Reason::Generating,
        }
    }

    /// Whether the made frame already in hand should be presented, given the real frame that
    /// followed it arrived late: presenting it then would show an older frame over a newer one.
    pub fn present_made(&self, m: &Measurements) -> bool {
        m.real_ms <= m.period_ms() * 1.15
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{AppleSoc, DeviceProfile};

    fn pro() -> DeviceProfile {
        let mut profile = DeviceProfile::BALANCED;
        profile.soc = Some(AppleSoc::A15);
        profile.promotion = true;
        profile
    }

    fn fast(hz: f32) -> Measurements {
        Measurements {
            real_ms: 1000.0 / (hz * 2.0), // real frames at twice the refresh
            gpu_ms: Some(1000.0 / (hz * 2.5)),
            made_share: 1.0,
            display_hz: hz,
            present_wait_ms: 0.2,
        }
    }

    /// Run the warm-up; the next `observe` is the first judged frame.
    fn warm_up(a: &mut Admission, m: &Measurements) {
        for frame in 0..WARM_FRAMES {
            let d = a.observe(m, true, false);
            assert!(!d.generate, "frame {frame}: {:?}", d.reason);
        }
    }

    #[test]
    fn off_is_off_and_says_so() {
        let mut a = Admission::new(pro(), FrameGenSettings::Off);
        let d = a.observe(&fast(120.0), true, false);
        assert!(!d.generate);
        assert_eq!(d.reason, Reason::Off);
    }

    #[test]
    fn an_a11_never_gets_frame_generation() {
        let mut a = Admission::new(DeviceProfile::LEGACY, FrameGenSettings::Auto);
        let d = a.observe(&fast(120.0), true, false);
        assert!(!d.generate);
        assert_eq!(d.reason, Reason::Unsupported);
    }

    #[test]
    fn a_slow_gpu_is_refused_in_auto_mode() {
        let mut a = Admission::new(pro(), FrameGenSettings::Auto);
        let slow = Measurements {
            real_ms: 30.0,
            gpu_ms: Some(28.0),
            made_share: 0.0,
            display_hz: 120.0,
            present_wait_ms: 0.0,
        };
        warm_up(&mut a, &slow);
        let d = a.observe(&slow, true, false);
        assert!(!d.generate);
        assert_eq!(d.reason, Reason::GpuBound);
    }

    #[test]
    fn a_fast_frame_generates_only_after_the_warm_up() {
        let mut a = Admission::new(pro(), FrameGenSettings::Auto);
        let m = fast(120.0);
        warm_up(&mut a, &m);
        let d = a.observe(&m, true, false);
        assert!(d.generate);
        assert!(d.present_timeout_ms <= WAIT_CEILING_MS);
    }

    #[test]
    fn a_present_stall_suspends_generation_even_in_force_mode() {
        let mut a = Admission::new(pro(), FrameGenSettings::Force);
        let mut m = fast(120.0);
        warm_up(&mut a, &m);
        assert!(a.observe(&m, true, false).generate);
        // The present thread took 20 ms: the render thread stalled.
        m.present_wait_ms = 20.0;
        let d = a.observe(&m, true, false);
        assert!(!d.generate);
        assert_eq!(d.reason, Reason::PresentStall);
        // And it stays suspended while the cool-down runs, even once the wait is gone.
        m.present_wait_ms = 0.0;
        for _ in 0..10 {
            assert!(!a.observe(&m, true, false).generate);
        }
    }

    #[test]
    fn made_frames_the_display_drops_step_the_multiplier_down_then_give_up() {
        let mut a = Admission::new(pro(), FrameGenSettings::Auto);
        let mut m = fast(120.0);
        m.made_share = 0.0;
        warm_up(&mut a, &m);
        let mut saw_dropping = false;
        for _ in 0..(DROP_STREAK * 4) {
            let d = a.observe(&m, true, false);
            saw_dropping |= d.reason == Reason::Dropping;
        }
        assert!(saw_dropping, "the policy must notice dropped made frames");
    }

    #[test]
    fn a_menu_or_a_shader_post_pass_turns_it_off_cleanly() {
        let mut a = Admission::new(pro(), FrameGenSettings::Auto);
        let m = fast(120.0);
        assert_eq!(a.observe(&m, false, false).reason, Reason::NotInWorld);
        assert_eq!(a.observe(&m, true, true).reason, Reason::ExternalPost);
    }

    #[test]
    fn the_reason_is_reported_once_per_transition() {
        // A fresh admission starts in the warm-up, so the first reason it is told is always a
        // transition, and the second identical one is not.
        let mut a = Admission::new(pro(), FrameGenSettings::Off);
        assert_eq!(a.take_reason_change(Reason::Off), Some(Reason::Off));
        assert_eq!(a.take_reason_change(Reason::Off), None);
        assert_eq!(a.take_reason_change(Reason::WarmUp), Some(Reason::WarmUp));
        assert_eq!(a.take_reason_change(Reason::WarmUp), None);
    }
}
