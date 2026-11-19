//! Frame-generation governor: "bật lên là đứng máy" must never happen again.
//!
//! ## Root cause found in the tree
//!
//! `crates/horizon-client-render/src/gfx/frame_gen.rs`:
//! * `FrameGen::frames()` calls `presenter.free.recv()` **twice with no timeout**;
//! * `Presenter::start()` runs `surface.get_current_texture()` and `queue.present()` on a
//!   **present thread**, uses `thread::sleep()` for pacing, and `Drop` does a blocking
//!   `join()`;
//! * `send.send(pair)` is a bounded `sync_channel(1)` blocking send;
//! * `FRAMES = 6` full-screen textures + an extra `world` texture per screen size.
//!
//! On a GPU whose FSR 3.1 optical-flow/interpolation dispatches exceed the frame budget,
//! the drawable pool starves; the unbounded `recv()` blocks the render thread forever —
//! exactly "game đứng, không thao tác được gì" — and 6+ full-resolution textures push a
//! jetsam-limited iPhone into memory pressure. Apple's own MetalFX guidance warns
//! "Deadlock: Ensure Present() is called from the render thread, not the present thread"
//! and requires **>= 30 real FPS** before interpolation (WWDC25 / game-porting-toolkit).
//!
//! ## The governor
//!
//! * capability gate by chip + OS + unified memory (MetalFX frame interpolation needs
//!   A14/M1 + iOS 26; Metal 3's interpolator needs iOS 18);
//! * a hard **bounded wait** for every cross-thread hand-off (`max_wait_ms` = 8 ms);
//! * a watchdog: a stalled presenter / missed frame trips an immediate `Off` + normal
//!   renderer fallback, with recovery requiring a sustained run of good frames;
//! * adaptive degradation on GPU time, low real FPS, memory pressure and thermals;
//! * fail-closed: unsupported devices get `UpscaleOnly`, never a blocking present thread.

use crate::apple::{Chip, ChipTier, ThermalState};

/// How an intermediate frame is produced on this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameGenMode {
    /// No made frames. The normal renderer acquires and presents on the render thread.
    Off,
    /// Apple MetalFX `MTLFXFrameInterpolator` / `MTL4FXFrameInterpolator`.
    /// Render thread owns presentation; the governor only supplies the pacing deadline.
    MetalFxInterpolation,
    /// The tree's FSR 3.1 WGSL optical flow, allowed only on capable devices with a
    /// bounded present path. Never used as a silent fallback on old chips.
    OpticalFlowAdaptive,
    /// Lower render resolution + a cheap spatial upscale (MetalFX on A14/M1+, a
    /// linear blit on older chips), no made frames. This is the honest
    /// "I still want the feature" answer on older hardware.
    UpscaleOnly,
}

impl FrameGenMode {
    pub fn makes_frames(self) -> bool {
        matches!(self, Self::MetalFxInterpolation | Self::OpticalFlowAdaptive)
    }
}

#[derive(Clone, Debug)]
pub struct DeviceCapability {
    pub chip: Chip,
    pub os_major: u32,
    /// `phys_footprint + os_proc_available_memory()` at launch, in MiB.
    pub process_limit_mb: u64,
    pub metal_fx_temporal_available: bool,
}
impl DeviceCapability {
    pub fn metal4_frame_interpolation(&self) -> bool {
        self.chip.tier.supports_metal4()
            && self.os_major >= 26
            && self.metal_fx_temporal_available
            && self.chip.unified_memory_mb >= 3072
    }
    pub fn metal3_frame_interpolation(&self) -> bool {
        self.chip.tier.supports_metal3_frame_interpolation()
            && self.os_major >= 18
            && self.metal_fx_temporal_available
            && self.chip.unified_memory_mb >= 3072
    }
    /// The tree's FSR compute path needs headroom the MetalFX path does not.
    pub fn optical_flow_feasible(&self) -> bool {
        self.chip.tier.supports_metal4()
            && self.chip.unified_memory_mb >= 4096
            && self.process_limit_mb >= 1400
    }
    pub fn spatial_upscaler(&self) -> bool {
        // The app's floor is iOS 16. A lower render resolution + linear upscale works
        // on every supported device; MetalFX's spatial scaler is used when available.
        self.os_major >= 16
    }
    /// What the player's "Frame generation" toggle is allowed to request.
    pub fn best_mode(&self) -> FrameGenMode {
        if self.metal4_frame_interpolation() || self.metal3_frame_interpolation() {
            FrameGenMode::MetalFxInterpolation
        } else if self.optical_flow_feasible() {
            FrameGenMode::OpticalFlowAdaptive
        } else if self.spatial_upscaler() {
            FrameGenMode::UpscaleOnly
        } else {
            FrameGenMode::Off
        }
    }
}

