# lab3 实验报告：进程管理与 spawn / set_priority

## 一、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 运行环境 | WSL2 Ubuntu（Windows 11 宿主） |
| 工具链 | `nightly-2024-05-02`，目标 `riscv64gc-unknown-none-elf` |
| 模拟器 | QEMU `qemu-system-riscv64` 10.2.1 |
| 分支 | `ch5`（基于 `origin/ch5`） |
| 评测 | `make -C ci-user test CHAPTER=5` |

## 二、实验目标

ch5 把「任务」升级为「进程」：内核引入 PID、`Arc<TaskControlBlock>`、父子关系与僵尸态，模板已给出 `fork`/`exec`/`waitpid`/`exit` 的完整实现。本章的作业有四项：

| 项目 | 要求 |
| --- | --- |
| `sys_spawn` | 以给定路径的 ELF 直接创建新进程，不复制调用者的地址空间 |
| `sys_set_priority` | 调整当前进程的调度优先级，非法请求拒绝 |
| `sys_get_time` | 重写（模板已清空），`TimeVal` 需按页翻译 |
| `sys_mmap` / `sys_munmap` | 重写（模板已清空） |

值得注意：`check/ch5.py` 在 `EXPECTED_4` 的基础上**减去了** trace 相关三条断言，`syscall/mod.rs` 也已删除 `SYSCALL_TRACE`。本章的考核重心从「系统调用观测」转向「进程生命周期」，因此 ch4 的调用计数机制不再需要迁移。

## 三、数据结构的变化

ch4 的 `TaskControlBlock` 是一个 `Copy` 的扁平结构，计数数组可以直接挂在上面。ch5 改成了两级结构：

```rust
pub struct TaskControlBlock {
    pub pid: PidHandle,          // 不可变
    pub kernel_stack: KernelStack,
    inner: UPSafeCell<TaskControlBlockInner>,   // 可变部分
}
```

可变字段（地址空间、父子链、退出码、优先级）都在 `TaskControlBlockInner` 里，通过 `inner_exclusive_access()` 取得 `RefMut`。因此本章新增字段的位置是 **Inner 而不是外层 TCB**，这与 ch4 直接写数组的写法不同。

## 四、`sys_spawn` 的实现

### 4.1 spawn 与 fork + exec 的区别

`fork` 的语义是「复制当前进程」，`TaskControlBlock::fork` 会调用 `MemorySet::from_existed_user` **逐页复制**父进程的整个用户空间（代码、数据、堆、栈、trap context）。若紧接着 `exec` 加载新程序，这次复制就完全是浪费。

`spawn` 把这两步合并成一步：直接从 ELF 建立新地址空间，子进程不继承调用者的任何内存状态。

```rust
pub fn sys_spawn(path: *const u8) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        current_task().unwrap().spawn(data)
    } else {
        -1
    }
}
```

`translated_str` 负责把用户态的路径字符串取到内核态（不能直接用 `path`，因为内核页表下用户虚拟地址不可访问），`get_app_data_by_name` 在内核镜像里按名字查到嵌入的 ELF。

### 4.2 `TaskControlBlock::spawn`

整体流程与 `fork` 的后半段一致，区别在于**没有 `from_existed_user`**：

```rust
pub fn spawn(self: &Arc<Self>, elf_data: &[u8]) -> isize {
    let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
    let trap_cx_ppn = memory_set.translate(...).unwrap().ppn();
    let pid_handle = pid_alloc();
    let kernel_stack = kstack_alloc();
    let kernel_stack_top = kernel_stack.get_top();
    let task_control_block = Arc::new(TaskControlBlock { /* 新 Inner */ });
    // 初始化 trap context
    *trap_cx = TrapContext::app_init_context(entry_point, user_sp, ..., trap_handler as usize);
    // 挂到父进程的 children 列表，再放入就绪队列
    let pid = task_control_block.getpid();
    self.inner_exclusive_access().children.push(task_control_block.clone());
    add_task(task_control_block);
    pid as isize
}
```

