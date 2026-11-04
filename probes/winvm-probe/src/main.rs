//! Probe for reims-vgpu's Windows VM (`vm.rs`, copied unchanged except its imports).
//! Run on Windows with `cargo run --release` and send back the full output.
//!
//! Each check prints PASS or FAIL with what it saw. The probe keeps going after a failure.
mod shim;
mod vm;

use shim::{HostAddr, HostVm, KernReturn, VmProt};
use vm::WindowsVm;

fn check(name: &str, ok: bool) -> bool {
    println!("{} {}", if ok { "PASS" } else { "FAIL" }, name);
    ok
}

/// The host page size, as the VM reports it.
fn page(vm: &WindowsVm) -> u64 {
    vm.page_size()
}

fn at(base: HostAddr, off: u64) -> HostAddr {
    HostAddr::from_addr(base.addr() + off as usize)
}

/// Fills `n` bytes at `a` with `byte`.
fn fill(a: HostAddr, byte: u8, n: u64) {
    // SAFETY: the caller passes a range the VM mapped readable and writable.
    unsafe { std::ptr::write_bytes(a.addr() as *mut u8, byte, n as usize) };
}

/// The first byte of each page in `[a, a + pages)`.
fn page_firsts(a: HostAddr, pages: u64, pg: u64) -> Vec<u8> {
    (0..pages)
        .map(|i| {
            // SAFETY: the caller passes a range the VM mapped readable.
            unsafe { *((a.addr() + (i * pg) as usize) as *const u8) }
        })
        .collect()
}

/// Whether every byte in `[a, a + n)` is `byte`.
fn all_are(a: HostAddr, byte: u8, n: u64) -> bool {
    // SAFETY: the caller passes a range the VM mapped readable.
    let s = unsafe { std::slice::from_raw_parts(a.addr() as *const u8, n as usize) };
    s.iter().all(|&b| b == byte)
}

