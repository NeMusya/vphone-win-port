//! Stand-ins for the three core types `vm.rs` imports, with the same shape as the real ones.
use std::ops::BitOr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostPtr(usize);

pub type HostAddr = HostPtr;

impl HostPtr {
    pub const fn from_addr(addr: usize) -> HostPtr {
        HostPtr(addr)
    }

    pub const fn addr(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernReturn(pub i32);

impl KernReturn {
    pub const SUCCESS: KernReturn = KernReturn(0);

    pub const fn is_success(self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmProt(pub i32);

impl VmProt {
    pub const NONE: VmProt = VmProt(0);
    pub const READ: VmProt = VmProt(1);
    pub const WRITE: VmProt = VmProt(2);
}

impl BitOr for VmProt {
    type Output = VmProt;

    fn bitor(self, rhs: VmProt) -> VmProt {
        VmProt(self.0 | rhs.0)
    }
}

pub trait HostVm: Send + Sync {
    fn page_size(&self) -> u64;
    fn page_shift(&self) -> u32;
    fn allocate_anywhere(&self, len: u64) -> Result<HostAddr, KernReturn>;
    fn allocate_fixed_overwrite(&self, addr: HostAddr, len: u64) -> Result<(), KernReturn>;
    fn remap_shared_overwrite(
        &self,
        dst: HostAddr,
        src: HostAddr,
        len: u64,
    ) -> Result<HostAddr, KernReturn>;
    fn protect_max(&self, addr: HostAddr, len: u64, prot: VmProt) -> Result<(), KernReturn>;
    fn deallocate(&self, addr: HostAddr, len: u64) -> KernReturn;
}
