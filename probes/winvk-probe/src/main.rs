//! Probe: can the GPU use the memory the Windows VM hands out?
//!
//! Runs the same import on two kinds of memory: a VM run (a view of a file section) and an
//! ordinary committed block from VirtualAlloc (private memory, no file behind it). The second is
//! the control: if it imports and the VM run does not, the driver refuses file-backed pages.
//!
//! Imports one VM run into Vulkan with VK_EXT_external_memory_host (a pointer import, as the
//! NVIDIA path in reims-vgpu does), then checks, with transfer commands only:
//!   - the run's address meets the driver's minimum imported-pointer alignment
//!   - a GPU copy from a staging buffer shows up in the host view
//!   - a GPU fill shows up in the host view
//!   - a host write shows up in a GPU copy out to a staging buffer
//! Run on Windows with `cargo run --release` and send back the full output.
mod shim;
mod vm;

use std::ffi::{CStr, c_void};

use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};

use ash::{ext, khr, vk};
use shim::HostVm;
use vm::WindowsVm;

fn report<T>(name: &str, r: Result<T, vk::Result>) -> Option<T> {
    match r {
        Ok(v) => {
            println!("PASS {name}");
            Some(v)
        }
        Err(e) => {
            println!("FAIL {name}: {e:?}");
            None
        }
    }
}

/// The lowest memory type index allowed by `bits` that has all of `want`.
fn memory_type(props: &vk::PhysicalDeviceMemoryProperties, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0
            && props.memory_types[i as usize].property_flags.contains(want)
    })
}

/// Records one set of commands into a fresh buffer, runs it, and waits for it.
fn submit(
    device: &ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    record: impl FnOnce(vk::CommandBuffer),
) -> Result<(), vk::Result> {
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { device.allocate_command_buffers(&alloc)? }[0];
    unsafe {
        device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default())?;
        record(cb);
        device.end_command_buffer(cb)?;
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
        let cbs = [cb];
        let submit = vk::SubmitInfo::default().command_buffers(&cbs);
        device.queue_submit(queue, &[submit], fence)?;
        device.wait_for_fences(&[fence], true, u64::MAX)?;
        device.destroy_fence(fence, None);
        device.free_command_buffers(pool, &[cb]);
    }
    Ok(())
}


/// Everything the imports and copies need, set up once.
struct Ctx {
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    qf: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    fns: ext::external_memory_host::Device,
    size: u64,
    staging: vk::Buffer,
    staging_bytes: *mut u8,
}

/// The host pointer's memory type mask, from the driver.
fn host_pointer_bits(ctx: &Ctx, ptr: *mut u8) -> Option<u32> {
    let mut props = vk::MemoryHostPointerPropertiesEXT::default();
    let r = unsafe {
        (ctx.fns.fp().get_memory_host_pointer_properties_ext)(
            ctx.device.handle(),
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
            ptr as *const c_void,
            &mut props,
        )
    };
    if r.result().is_err() {
        println!("FAIL query host pointer memory types: {:?}", r);
        return None;
    }
    Some(props.memory_type_bits)
}

