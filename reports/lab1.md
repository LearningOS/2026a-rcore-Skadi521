# lab1 实验报告：系统调用计数与 trace

## 一、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 目标平台 | `riscv64gc-unknown-none-elf`，QEMU `virt` 机器 |
| Rust 工具链 | `nightly-2024-05-02`（随仓库 `rust-toolchain.toml` 固定） |
| QEMU | 10.2.1（本地），课程 CI 为 8.2.2 |
| 分支 | `ch3`（多道程序与分时多任务） |
| 验证方式 | `make -C ci-user test CHAPTER=3` |

## 二、实验目标

ch3 实验要求在已有内核上完成两项功能：

1. **系统调用计数**：为每个任务统计它调用各个系统调用的次数，可被任务自身查询；
2. **trace 系统调用**（`syscall_id = 410`）：支持三种请求——读取任务内存、写入任务内存、查询某系统调用的调用次数。

## 三、系统调用计数

### 3.1 数据结构

计数以「每个任务独立」为粒度，直接放在任务控制块（TCB）里，用一个按系统调用号索引的数组保存：

```rust
// os/src/task/task.rs
pub struct TaskControlBlock {
    pub task_status: TaskStatus,
    pub task_cx: TaskContext,
    /// How many times this task has called each syscall, indexed by syscall id
    pub syscall_times: [u32; MAX_SYSCALL_NUM],
}
```

`MAX_SYSCALL_NUM` 定义在 `os/src/config.rs`，取 500，足以覆盖 ch3 用到的全部系统调用号（64、93、124、169、410）。数组是 `Copy` 类型，所以没有破坏 `TaskControlBlock` 原有的 `#[derive(Copy, Clone)]` 约束，任务管理器的数组式初始化无需改动结构。

### 3.2 计数时机

计数点放在陷门处理函数里、**系统调用分发之前**：

```rust
// os/src/trap/mod.rs
Trap::Exception(Exception::UserEnvCall) => {
    cx.sepc += 4;
    // Count the syscall before dispatching it, so that a query issued by
    // the syscall itself (e.g. sys_trace) also sees its own call.
    increase_syscall_count(cx.x[17]);
    cx.x[10] = syscall(cx.x[17], [cx.x[10], cx.x[11], cx.x[12]]) as usize;
}
```

`x[17]`（即 `a7`）携带系统调用号。选择「分发前」而不是「分发后」是有意的：`sys_trace` 查询自身次数时，本次调用必须已被计入。测试程序的注释也明确写了这一点，断言值为 2 而非 1 正是对该时序的检验。

计数与查询通过任务管理器暴露的两个接口访问当前任务：

```rust
// os/src/task/mod.rs
fn inc_syscall_count(&self, syscall_id: usize) {
    if syscall_id >= MAX_SYSCALL_NUM { return; }
    let mut inner = self.inner.exclusive_access();
    let current = inner.current_task;
    inner.tasks[current].syscall_times[syscall_id] += 1;
}

fn syscall_count(&self, syscall_id: usize) -> isize {
    if syscall_id >= MAX_SYSCALL_NUM { return -1; }
    let inner = self.inner.exclusive_access();
    let current = inner.current_task;
    inner.tasks[current].syscall_times[syscall_id] as isize
}
```

## 四、trace 系统调用

### 4.1 请求语义

用户库中 `TraceRequest` 是 `#[repr(C)]` 枚举，编号即内核侧的请求号：

| request | 名称 | 参数含义 | 返回值 |
| ---: | --- | --- | --- |
| 0 | `Read` | `id` 为用户空间地址 | 该地址处的 1 字节，失败返回 -1 |
| 1 | `Write` | `id` 为地址，`data` 为写入值 | 0 |
| 2 | `Syscall` | `id` 为系统调用号 | 该任务对此系统调用的累计调用次数 |

### 4.2 实现

```rust
// os/src/syscall/process.rs
const TRACE_READ: usize = 0;
const TRACE_WRITE: usize = 1;
const TRACE_SYSCALL: usize = 2;

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
```

使用 `read_volatile` / `write_volatile` 而非普通解引用，是因为这两个接口按语义就是对任务内存的「观察」动作，编译器的读写合并或消除会改变语义。

ch3 的内核与用户程序处在同一片物理内存上、没有页表，用户传入的地址可以直接访问，因此这里没有做地址合法性检查。地址空间隔离是 ch4 的内容：届时用户地址需要经过页表转换，并检查是否落在用户区的映射范围内。

## 五、思考：为什么 `println!` 产生两次 write 系统调用

测试程序断言 `count_syscall(SYSCALL_WRITE) == 2`，而它只调用了 `println!("string from task trace test\n")` 一次。原因在用户库的两处实现：

1. `println!` 宏把格式串与 `"\n"` 用 `concat!` 拼在一起，所以实际输出的是 `"string from task trace test\n\n"`——**两个换行符**；
2. `ConsoleBuffer::write_str` 逐字节入缓冲，遇到 `\n` 就 `flush()`，而每次 flush 对应一次 `write` 系统调用。

两个换行符各触发一次 flush，于是正好两次 write。

## 六、运行结果

```
[PASS] found <get_time OKxxxxx! (\d+)>
[PASS] found <Test sleep OKxxxxx!>
[PASS] found <current time_msec = (\d+)>
[PASS] found <time_msec = (\d+) after sleeping (\d+) ticks, delta = (\d+)ms!>
[PASS] found <Test sleep1 passedxxxxx!>
[PASS] found <string from task trace test>
[PASS] found <Test trace OKxxxxx!>
Test passedxxxxx: 7/7
Report for lab1 found.
```

七个断言全部通过。输出中 `xxxxx` 是检查器每轮随机化后的后缀，用于防止写死输出。


## 七、代码改动清单

| 文件 | 改动 |
| --- | --- |
| `os/src/config.rs` | 新增 `MAX_SYSCALL_NUM = 500` |
| `os/src/task/task.rs` | TCB 新增 `syscall_times` 数组 |
| `os/src/task/mod.rs` | 初始化新字段；新增计数与查询接口 |
| `os/src/trap/mod.rs` | 系统调用分发前计数 |
| `os/src/syscall/process.rs` | 实现 `sys_trace` 三类请求 |
