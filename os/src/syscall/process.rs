//! Process management syscalls
use crate::{
    task::{exit_current_and_run_next, get_syscall_count, suspend_current_and_run_next},
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("[kernel] Application exited with code {}", exit_code);
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    unsafe {
        *ts = TimeVal {
            sec: us / 1_000_000,
            usec: us % 1_000_000,
        };
    }
    0
}

/// Identifier of each `sys_trace` request, must stay in sync with
/// `TraceRequest` in the user library (`user_lib::TraceRequest`).
const TRACE_READ: usize = 0;
const TRACE_WRITE: usize = 1;
const TRACE_SYSCALL: usize = 2;

/// Inspect or modify this task's own state at the request of the task itself.
///
/// * `TRACE_READ`: `id` is an address of this task, return the byte stored there.
/// * `TRACE_WRITE`: write the low byte of `data` to the address `id`.
/// * `TRACE_SYSCALL`: return how many times this task has called syscall `id`.
///
/// ch3 maps user memory one-to-one without a page table, so the addresses the
/// task passes in are directly usable here. Access checking belongs to the
/// address-space chapter (ch4).
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    match trace_request {
        TRACE_READ => unsafe { core::ptr::read_volatile(id as *const u8) as isize },
        TRACE_WRITE => {
            unsafe { core::ptr::write_volatile(id as *mut u8, data as u8) };
            0
        }
        TRACE_SYSCALL => get_syscall_count(id),
        _ => -1,
    }
}
