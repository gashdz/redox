//! # ACPI
//! Code to parse the ACPI tables

use core::intrinsics::{atomic_load, atomic_store};
use x86::controlregs;

use allocator::{HEAP_START, HEAP_SIZE};
use memory::{Frame, FrameAllocator};
use paging::{entry, ActivePageTable, Page, PhysicalAddress, VirtualAddress};
use start::kstart_ap;

use self::local_apic::{LocalApic, LocalApicIcr};
use self::madt::{Madt, MadtEntry};
use self::rsdt::Rsdt;
use self::sdt::Sdt;
use self::xsdt::Xsdt;

pub mod local_apic;
pub mod madt;
pub mod rsdt;
pub mod sdt;
pub mod xsdt;

const TRAMPOLINE: usize = 0x7E00;
const AP_STARTUP: usize = 0x8000;

pub fn init_sdt<A>(sdt: &'static Sdt, allocator: &mut A, active_table: &mut ActivePageTable)
    where A: FrameAllocator
{
    print!("  ");
    for &c in sdt.signature.iter() {
        print!("{}", c as char);
    }
    println!(":");

    if let Some(madt) = Madt::new(sdt) {
        println!("    {:>016X}: {}", madt.local_address, madt.flags);

        let mut local_apic = LocalApic::new();

        let me = local_apic.id() as u8;

        for madt_entry in madt.iter() {
            println!("      {:?}", madt_entry);
            match madt_entry {
                MadtEntry::LocalApic(asp_local_apic) => if asp_local_apic.id == me {
                    println!("        This is my local APIC");
                } else {
                    if asp_local_apic.flags & 1 == 1 {
                        // Map trampoline
                        {
                            if active_table.translate_page(Page::containing_address(VirtualAddress::new(TRAMPOLINE))).is_none() {
                                active_table.identity_map(Frame::containing_address(PhysicalAddress::new(TRAMPOLINE)), entry::PRESENT | entry::WRITABLE, allocator);
                            }
                        }

                        // Map a stack
                        /*
                        let stack_start = HEAP_START + HEAP_SIZE + 4096 + (asp_local_apic.id as usize * (1024 * 1024 + 4096));
                        let stack_end = stack_start + 1024 * 1024;
                        {
                            let start_page = Page::containing_address(VirtualAddress::new(stack_start));
                            let end_page = Page::containing_address(VirtualAddress::new(stack_end - 1));

                            for page in Page::range_inclusive(start_page, end_page) {
                                active_table.map(page, entry::WRITABLE | entry::NO_EXECUTE, allocator);
                            }
                        }
                        */

                        let ap_ready = TRAMPOLINE as *mut u64;
                        let ap_stack_start = unsafe { ap_ready.offset(1) };
                        let ap_stack_end = unsafe { ap_ready.offset(2) };
                        let ap_code = unsafe { ap_ready.offset(3) };

                        // Set the ap_ready to 0, volatile
                        unsafe { atomic_store(ap_ready, 0) };
                        unsafe { atomic_store(ap_stack_start, 0x1000) };
                        unsafe { atomic_store(ap_stack_end, 0x7000) };
                        unsafe { atomic_store(ap_code, kstart_ap as u64) };

                        // Send INIT IPI
                        {
                            let icr = 0x00004500 | (asp_local_apic.id as u64) << 32;
                            println!("        Sending IPI to {}: {:>016X} {:?}", asp_local_apic.id, icr, LocalApicIcr::from_bits(icr));
                            local_apic.set_icr(icr);
                        }

                        // Send START IPI
                        {
                            let ap_segment = (AP_STARTUP >> 12) & 0xFF;
                            let icr = 0x00004600 | ((asp_local_apic.id as u64) << 32) | ap_segment as u64; //Start at 0x0800:0000 => 0x8000. Hopefully the bootloader code is still there
                            println!("        Sending SIPI to {}: {:>016X} {:?}", asp_local_apic.id, icr, LocalApicIcr::from_bits(icr));
                            local_apic.set_icr(icr);
                        }

                        // Wait for trampoline ready
                        println!("        Waiting for AP {}", asp_local_apic.id);
                        while unsafe { atomic_load(ap_ready) } == 0 {
                            unsafe { asm!("pause" : : : : "intel", "volatile") };
                        }
                        println!("        AP {} is ready!", asp_local_apic.id);
                    } else {
                        println!("        CPU Disabled");
                    }
                },
                _ => ()
            }
        }
    }else {
        println!("    {:?}", sdt);
    }
}

/// Parse the ACPI tables to gather CPU, interrupt, and timer information
pub unsafe fn init<A>(allocator: &mut A, active_table: &mut ActivePageTable) -> Option<Acpi>
    where A: FrameAllocator
{
    let start_addr = 0xE0000;
    let end_addr = 0xFFFFF;

    // Map all of the ACPI table space
    {
        let start_frame = Frame::containing_address(PhysicalAddress::new(start_addr));
        let end_frame = Frame::containing_address(PhysicalAddress::new(end_addr));
        for frame in Frame::range_inclusive(start_frame, end_frame) {
            if active_table.translate_page(Page::containing_address(VirtualAddress::new(frame.start_address().get()))).is_none() {
                active_table.identity_map(frame, entry::PRESENT | entry::NO_EXECUTE, allocator);
            }
        }
    }

    // Search for RSDP
    if let Some(rsdp) = RSDP::search(start_addr, end_addr) {
        println!("{:?}", rsdp);

        let get_sdt = |sdt_address: usize, allocator: &mut A, active_table: &mut ActivePageTable| -> &'static Sdt {
            if active_table.translate_page(Page::containing_address(VirtualAddress::new(sdt_address))).is_none() {
                let sdt_frame = Frame::containing_address(PhysicalAddress::new(sdt_address));
                active_table.identity_map(sdt_frame, entry::PRESENT | entry::NO_EXECUTE, allocator);
            }
            &*(sdt_address as *const Sdt)
        };

        let rxsdt = get_sdt(rsdp.sdt_address(), allocator, active_table);

        for &c in rxsdt.signature.iter() {
            print!("{}", c as char);
        }
        println!(":");
        if let Some(rsdt) = Rsdt::new(rxsdt) {
            for sdt_address in rsdt.iter() {
                let sdt = get_sdt(sdt_address, allocator, active_table);
                init_sdt(sdt, allocator, active_table);
            }
        } else if let Some(xsdt) = Xsdt::new(rxsdt) {
            for sdt_address in xsdt.iter() {
                let sdt = get_sdt(sdt_address, allocator, active_table);
                init_sdt(sdt, allocator, active_table);
            }
        } else {
            println!("UNKNOWN RSDT OR XSDT SIGNATURE");
        }
    } else {
        println!("NO RSDP FOUND");
    }

    None
}

pub struct Acpi;

/// RSDP
#[derive(Copy, Clone, Debug)]
#[repr(packed)]
pub struct RSDP {
    signature: [u8; 8],
    checksum: u8,
    oemid: [u8; 6],
    revision: u8,
    rsdt_address: u32,
    length: u32,
    xsdt_address: u64,
    extended_checksum: u8,
    reserved: [u8; 3]
}

impl RSDP {
    /// Search for the RSDP
    pub fn search(start_addr: usize, end_addr: usize) -> Option<RSDP> {
        for i in 0 .. (end_addr + 1 - start_addr)/16 {
            let rsdp = unsafe { &*((start_addr + i * 16) as *const RSDP) };
            if &rsdp.signature == b"RSD PTR " {
                return Some(*rsdp);
            }
        }
        None
    }

    /// Get the RSDT or XSDT address
    pub fn sdt_address(&self) -> usize {
        if self.revision >= 2 {
            self.xsdt_address as usize
        } else {
            self.rsdt_address as usize
        }
    }
}