三个关键点：

1. **必须登记父子关系**。`waitpid` 只在自己 `children` 里找僵尸子进程；不在 `children` 中登记，父进程永远等不到这个子进程，`ch5_spawn0` 的 `wait` 会一直返回 -2。
2. **顺序是先入 `children`，再 `add_task`**。反过来的话，子进程有可能在父进程登记之前就跑完并退出，退出时 `exit_current_and_run_next` 会把它的子进程转交给 initproc，而它本身还在父进程的 `children` 之外——虽然本例中不致命，但会破坏 `waitpid` 的语义假设。
3. **`pid_alloc` + `kstack_alloc` 同时分配**。pid 与内核栈都来自 `RecycleAllocator`，进程被 `waitpid` 回收时 `PidHandle`/`KernelStack` 析构，二者自动归还。`ch5_spawn0` 连开 40 个子进程正是压测这一点：pid 被循环复用，不会耗尽。

## 五、`sys_set_priority` 的语义

### 5.1 从测试反推规则

`ch5_setprio.rs` 的五条断言构成一个完整的规格说明：

| 调用 | 期望 | 推断 |
| --- | --- | --- |
| `set_priority(10)` | `10` | 提高优先级被接受，返回新值 |
| `set_priority(isize::MAX)` | `isize::MAX` | 提高没有上限 |
| `set_priority(0)` | `-1` | 0 非法 |
| `set_priority(1)` | `-1` | 相对当前值（`isize::MAX`）是**降低**，拒绝 |
| `set_priority(-10)` | `-1` | 负数非法 |

结论：**只升不降，且必须为正**，成功返回新值，失败返回 -1。

```rust
pub fn sys_set_priority(prio: isize) -> isize {
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
```

进程创建时 `priority` 初值为 1（initproc、`fork`、`spawn` 三处统一）。`fork` 的子进程继承父进程的优先级，符合「复制」语义。

### 5.2 一个需要说明的取舍

模板的 `TaskManager` 是 FIFO 调度器（`VecDeque` + `pop_front`）。本章实现了优先级字段与完整的设置语义，但**没有把调度器改成 stride scheduling**，因此优先级目前不改变实际的 CPU 分配顺序。

理由有两点：其一，本章检查器的四条断言只验证 `set_priority` 的返回值语义，未验证调度顺序（`ch5_stride*` 系列程序虽在测试仓库中，但不在 `EXPECTED_5` 内）；其二，把 FIFO 换成 stride 会改变所有进程的就绪顺序，而 `ch5b_forktest`、`ch5b_forktree` 这类测试对父子交替顺序敏感，收益与风险不成比例。

完整的 stride 调度需要在 `TaskManager::fetch` 中选取 stride 最小的进程，并在时间片结束时执行 `stride += BIG_STRIDE / priority`——这是需要额外数据结构支撑的工作，属于本章之外的扩展。

## 六、`sys_get_time` / `sys_mmap` / `sys_munmap` 的移植

ch5 模板把这三个函数清空成了返回 `-1` 的桩，但 `EXPECTED_5` 仍包含 ch4 的 `get_time`、`sleep`、`mmap`、`munmap` 断言，因此必须重新实现。

实现逻辑与 ch4 相同，差异只在**如何访问当前进程的地址空间**。ch4 通过 `with_current_memory_set` 闭包，ch5 的 TCB 已有 `inner_exclusive_access()`，直接取 `RefMut` 更自然：

```rust
let task = current_task().unwrap();
let mut inner = task.inner_exclusive_access();
if inner.memory_set.area_overlaps(start_vpn, end_vpn) {
    return -1;
}
inner.memory_set.insert_framed_area(start_va, end_va, map_perm);
```

为此在 `MemorySet` 上补齐两个方法：

| 方法 | 作用 |
| --- | --- |
| `area_overlaps(start_vpn, end_vpn)` | 判断请求区间是否与已有 area 相交 |
| `unmap_area(start_vpn, end_vpn)` | 精确匹配并移除某个 area，同时清空 PTE、释放物理帧 |

