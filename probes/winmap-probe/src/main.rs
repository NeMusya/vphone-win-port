//! Probe: do the Win32 calls that reims-vgpu's Windows memory layer would use behave as the
//! Linux design assumes? Run on Windows with `cargo run --release`.
//!
//! Each check prints PASS or FAIL. Please send back the full output.
use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE,
    MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION,
    MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, MapViewOfFile3, PAGE_NOACCESS, PAGE_PROTECTION_FLAGS,
    PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc2, VirtualFree, VirtualProtect,
    VirtualQuery,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The current process's pseudo-handle. The process-taking calls below take this, not NULL.
fn cur() -> HANDLE {
    // SAFETY: a pseudo-handle, needs no close.
    unsafe { GetCurrentProcess() }
}

fn last(what: &str) -> ! {
    // SAFETY: reads the calling thread's last error.
    let code = unsafe { GetLastError() };
    panic!("{what} failed: Win32 error {code}");
}

fn check(name: &str, ok: bool) {
    println!("{} {}", if ok { "PASS" } else { "FAIL" }, name);
}

fn page_size() -> usize {
    // SAFETY: a zeroed SYSTEM_INFO is a valid out-parameter for GetSystemInfo.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is writable.
    unsafe { GetSystemInfo(&mut info) };
    info.dwPageSize as usize
}

/// An anonymous (pagefile-backed) section of `len` bytes: the stand-in for memfd.
fn section(len: usize) -> HANDLE {
    // SAFETY: an anonymous section; the length is split into its two 32-bit halves.
    let h = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            null(),
            PAGE_READWRITE,
            (len as u64 >> 32) as u32,
            len as u32,
            null(),
        )
    };
    if h.is_null() {
        last("CreateFileMappingW");
    }
    h
}

fn view(sec: HANDLE, len: usize) -> *mut u8 {
    // SAFETY: a view of a live section of `len` bytes.
    let v = unsafe { MapViewOfFile(sec, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, len) };
    if v.Value.is_null() {
        last("MapViewOfFile");
    }
    v.Value as *mut u8
}

fn unmap(p: *mut u8) {
    // SAFETY: `p` is the base of a view made by MapViewOfFile or MapViewOfFile3.
    unsafe {
        UnmapViewOfFile2(
            cur(),
            MEMORY_MAPPED_VIEW_ADDRESS { Value: p as *mut c_void },
            0,
        )
    };
}