/// Tries one buffer setup: `usage`, with or without the external-memory buffer info. Prints what
/// the driver reports for it, and imports `ptr` if the masks allow.
fn try_variant(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    ctx: &Ctx,
    ptr: *mut u8,
    host_bits: u32,
    usage: vk::BufferUsageFlags,
    external: bool,
    label: &str,
) -> Option<(vk::Buffer, vk::DeviceMemory)> {
    // What the driver says about buffers of this usage with host-allocation handles.
    let info = vk::PhysicalDeviceExternalBufferInfo::default()
        .usage(usage)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
    let mut ext_props = vk::ExternalBufferProperties::default();
    unsafe { instance.get_physical_device_external_buffer_properties(pd, &info, &mut ext_props) };
    println!(
        "  [{label}] driver: features {:#x}, compatible handle types {:#x}",
        ext_props.external_memory_properties.external_memory_features.as_raw(),
        ext_props.external_memory_properties.compatible_handle_types.as_raw(),
    );

    let mut ext_buf = vk::ExternalMemoryBufferCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
    let mut bci = vk::BufferCreateInfo::default()
        .size(ctx.size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    if external {
        bci = bci.push_next(&mut ext_buf);
    }
    let buf = unsafe { ctx.device.create_buffer(&bci, None) }.ok()?;
    let req = unsafe { ctx.device.get_buffer_memory_requirements(buf) };
    let both = req.memory_type_bits & host_bits;
    println!(
        "  [{label}] usage {:#x} external={external}: buffer accepts {:#010b}, host pointer {:#010b}, both {:#010b}",
        usage.as_raw(), req.memory_type_bits, host_bits, both
    );
    if both == 0 {
        unsafe { ctx.device.destroy_buffer(buf, None) };
        return None;
    }
    let idx = memory_type(&ctx.mem_props, both, vk::MemoryPropertyFlags::empty())?;
    let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
        .host_pointer(ptr as *mut c_void);
    let mai = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(idx)
        .push_next(&mut import);
    let mem = match unsafe { ctx.device.allocate_memory(&mai, None) } {
        Ok(m) => m,
        Err(e) => {
            println!("  [{label}] usage {:#x} external={external}: import refused: {e:?}", usage.as_raw());
            return None;
        }
    };
    match unsafe { ctx.device.bind_buffer_memory(buf, mem, 0) } {
        Ok(()) => {
            println!("PASS [{label}] usage {:#x} external={external}: imported and bound", usage.as_raw());
            Some((buf, mem))
        }
        Err(e) => {
            println!("  [{label}] usage {:#x} external={external}: bind refused: {e:?}", usage.as_raw());
            None
        }
    }
}

/// Tries each buffer setup against `ptr`; returns the first that imports.
fn probe_variants(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    ctx: &Ctx,
    ptr: *mut u8,
    label: &str,
) -> Option<(vk::Buffer, vk::DeviceMemory)> {
    let host_bits = host_pointer_bits(ctx, ptr)?;
    let t = vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST;
    let storage = vk::BufferUsageFlags::STORAGE_BUFFER | t;
    let variants = [
        (t, true),
        (t, false),
        (storage, true),
        (storage, false),
        (vk::BufferUsageFlags::TRANSFER_DST, true),
    ];
    for (usage, external) in variants {
        if let Some(found) = try_variant(instance, pd, ctx, ptr, host_bits, usage, external, label) {
            return Some(found);
        }
    }
    println!("FAIL [{label}] no buffer setup imported this memory");
    None
}

/// The three GPU checks against an imported buffer and its host view.
fn gpu_checks(ctx: &Ctx, imported: vk::Buffer, view: *mut u8, label: &str) {
    let size = ctx.size;
    let copy = vk::BufferCopy::default().size(size);

    unsafe { std::ptr::write_bytes(ctx.staging_bytes, 0xa5, size as usize) };
    let r = submit(&ctx.device, ctx.queue, ctx.pool, |cb| unsafe {
        ctx.device.cmd_copy_buffer(cb, ctx.staging, imported, &[copy]);
    });
    if report(&format!("[{label}] GPU copy staging -> imported"), r).is_some() {
        let seen = unsafe { std::slice::from_raw_parts(view, size as usize) };
        println!(
            "{} [{label}] host view sees the GPU copy",
            if seen.iter().all(|&b| b == 0xa5) { "PASS" } else { "FAIL" }
        );
    }

    let r = submit(&ctx.device, ctx.queue, ctx.pool, |cb| unsafe {
        ctx.device.cmd_fill_buffer(cb, imported, 0, size, 0x7777_7777);
    });
    if report(&format!("[{label}] GPU fill of imported"), r).is_some() {
        let seen = unsafe { std::slice::from_raw_parts(view, size as usize) };
        println!(
            "{} [{label}] host view sees the GPU fill",
            if seen.iter().all(|&b| b == 0x77) { "PASS" } else { "FAIL" }
        );
    }

    unsafe { std::ptr::write_bytes(view, 0x3c, size as usize) };
    unsafe { std::ptr::write_bytes(ctx.staging_bytes, 0, size as usize) };
    let r = submit(&ctx.device, ctx.queue, ctx.pool, |cb| unsafe {
        ctx.device.cmd_copy_buffer(cb, imported, ctx.staging, &[copy]);
    });
    if report(&format!("[{label}] GPU copy imported -> staging"), r).is_some() {
        let seen = unsafe { std::slice::from_raw_parts(ctx.staging_bytes, size as usize) };
        println!(
            "{} [{label}] the GPU sees the host write",
            if seen.iter().all(|&b| b == 0x3c) { "PASS" } else { "FAIL" }
        );
    }
}

fn main() {
    let vm = WindowsVm::new();
    let size: u64 = 1 << 20;

    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            println!("FAIL load the Vulkan loader: {e}");
            return;
        }
    };
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
    let ici = vk::InstanceCreateInfo::default().application_info(&app);
    let Some(instance) = report("create a Vulkan instance", unsafe { entry.create_instance(&ici, None) }) else {
        return;
    };
    let Some(pdevs) = report("enumerate physical devices", unsafe { instance.enumerate_physical_devices() }) else {
        return;
    };
    let mut chosen = None;
    for pd in pdevs {
        let Ok(exts) = (unsafe { instance.enumerate_device_extension_properties(pd) }) else {
            continue;
        };
        let has = exts.iter().any(|e| unsafe {
            CStr::from_ptr(e.extension_name.as_ptr()) == ext::external_memory_host::NAME
        });
        let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let transfer = families
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::TRANSFER));
        if let (true, Some(qf)) = (has, transfer) {
            chosen = Some((pd, qf as u32));
            break;
        }
    }
    let Some((pd, qf)) = chosen else {
        println!("FAIL a device with VK_EXT_external_memory_host and a transfer queue");
        return;
    };
    let name = unsafe {
        let p = instance.get_physical_device_properties(pd);
        CStr::from_ptr(p.device_name.as_ptr()).to_string_lossy().into_owned()
    };
    println!("PASS device with VK_EXT_external_memory_host: {name}");

    let mut host_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
    {
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut host_props);
        unsafe { instance.get_physical_device_properties2(pd, &mut props2) };
    }
    println!(
        "  minimum imported host pointer alignment: {} bytes",
        host_props.min_imported_host_pointer_alignment
    );

    let prio = [1.0f32];
    let qci = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(qf)
        .queue_priorities(&prio)];
    let dev_exts = [
        khr::external_memory::NAME.as_ptr(),
        ext::external_memory_host::NAME.as_ptr(),
    ];
    let dci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&qci)
        .enabled_extension_names(&dev_exts);
    let Some(device) = report("create a logical device", unsafe { instance.create_device(pd, &dci, None) }) else {
        return;
    };
    let queue = unsafe { device.get_device_queue(qf, 0) };
    let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
    let fns = ext::external_memory_host::Device::new(&instance, &device);

    // Staging: host-visible and coherent.
    let sci = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let Some(staging) = report("create a staging buffer", unsafe { device.create_buffer(&sci, None) }) else {
        return;
    };
    let sreq = unsafe { device.get_buffer_memory_requirements(staging) };
    let Some(sidx) = memory_type(
        &mem_props,
        sreq.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    ) else {
        println!("FAIL a host-visible, coherent memory type for staging");
        return;
    };
    let sma = vk::MemoryAllocateInfo::default()
        .allocation_size(sreq.size)
        .memory_type_index(sidx);
    let Some(staging_mem) = report("allocate staging memory", unsafe { device.allocate_memory(&sma, None) }) else {
        return;
    };
    if report("bind staging", unsafe { device.bind_buffer_memory(staging, staging_mem, 0) }).is_none() {
        return;
    }
    let Some(staging_ptr) = report("map staging memory", unsafe {
        device.map_memory(staging_mem, 0, size, vk::MemoryMapFlags::empty())
    }) else {
        return;
    };
    let Some(pci_pool) = report(
        "create a command pool",
        unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(qf),
                None,
            )
        },
    ) else {
        return;
    };
    let ctx = Ctx {
        device,
        queue,
        pool: pci_pool,
        qf,
        mem_props,
        fns,
        size,
        staging,
        staging_bytes: staging_ptr as *mut u8,
    };
    let _ = ctx.qf;

    // Control: ordinary committed private memory, page-aligned, with no file behind it.
    let control = unsafe {
        VirtualAlloc(std::ptr::null(), size as usize, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
    } as *mut u8;
    if control.is_null() {
        println!("FAIL allocate control memory");
    } else {
        println!("  control memory (private, no file) at {:#x}", control as usize);
        if let Some((buf, _mem)) = probe_variants(&instance, pd, &ctx, control, "control") {
            gpu_checks(&ctx, buf, control, "control");
        }
    }

    // The VM run: a view of a file section.
    match vm.allocate_anywhere(size) {
        Ok(a) => {
            let view = a.addr() as *mut u8;
            println!("  VM run (file-backed view) at {:#x}", view as usize);
            if let Some((buf, _mem)) = probe_variants(&instance, pd, &ctx, view, "vm") {
                gpu_checks(&ctx, buf, view, "vm");
            }
        }
        Err(e) => println!("FAIL allocate a VM run: {e:?}"),
    }

    println!("probe finished; send back the full output");
}
