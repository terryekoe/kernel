//! # Physical Memory Manager (Frame Allocator)
//!
//! Manages physical memory by tracking which 4 KiB "frames" are free or in use.
//!
//! ## Why a Frame Allocator?
//! The CPU uses **paging** to map virtual addresses to physical addresses.
//! Each page maps to a physical "frame" (4096 bytes). Before we can create
//! new page tables, stacks, or allocate memory for user-space processes,
//! we need to know which frames are available.
//!
//! ## Design
//! The bootloader provides a **memory map** describing which regions of
//! physical memory are usable. We iterate through it and hand out frames
//! one at a time. This is a simple "bump allocator" — fast but cannot
//! reclaim freed frames. A bitmap or buddy allocator will replace this later.

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use x86_64::structures::paging::{FrameAllocator, PhysFrame, Size4KiB, OffsetPageTable, PageTable};
use x86_64::{PhysAddr, VirtAddr};
use lazy_static::lazy_static;
use spin::Mutex;

lazy_static! {
    static ref MEMORY_REGIONS: Mutex<Option<&'static MemoryRegions>> = Mutex::new(None);
    // Track where we are allocating DMA memory from (phys addr)
    static ref DMA_ALLOCATOR_STATE: Mutex<Option<PhysAddr>> = Mutex::new(None);
}

pub fn init_regions(regions: &'static MemoryRegions) {
    *MEMORY_REGIONS.lock() = Some(regions);
}

/// Allocate physically contiguous frames for DMA.
/// This implementation steals memory from the *end* of the largest usable region
/// to avoid conflict with the main frame allocator (which starts from the beginning).
pub fn allocate_contiguous_frames(pages: usize) -> Option<PhysAddr> {
    let mut state = DMA_ALLOCATOR_STATE.lock();
    
    // If not initialized, find the suitable region end
    if state.is_none() {
        let regions = MEMORY_REGIONS.lock();
        if let Some(regions) = *regions {
            // Find the largest usable region
            let region = regions.iter()
                .filter(|r| r.kind == MemoryRegionKind::Usable)
                .max_by_key(|r| r.end - r.start)?;
            
            // Start allocating from the end
            *state = Some(PhysAddr::new(region.end));
        } else {
             return None; // Not initialized
        }
    }

    if let Some(mut current_end) = *state {
        let size = (pages * 4096) as u64;
        // Align down? Frames are 4K aligned.
        let new_end = current_end - size;
        
        // Update state
        *state = Some(new_end);
        
        // Ensure aligned
        let aligned_addr = new_end.align_down(4096u64);
        if aligned_addr != new_end {
             // If we weren't aligned (region end wasn't?), align further down
             let final_addr = aligned_addr;
             *state = Some(final_addr);
             return Some(final_addr);
        }
        return Some(aligned_addr);
    }
    None
}

/// Initialize a new OffsetPageTable.
///
/// This allows us to access arbitrary physical frames by adding `physical_memory_offset`
/// to the physical address.
///
/// # Safety
/// The caller must guarantee that the complete physical memory is mapped to virtual memory
/// at the passed `physical_memory_offset`.
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let level_4_table = active_level_4_table(physical_memory_offset);
    OffsetPageTable::new(level_4_table, physical_memory_offset)
}

/// Returns a mutable reference to the active level 4 table.
unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();

    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    &mut *page_table_ptr
}

/// A frame allocator that returns usable frames from the bootloader's memory map.
///
/// This is a simple bump allocator: it walks through all usable memory regions
/// and yields frames sequentially. It does NOT support deallocation (yet).
pub struct BootInfoFrameAllocator {
    /// Reference to the memory map provided by the bootloader.
    memory_regions: &'static MemoryRegions,
    /// Index of the next frame to return (across all usable regions).
    next: usize,
}

impl BootInfoFrameAllocator {
    /// Create a new `BootInfoFrameAllocator` from the bootloader's memory map.
    ///
    /// # Safety
    /// The caller must guarantee that the memory map is valid and that all
    /// frames marked as `Usable` are truly unused (not occupied by kernel code,
    /// page tables, or the bootloader itself).
    pub unsafe fn init(memory_regions: &'static MemoryRegions) -> Self {
        BootInfoFrameAllocator {
            memory_regions,
            next: 0,
        }
    }

    /// Returns an iterator over all usable physical frames in the memory map.
    ///
    /// Each "usable" memory region is divided into 4 KiB frames.
    /// This iterator yields every such frame across all usable regions.
    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> + '_ {
        // Step 1: Filter the memory map to only "Usable" regions.
        let usable_regions = self
            .memory_regions
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable);

        // Step 2: Convert each region into a range of physical addresses.
        let addr_ranges = usable_regions.map(|r| r.start..r.end);

        // Step 3: Convert address ranges into 4 KiB-aligned frame start addresses.
        let frame_addresses = addr_ranges.flat_map(|r| r.step_by(4096));

        // Step 4: Convert addresses into PhysFrame objects.
        frame_addresses.map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

/// Implementation of the `FrameAllocator` trait from the `x86_64` crate.
///
/// This allows our allocator to be used with the crate's page table management
/// functions (e.g., mapping new pages).
unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}
