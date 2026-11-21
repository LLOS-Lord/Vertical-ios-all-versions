//! What this device is: its chip, its memory and how fast its display refreshes.
//!
//! Everything here is optional by design. On an iPhone the real answers come from
//! `ProcessInfo.physicalMemory`, `sysctl hw.machine` and `UIScreen.maximumFramesPerSecond`, none of
//! which the engine can see from a pure-Rust crate on every platform; the launcher's Swift shim
//! reports them (`horizon-ios`), and where it does not, `HORIZON_DEVICE` /
//! `HORIZON_MEMORY_MIB` / `HORIZON_DISPLAY_HZ` answer instead, and the desktop's own numbers are
//! used as the last resort.
//!
//! The point is that *some* honest answer reaches [`horizon_frame_sched::Admission`]: an A11 asked
//! to generate frames freezes, and the fix starts by knowing the game is on an A11.

use horizon_frame_sched::DeviceProfile;

/// The process's environment, read once. `None` for a variable means "not set".
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The machine identifier as `hw.machine` reports it ("iPhone11,8", "iPad13,4", "Mac14,2").
pub fn hw_machine() -> Option<String> {
    var("HORIZON_DEVICE").or_else(|| var("SIMULATOR_MODEL_IDENTIFIER"))
}

/// Physical memory in MiB, when the shim or the environment reports it.
pub fn physical_mib() -> Option<u64> {
    var("HORIZON_MEMORY_MIB").and_then(|v| v.trim().parse::<u64>().ok())
}

/// The display's refresh rate in Hz, when it is known before the first frame (ProMotion: 120).
pub fn display_hz() -> Option<f32> {
    var("HORIZON_DISPLAY_HZ")
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|hz| hz.is_finite() && *hz >= 24.0)
}

/// Whether the panel is ProMotion (an adaptive 24–120 Hz panel), which is what gives frame
/// generation a refresh window to fill.
pub fn promotion() -> bool {
    var("HORIZON_PROMOTION").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        || display_hz().is_some_and(|hz| hz >= 90.0)
}

/// This device's profile. An iPhone or iPad identifier decides the tier from its chip; a desktop is
/// levelled by its memory.
pub fn probe() -> DeviceProfile {
    let machine = hw_machine();
    let mut device = match machine.as_deref() {
        Some(m) if m.starts_with("iPhone") || m.starts_with("iPad") || m.starts_with("iPod") => {
            DeviceProfile::from_hw_machine(m)
        }
        _ => DeviceProfile::desktop(physical_mib().unwrap_or(8192)),
    };
    if let Some(mib) = physical_mib() {
        device.physical_mib = mib;
        // A shim that knows the memory knows the tier better than a model identifier does: a
        // 3 GB iPhone identifier and a 3 GB report agree, and a report alone is enough.
        if device.soc.is_none() {
            device.tier = horizon_frame_sched::DeviceTier::from_memory_mib(mib, None);
        }
    }
    device.promotion = promotion();
    device
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_iphone_identifier_reaches_the_profile() {
        // The probe reads the environment, so this test drives the pieces it can: the identifier →
        // profile path is `horizon-frame-sched`'s, and what matters here is that a phone's
        // identifier is recognised as a phone's.
        for machine in ["iPhone11,8", "iPhone14,2", "iPad13,4"] {
            let device = DeviceProfile::from_hw_machine(machine);
            assert!(
                device.soc.is_some(),
                "{machine} should name an Apple SoC, got {:?}",
                device.soc
            );
        }
        let desktop = probe();
        assert!(desktop.physical_mib > 0);
        assert!(desktop.memory_ceiling_mib() <= desktop.physical_mib);
    }

    #[test]
    fn a_desktop_is_levelled_by_memory() {
        let small = DeviceProfile::desktop(2048);
        let big = DeviceProfile::desktop(16384);
        assert!(small.tier < big.tier);
        assert!(small.memory_ceiling_mib() < big.memory_ceiling_mib());
    }
}
