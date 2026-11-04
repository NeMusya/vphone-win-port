//! `HostVm` on Windows: Mach VM's calls over file sections and placeholders.
//!
//! **The model.** Memory is either a *run* (a view of a file section, mapped over a placeholder)
//! or a *free piece* (a placeholder reservation that maps nothing). Every byte the VM hands out
//! is in a run. Released memory returns to the free list as placeholders and is never released
//! to the OS: a released page-sized piece cannot be reserved again by its address (reservations
//! start on the 64 KB allocation granule, probe 2), so freed pieces are reused instead. New
//! pieces come from fresh reservations, made in whole granules.
//!
//! **Splitting.** A call on a range first *isolates* it: every run the range touches is unmapped
//! back to a placeholder; the free pieces are cut at the range's edges (`VirtualFree` with
//! `MEM_PRESERVE_PLACEHOLDER` splits a placeholder, probe 1); and whatever part of a run lies
//! outside the range is mapped again from the run's section at the matching offset. What is left
//! inside the range is a set of exact placeholder pieces, which a new view can replace (fixed
//! overwrite, remap) or which go back to the free list (deallocate).
//!
//! **Limits.** Pieces are never merged, so a request larger than any single free piece grows the
//! reservation instead, even when the free pieces together are large enough. A fixed mapping can
//! only be placed over memory the VM already owns: a range with no run or free piece under it is
//! refused (`KERN_INVALID_ADDRESS`), since Windows cannot reserve a piece of a granule on demand.
//! A run's pieces are separate runs, so a remap source must lie inside one of them.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::shim::{HostAddr, HostVm, KernReturn, VmProt};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
    MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile3, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc2,
    VirtualFree, VirtualProtect, VirtualQuery,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Mach's codes for the refusals this file makes, as the Linux VM uses them.
const INVALID_ADDRESS: KernReturn = KernReturn(1);
const PROTECTION_FAILURE: KernReturn = KernReturn(2);
const NO_SPACE: KernReturn = KernReturn(3);
const INVALID_ARGUMENT: KernReturn = KernReturn(4);
const FAILURE: KernReturn = KernReturn(5);

/// The host's page size and allocation granule, read once.
fn system_sizes() -> (u64, u64) {
    static SIZES: OnceLock<(u64, u64)> = OnceLock::new();
    *SIZES.get_or_init(|| {
        // SAFETY: a zeroed SYSTEM_INFO is a valid out-parameter for GetSystemInfo.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is writable.
        unsafe { GetSystemInfo(&mut info) };
        (u64::from(info.dwPageSize), u64::from(info.dwAllocationGranularity))
    })
}

fn page_size() -> u64 {
    system_sizes().0
}

fn granule() -> u64 {
    system_sizes().1
}

/// The current process's pseudo-handle, which the process-taking calls take (not NULL).
fn this_process() -> HANDLE {
    // SAFETY: a pseudo-handle; it needs no close.
    unsafe { GetCurrentProcess() }
}

/// A file section, closed when the last run that maps it goes.
struct Section(HANDLE);

// SAFETY: a section handle is usable from any thread until it is closed.
unsafe impl Send for Section {}
// SAFETY: as for `Send`.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the handle was made by CreateFileMappingW and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// One run of memory: a view of `len` bytes of `section` from `offset`.
#[derive(Clone)]
struct Run {
    len: u64,
    section: Arc<Section>,
    offset: u64,
}

/// The VM's state, process-wide as the Linux side's file table is.
#[derive(Default)]
struct State {
    /// Runs by start address.
    runs: BTreeMap<u64, Run>,
    /// Free placeholder pieces by start address, with their lengths.
    free: BTreeMap<u64, u64>,
}

static STATE: Mutex<State> = Mutex::new(State {
    runs: BTreeMap::new(),
    free: BTreeMap::new(),
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn round_up_to(len: u64, unit: u64) -> Result<u64, KernReturn> {
    len.checked_add(unit - 1)
        .map(|l| l / unit * unit)
        .ok_or(NO_SPACE)
}

/// Checks a page-aligned, non-empty range and rounds its length up to whole pages.
fn page_range(addr: u64, len: u64) -> Result<u64, KernReturn> {
    if addr % page_size() != 0 || len == 0 {
        return Err(INVALID_ARGUMENT);
    }
    round_up_to(len, page_size())
}

/// An anonymous section of `len` bytes (a whole number of pages).
fn section(len: u64) -> Result<Arc<Section>, KernReturn> {
    // SAFETY: an anonymous section; the length is split into its two 32-bit halves.
    let h = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            null(),
            PAGE_READWRITE,
            (len >> 32) as u32,
            len as u32,
            null(),
        )
    };
    if h.is_null() {
        return Err(NO_SPACE);
    }
    Ok(Arc::new(Section(h)))
}

