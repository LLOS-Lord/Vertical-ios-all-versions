//! `horizon-ios-runtime` — fail-closed runtime policy for the Horizon iOS port.
//!
//! ## Why a separate, dependency-free crate
//!
//! The 60-crate engine needs a Mac + Xcode 26 + the iOS 26 SDK to compile. This crate
//! holds the *logic* of the five reported bugs and the six requested subsystems, so
//! every invariant is compiled and unit-tested on any host (`cargo test` on Linux).
//! The engine-side wiring in `crates/horizon-client*`, `packaging/ios/Launcher` and
//! `horizon-threads` delegates to it; nothing here invents an Apple API —
//! `os_proc_available_memory`, `MTLFXFrameInterpolator`, QoS and CADisplayLink are
//! documented integration points, not simulated calls.
//!
//! ## Modules
//!
//! | Module | Bug / request |
//! |---|---|
//! | [`version`] | Lobby shows 1.20.4 but the world is 26.3. Real version bindings, fake-manifest detection, fail-closed activation. |
//! | [`mods`] | Installed/ported mods do nothing. Explicit verdicts, version gate, hook bridge, content revision → real reload/restart. |
//! | [`framegen`] | Frame generation hangs older iPhones. Capability gate, bounded waits, watchdog, jetsam/thermal degradation, honest fallback. |
//! | [`input`] | Touch delay + jitter, no trade/ride/dismount. Hardware-timestamp resampling, scroll smoothing, entity-context actions. |
//! | [`memory`] | No RAM manager. `phys_footprint + os_proc_available_memory` ceiling, purge/evict unloaded regions. |
//! | [`scheduler`] | Rational multi-threaded pipeline. Priority/QoS, bounded queues, frame budget, drop invisible/cosmetic work. |
//! | [`visibility`] | Invisible block/entity culling + image/GPU workload guard. Deterministic frustum/occlusion/budget policy. |
//! | [`apple`] | A14/M1 Metal 4 floor, P/E-core worker budgets, QoS, thermal scale. |
//!
//! ## Verification levels (never conflated)
//!
//! * **Compiled / unit-tested here:** every invariant below, on the host toolchain.
//! * **Design / wiring:** the SwiftUI/Metal/winit integration described in
//!   `/home/user/RESEARCH.md` and `docs/ios-runtime-contract.md`.
//! * **Needs a physical device:** frame pacing, jetsam measurements, thermal behaviour,
//!   latency in ms and Geekbench-like performance. No claim here is device-verified.

pub mod apple;
pub mod framegen;
pub mod input;
pub mod memory;
pub mod mods;
pub mod scheduler;
pub mod version;
pub mod visibility;

pub use apple::{Chip, ChipTier, Qos, ThermalState, WorkerBudget, chips};
pub use framegen::{DeviceCapability, FrameGenDecision, FrameGenGovernor, FrameGenMode};
pub use input::{
    EntityTarget, ScrollSmoother, TouchAction, TouchResampler, TouchSample, resolve_press,
};
pub use memory::{ChunkCache, ChunkKey, MemoryGovernor, MemorySnapshot, Pressure, jetsam_limit_mb};
pub use mods::{
    HookBridge, ModDescriptor, ModOrigin, ModRuntime, ModVerdict, NativeHookBridge, NoCodeBridge,
    Reload,
};
pub use scheduler::{Priority, Scheduler, SchedulerConfig, WorkKind};
pub use version::{
    Activation, SimulationKind, SimulationPack, VersionBinding, VersionError, VersionRuntime,
    detect_fake_manifest,
};
pub use visibility::{
    Aabb, Candidate, CandidateKind, CullDecision, Frustum, GpuBudget, Plane, VisibilitySet, cull,
};

/// The gate the iOS launcher calls before showing a version row as "playable".
/// It refuses the exact fake the current tree writes into `horizon-resources.json`.
pub fn assert_manifest_is_real(
    label: &str,
    engine_version: &str,
    data_version: u32,
    simulation_version: Option<&str>,
) -> Result<(), VersionError> {
    detect_fake_manifest(label, engine_version, data_version, simulation_version)
}
