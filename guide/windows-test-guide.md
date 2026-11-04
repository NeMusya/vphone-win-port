# reims-vgpu on Windows: test-run guide

This guide takes the Windows port from "type-checks" to "runs tests on a real GPU". It covers
setup, applying the patches, building, and the tests to run, in order, with what each one should
show. Send back the full output of every step, including the passing ones.

Run it on the R9 360 machine. Don't run it during your gaming session: the build is CPU-heavy
and the tests use the GPU.

## 0. What you need

- Windows 10 version 1803 or later (the VM relies on placeholder APIs). Windows 11 is fine.
- A Vulkan driver for your GPU (the AMD driver you already have is enough).
- Visual Studio 2022 Build Tools with the "Desktop development with C++" workload (provides the
  MSVC linker and the Windows SDK).
- Rust, version 1.98.1, via rustup:

      rustup toolchain install 1.98.1-x86_64-pc-windows-msvc
      rustup default 1.98.1-x86_64-pc-windows-msvc

- Git for Windows. It includes `patch.exe` (in `usr\bin`), which the patch steps use. Add that
  folder to PATH, or call it by its full path.

## 1. Folder layout

The reims-vgpu workspace points at `../metal2vulkan`, so the two folders must sit side by side
under one parent, and the metal2vulkan folder must be named exactly `metal2vulkan`:

    D:\vphone\
        reims-vgpu-83dcaf58e484cff369996da7cc10a0cdd0dc8bc3\
        metal2vulkan\

Unzip the two original archives into `D:\vphone\`, then rename the metal2vulkan folder from
`metal2vulkan-cd1a08ae5442065a11af0fb79d1bdc35c1f0e1b9` to `metal2vulkan`.

## 2. Apply the patches

Dry-run first. Each command should report no failures.

    cd D:\vphone\metal2vulkan
    patch -p2 --dry-run -i D:\path\to\metal2vulkan-windows-msvc.patch

    cd D:\vphone\reims-vgpu-83dcaf58e484cff369996da7cc10a0cdd0dc8bc3
    patch -p1 --dry-run -i D:\path\to\reims-vgpu-windows-msvc.patch

If both dry runs are clean, apply them by removing `--dry-run`. The strip levels differ on
purpose: `-p2` for metal2vulkan, `-p1` for reims-vgpu. Using the wrong one fails every hunk.

Expected: `patch` reports "patching file ..." for these files.

- metal2vulkan: `buffer_descriptor.rs`, `rebind.rs`, `texture.rs`
- reims-vgpu: about 15 files, including `os/windows.rs`, `os/windows/vm.rs`, `Cargo.lock`, and the
  new `native/windows.rs`

## 3. Build

From the reims-vgpu folder:

    cd D:\vphone\reims-vgpu-83dcaf58e484cff369996da7cc10a0cdd0dc8bc3
    cargo check -p reims-vgpu-c --features vulkan --target x86_64-pc-windows-msvc
    cargo build -p reims-vgpu-c --features vulkan --release

Expected: the `check` passes, as it did in the container. The `build` may fail in a C build
script (see Known problems below). If it does, send the full error and stop here; the later steps
need the build.

## 4. Tests, in order

Each step is independent: keep going after a failure, and send all of the output.

**4a. The runtime's host-memory tests** (the import path the patch changed):

    cd D:\vphone\metal2vulkan
    cargo test -p metal2vulkan-runtime --lib host_memory -- --nocapture

What to look for:
- The tests run and pass, or some are skipped because no device is found. Both are fine.
- The line `reims-vgpu: the driver's buffers for host allocations take none of the memory types`
  should appear if the fallback ran. On the R9 360 it should appear, since that is the case the
  fallback was written for. Its absence doesn't mean failure (the spec path may have worked).

If the test run fails to compile on Windows, send the errors. The runtime's test helpers were
written for Linux and macOS, so some compile errors here are expected.

**4b. The Vulkan crate's tests:**

    cd D:\vphone\reims-vgpu-83dcaf58e484cff369996da7cc10a0cdd0dc8bc3
    cargo test -p reims-vgpu-vulkan --lib

Expected to fail to compile. `os.rs`'s test module names `PAGE`, `mapped` and `writable`, which
the Windows OS layer doesn't define yet. Send the error list, since it's the next task to fix. Don't
work around it yourself.

**4c. Regression check on the standalone probes.** Each probe is its own project. Run each one
with `cargo run --release`. They should all still pass:

- `winmap-probe` (13 checks, placeholders and file views)
- `winrelease-probe2` (release of split pieces)
- `winvm-probe` (25 checks, the VM through `vm.rs`)
- `winvk-probe` (Vulkan import, both memory blocks, six GPU checks)

A FAIL in any of these is a regression, and it matters more than a failure in 4a or 4b.

## 5. What to send back

For each step, the full console output, with the exact command you ran. For 4a, also send the
test names that failed, if any. Don't trim the output: the passing lines show what was checked.

## Known problems

- **Build script failures.** Some crates run C or CMake build scripts. The SDL window library is
  not part of the Windows build (it's gated off), so CMake shouldn't be needed. If a build script
  asks for `make`, `cmake` or a C compiler, send the message; don't install anything to get past it.
- **Vulkan spec deviation.** The fallback creates import buffers without the external-memory info.
  That's what makes the import work on this driver, and it's against the Vulkan specification.
  Validation layers will report it. That's expected, not a regression.
- **One driver.** Everything here is on the R9 360. NVIDIA is untested, and so is every other GPU.
- **Not yet done:** the C API still refuses guest-memory maps on Windows, and QEMU's `apple-gfx`
  device doesn't create its RAM as a Windows file mapping. The runtime tests don't exercise those.
