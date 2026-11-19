//! Apple Silicon device profiles, QoS scheduling hints and thermal state.
//!
//! Sources: Apple "Tune CPU job scheduling for Apple silicon games" (Tech Talks
//! 110147) — scale the thread count to the core count, avoid many tiny jobs, prefer
//! QoS over raw priorities, never `sleep(0)`; WWDC22 "Profile and optimize your
//! game's memory" — unified memory makes accessed Metal resources count toward the
//! process footprint; WWDC25 "Discover Metal 4" — Metal 4 / MetalFX frame
//! interpolation needs M1+ or A14+.

/// GPU capability tier, used only for *published* API availability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipTier {
    /// Pre-A14: Metal 2/3, no MetalFX frame interpolation, tight jetsam budget.
    Legacy,
    /// A14 / M1 and later: Metal 4 capable.
    A14OrM1,
    /// A17 Pro / M3 and later: more headroom, faster optical flow.
    M3OrNewer,
}
impl ChipTier {
    pub fn supports_metal4(self) -> bool {
        matches!(self, Self::A14OrM1 | Self::M3OrNewer)
    }
    pub fn supports_metal3_frame_interpolation(self) -> bool {
        // MTLFXFrameInterpolator (Metal 3) is available from iOS 18 / macOS 15 on
        // Apple GPUs; keep the same chip floor as Metal 4 to stay conservative.
        self.supports_metal4()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    pub name: &'static str,
    pub tier: ChipTier,
    pub p_cores: usize,
    pub e_cores: usize,
    /// Unified memory accessible to CPU + GPU, MiB.
    pub unified_memory_mb: u64,
}

/// A few representative, conservative profiles. The app should fill these from
/// `sysctl hw.memsize` / `MTLDevice` at runtime; these are fail-closed defaults.
pub mod chips {
    use super::{Chip, ChipTier};
    pub fn a10_fusion() -> Chip {
        Chip {
            name: "A10 Fusion",
            tier: ChipTier::Legacy,
            p_cores: 2,
            e_cores: 2,
            unified_memory_mb: 2048,
        }
    }
    pub fn a12_bionic() -> Chip {
        Chip {
            name: "A12 Bionic",
            tier: ChipTier::Legacy,
            p_cores: 2,
            e_cores: 4,
            unified_memory_mb: 3072,
        }
    }
    pub fn a14_bionic() -> Chip {
        Chip {
            name: "A14 Bionic",
            tier: ChipTier::A14OrM1,
            p_cores: 2,
            e_cores: 4,
            unified_memory_mb: 4096,
        }
    }
    pub fn a17_pro() -> Chip {
        Chip {
            name: "A17 Pro",
            tier: ChipTier::M3OrNewer,
            p_cores: 2,
            e_cores: 4,
            unified_memory_mb: 8192,
        }
    }
    pub fn m1() -> Chip {
        Chip {
            name: "M1",
            tier: ChipTier::A14OrM1,
            p_cores: 4,
            e_cores: 4,
            unified_memory_mb: 8192,
        }
    }
    pub fn m3() -> Chip {
        Chip {
            name: "M3",
            tier: ChipTier::M3OrNewer,
            p_cores: 4,
            e_cores: 4,
            unified_memory_mb: 8192,
        }
    }
}

/// Grand Central Dispatch QoS classes, highest first. On Apple platforms higher QoS
/// values are more important (opposite of Linux nice).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Qos {
    Background = 9,
    Utility = 17,
    Default = 21,
    UserInitiated = 25,
    UserInteractive = 33,
}
impl Qos {
    pub fn gcd_name(self) -> &'static str {
        match self {
            Self::Background => "QOS_CLASS_BACKGROUND",
            Self::Utility => "QOS_CLASS_UTILITY",
            Self::Default => "QOS_CLASS_DEFAULT",
            Self::UserInitiated => "QOS_CLASS_USER_INITIATED",
            Self::UserInteractive => "QOS_CLASS_USER_INTERACTIVE",
        }
    }
    /// Low QoS threads are normally confined to E cores (Apple Tech Talk 110147).
    pub fn confined_to_efficiency_cores(self) -> bool {
        matches!(self, Self::Background | Self::Utility)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThermalState {
    Nominal,
    Fair,
    Serious,
    Critical,
}
impl ThermalState {
    pub fn scale(self) -> f32 {
        match self {
            Self::Nominal => 1.0,
            Self::Fair => 0.85,
            Self::Serious => 0.6,
            Self::Critical => 0.35,
        }
    }
}

/// Thread budgets. One shared pool, scaled to the core count; no per-framework pools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerBudget {
    /// Latency-critical work (input, render, tick, visible chunk meshing).
    pub foreground: usize,
    /// Chunk generation, lighting, IO, telemetry — low QoS on E cores.
    pub background: usize,
    /// Total OS threads the game may own (excluding the main/render + audio threads).
    pub total: usize,
}
impl WorkerBudget {
    pub fn for_chip(chip: &Chip, thermal: ThermalState) -> Self {
        let cores = chip.p_cores + chip.e_cores;
        let foreground = chip.p_cores.max(1);
        let background = (chip.e_cores.max(1) as f32 * thermal.scale()).ceil() as usize;
        Self {
            foreground,
            background: background.max(1),
            total: cores.max(2),
        }
    }
}

/// QoS for a job class. The engine must *set* this on its worker threads
/// (`pthread_set_qos_class_self_np` / `dispatch_queue_attr_make_with_qos_class`).
pub fn qos_for(priority: crate::scheduler::Priority) -> Qos {
    use crate::scheduler::Priority;
    match priority {
        Priority::Critical => Qos::UserInteractive,
        Priority::High => Qos::UserInitiated,
        Priority::Normal => Qos::Default,
        Priority::Low => Qos::Utility,
        Priority::Background => Qos::Background,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Priority;

    #[test]
    fn metal4_support_follows_the_a14_m1_floor() {
        assert!(!chips::a10_fusion().tier.supports_metal4());
        assert!(!chips::a12_bionic().tier.supports_metal4());
        assert!(chips::a14_bionic().tier.supports_metal4());
        assert!(chips::m1().tier.supports_metal4());
        assert!(chips::a17_pro().tier.supports_metal4());
    }

    #[test]
    fn worker_budget_never_exceeds_the_core_count_and_shrinks_with_heat() {
        let b = WorkerBudget::for_chip(&chips::m1(), ThermalState::Nominal);
        assert_eq!(b.total, 8);
        assert_eq!(b.foreground, 4);
        assert_eq!(b.background, 4);
        let hot = WorkerBudget::for_chip(&chips::m1(), ThermalState::Critical);
        assert!(hot.background < b.background);
        assert!(hot.total <= b.total);
    }

    #[test]
    fn low_qos_is_confined_to_efficiency_cores() {
        assert!(Qos::Background.confined_to_efficiency_cores());
        assert!(Qos::Utility.confined_to_efficiency_cores());
        assert!(!Qos::UserInteractive.confined_to_efficiency_cores());
        assert_eq!(qos_for(Priority::Critical), Qos::UserInteractive);
        assert_eq!(qos_for(Priority::Background), Qos::Background);
    }

    #[test]
    fn thermal_scale_is_monotonic() {
        assert!(ThermalState::Nominal.scale() > ThermalState::Fair.scale());
        assert!(ThermalState::Fair.scale() > ThermalState::Serious.scale());
        assert!(ThermalState::Serious.scale() > ThermalState::Critical.scale());
    }
}