/// A placeholder reservation of `len` bytes, anywhere.
fn reserve_anywhere(len: u64) -> Result<u64, KernReturn> {
    // SAFETY: a placeholder reservation in this process; no extended parameters.
    let p = unsafe {
        VirtualAlloc2(
            this_process(),
            null(),
            len as usize,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            null_mut(),
            0,
        )
    };
    if p.is_null() {
        return Err(NO_SPACE);
    }
    Ok(p as usize as u64)
}

/// Replaces the placeholder of exactly `len` bytes at `addr` with a view of `section` from
/// `offset`.
fn map_over(addr: u64, section: &Section, offset: u64, len: u64) -> Result<(), KernReturn> {
    // SAFETY: the placeholder at `addr` of `len` bytes is replaced by a view of `section`.
    let view = unsafe {
        MapViewOfFile3(
            section.0,
            this_process(),
            addr as usize as *const c_void,
            offset,
            len as usize,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            null_mut(),
            0,
        )
    };
    if view.Value.is_null() {
        return Err(FAILURE);
    }
    Ok(())
}

/// Unmaps the view at `addr` back to a placeholder of the same size.
fn unmap_to_placeholder(addr: u64) -> Result<(), KernReturn> {
    // SAFETY: `addr` is the base of a whole view made by `map_over`.
    let ok = unsafe {
        UnmapViewOfFile2(
            this_process(),
            MEMORY_MAPPED_VIEW_ADDRESS {
                Value: addr as usize as *mut c_void,
            },
            MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if ok == 0 {
        return Err(FAILURE);
    }
    Ok(())
}

/// Splits `[start, start + len)` off the front of the placeholder that starts at `start`
/// (probe 1: this works at a placeholder's base and inside it alike).
fn split_front(start: u64, len: u64) -> Result<(), KernReturn> {
    // SAFETY: a placeholder piece of this process, split at `start`.
    let ok = unsafe {
        VirtualFree(
            start as usize as *mut c_void,
            len as usize,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    };
    if ok == 0 {
        return Err(FAILURE);
    }
    Ok(())
}

/// The run that contains `addr`, if any.
fn run_at(runs: &BTreeMap<u64, Run>, addr: u64) -> Option<(u64, Run)> {
    let (&start, run) = runs.range(..=addr).next_back()?;
    (start + run.len > addr).then(|| (start, run.clone()))
}

/// The free piece that contains `addr`, if any.
fn piece_at(free: &BTreeMap<u64, u64>, addr: u64) -> Option<(u64, u64)> {
    let (&start, &len) = free.range(..=addr).next_back()?;
    (start + len > addr).then_some((start, len))
}

/// Cuts the free piece containing `cut` (if it straddles `cut`) so that a piece starts there.
fn cut_free(free: &mut BTreeMap<u64, u64>, cut: u64) -> Result<(), KernReturn> {
    let Some((start, len)) = piece_at(free, cut) else {
        return Ok(());
    };
    if start == cut {
        return Ok(());
    }
    split_front(start, cut - start)?;
    free.insert(start, cut - start);
    free.insert(cut, start + len - cut);
    Ok(())
}

/// Whether the sorted, possibly overlapping spans cover `[start, end)`.
fn covers(spans: &mut [(u64, u64)], start: u64, end: u64) -> bool {
    spans.sort_unstable();
    let mut at = start;
    for &(s, e) in spans.iter() {
        if s > at {
            break;
        }
        at = at.max(e);
        if at >= end {
            return true;
        }
    }
    at >= end
}

/// Isolates `[start, start + len)`: unmaps every run it touches back to placeholders, cuts the
/// pieces at its edges, remaps the parts of those runs that lie outside it, and takes the pieces
/// inside it out of the free list. Returns those pieces, ascending. With `allow_gaps` false the
/// range must be wholly owned by runs and free pieces, or the call is refused before anything
/// changes.
fn isolate(st: &mut State, start: u64, len: u64, allow_gaps: bool) -> Result<Vec<(u64, u64)>, KernReturn> {
    let end = start + len;

    // 1. What the range touches.
    let hit_runs: Vec<(u64, Run)> = st
        .runs
        .range(..end)
        .filter(|(s, r)| **s + r.len > start)
        .map(|(s, r)| (*s, r.clone()))
        .collect();
    let hit_free: Vec<(u64, u64)> = st
        .free
        .range(..end)
        .filter(|(s, l)| **s + **l > start)
        .map(|(s, l)| (*s, *l))
        .collect();
    if !allow_gaps {
        let mut spans: Vec<(u64, u64)> = hit_runs
            .iter()
            .map(|(s, r)| (*s, s + r.len))
            .chain(hit_free.iter().map(|(s, l)| (*s, s + l)))
            .collect();
        if !covers(&mut spans, start, end) {
            return Err(INVALID_ADDRESS);
        }
    }

    // 2. Unmap the runs to placeholders; their pieces join the free list. Whatever lies outside
    //    the range is remapped below, from the same section.
    let mut survivors: Vec<(u64, u64, Arc<Section>, u64)> = Vec::new();
    for (s, run) in &hit_runs {
        unmap_to_placeholder(*s)?;
        st.runs.remove(s);
        st.free.insert(*s, run.len);
        let e = s + run.len;
        if *s < start {
            survivors.push((*s, start - s, run.section.clone(), run.offset));
        }
        if e > end {
            survivors.push((end, e - end, run.section.clone(), run.offset + (end - s)));
        }
    }

    // 3. Cut the pieces at the range's edges.
    cut_free(&mut st.free, start)?;
    cut_free(&mut st.free, end)?;

    // 4. The pieces inside the range are claimed.
    let claimed: Vec<(u64, u64)> = st
        .free
        .range(start..end)
        .map(|(s, l)| (*s, *l))
        .collect();
    for (s, _) in &claimed {
        st.free.remove(s);
    }

    // 5. Map the surviving parts back, each exactly onto its piece.
    for (s, l, section, offset) in survivors {
        st.free.remove(&s);
        map_over(s, &section, offset, l)?;
        st.runs.insert(
            s,
            Run {
                len: l,
                section,
                offset,
            },
        );
    }
    Ok(claimed)
}

/// A piece of `len` bytes (a whole number of pages) from the free list: the first that fits,
/// or a new reservation of whole granules when none does.
fn claim_fresh(st: &mut State, len: u64) -> Result<u64, KernReturn> {
    for _ in 0..2 {
        let fit = st.free.iter().find(|(_, l)| **l >= len).map(|(s, l)| (*s, *l));
        if let Some((start, piece)) = fit {
            st.free.remove(&start);
            if piece > len {
                if let Err(e) = split_front(start, len) {
                    st.free.insert(start, piece);
                    return Err(e);
                }
                st.free.insert(start + len, piece - len);
            }
            return Ok(start);
        }
        let block = round_up_to(len, granule())?;
        let base = reserve_anywhere(block)?;
        st.free.insert(base, block);
    }
    Err(NO_SPACE)
}

/// Makes a new run of `len` bytes, zero-filled, at a fresh piece.
fn new_run(len: u64) -> Result<u64, KernReturn> {
    let len = page_range(0, len)?;
    let sec = section(len)?;
    let mut st = state();
    let at = claim_fresh(&mut st, len)?;
    if let Err(e) = map_over(at, &sec, 0, len) {
        st.free.insert(at, len);
        return Err(e);
    }
    st.runs.insert(
        at,
        Run {
            len,
            section: sec,
            offset: 0,
        },
    );
    Ok(at)
}

/// The Windows host VM. Stateless itself: its runs and free pieces are the process's.
#[derive(Debug, Default)]
pub struct WindowsVm;

impl WindowsVm {
    pub fn new() -> WindowsVm {
        WindowsVm
    }

    /// Whether `len` bytes at `addr` are readable: committed and not `PAGE_NOACCESS`, by
    /// `VirtualQuery` over each region the range touches.
    pub fn readable(&self, addr: HostAddr, len: u64) -> bool {
        if len == 0 {
            return true;
        }
        let end = addr.addr() as u64 + len;
        let mut at = addr.addr() as u64;
        while at < end {
            let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a writable MEMORY_BASIC_INFORMATION of the size passed.
            let got = unsafe {
                VirtualQuery(
                    at as usize as *const c_void,
                    &mut mbi,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if got == 0 || mbi.State != MEM_COMMIT || mbi.Protect == PAGE_NOACCESS {
                return false;
            }
            let region_end = mbi.BaseAddress as usize as u64 + mbi.RegionSize as u64;
            if region_end <= at {
                return false;
            }
            at = region_end;
        }
        true
    }
}

impl HostVm for WindowsVm {
    fn page_size(&self) -> u64 {
        page_size()
    }

    fn page_shift(&self) -> u32 {
        page_size().trailing_zeros()
    }

    fn allocate_anywhere(&self, len: u64) -> Result<HostAddr, KernReturn> {
        new_run(len).map(|a| HostAddr::from_addr(a as usize))
    }

    fn allocate_fixed_overwrite(&self, addr: HostAddr, len: u64) -> Result<(), KernReturn> {
        let addr = addr.addr() as u64;
        let len = page_range(addr, len)?;
        let sec = section(len)?;
        let mut st = state();
        let claimed = isolate(&mut st, addr, len, false)?;
        for (s, l) in claimed {
            let offset = s - addr;
            map_over(s, &sec, offset, l)?;
            st.runs.insert(
                s,
                Run {
                    len: l,
                    section: sec.clone(),
                    offset,
                },
            );
        }
        Ok(())
    }

    fn remap_shared_overwrite(
        &self,
        dst: HostAddr,
        src: HostAddr,
        len: u64,
    ) -> Result<HostAddr, KernReturn> {
        let (dst, src) = (dst.addr() as u64, src.addr() as u64);
        let len = page_range(dst, len)?;
        if src % page_size() != 0 {
            return Err(INVALID_ARGUMENT);
        }
        let mut st = state();
        // The source must lie wholly inside one run: its pages have files behind them. The run is
        // copied out first, since isolating the destination may unmap it.
        let (src_start, source) = run_at(&st.runs, src)
            .filter(|(s, r)| src + len <= s + r.len)
            .ok_or(INVALID_ADDRESS)?;
        let claimed = isolate(&mut st, dst, len, false)?;
        for (s, l) in claimed {
            let offset = source.offset + (src + (s - dst) - src_start);
            map_over(s, &source.section, offset, l)?;
            st.runs.insert(
                s,
                Run {
                    len: l,
                    section: source.section.clone(),
                    offset,
                },
            );
        }
        Ok(HostAddr::from_addr(dst as usize))
    }

    fn protect_max(&self, addr: HostAddr, len: u64, prot: VmProt) -> Result<(), KernReturn> {
        let addr = addr.addr() as u64;
        let len = page_range(addr, len)?;
        {
            let st = state();
            let mut spans: Vec<(u64, u64)> = st
                .runs
                .range(..addr + len)
                .filter(|(s, r)| **s + r.len > addr)
                .map(|(s, r)| (*s, s + r.len))
                .collect();
            if !covers(&mut spans, addr, addr + len) {
                return Err(INVALID_ADDRESS);
            }
        }
        let flags = match prot {
            VmProt::NONE => PAGE_NOACCESS,
            VmProt::READ => PAGE_READONLY,
            _ => PAGE_READWRITE,
        };
        let mut old = 0u32;
        // SAFETY: the range lies in runs this process made; `old` is writable.
        let ok = unsafe {
            VirtualProtect(addr as usize as *const c_void, len as usize, flags, &mut old)
        };
        if ok == 0 {
            return Err(PROTECTION_FAILURE);
        }
        Ok(())
    }

    fn deallocate(&self, addr: HostAddr, len: u64) -> KernReturn {
        let addr = addr.addr() as u64;
        if len == 0 {
            return KernReturn::SUCCESS;
        }
        let len = match page_range(addr, len) {
            Ok(l) => l,
            Err(e) => return e,
        };
        let mut st = state();
        match isolate(&mut st, addr, len, true) {
            Ok(claimed) => {
                for (s, l) in claimed {
                    st.free.insert(s, l);
                }
                KernReturn::SUCCESS
            }
            Err(e) => e,
        }
    }
}

/// Memory a view can be made of: a new run, zero-filled.
pub(crate) fn allocate_aliasable(len: u64) -> Result<HostAddr, KernReturn> {
    WindowsVm::new().allocate_anywhere(len)
}

/// Zeroed host memory for an import: a new run.
pub(crate) fn allocate_zeros(len: u64) -> Result<HostAddr, KernReturn> {
    allocate_aliasable(len)
}