/// One frame's measured state. All values come from the engine's own timing/GPU queries.
#[derive(Clone, Copy, Debug)]
pub struct FrameSample {
    /// REAL rendered frames per second (before any made frames).
    pub real_fps: f32,
    /// GPU time of the whole frame, including interpolation, in ms.
    pub gpu_ms: f32,
    /// How long the presenter failed to return a drawable / free texture, in ms.
    pub presenter_stall_ms: u32,
    /// `os_proc_available_memory()` in MiB.
    pub available_memory_mb: u64,
    pub thermal: ThermalState,
    pub foreground: bool,
    /// Display refresh interval in ms (8.33 at 120 Hz, 16.67 at 60 Hz).
    pub frame_interval_ms: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FrameGenDecision {
    /// Render normally; present on the render thread.
    Normal,
    /// Make one intermediate frame; the display link owns the presentation deadline.
    Generate { deadline_ms: f32 },
    /// Render at a lower resolution and upscale; no made frames.
    UpscaleOnly,
}

impl FrameGenDecision {
    pub fn generates(&self) -> bool {
        matches!(self, FrameGenDecision::Generate { .. })
    }
}

/// The longest any frame-generation hand-off may block the render thread.
pub const MAX_WAIT_MS: u32 = 8;
/// Apple requires at least 30 real FPS before interpolation is useful/stable.
pub const MIN_REAL_FPS: f32 = 30.0;
/// GPU time allowed for the interpolation itself (a 60 Hz frame is 16.67 ms).
pub const MAX_INTERPOLATION_GPU_MS: f32 = 8.0;
/// A presenter that misses two display intervals is considered stalled.
pub const STALL_FACTOR: f32 = 2.0;
/// Recovery needs 2 seconds of continuously good frames before re-enabling.
pub const RECOVERY_FRAMES: u32 = 120;

#[derive(Clone, Debug)]
pub struct FrameGenGovernor {
    requested: bool,
    mode: FrameGenMode,
    pub watchdog_trips: u32,
    good_frames: u32,
    bad_frames: u32,
    last_reason: String,
    cooldown_frames: u32,
}

impl Default for FrameGenGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameGenGovernor {
    pub fn new() -> Self {
        Self {
            requested: false,
            mode: FrameGenMode::Off,
            watchdog_trips: 0,
            good_frames: 0,
            bad_frames: 0,
            last_reason: "off by default".into(),
            cooldown_frames: 0,
        }
    }

    pub fn mode(&self) -> FrameGenMode {
        self.mode
    }
    pub fn reason(&self) -> &str {
        &self.last_reason
    }
    /// Every cross-thread hand-off in `frame_gen.rs` must use this bound.
    pub fn max_wait_ms() -> u32 {
        MAX_WAIT_MS
    }

    /// The player's toggle. Capability is checked *before* anything is started.
    pub fn request(&mut self, want: bool, cap: &DeviceCapability) -> Result<FrameGenMode, String> {
        self.requested = want;
        if !want {
            self.mode = FrameGenMode::Off;
            self.last_reason = "turned off".into();
            return Ok(self.mode);
        }
        let best = cap.best_mode();
        if best == FrameGenMode::Off {
            self.mode = FrameGenMode::Off;
            self.last_reason = format!(
                "{} / iOS {} cannot run frame interpolation; use a lower render distance instead",
                cap.chip.name, cap.os_major
            );
            return Err(self.last_reason.clone());
        }
        self.mode = best;
        self.good_frames = 0;
        self.bad_frames = 0;
        self.cooldown_frames = 0;
        self.last_reason = format!("enabled: {best:?} on {}", cap.chip.name);
        Ok(best)
    }

