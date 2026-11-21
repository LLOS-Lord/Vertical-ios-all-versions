//! Read-only process memory diagnostics.
/// The process's memory counters.
#[cfg(windows)]
fn counters() -> Option<windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS_EX2> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX2,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
    // SAFETY: all-zero is a valid value of this plain C struct, which starts with the fields of the
    // one the call is declared with, and the call writes at most `size` bytes into it; the current
    // process's pseudo-handle needs no closing.
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS_EX2 = std::mem::zeroed();
        counters.cb = size;
        (K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast(), size) != 0)
            .then_some(counters)
    }
}

/// The memory this process holds in RAM, in bytes.
#[cfg(windows)]
pub fn resident_bytes() -> Option<u64> {
    Some(counters()?.WorkingSetSize as u64)
}

/// The private memory this process holds in RAM, all it holds in RAM and the machine's RAM, in
/// bytes.
#[cfg(windows)]
pub fn usage() -> Option<[u64; 3]> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let counters = counters()?;
    let resident = counters.WorkingSetSize as u64;
    let private = match counters.PrivateWorkingSetSize {
        // Windows before 10 22H2 leaves it empty: the private commit is the nearest it has.
        0 => (counters.PrivateUsage as u64).min(resident),
        bytes => bytes as u64,
    };
    // SAFETY: all-zero is a valid value of this plain C struct, its length is set as the call
    // requires, and the call only writes into it.
    let ram = unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        (GlobalMemoryStatusEx(&mut status) != 0).then_some(status.ullTotalPhys)
    }?;
    Some([private, resident, ram])
}

/// [`usage`]'s counters: not read on this system.
#[cfg(not(windows))]
pub fn usage() -> Option<[u64; 3]> {
    None
}

/// The memory this process holds in RAM, in bytes.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: reads one system constant.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages * u64::try_from(page).ok()?)
}

/// The memory this process holds in RAM, in bytes.
#[cfg(target_os = "macos")]
pub fn resident_bytes() -> Option<u64> {
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: all-zero is a valid value of this plain C struct, and the call writes at most `size`
    // bytes into it.
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let written = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        );
        (written == size).then_some(info.pti_resident_size)
    }
}

/// The memory this process holds in RAM, in bytes. libproc's proc_pidinfo is
/// macOS-only; iOS and the other Apple platforms expose this through Mach.
#[cfg(all(target_vendor = "apple", not(target_os = "macos")))]
pub fn resident_bytes() -> Option<u64> {
    // `phys_footprint` where it can be read: it is the metric jetsam compares against, and it
    // counts compressed and IOKit-backed pages that `resident_size` misses. `resident_size` stays
    // the fallback because it is the older, always-present counter.
    footprint_bytes().or_else(mach_resident_bytes)
}

