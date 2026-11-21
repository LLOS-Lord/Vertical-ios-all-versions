//! Frame pacing and frame-generation admission control.
//!
//! # Why this crate exists
//!
//! Frame generation (`crates/horizon-client-render/src/gfx/frame_gen.rs`) interpolates a frame
//! between two real ones and presents it from its own thread. It is a *multiplier*: it only ever
//! helps when the real frames arrive fast enough that two of them fit in the display's refresh
//! window, and it costs a queue submission, two extra full-size textures and a second presenting
//! thread. On an iPhone the arithmetic is unforgiving:
//!
//! * Every generated frame is a full-screen `copy_texture_to_texture` plus a present. On an A11 the
//!   copy alone can cost more than the time the made frame buys back, so the GPU queue lengthens,
//!   the real frame lands late, and the picture *stalls* rather than doubling.
//! * `FrameGen::frames` blocks the render thread on `presenter.free.recv()` — twice. If the present
//!   thread is late (thermal throttling, a background app, a 60 Hz panel presenting a 120 Hz pair)
//!   the render thread waits with no timeout, so a user sees a frozen game that no input reaches.
//! * Generating at full rate doubles the presents the display has to absorb. A 60 Hz panel asked
//!   for 120 presents a second drops them, and the drops are exactly what a player calls "lag".
//!
//! # What this crate decides
//!
//! [`Admission`] answers, once per real frame and from measurements only, whether the next frame
//! may be generated, at what multiplier, and how long the renderer may wait for the present thread.
//! [`DeviceTier`] scales the answer to the chip the game is running on. [`ThermalGovernor`] takes
//! it away again when the device is hot, because a hot Apple SoC loses clocks long before it
//! reports a problem, and Frame Generation is the first thing that should yield.
//!
//! Everything here is deterministic and time is passed in, so the whole policy is unit-testable
//! without a GPU: see the tests at the bottom of each module.

#![forbid(unsafe_code)]

pub mod admission;
pub mod budgets;
pub mod device;
pub mod thermal;

pub use admission::{Admission, Decision, FrameGenSettings, Measurements, Reason};
pub use budgets::{Budget, FrameKind, FrameLedger};
pub use device::{AppleSoc, DeviceTier, DeviceProfile};
pub use thermal::{ThermalAction, ThermalGovernor, ThermalLevel};

/// One 60 Hz refresh window in seconds. The unit the pacer talks in when it says "half a frame".
pub const REFRESH_60: f64 = 1.0 / 60.0;