    /// Called once per real frame. Never blocks, never waits on a thread.
    pub fn on_frame(&mut self, s: FrameSample) -> FrameGenDecision {
        // The cooldown must tick down while the *normal renderer* is proving it is
        // healthy again — even though frame generation itself is off.
        if self.cooldown_frames > 0 {
            self.cooldown_frames -= 1;
            return FrameGenDecision::Normal;
        }
        if !self.requested || self.mode == FrameGenMode::Off {
            return FrameGenDecision::Normal;
        }
        let stall = s.presenter_stall_ms as f32 > STALL_FACTOR * s.frame_interval_ms.max(1.0);
        let mut reason = self.failure_reason(s);
        if reason.is_none() && stall {
            reason = Some(format!(
                "presenter stalled for {} ms (> {:.1} ms display interval)",
                s.presenter_stall_ms,
                STALL_FACTOR * s.frame_interval_ms.max(1.0)
            ));
        }
        if let Some(reason) = reason {
            self.bad_frames += 1;
            self.good_frames = 0;
            self.last_reason = reason;
            // A stall is a hang, not a slow frame: trip the watchdog immediately.
            let stall = s.presenter_stall_ms as f32 > STALL_FACTOR * s.frame_interval_ms.max(1.0);
            if stall || self.mode == FrameGenMode::OpticalFlowAdaptive || self.bad_frames >= 20 {
                self.trip_watchdog();
                return FrameGenDecision::Normal;
            }
            return FrameGenDecision::Normal;
        }
        self.good_frames += 1;
        self.bad_frames = 0;
        match self.mode {
            FrameGenMode::UpscaleOnly => FrameGenDecision::UpscaleOnly,
            FrameGenMode::MetalFxInterpolation | FrameGenMode::OpticalFlowAdaptive => {
                // Presentation is owned by the render thread; the deadline comes from
                // CADisplayLink.targetTimestamp, never from thread::sleep on a present thread.
                FrameGenDecision::Generate {
                    deadline_ms: s.frame_interval_ms / 2.0,
                }
            }
            FrameGenMode::Off => FrameGenDecision::Normal,
        }
    }

    fn failure_reason(&self, s: FrameSample) -> Option<String> {
        if !s.foreground {
            return Some("backgrounded: stop presenting off the main run loop".into());
        }
        if s.real_fps < MIN_REAL_FPS {
            return Some(format!(
                "only {:.1} real FPS: interpolation needs >= {MIN_REAL_FPS:.0} real FPS",
                s.real_fps
            ));
        }
        let interval = s.frame_interval_ms.max(1.0);
        // Leave at least half the interpolation GPU allowance as headroom, but never
        // demand more than 75% of a very short (120 Hz) display interval.
        let gpu_budget = (interval - MAX_INTERPOLATION_GPU_MS / 2.0).max(interval * 0.75);
        if s.gpu_ms > gpu_budget {
            return Some(format!(
                "GPU {:.1} ms exceeds the {:.1} ms frame budget",
                s.gpu_ms, gpu_budget
            ));
        }
        if s.available_memory_mb < 180 {
            return Some(format!(
                "{} MiB left before jetsam; releasing frame-generation textures",
                s.available_memory_mb
            ));
        }
        if matches!(s.thermal, ThermalState::Serious | ThermalState::Critical) {
            return Some(format!("thermal {:?}: degrading", s.thermal));
        }
        None
    }

    fn trip_watchdog(&mut self) {
        self.watchdog_trips += 1;
        self.mode = FrameGenMode::Off;
        self.requested = false;
        self.good_frames = 0;
        self.bad_frames = 0;
        self.cooldown_frames = RECOVERY_FRAMES;
        self.last_reason = format!(
            "watchdog #{}: presenter stalled, fell back to the normal renderer for {} frames",
            self.watchdog_trips, RECOVERY_FRAMES
        );
    }

