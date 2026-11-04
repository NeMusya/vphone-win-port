//! Probe 2: is a split piece really freed by VirtualFree(MEM_RELEASE, size 0)?
//! Asks VirtualQuery (not a reservation attempt) and checks the allocation granularity.
//! Run on Windows with `cargo run --release` and send back the full output.
use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Memory::{
    MEM_FREE, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER,
    MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualAlloc2, VirtualFree, VirtualQuery,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

fn err() -> u32 {
    // SAFETY: reads the thread's last error.
    unsafe { GetLastError() }
}

fn query(at: *mut c_void) -> MEMORY_BASIC_INFORMATION {
    // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is a valid out-parameter.
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `mbi` is writable and the size is passed.
    let got = unsafe {
        VirtualQuery(at, &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>())
    };
    assert!(got != 0, "VirtualQuery: {}", err());
    mbi
}

fn show(label: &str, at: *mut c_void) {
    let m = query(at);
    println!(
        "  {label}: base {:p} region {} bytes, state {:#x} ({}), allocation base {:p}",
        m.BaseAddress,
        m.RegionSize,
        m.State,
        if m.State == MEM_FREE { "FREE" } else { "not free" },
        m.AllocationBase,
    );
}

fn check(name: &str, ok: bool) {
    println!("{} {}", if ok { "PASS" } else { "FAIL" }, name);
}

fn main() {
    // SAFETY: a zeroed SYSTEM_INFO is a valid out-parameter.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    let page = info.dwPageSize as usize;
    let gran = info.dwAllocationGranularity as usize;
    println!("page size {page}, allocation granularity {gran}");

    // 4 pages as one placeholder, split into three pieces: [0,1) [1,2) [2,4).
    // SAFETY: a placeholder reservation of 4 pages, anywhere.
    let p = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            null(),
            4 * page,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            null_mut(),
            0,
        )
    };
    assert!(!p.is_null(), "reserve: {}", err());
    // SAFETY: splits the placeholder at page boundaries, as probe 1 did.
    let s0 = unsafe { VirtualFree(p, page, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) };
    let p1 = unsafe { (p as *mut u8).add(page) } as *mut c_void;
    let s1 = unsafe { VirtualFree(p1, page, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) };
    check("split into three pieces", s0 != 0 && s1 != 0);
    println!("before the release:");
    show("piece 1 (page 1)", p1);

    // Release piece 1 with size 0, then ask the OS what it is now.
    // SAFETY: piece 1's base address, size 0 as MEM_RELEASE requires.
    let r = unsafe { VirtualFree(p1, 0, MEM_RELEASE) };
    println!("VirtualFree(piece 1, size 0) returned {r}, error {}", if r == 0 { err() } else { 0 });
    println!("after the release:");
    show("piece 1 (page 1)", p1);
    let m = query(p1);
    check(
        "VirtualQuery says page 1 is FREE after the release",
        m.State == MEM_FREE,
    );

    // Is the neighbours' state intact? Page 0 and pages 2-3 must still be reserved.
    show("piece 0 (page 0)", p);
    show("pieces 2-3 (page 2)", unsafe { (p as *mut u8).add(2 * page) } as *mut c_void);
    check(
        "page 0 is still reserved",
        query(p).State == MEM_RESERVE,
    );

    // The 64 KB question: does a reservation at a page-aligned but not granularity-aligned
    // address work? (Run this on a fresh address that is free.)
    // SAFETY: a placeholder at a page offset inside the first granule, if free.
    let unaligned = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            p1,
            page,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            null_mut(),
            0,
        )
    };
    if unaligned.is_null() {
        println!("  reserve at page 1 (not granularity-aligned) failed, error {}", err());
    } else {
        println!("  reserve at page 1 (not granularity-aligned) succeeded at {unaligned:p}");
        // SAFETY: releasing what we just reserved.
        unsafe { VirtualFree(unaligned, 0, MEM_RELEASE) };
    }

    // Clean up the remaining pieces.
    // SAFETY: the pieces still reserved: page 0 and pages 2-3.
    unsafe {
        VirtualFree(p, 0, MEM_RELEASE);
        VirtualFree((p as *mut u8).add(2 * page) as *mut c_void, 0, MEM_RELEASE);
    }
    println!("probe 2 finished; send back the full output");
}
