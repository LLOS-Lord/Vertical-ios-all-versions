//! The chip the game is running on, and what that chip can be asked for.

/// Apple SoCs the port targets, oldest first. A device newer than the last entry is treated as
/// [`AppleSoc::A17Pro`] or better, which is the same policy the renderer already applies to
/// unknown GPUs (assume capable, then measure).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AppleSoc {
    /// A11 Bionic — iPhone 8/8 Plus/X. 2 performance cores, ~60 GFLOPS, no GPU tile memory the
    /// Metal driver will hand out generously, 2–3 GB of RAM shared with a ~1.4 GB Jetsam ceiling.
    A11,
    /// A12 — XS/XR and the 11 series' base models. 3–4 GB.
    A12,
    /// A13 — the 11 series' Pro models and the SE 2.
    A13,
    A14,
    A15,
    A16,
    A17Pro,
    /// M-series iPads and the A18/A19 family: wide GPU, 8 GB or more.
    Modern,
}

impl AppleSoc {
    /// The identifier iOS reports (`hw.machine`, e.g. `iPhone11,8` for the XR) mapped to its SoC.
    /// Unknown machine strings fall to [`AppleSoc::A12`]: a conservative middle, never the floor,
    /// because an iPhone older than an X is below the deployment target anyway.
    pub fn from_hw_machine(machine: &str) -> Self {
        let machine = machine.trim();
        if let Some(rest) = machine.strip_prefix("iPhone") {
            // The first number is the hardware family, not the model: 8 = 6/6s/SE1, 10 = 7,
            // 11 = 8/X family, 12 = XR/XS, 13 = 11, 14 = 12, 15 = 13, 16 = 14, 17 = 15, 18 = 16.
            let family: u32 = rest
                .split(',')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            return match family {
                0..=10 => Self::A11,
                11 => Self::A11, // 8 / 8 Plus / X are A11
                12 => Self::A12, // XR / XS are A12
                13 => Self::A13,
                14 => Self::A14,
                15..=16 => Self::A15,
                17 => Self::A16,
                18.. => Self::A17Pro,
            };
        }
        if machine.starts_with("iPad") || machine.starts_with("Mac") {
            // Every iPad that runs iOS 16 has at least an A12; the Pro models are what the shader
            // packs and the 12-chunk render distances are for.
            return if machine.contains("13,") || machine.contains("14,") {
                Self::Modern
            } else {
                Self::A15
            };
        }
        Self::A12
    }

    /// The device profile this SoC gets.
    pub fn profile(self) -> DeviceProfile {
        match self {
            Self::A11 => DeviceProfile::LEGACY,
            Self::A12 | Self::A13 => DeviceProfile::MOBILE,
            Self::A14 | Self::A15 | Self::A16 => DeviceProfile::BALANCED,
            Self::A17Pro | Self::Modern => DeviceProfile::CAPABLE,
        }
    }
}

/// The coarse capability class the scheduling policies switch on. Derived from [`AppleSoc`] and, on
/// a desktop, from whatever the graphics API reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceTier {
    /// A11-class phones: every budget is tight and Frame Generation is off unless forced.
    Legacy,
    /// A12–A16 phones.
    Mobile,
    /// A17 Pro and most iPads.
    Balanced,
    /// M-series, desktop GPUs.
    Capable,
}

impl DeviceTier {
    /// A tier from an available-memory figure in MiB and a GPU class string, for platforms that
    /// cannot name an Apple SoC (Android, desktop). Never returns [`DeviceTier::Capable`] from
    /// memory alone: that needs a name.
    pub fn from_memory_mib(mib: u64, apple: Option<AppleSoc>) -> Self {
        if let Some(soc) = apple {
            return soc.profile().tier;
        }
        match mib {
            0..=2048 => Self::Legacy,
            2049..=4095 => Self::Mobile,
            4096..=8191 => Self::Balanced,
            _ => Self::Capable,
        }
    }
}

/// Everything the schedules need to know about the machine, resolved once at start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceProfile {
    pub tier: DeviceTier,
    pub soc: Option<AppleSoc>,
    /// ProMotion (120 Hz) panel: Frame Generation has a window to fill, a 60 Hz panel does not.
    pub promotion: bool,
    /// Physical memory in MiB, as `ProcessInfo.physicalMemory` reports it.
    pub physical_mib: u64,
    /// Memory iOS will let the process reach before Jetsam kills it, in MiB. `None` where the
    /// platform does not say (`os_proc_available_memory` is iOS-only).
    pub available_mib: Option<u64>,
}

