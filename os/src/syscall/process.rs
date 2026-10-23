//! Process management syscalls
use crate::config::PAGE_SIZE;
use crate::mm::{translated_byte_buffer, MapPermission, PTEFlags, PageTable, VirtAddr};
use crate::task::{
    change_program_brk, current_syscall_count, current_user_token, exit_current_and_run_next,
    suspend_current_and_run_next, with_current_memory_set,
};
use crate::timer::get_time_us;
use core::mem::size_of;

/// The three kinds of requests accepted by `sys_trace`, matching `TraceRequest`
/// in the user library.
const TRACE_READ: usize = 0;
const TRACE_WRITE: usize = 1;
const TRACE_SYSCALL: usize = 2;

/// Permission bits carried by the `port` argument of `mmap`.
const PORT_R: usize = 1;
const PORT_W: usize = 2;
const PORT_X: usize = 4;

/// Exclusive upper bound of the user address space under SV39.
///
/// Anything at or above it belongs to the kernel half (trampoline, trap context,
/// kernel stacks), so it must never be accepted as a user pointer. The check is
/// done on the raw value because [`VirtAddr`] masks the input to 39 bits, and a
/// huge pointer such as `isize::MAX` would otherwise alias the trampoline page.
const MAX_USER_VA: usize = 1 << 38;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    let time_val = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    // The user pointer is a virtual address: it must be translated page by page,
    // because a `TimeVal` may straddle the boundary of two pages.
    let src = unsafe {
        core::slice::from_raw_parts(&time_val as *const TimeVal as *const u8, size_of::<TimeVal>())
    };
    let copies = translated_byte_buffer(current_user_token(), ts as *const u8, src.len());
    let mut copied = 0;
    for buffer in copies {
        let len = buffer.len();
        buffer.copy_from_slice(&src[copied..copied + len]);
        copied += len;
    }
    if copied == src.len() {
        0
    } else {
        -1
    }
}

/// TODO: Finish sys_trace to pass testcases
/// HINT: You might reimplement it with virtual memory management.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        TRACE_READ => trace_read(id),
        TRACE_WRITE => trace_write(id, data as u8),
        TRACE_SYSCALL => current_syscall_count(id),
        _ => -1,
    }
}

/// Read one byte from the current task's address space, honouring the page permission.
fn trace_read(addr: usize) -> isize {
    if addr >= MAX_USER_VA {
        return -1;
    }
    let va = VirtAddr::from(addr);
    let page_table = PageTable::from_token(current_user_token());
    match page_table.translate(va.floor()) {
        // The `U` bit keeps kernel-only pages (e.g. the trampoline) out of reach.
        Some(pte) if pte.is_valid() && pte.flags().contains(PTEFlags::U) && pte.readable() => {
            pte.ppn().get_bytes_array()[va.page_offset()] as isize
        }
        _ => -1,
    }
}

/// Write one byte into the current task's address space, honouring the page permission.
fn trace_write(addr: usize, value: u8) -> isize {
    if addr >= MAX_USER_VA {
        return -1;
    }
    let va = VirtAddr::from(addr);
    let page_table = PageTable::from_token(current_user_token());
    match page_table.translate(va.floor()) {
        Some(pte) if pte.is_valid() && pte.flags().contains(PTEFlags::U) && pte.writable() => {
            pte.ppn().get_bytes_array()[va.page_offset()] = value;
            0
        }
        _ => -1,
    }
}

// YOUR JOB: Implement mmap.
/// Map `[start, start + len)` of the current task as freshly allocated frames.
///
/// `start` must be page aligned; `len` is rounded up to a whole number of pages.
/// `port` carries the R/W/X permission bits. The request is rejected if the range
/// overlaps any existing mapping or if the permission bits are invalid.
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!("kernel: sys_mmap");
    if start % PAGE_SIZE != 0 || len == 0 {
        return -1;
    }
    // Only R/W/X are meaningful, and at least one of them must be requested.
    if port == 0 || port & !(PORT_R | PORT_W | PORT_X) != 0 {
        return -1;
    }
    let Some(end) = start.checked_add(len) else {
        return -1;
    };
    if end > MAX_USER_VA {
        return -1;
    }
    let start_va = VirtAddr::from(start);
    let end_va = VirtAddr::from(end);
    let start_vpn = start_va.floor();
    let end_vpn = end_va.ceil();
    if start_vpn >= end_vpn {
        return -1;
    }
    let mut map_perm = MapPermission::U;
    if port & PORT_R != 0 {
        map_perm |= MapPermission::R;
    }
    if port & PORT_W != 0 {
        map_perm |= MapPermission::W;
    }
    if port & PORT_X != 0 {
        map_perm |= MapPermission::X;
    }
    with_current_memory_set(|memory_set| {
        if memory_set.area_overlaps(start_vpn, end_vpn) {
            return -1;
        }
        memory_set.insert_framed_area(start_va, end_va, map_perm);
        0
    })
}

// YOUR JOB: Implement munmap.
/// Unmap the region `[start, start + len)` of the current task.
///
/// Both `start` and `len` must be page aligned, and the region must exactly match
/// a previously mapped area.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    if start % PAGE_SIZE != 0 || len == 0 || len % PAGE_SIZE != 0 {
        return -1;
    }
    let Some(end) = start.checked_add(len) else {
        return -1;
    };
    if end > MAX_USER_VA {
        return -1;
    }
    let start_vpn = VirtAddr::from(start).floor();
    let end_vpn = VirtAddr::from(end).ceil();
    with_current_memory_set(|memory_set| {
        if memory_set.unmap_area(start_vpn, end_vpn) {
            0
        } else {
            -1
        }
    })
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
