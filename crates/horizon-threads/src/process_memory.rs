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
    // TASK_VM_INFO is a Mach constant (mach/task_info.h), not something libc exports.
    const TASK_VM_INFO: libc::c_int = 22;
    let mut info = std::mem::MaybeUninit::<libc::task_vm_info>::zeroed();
    // The call takes and returns the size in natural_t words. libc's struct and its count constant
    // are the SDK's, so the two cannot disagree about the layout.
    let mut count = (std::mem::size_of::<libc::task_vm_info>()
        / std::mem::size_of::<libc::natural_t>()) as libc::mach_msg_type_number_t;
    // SAFETY: `info` is exactly `count` natural_t words of space for the struct the call writes,
    // and the kernel never writes more than `count` words back. The task port is borrowed from
    // libSystem and needs no deallocation.
    let result = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            TASK_VM_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: success means the kernel filled the struct; `phys_footprint` is plain data.
    Some(unsafe { info.assume_init() }.phys_footprint)
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

/// The symbol above, resolved from libSystem. Declared rather than pulled through a crate: it is
/// one C function with an obvious signature, and it is only called on iOS 13+ (the deployment
/// target is 16.0), so no weaker-linked fallback is needed.
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
