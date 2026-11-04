# vphone-win-port

**EARLY ALPHA.** This is a work-in-progress port of the vphone-linux GPU stack (reims-vgpu and
metal2vulkan) to native Windows. Nothing here runs a virtual machine yet. The code type-checks and
builds for Windows, and a handful of the individual pieces have been verified on real hardware,
but the full path has not.

## What is here

- `patches/`: patches against the upstream snapshots (see below).
- `probes/`: small standalone Windows programs that check one layer each: placeholder memory
  mapping, releasing split memory, the Windows VM itself, and Vulkan import of that memory.
- `guide/windows-test-guide.md`: how to apply the patches, build, and run the tests.

## Upstream snapshots the patches apply to

| Project       | Snapshot                                       |
|---------------|------------------------------------------------|
| reims-vgpu    | `83dcaf58e484cff369996da7cc10a0cdd0dc8bc3`    |
| metal2vulkan  | `cd1a08ae5442065a11af0fb79d1bdc35c1f0e1b9`    |

The upstream sources are not included here. Get them from their original archives.

## Applying the patches

Lay the two projects out side by side, with the metal2vulkan folder named exactly `metal2vulkan`.
Apply in this order:

1. metal2vulkan (run from the metal2vulkan folder): `patch -p2 -i patches/metal2vulkan-windows-msvc.patch`,
   then `metal2vulkan-devtools-windows.patch`, then `metal2vulkan-runtime-tests-windows.patch`
2. reims-vgpu (run from the reims-vgpu folder): `patch -p1 -i patches/reims-vgpu-windows-msvc.patch`,
   then `reims-vgpu-tests-windows.patch`, then `reims-vgpu-tests-windows-2.patch`

Dry-run each one first with `--dry-run`. The guide has the full steps.

## Status

- Type-checks and builds for `x86_64-pc-windows-msvc`, including the release build of the C API
  (`reims-vgpu-c`, with the `vulkan` feature).
- Runtime host-memory tests pass on Windows (11 of 11).
- The Windows VM (placeholder-based memory with run splitting) passes its probe (25 checks).
- Vulkan import of VM memory works on an AMD Radeon R9 360, using a buffer setup that departs from
  the Vulkan specification (see the notes in the patch).

## Not done

- The Windows OS layer has compile-only stubs for surfaces and window creation, which stop the
  process with a message naming what is missing.
- The C API still refuses guest-memory maps on Windows.
- QEMU's `apple-gfx` device is not yet adapted to Windows file mappings.
- NVIDIA has not been tested. The reims-vgpu reference platform is NVIDIA.
- The Vulkan import fallback departs from the specification and will be reported by validation
  layers.

## Licensing

The upstream projects are licensed under GPL-2.0 and LGPL-3.0. This repository carries patches
and notes only. Apple firmware is not included and must not be added.