fn main() {
    let vm = WindowsVm::new();
    let pg = page(&vm);
    println!("page size {pg}");
    let mut failures = 0;
    let mut note = |ok: bool| {
        if !ok {
            failures += 1;
        }
    };

    // 1. A new run is zero-filled and writable.
    let a = vm.allocate_anywhere(3 * pg).expect("allocate_anywhere");
    note(check("1: new run is zero-filled", all_are(a, 0, 3 * pg)));
    fill(a, 0x11, 3 * pg);
    note(check("1: new run is writable", all_are(a, 0x11, 3 * pg)));

    // 2. Remap aliases a whole run: writes through either address show in both.
    let dst = vm.allocate_anywhere(3 * pg).expect("allocate_anywhere (dst)");
    let r = vm.remap_shared_overwrite(dst, a, 3 * pg);
    note(check("2: remap over an exact run succeeds", r.is_ok()));
    note(check("2: the alias shows the source's bytes", all_are(dst, 0x11, 3 * pg)));
    fill(dst, 0x22, pg);
    note(check("2: a write through the alias shows in the source", all_are(a, 0x22, pg)));
    note(check("2: the rest of the source is unchanged", all_are(at(a, pg), 0x11, 2 * pg)));

    // 3. Partial deallocate: pages 1-2 of a 4-page run go; pages 0 and 3 stay.
    let b = vm.allocate_anywhere(4 * pg).expect("allocate_anywhere (b)");
    for i in 0..4u64 {
        fill(at(b, i * pg), (i as u8) + 1, pg);
    }
    let d = vm.deallocate(at(b, pg), 2 * pg);
    note(check("3: partial deallocate succeeds", d == KernReturn::SUCCESS));
    note(check(
        "3: pages 0 and 3 keep their bytes",
        page_firsts(b, 1, pg) == vec![1] && page_firsts(at(b, 3 * pg), 1, pg) == vec![4],
    ));
    note(check(
        "3: the freed middle is not readable",
        !vm.readable(at(b, pg), 2 * pg),
    ));
    note(check("3: page 0 is still readable", vm.readable(b, pg)));

    // 4. Freed pieces are reused, and reuse gives zeroed memory.
    let c = vm.allocate_anywhere(2 * pg).expect("allocate_anywhere (c)");
    note(check("4: a new allocation of the freed size is zero-filled", all_are(c, 0, 2 * pg)));
    note(check("4: the new allocation is readable", vm.readable(c, 2 * pg)));
    fill(c, 0x33, 2 * pg);
    note(check("4: the new allocation is writable", all_are(c, 0x33, 2 * pg)));

    // 5. Fixed overwrite inside an existing run replaces just that page.
    let e = vm.allocate_anywhere(4 * pg).expect("allocate_anywhere (e)");
    fill(e, 0xaa, 4 * pg);
    let f = vm.allocate_fixed_overwrite(at(e, pg), pg);
    note(check("5: fixed overwrite over a run's page succeeds", f.is_ok()));
    note(check("5: the overwritten page is zero-filled", all_are(at(e, pg), 0, pg)));
    note(check(
        "5: the neighbouring pages keep their bytes",
        all_are(e, 0xaa, pg) && all_are(at(e, 2 * pg), 0xaa, 2 * pg),
    ));

    // 6. Protection: lower a page to read-only, raise it again.
    let p = vm.protect_max(e, pg, VmProt::READ);
    note(check("6: protect to read-only succeeds", p.is_ok()));
    note(check("6: a read-only page is still readable", vm.readable(e, pg)));
    let p = vm.protect_max(e, pg, VmProt::READ | VmProt::WRITE);
    note(check("6: protect back to read-write succeeds", p.is_ok()));
    fill(e, 0x44, pg);
    note(check("6: the page is writable again", all_are(e, 0x44, pg)));

    // 7. Deallocating what is already free is not an error (Mach allows it).
    let gone = vm.deallocate(at(b, pg), 2 * pg);
    note(check("7: deallocate of an already-free range succeeds", gone == KernReturn::SUCCESS));

    // 8. A fixed mapping over memory the VM never owned is refused (by design: see vm.rs).
    let wild = HostAddr::from_addr(0x1000_0000_0000);
    let w = vm.allocate_fixed_overwrite(wild, pg);
    note(check("8: fixed mapping over unowned memory is refused", w.is_err()));

    // 9. A remap whose source is not a run is refused.
    let r = vm.remap_shared_overwrite(at(e, 2 * pg), wild, pg);
    note(check("9: remap from unowned memory is refused", r.is_err()));

    // 10. Stress: many allocations and frees of mixed sizes, each filled and checked.
    let mut live: Vec<(HostAddr, u64, u8)> = Vec::new();
    let mut ok = true;
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    for step in 0..300u32 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        if live.len() < 40 && (seed % 3 != 0 || live.is_empty()) {
            let pages = 1 + (seed >> 8) % 6;
            let byte = (step % 200) as u8 + 1;
            match vm.allocate_anywhere(pages * pg) {
                Ok(a) => {
                    fill(a, byte, pages * pg);
                    live.push((a, pages, byte));
                }
                Err(e) => {
                    println!("  stress step {step}: allocate failed with {e:?}");
                    ok = false;
                    break;
                }
            }
        } else {
            let i = (seed as usize) % live.len();
            let (a, pages, byte) = live.swap_remove(i);
            if !all_are(a, byte, pages * pg) {
                println!("  stress step {step}: contents changed before the free");
                ok = false;
                break;
            }
            if vm.deallocate(a, pages * pg) != KernReturn::SUCCESS {
                println!("  stress step {step}: deallocate failed");
                ok = false;
                break;
            }
        }
    }
    for (a, pages, byte) in &live {
        if !all_are(*a, *byte, pages * pg) {
            println!("  stress: a live allocation was corrupted");
            ok = false;
        }
    }
    note(check("10: 300 mixed allocate/deallocate steps keep every byte", ok));

    println!();
    if failures == 0 {
        println!("all checks in the probe passed");
    } else {
        println!("{failures} check(s) failed; send back the full output");
    }
}
