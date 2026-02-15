use core::ptr::NonNull;
use virtio_drivers::{BufferDirection, Hal};
use crate::memory;
use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::{
    structures::paging::mapper::Translate,
    VirtAddr as X86VirtAddr, PhysAddr as X86PhysAddr,
};
use alloc::alloc::{alloc_zeroed, dealloc, Layout};

pub struct VirtioHal;

lazy_static! {
    static ref PHYSICAL_MEMORY_OFFSET: Mutex<Option<u64>> = Mutex::new(None);
}

pub fn init(physical_memory_offset: u64) {
    *PHYSICAL_MEMORY_OFFSET.lock() = Some(physical_memory_offset);
}

unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (usize, NonNull<u8>) {
        // Use our new contiguous allocator
        let phys_addr = memory::allocate_contiguous_frames(pages)
            .expect("VirtioHal: DMA allocation failed (contiguous)");
            
        // Get generic virtual address (via offset map)
        let ptr = unsafe { Self::mmio_phys_to_virt(phys_addr.as_u64() as usize, pages * 4096) };

        // Zero the memory (safety: we own it)
        unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0, pages * 4096) };

        (phys_addr.as_u64() as usize, ptr)
    }

    unsafe fn dma_dealloc(_paddr: usize, _vaddr: NonNull<u8>, _pages: usize) -> i32 {
        // We hacked memory::allocate_contiguous_frames to steal memory and never return it.
        // So dealloc is a no-op.
        // This is fine for now as we don't really free DMA buffers (queues live forever).
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: usize, _size: usize) -> NonNull<u8> {
        let offset = PHYSICAL_MEMORY_OFFSET.lock().expect("HAL not initialized");
        // offset is u64 here because expect returns copy of Option content
        let virt_addr = X86VirtAddr::new(paddr as u64 + offset);
        NonNull::new(virt_addr.as_mut_ptr()).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> usize {
        let virt_addr = X86VirtAddr::new(buffer.as_ptr() as *mut u8 as u64);
        let phys = virt_to_phys_addr(virt_addr);
        phys.as_u64() as usize
    }

    unsafe fn unshare(_paddr: usize, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Nothing to do for shared memory on x86_64
    }
}

fn virt_to_phys_addr(virt_addr: X86VirtAddr) -> X86PhysAddr {
    let offset = PHYSICAL_MEMORY_OFFSET.lock().expect("HAL not initialized");
    let physical_memory_offset = X86VirtAddr::new(offset);
    
    // Create a temporary mapper to translate the address
    // SAFETY: We assume PHYSICAL_MEMORY_OFFSET is correct and complete.
    // Calling active_level_4_table etc is safe-ish if we don't mutate while paging is active in a way that races.
    // Translating is read-only.
    // Note: Creating OffsetPageTable requires &mut PageTable, which might be tricky with concurrency if we had threads.
    // For now, single threaded, it's fine.
    
    // BUT we need to call memory::init or equivalent. 
    // memory::init takes `VirtAddr`. 
    // Let's assume we can use the helper from memory.rs if we make it pub or duplicate.
    // Actually, recreating the mapper every time is heavy.
    // Optimization: If the address is in the physical memory map region (offset + phys), we can just subtract.
    // If it's in the kernel image (static HEAP), it might not be.
    
    // Let's try the mapper approach for correctness.
    let mapper = unsafe { memory::init(physical_memory_offset) };
    mapper.translate_addr(virt_addr).expect("VirtioHal: Failed to translate virtual address")
}