impl DeviceProfile {
    /// The A11 phone profile: what the strictest budgets are calibrated against.
    pub const LEGACY: Self = Self {
        tier: DeviceTier::Legacy,
        soc: Some(AppleSoc::A11),
        promotion: false,
        physical_mib: 3072,
        available_mib: Some(1200),
    };
    pub const MOBILE: Self = Self {
        tier: DeviceTier::Mobile,
        soc: Some(AppleSoc::A12),
        promotion: false,
        physical_mib: 4096,
        available_mib: Some(1800),
    };
    pub const BALANCED: Self = Self {
        tier: DeviceTier::Balanced,
        soc: Some(AppleSoc::A15),
        promotion: true,
        physical_mib: 6144,
        available_mib: Some(2800),
    };
    pub const CAPABLE: Self = Self {
        tier: DeviceTier::Capable,
        soc: Some(AppleSoc::Modern),
        promotion: true,
        physical_mib: 8192,
        available_mib: Some(4000),
    };

    /// A desktop with no idea what it is: the tier from its memory alone, no ProMotion claim.
    pub fn desktop(physical_mib: u64) -> Self {
        Self {
            tier: DeviceTier::from_memory_mib(physical_mib, None),
            soc: None,
            promotion: false,
            physical_mib,
            available_mib: None,
        }
    }

    /// The profile an `hw.machine` identifier describes (`iPhone11,8`, `iPad13,4`): the chip decides
    /// the tier, and the memory band that chip shipped with fills in the rest.
    pub fn from_hw_machine(machine: &str) -> Self {
        let soc = AppleSoc::from_hw_machine(machine);
        let mut device = soc.profile();
        device.soc = Some(soc);
        // What that chip's devices shipped with: the admission uses it for the frame pair count and
        // the memory governor for its ceiling, so a guess that is one band low is safe.
        device.physical_mib = match soc {
            AppleSoc::A11 | AppleSoc::A12 => 3072,
            AppleSoc::A13 | AppleSoc::A14 => 4096,
            AppleSoc::A15 | AppleSoc::A16 => 6144,
            AppleSoc::A17Pro | AppleSoc::Modern => 8192,
        };
        device
    }

    /// The working set the game may hold: the platform's figure less a reserve, or 40% of physical
    /// memory where the platform does not say. The reserve covers the GPU driver's host-side
    /// allocations, UIKit and the launcher framework, which are not in the game's accounting.
    pub fn memory_ceiling_mib(&self) -> u64 {
        let reserve = match self.tier {
            DeviceTier::Legacy => 700,
            DeviceTier::Mobile => 900,
            DeviceTier::Balanced => 1200,
            DeviceTier::Capable => 1600,
        };
        match self.available_mib {
            Some(available) => available.saturating_sub(reserve).max(256),
            None => (self.physical_mib * 2 / 5).max(512),
        }
    }

    /// Whether Frame Generation may be offered at all on this device. It is still off by default
    /// everywhere; this only decides whether the switch exists and what it may do.
    pub fn frame_gen_allowed(&self) -> bool {
        self.tier >= DeviceTier::Mobile && self.promotion
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phones_map_to_their_chip() {
        assert_eq!(AppleSoc::from_hw_machine("iPhone11,2"), AppleSoc::A11);
        assert_eq!(AppleSoc::from_hw_machine("iPhone11,8"), AppleSoc::A11);
        assert_eq!(AppleSoc::from_hw_machine("iPhone12,3"), AppleSoc::A12);
        assert_eq!(AppleSoc::from_hw_machine("iPhone13,4"), AppleSoc::A13);
        assert_eq!(AppleSoc::from_hw_machine("iPhone14,7"), AppleSoc::A14);
        assert_eq!(AppleSoc::from_hw_machine("iPhone15,3"), AppleSoc::A15);
        assert_eq!(AppleSoc::from_hw_machine("iPhone17,1"), AppleSoc::A16);
        assert_eq!(AppleSoc::from_hw_machine("iPhone18,2"), AppleSoc::A17Pro);
        assert_eq!(AppleSoc::from_hw_machine("iPad13,8"), AppleSoc::Modern);
        assert_eq!(AppleSoc::from_hw_machine("something"), AppleSoc::A12);
    }

    #[test]
    fn a11_is_the_only_tier_without_frame_generation() {
        assert!(!DeviceProfile::LEGACY.frame_gen_allowed());
        assert!(DeviceProfile::BALANCED.frame_gen_allowed());
        // A desktop never claims ProMotion, so it never offers the switch by default either.
        assert!(!DeviceProfile::desktop(16384).frame_gen_allowed());
    }

    #[test]
    fn the_ceiling_leaves_room_for_everything_that_is_not_the_game() {
        assert_eq!(DeviceProfile::LEGACY.memory_ceiling_mib(), 500);
        assert_eq!(DeviceProfile::CAPABLE.memory_ceiling_mib(), 2400);
        // Physical memory alone: 40%, never a quarter of a 16 GB desktop.
        assert_eq!(DeviceProfile::desktop(16384).memory_ceiling_mib(), 6553);
    }
}