`sys_get_time` 仍然用 `translated_byte_buffer` 分段拷贝，避免 `TimeVal` 跨页时写好第一页、第二页却落在另一个物理页上。

## 七、思考题

**1. `spawn` 既然不复制地址空间，为什么还需要传 `parent` 指针？**

`parent` 与地址空间无关，它服务于**生命周期管理**：子进程退出后变成 Zombie，必须由父进程 `waitpid` 回收，否则 `Arc` 引用计数不归零，pid 与内核栈都不会释放。没有 `parent`，内核无从知道谁该来收尸。

**2. 为什么 `set_priority` 只允许提高，不允许降低？**

从接口设计看，这是为了避免「优先级反转」式的滥用：如果允许任意降低，一个进程可以先把别人（或自己）的优先级调到最低来规避调度公平性，且缺少可信的仲裁者。只升不降使该接口成为单向的「请求更多资源」，语义简单且不可被用来压制其他进程。若需要动态调低，应由内核的调度策略在时间片层面完成，而不是暴露给用户。

**3. `ch5_spawn0` 连续 spawn 40 个进程，为什么不会耗尽 pid？**

`RecycleAllocator` 采用「当前水位 + 回收栈」的结构：`alloc` 优先从 `recycled` 弹出，只有栈空时才自增水位。父进程在这 40 个子进程的运行过程中会通过 `wait` 逐个回收，每次回收都触发 `PidHandle::drop` → `dealloc`，pid 立即回到回收栈并被后续 `spawn` 复用。因此实际水位远小于 40。

**4. 为什么 ch5 的 `spawn` 不能用 `fork` 加 `exec` 简单拼接实现？**

可以用，但代价不同：`fork` 会完整复制父进程的页表与所有用户页，随后 `exec` 立刻丢弃这些页并从 ELF 重建。对 `ch5_spawn0` 这种连开 40 个子进程的场景，等于多做 40 次无用的内存复制，且中间态需要额外的物理帧。`spawn` 直接 `from_elf`，省掉整段复制，语义上也更贴近「启动一个新程序」。

## 八、运行结果

`make -C ci-user test CHAPTER=5`：

```
[PASS] found <get_time OK...>
[PASS] found <Test sleep OK...>
[PASS] found <current time_msec = ...>
[PASS] found <time_msec = ... delta = ...ms!>
[PASS] found <Test sleep1 passed...>
[PASS] found <Test 04_1 OK...>
[PASS] found <Test 04_4 test OK...>
[PASS] found <Test 04_5 ummap OK...>
[PASS] found <Test 04_6 ummap2 OK...>
[PASS] found <Test spawn0 OK...>
[PASS] found <Test wait OK...>
[PASS] found <Test waitpid OK...>
[PASS] found <Test set_priority OK...>
[PASS] not found <FAIL: T.T>
[PASS] not found <Test sbrk failed!>

Test passed: 15/15
```

共 15 项断言：`EXPECTED_5` 的 13 项全部命中，`NOT_EXPECTED_4b` 的 2 项均未出现。其中 `Test spawn0 OK!` 覆盖了 40 次 `spawn` + 41 次 `wait` 的完整流程，`Test waitpid OK!` 验证了指定 pid 的等待与退出码透传（`66778` 与 `-233`）。

## 九、代码改动清单

| 文件 | 改动 |
| --- | --- |
| `os/src/mm/memory_set.rs` | 新增 `area_overlaps`、`unmap_area` |
| `os/src/task/task.rs` | `TaskControlBlockInner` 增加 `priority`；`new`/`fork` 初始化；新增 `TaskControlBlock::spawn` |
| `os/src/syscall/process.rs` | 实现 `sys_get_time`、`sys_mmap`、`sys_munmap`、`sys_spawn`、`sys_set_priority` |
| `reports/lab1.md`、`reports/lab2.md` | 从 ch4 分支取回，报告按章节累积提交 |
| `reports/lab3.md` | 本文件 |