    /// Explicit recovery after a sustained clean run (the engine calls this once its
    /// normal renderer has presented RECOVERY_FRAMES good frames).
    pub fn try_recover(&mut self, good_frames: u32, cap: &DeviceCapability) -> bool {
        if self.cooldown_frames > 0 || good_frames < RECOVERY_FRAMES {
            return false;
        }
        self.request(true, cap).is_ok()
    }
}

/// Guard-rail for the legacy FSR present path: how many full-screen textures it may hold.
/// `FRAMES` in the tree is 6; on a 1384 MiB jetsam budget that alone is > 200 MiB at
/// 1080p HDR. Three is the triple-buffering minimum for a non-blocking present.
pub fn max_present_textures(cap: &DeviceCapability) -> usize {
    if cap.process_limit_mb >= 2000 && cap.chip.tier == ChipTier::M3OrNewer {
        4
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apple::chips;

    fn cap(chip: Chip, os: u32) -> DeviceCapability {
        DeviceCapability {
            chip,
            os_major: os,
            process_limit_mb: 2099,
            metal_fx_temporal_available: true,
        }
    }
    fn good(interval: f32) -> FrameSample {
        FrameSample {
            real_fps: 60.0,
            gpu_ms: 6.0,
            presenter_stall_ms: 0,
            available_memory_mb: 900,
            thermal: ThermalState::Nominal,
            foreground: true,
            frame_interval_ms: interval,
        }
    }

    #[test]
    fn an_old_iphone_never_gets_a_blocking_interpolation_path() {
        let mut g = FrameGenGovernor::new();
        let old = cap(chips::a10_fusion(), 18);
        assert_eq!(old.best_mode(), FrameGenMode::UpscaleOnly);
        assert_eq!(g.request(true, &old).unwrap(), FrameGenMode::UpscaleOnly);
        assert!(g.on_frame(good(16.67)) == FrameGenDecision::UpscaleOnly);
        assert!(!g.on_frame(good(16.67)).generates());
    }

    #[test]
    fn metal4_devices_get_metalfx_and_a_display_link_deadline() {
        let mut g = FrameGenGovernor::new();
        let modern = cap(chips::a17_pro(), 26);
        assert_eq!(modern.best_mode(), FrameGenMode::MetalFxInterpolation);
        g.request(true, &modern).unwrap();
        let d = g.on_frame(good(8.33));
        assert_eq!(
            d,
            FrameGenDecision::Generate {
                deadline_ms: 8.33 / 2.0
            }
        );
    }

    #[test]
    fn a_stalled_presenter_trips_the_watchdog_immediately() {
        let mut g = FrameGenGovernor::new();
        let modern = cap(chips::a17_pro(), 26);
        g.request(true, &modern).unwrap();
        let mut s = good(16.67);
        s.presenter_stall_ms = 40;
        assert_eq!(g.on_frame(s), FrameGenDecision::Normal);
        assert_eq!(g.mode(), FrameGenMode::Off);
        assert_eq!(g.watchdog_trips, 1);
        assert!(g.reason().contains("presenter stalled"));
        // No recovery until the cooldown and a sustained clean run.
        assert!(!g.try_recover(10, &modern));
        assert_eq!(g.on_frame(good(16.67)), FrameGenDecision::Normal);
    }

    #[test]
    fn low_real_fps_disables_made_frames() {
        let mut g = FrameGenGovernor::new();
        let modern = cap(chips::m1(), 26);
        g.request(true, &modern).unwrap();
        let mut s = good(16.67);
        s.real_fps = 22.0;
        assert_eq!(g.on_frame(s), FrameGenDecision::Normal);
        assert!(g.reason().contains("30 real FPS"));
    }

    #[test]
    fn memory_pressure_and_thermals_degrade_before_jetsam_kills_the_app() {
        let mut g = FrameGenGovernor::new();
        let modern = cap(chips::a17_pro(), 26);
        g.request(true, &modern).unwrap();
        let mut s = good(16.67);
        s.available_memory_mb = 90;
        assert_eq!(g.on_frame(s), FrameGenDecision::Normal);
        let mut s = good(16.67);
        s.thermal = ThermalState::Critical;
        assert_eq!(g.on_frame(s), FrameGenDecision::Normal);
        assert!(g.reason().contains("thermal"));
    }

    #[test]
    fn gpu_over_budget_and_backgrounding_are_bounded() {
        let mut g = FrameGenGovernor::new();
        let modern = cap(chips::a17_pro(), 26);
        g.request(true, &modern).unwrap();
        let mut s = good(16.67);
        s.gpu_ms = 30.0;
        assert_eq!(g.on_frame(s), FrameGenDecision::Normal);
        let mut s = good(16.67);
        s.foreground = false;
        assert_eq!(g.on_frame(s), FrameGenDecision::Normal);
        assert_eq!(FrameGenGovernor::max_wait_ms(), 8);
    }

    #[test]
    fn recovery_needs_a_sustained_clean_run() {
        let mut g = FrameGenGovernor::new();
        let modern = cap(chips::a17_pro(), 26);
        g.request(true, &modern).unwrap();
        let mut s = good(16.67);
        s.presenter_stall_ms = 100;
        g.on_frame(s);
        for _ in 0..RECOVERY_FRAMES {
            g.on_frame(good(16.67));
        }
        assert!(g.try_recover(RECOVERY_FRAMES, &modern));
        assert_eq!(g.mode(), FrameGenMode::MetalFxInterpolation);
    }

    #[test]
    fn present_texture_count_is_capped_by_the_jetsam_budget() {
        assert_eq!(max_present_textures(&cap(chips::a14_bionic(), 26)), 3);
        assert_eq!(max_present_textures(&cap(chips::m3(), 26)), 4);
    }

    #[test]
    fn off_by_default_and_turning_off_is_always_safe() {
        let mut g = FrameGenGovernor::new();
        assert_eq!(g.mode(), FrameGenMode::Off);
        assert_eq!(g.on_frame(good(16.67)), FrameGenDecision::Normal);
        let modern = cap(chips::a17_pro(), 26);
        g.request(true, &modern).unwrap();
        g.request(false, &modern).unwrap();
        assert_eq!(g.on_frame(good(16.67)), FrameGenDecision::Normal);
    }
}