fn main() {
    let page = page_size();
    println!("page size: {page} bytes (reims-vgpu assumes the host's page size)");
    let len = 4 * page;

    // 1. Two views of one section alias.
    let sec = section(len);
    let a = view(sec, len);
    let b = view(sec, len);
    // SAFETY: both are views of `len` bytes.
    unsafe { a.write(0x5a) };
    check(
        "two views of one section alias",
        unsafe { b.read() } == 0x5a,
    );

    // 2. A view keeps the section's bytes after another view of it is unmapped.
    unmap(a);
    check(
        "a view keeps the bytes after another view is unmapped",
        unsafe { b.read() } == 0x5a,
    );
    unmap(b);

    // 3. Reserve a placeholder, then place a file view over it.
    // SAFETY: a placeholder reservation of `len` bytes at any address.
    let region = unsafe {
        VirtualAlloc2(
            null_mut(),
            null(),
            len,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            null_mut(),
            0,
        )
    } as *mut u8;
    check("VirtualAlloc2 reserves a placeholder", !region.is_null());
    if region.is_null() {
        last("VirtualAlloc2 (placeholder)");
    }
    // SAFETY: the placeholder just reserved is replaced by a view of `sec`.
    let placed = unsafe {
        MapViewOfFile3(
            sec,
            cur(),
            region as *const c_void,
            0,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            null_mut(),
            0,
        )
    }
    .Value as *mut u8;
    check(
        "MapViewOfFile3 places a view over a placeholder",
        !placed.is_null(),
    );
    if placed.is_null() {
        last("MapViewOfFile3");
    }
    // SAFETY: the view is `len` bytes, readable and writable.
    unsafe { placed.write(0x11) };

    // 4. Replace page 1 with a page of a second section (Linux: mmap MAP_FIXED over it).
    // Unmap the whole view back to a placeholder, then map the two pieces again.
    let sec2 = section(len);
    // SAFETY: `placed` is the base of the view made above.
    let back = unsafe {
        UnmapViewOfFile2(
            cur(),
            MEMORY_MAPPED_VIEW_ADDRESS {
                Value: placed as *mut c_void,
            },
            MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if back == 0 {
        // SAFETY: reads the calling thread's last error.
        let code = unsafe { GetLastError() };
        println!("  UnmapViewOfFile2 error code: {code}");
    }
    check("UnmapViewOfFile2 back to a placeholder", back != 0);
    // The placeholder is still one range of `len` bytes. A view must match its placeholder's
    // size exactly, so split it first: VirtualFree with MEM_PRESERVE_PLACEHOLDER splits a
    // placeholder instead of releasing it.
    // SAFETY: the placeholder reservation at `region`, split at page boundaries.
    let split0 = unsafe {
        VirtualFree(
            region as *mut c_void,
            page,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    };
    check("split the placeholder at page 0 | the rest", split0 != 0);
    if split0 == 0 {
        // SAFETY: reads the calling thread's last error.
        println!("  VirtualFree (split 0) error code: {}", unsafe { GetLastError() });
    }
    // SAFETY: the rest of the placeholder, split again after page 1.
    let split1 = unsafe {
        VirtualFree(
            region.add(page) as *mut c_void,
            page,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    };
    check("split the placeholder at page 1 | the rest", split1 != 0);
    if split1 == 0 {
        // SAFETY: reads the calling thread's last error.
        println!("  VirtualFree (split 1) error code: {}", unsafe { GetLastError() });
    }

    // SAFETY: the placeholder at `region` is now split: map `sec` over page 0 and `sec2`
    // over page 1.
    let p0 = unsafe {
        MapViewOfFile3(
            sec,
            cur(),
            region as *const c_void,
            0,
            page,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            null_mut(),
            0,
        )
    }
    .Value as *mut u8;
    let p1 = unsafe {
        MapViewOfFile3(
            sec2,
            cur(),
            region.add(page) as *const c_void,
            page as u64,
            page,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            null_mut(),
            0,
        )
    }
    .Value as *mut u8;
    check(
        "page 0 and page 1 map from two sections",
        !p0.is_null() && !p1.is_null(),
    );
    if p0.is_null() || p1.is_null() {
        last("MapViewOfFile3 (split)");
    }
    check(
        "page 0 still shows the first section",
        unsafe { p0.read() } == 0x11,
    );
    check(
        "page 1 shows the second section (zeroed)",
        unsafe { p1.read() } == 0,
    );

    // 5. Protection: lower page 0 to read-only, then raise it again (Linux's mprotect does this).
    let mut old: PAGE_PROTECTION_FLAGS = 0;
    let lowered = unsafe { VirtualProtect(p0 as *const c_void, page, PAGE_READONLY, &mut old) };
    check("VirtualProtect lowers a page", lowered != 0);
    let raised = unsafe { VirtualProtect(p0 as *const c_void, page, PAGE_READWRITE, &mut old) };
    check(
        "VirtualProtect raises it again (Mach refuses this; Linux and Windows allow it)",
        raised != 0,
    );

    // 6. VirtualQuery sees the page as committed and mapped.
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let got = unsafe {
        VirtualQuery(
            p1 as *const c_void,
            &mut mbi,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    check("VirtualQuery answers for a mapped page", got != 0);

    // Teardown.
    unmap(p0);
    unmap(p1);
    // SAFETY: the reservation's pieces are all unmapped; release the reservation.
    unsafe { VirtualFree(region as *mut c_void, 0, MEM_RELEASE) };
    unsafe { CloseHandle(sec) };
    unsafe { CloseHandle(sec2) };
    println!("probe finished; send back the full output");
}
