//! Process management syscalls
//!
use alloc::sync::Arc;
use core::mem::size_of;

use crate::{
    config::PAGE_SIZE,
    fs::{open_file, OpenFlags},
    mm::{
        translated_byte_buffer, translated_refmut, translated_str, MapPermission, VirtAddr,
    },
    task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next,
    },
    timer::get_time_us,
};

/// Permission bits carried by the `port` argument of `mmap`.
const PORT_R: usize = 1;
const PORT_W: usize = 2;
const PORT_X: usize = 4;

/// Exclusive upper bound of the user address space under SV39.
///
/// Anything at or above it belongs to the kernel half (trampoline, trap context,
/// kernel stacks). The check is done on the raw value because [`VirtAddr`] masks
/// its input to 39 bits, so a huge pointer would otherwise alias a kernel page.
const MAX_USER_VA: usize = 1 << 38;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

pub fn sys_yield() -> isize {
    //trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        let task = current_task().unwrap();
        task.exec(all_data.as_slice());
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    //trace!("kernel: sys_waitpid");
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
///
/// The user pointer is a virtual address and a `TimeVal` may straddle a page
/// boundary, so the structure is copied page by page instead of being written
/// through the raw pointer, which would fault under the kernel page table.
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel:pid[{}] sys_get_time", current_task().unwrap().pid.0);
    let us = get_time_us();
    let time_val = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    let src = unsafe {
        core::slice::from_raw_parts(&time_val as *const TimeVal as *const u8, size_of::<TimeVal>())
    };
    let mut copied = 0;
    for buffer in translated_byte_buffer(current_user_token(), ts as *const u8, src.len()) {
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

/// YOUR JOB: Implement mmap.
///
/// Map `[start, start + len)` of the current process as freshly allocated frames.
/// `start` must be page aligned, `len` is rounded up to whole pages, and `port`
/// carries the R/W/X permission bits. The request is rejected when it overlaps an
/// existing mapping or when the permission bits are invalid.
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!("kernel:pid[{}] sys_mmap", current_task().unwrap().pid.0);
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
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.memory_set.area_overlaps(start_vpn, end_vpn) {
        return -1;
    }
    inner
        .memory_set
        .insert_framed_area(start_va, end_va, map_perm);
    0
}

/// YOUR JOB: Implement munmap.
///
/// Unmap the region `[start, start + len)` of the current process. Both `start`
/// and `len` must be page aligned, and the region must exactly match a previously
/// mapped area.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_munmap", current_task().unwrap().pid.0);
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
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner.memory_set.unmap_area(start_vpn, end_vpn) {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// YOUR JOB: Implement spawn.
/// HINT: fork + exec =/= spawn
///
/// `spawn` builds a brand new address space from the given ELF and schedules it,
/// without copying the caller's address space (which is what makes it cheaper
/// than `fork` immediately followed by `exec`).
pub fn sys_spawn(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_spawn", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        current_task().unwrap().spawn(all_data.as_slice())
    } else {
        -1
    }
}

// YOUR JOB: Set task priority.
///
/// A process may only raise its own priority: the new value must be strictly
/// greater than the current one. Returns the applied priority, or -1 on refusal.
pub fn sys_set_priority(prio: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_priority({})",
        current_task().unwrap().pid.0,
        prio
    );
    if prio < 1 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if prio as usize > inner.priority {
        inner.priority = prio as usize;
        prio
    } else {
        -1
    }
}