/// `task_vm_info.phys_footprint`: what jetsam actually measures, in bytes.
///
/// `resident_size` is the wrong number for this purpose — it misses compressed and IOKit-backed
/// pages — so a budget built on it under-reads and lets the process walk into a kill it could have
/// seen coming. iOS exposes it through `task_info(TASK_VM_INFO)`, the same Mach call the resident
/// count uses.
#[cfg(target_vendor = "apple")]
pub fn footprint_bytes() -> Option<u64> {
    // The layout lives in [`task_vm_info_layout`] so a host test can check its offsets: the Apple
    // SDK is not on a Linux runner, so nothing else in CI can compile this branch at all.
    let mut info = std::mem::MaybeUninit::<task_vm_info_layout::TaskVmInfo>::zeroed();
    // `task_info` takes and returns the buffer's size in natural_t (32-bit) words.
    let mut count = task_vm_info_layout::TaskVmInfo::COUNT;
    let wanted = count;
    // SAFETY: `info` is exactly `count` natural_t words of space, and the kernel writes at most
    // that many words into it. The task port is libSystem's own and must not be deallocated.
    let result = unsafe {
        libc::task_info(
            mach_task_self(),
            TASK_VM_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    // A kernel older than rev1 stops early and reports fewer words: the field would then be
    // uninitialised, so its value is not a measurement and must not be passed off as one.
    if result != 0 || count < wanted {
        return None;
    }
    // SAFETY: the kernel filled the whole struct (it wrote `wanted` words, or we returned above),
    // and every field is plain data.
    Some(unsafe { info.assume_init() }.phys_footprint)
}

/// `TASK_VM_INFO`'s flavour number (`osfmk/mach/task_info.h`).
#[cfg(target_vendor = "apple")]
const TASK_VM_INFO: libc::task_flavor_t = 22;

/// `mach_task_self()`, the current task's port. `libc` only declares the `mach_task_self_` *static*
/// and only for macOS, so the function is declared here; libSystem exports it on every Apple
/// platform, and it returns a borrowed port that needs no deallocation.
#[cfg(target_vendor = "apple")]
fn mach_task_self() -> libc::mach_port_t {
    unsafe extern "C" {
        fn mach_task_self() -> libc::mach_port_t;
    }
    // SAFETY: no arguments, no preconditions; the port it returns is the borrowed current-task port.
    unsafe { mach_task_self() }
}

/// `os_proc_available_memory()`: how many more bytes this process may allocate before jetsam kills
/// it, in bytes.
///
/// iOS 13+. Returns `Some(0)` when the process is *already* over the limit, which is why the RAM
/// budget sums it with the footprint rather than using it alone: a limit of `footprint + 0` is a
/// real limit, not an absent one. `None` where the symbol is not there (macOS, and any iOS before
/// 13), so the caller falls back to the tier table rather than to a wrong number.
#[cfg(all(target_vendor = "apple", not(target_os = "macos")))]
pub fn available_memory_bytes() -> Option<u64> {
    // SAFETY: `os_proc_available_memory` takes no arguments and has no preconditions; it is
    // exported by libSystem from iOS 13 on.
    Some(unsafe { os_proc_available_memory() as u64 })
}

// `os_proc_available_memory`, resolved from libSystem. Declared here rather than pulled through a
// crate: it is one C function with an obvious signature, and it is called only on iOS 13+ (the
// deployment target is 16.0), so no weaker-linked fallback is needed.
#[cfg(all(target_vendor = "apple", not(target_os = "macos")))]
unsafe extern "C" {
    fn os_proc_available_memory() -> usize;
}

/// Jetsam's limit for this process, in bytes: what it holds now plus what it may still allocate.
///
/// This is the only honest way to learn it. The limit is a per-process property the kernel decides
/// from the device class and the foreground state; it is not derived from installed RAM (two phones
/// with 4 GB can be held 50 % apart), and nothing in the SDK reports it directly. Summing the two
/// counters gives it for any device without a table.
#[cfg(target_vendor = "apple")]
pub fn jetsam_limit_bytes() -> Option<u64> {
    let footprint = footprint_bytes()?;
    let available = available_memory_bytes()?;
    // `available` is 0 both for "no headroom left" and for "already over": the limit is then at
    // least what the process already holds.
    Some(footprint.saturating_add(available))
}

/// The machine identifier (`hw.machine`, e.g. `iPhone11,8`): what tells an A11 from an A15.
///
/// `ram.rs` read this from an environment variable, which nothing sets on a phone, so every device
/// fell through to `AppleSoc::from_hw_machine("")` and was classified **A12** — the wrong tier, and
/// the wrong thermal table and memory ceiling with it. The phone's own identifier is the answer, and
/// `sysctlbyname` is how iOS hands it over.
#[cfg(target_vendor = "apple")]
pub fn hw_machine() -> Option<String> {
    let mut size = 0usize;
    // SAFETY: a NUL-terminated name and a null buffer, which asks for the length only.
    let sized = unsafe {
        libc::sysctlbyname(
            c"hw.machine".as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if sized != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is exactly `size` bytes, the length the first call reported.
    let read = unsafe {
        libc::sysctlbyname(
            c"hw.machine".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    // The value is a NUL-terminated C string ("iPhone11,8\0").
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// `ProcessInfo.physicalMemory`'s number from the kernel (`hw.memsize`): the RAM the machine has.
///
/// Read for the device profile, which distinguishes a 3 GB phone from a 6 GB one. `sysctl` is the
/// cheap way and needs no Foundation call from Rust.
#[cfg(target_vendor = "apple")]
pub fn physical_memory_bytes() -> Option<u64> {
    let mut mib = 0u64;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a 64-bit integer; `mib` has that many bytes of space and `size` says
    // so, which is what the call writes at most.
    let read = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut mib).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (read == 0).then_some(mib)
}

/// [`physical_memory_bytes`] where the platform has it: none.
#[cfg(not(target_vendor = "apple"))]
pub fn physical_memory_bytes() -> Option<u64> {
    None
}

/// [`jetsam_limit_bytes`] where the platform has it: no jetsam outside Apple's platforms.
#[cfg(not(target_vendor = "apple"))]
pub fn jetsam_limit_bytes() -> Option<u64> {
    None
}

/// [`hw_machine`] where the platform has it: none, so the caller uses its own fallback.
#[cfg(not(target_vendor = "apple"))]
pub fn hw_machine() -> Option<String> {
    None
}

/// [`available_memory_bytes`] where the platform has it: none, so the RAM budget falls back to its
/// tier table rather than inventing a limit.
#[cfg(not(all(target_vendor = "apple", not(target_os = "macos"))))]
pub fn available_memory_bytes() -> Option<u64> {
    None
}

/// [`footprint_bytes`] where the platform has it: the resident count is the nearest it has.
#[cfg(not(target_vendor = "apple"))]
pub fn footprint_bytes() -> Option<u64> {
    resident_bytes()
}

#[cfg(all(target_vendor = "apple", any(not(target_os = "macos"), test)))]
fn mach_resident_bytes() -> Option<u64> {
    // mach_init.h defines mach_task_self() as this libSystem variable. libc's
    // Rust accessor is macOS-only, although the SDK exports it on iOS too.
    unsafe extern "C" {
        static mut mach_task_self_: libc::mach_port_t;
    }
    // MACH_TASK_BASIC_INFO is the SDK's architecture-independent, 64-bit form.
    // Its count is in natural_t words, not bytes (mach/task_info.h).
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::uninit();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: info has space for exactly `count` natural_t words; task_info writes
    // that many at most. The current task port is borrowed and needs no deallocation.
    let result = unsafe {
        libc::task_info(
            mach_task_self_,
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS || count != libc::MACH_TASK_BASIC_INFO_COUNT {
        return None;
    }
    // SAFETY: success with the full count initializes the SDK's plain-data struct.
    Some(unsafe { info.assume_init() }.resident_size)
}

/// The memory this process holds in RAM, in bytes: not known on this system.
#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
)))]
pub fn resident_bytes() -> Option<u64> {
    None
}

#[cfg(all(test, any(windows, target_os = "linux", target_vendor = "apple")))]
mod tests {
    use super::*;

    #[test]
    fn touched_memory_counts_as_resident() {
        const SIZE: usize = 256 << 20;
        let before = resident_bytes().unwrap();
        #[cfg(target_vendor = "apple")]
        let mach_before = mach_resident_bytes().expect("current task memory");
        let block = vec![1u8; SIZE];
        let during = resident_bytes().unwrap();
        #[cfg(target_vendor = "apple")]
        let mach_during = mach_resident_bytes().expect("current task memory after allocation");
        std::hint::black_box(&block);
        assert!(
            during >= before + SIZE as u64 / 2,
            "{before} then {during} bytes"
        );
        #[cfg(target_vendor = "apple")]
        assert!(
            mach_during >= mach_before + SIZE as u64 / 2,
            "Mach task: {mach_before} then {mach_during} bytes"
        );
    }
}

/// The `task_vm_info` layout, where this target compiles it.
#[cfg(target_vendor = "apple")]
pub mod task_vm_info_layout {
    /// The struct handed to `task_info(TASK_VM_INFO)`, laid out against XNU's
    /// `osfmk/mach/task_info.h`. Public so a host test can check the offsets it will read on a
    /// device: nothing else in CI can, because the Apple SDK is not on a Linux runner.
    #[repr(C)]
    pub struct TaskVmInfo {
        pub virtual_size: u64,
        pub region_count: i32,
        pub page_size: i32,
        pub resident_size: u64,
        pub resident_size_peak: u64,
        pub counters: [u64; 14],
        pub phys_footprint: u64,
    }

    impl TaskVmInfo {
        /// The buffer size `task_info` expects, in natural_t (32-bit) words.
        pub const COUNT: u32 =
            (std::mem::size_of::<Self>() / std::mem::size_of::<u32>()) as u32;
    }
}

#[cfg(test)]
mod layout_tests {
    /// Where `phys_footprint` sits inside `task_vm_info`, and how big the buffer is — in **words**.
    ///
    /// These are the numbers a device reads the kernel's answer with, and no Linux CI can compile
    /// the Apple branch to check them, so they are pinned here: XNU's `osfmk/mach/task_info.h` has
    /// `virtual_size`, `region_count`, `page_size`, `resident_size`, `resident_size_peak` (32 bytes:
    /// two 64-bit fields around two 32-bit ones), fourteen `mach_vm_size_t` counters (112 bytes),
    /// then `phys_footprint` added for rev1.
    ///
    /// The header's own math agrees: `TASK_VM_INFO_REV0_COUNT = REV1 - 2` and rev0 is
    /// `2 + 1 + 1 + 2 + 2 + 28 = 36` words, so rev1 is 38 words = 152 bytes with the field at 144.
    /// Getting this wrong would read a neighbouring counter as the footprint and quietly feed the
    /// RAM budget a number that is not the one jetsam checks.
    #[test]
    fn the_task_vm_info_offsets_match_the_header() {
        // rev0 is 36 words: (2 + 1 + 1 + 2 + 2) for the five leading fields, 28 for the counters.
        assert_eq!(36, 2 + 1 + 1 + 2 + 2 + 28, "rev0 words, from the header's field widths");
        // phys_footprint is the next word after rev0, so byte offset 36 * 4 = 144.
        let offset = 36 * 4;
        assert_eq!(offset, 144, "phys_footprint offset");
        assert_eq!(offset + 8, 152, "struct size");
        assert_eq!(152 / 4, 38, "TASK_VM_INFO_COUNT in natural_t words");
    }

    /// The same numbers again, measured from the struct that is actually used on a device rather
    /// than recomputed: on any target the field widths and `repr(C)` are the same as Apple's, so a
    /// host test measures the very layout the phone will hand to `task_info`.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn the_declared_struct_has_the_offsets_the_header_says() {
        use super::task_vm_info_layout::TaskVmInfo;
        assert_eq!(std::mem::offset_of!(TaskVmInfo, resident_size), 16);
        assert_eq!(std::mem::offset_of!(TaskVmInfo, counters), 32);
        assert_eq!(std::mem::offset_of!(TaskVmInfo, phys_footprint), 144);
        assert_eq!(std::mem::size_of::<TaskVmInfo>(), 152);
        assert_eq!(TaskVmInfo::COUNT, 38);
    }
}
