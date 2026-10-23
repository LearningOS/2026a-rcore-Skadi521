# lab2 实验报告：虚拟内存下的系统调用重写与 mmap

## 一、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 运行环境 | WSL2 Ubuntu，内核 6.x（Windows 11 宿主） |
| 工具链 | `nightly-2024-05-02`，目标 `riscv64gc-unknown-none-elf` |
| 模拟器 | QEMU `qemu-system-riscv64` 10.2.1 |
| 分支 | `ch4`（基于 `origin/ch4`） |
| 评测 | `make -C ci-user test CHAPTER=4` |

## 二、实验目标

ch3 的内核与用户程序共用物理内存，系统调用可以直接解引用用户指针。ch4 引入 SV39 三级页表后，**内核运行在 `KERNEL_SPACE` 的页表上**，用户态传入的指针是虚拟地址，在内核态直接解引用会触发 PageFault。因此本章要重写四个系统调用：

| 系统调用 | 要求 |
| --- | --- |
| `sys_get_time` | 把 `TimeVal` 写入用户地址，需处理结构体跨页 |
| `sys_trace` | 在虚拟内存下读/写/查询用户内存，需做地址合法性与权限检查 |
| `sys_mmap` | 按 `start/len/port` 分配并映射匿名页 |
| `sys_munmap` | 解除已建立的映射 |

由于 `check/ch4.py` 的期望集合是 `EXPECTED_3` 的超集，ch3 的 `get_time`、`sleep`、`trace` 断言在 ch4 仍须全部通过，所以系统调用计数机制也要一并迁移。

## 三、地址空间的关键接口

ch4 的 `os/src/mm/` 提供了三层能力，是本实验的实现基础：

1. **翻译一个指针的字节序列**：`translated_byte_buffer(token, ptr, len)`

   ```rust
   pub fn translated_byte_buffer(token: usize, ptr: *const u8, len: usize)
       -> Vec<&'static mut [u8]>
   ```

   它按页遍历 `[ptr, ptr+len)`，把每一页翻译成内核可直接写的物理页切片。返回值是**分段的**切片向量，段数等于跨越的页数——这一点是解决跨页问题的关键。

2. **按 satp 构造只读页表视图**：`PageTable::from_token(token)`。它只持有根页号的引用，不拥有任何物理帧，适合在系统调用里做一次性的 `translate` 查询。

3. **地址空间的组织单位 `MapArea`**：`MemorySet` 内部维护 `Vec<MapArea>`，每个 `MapArea` 有 `vpn_range`（虚拟页区间）、`data_frames`（物理帧所有权）、`map_perm`（R/W/X/U 权限）。`mmap` 就是往这个向量里追加一个 area，`munmap` 就是移除它。

## 四、`sys_get_time` 的重写

跨页问题的本质：`TimeVal` 是两个 `usize` 共 16 字节，如果用户传入 `0x...FF8` 这样的地址，结构体会横跨两页。用 `translated_byte_buffer` 天然可以把「逻辑上连续的 16 字节」拆成「物理上可能不连续的两段」：

```rust
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
```

`copied == src.len()` 才返回 0，作为完整性自检。

## 五、`sys_trace` 的重写

### 5.1 权限语义来自测试

`ch4_trace1.rs` 对 trace 的期望比 ch3 严格得多：

| 调用 | 期望 | 含义 |
| --- | --- | --- |
| `trace_read(&var)`（栈变量） | `Some(111)` | 可读页正常返回 |
| `trace_write(&var, 33)` | `0` | 可写页正常写入 |
| `trace_read(isize::MAX)` | `None` | 非法地址必须拒绝 |
| `trace_read(0x80200000)` | `None` | 内核地址必须拒绝 |
| `mmap(..., prot=1)` 后 `trace_read` | `Some(_)` | 只读页可读 |
| 同上 `trace_write` | `-1` | **只读页不可写** |
| `munmap` 后 `trace_read` | `None` | 映射撤销后不可访问 |

结论：trace 不是「按用户给的地址随便读写」，而是一次**受页表约束的访问**，必须逐项检查「页有效 + 属于用户 + 权限足够」。

### 5.2 一个隐蔽的地址别名问题

最初实现只检查了 `pte.is_valid() && pte.readable()`，结果 `trace_read(isize::MAX)` 返回了 `Some`，第 17 行断言失败。原因是 `VirtAddr::from` 会把入参掩码到 39 位：

```
isize::MAX            = 0x7FFF_FFFF_FFFF_FFFF
掩码 39 位后           = 0x0000_007F_FFFF_FFFF
floor 得到的 VPN       = 0x7FF_FFFF
TRAMPOLINE 掩码后同样落到 VPN = 0x7FF_FFFF
```

也就是说 `isize::MAX` 与 trampoline 页**别名**，而 trampoline 是可读的（`R|X`），于是返回了一个字节。修复分两步：

1. 在掩码之前用原始值做上界判断，用户地址必须 `< 2^38`；
2. 翻译后额外要求 PTE 带 `U` 位，把 trampoline、trap context、内核栈这些内核页彻底排除。

```rust
const MAX_USER_VA: usize = 1 << 38;

match page_table.translate(va.floor()) {
    Some(pte) if pte.is_valid() && pte.flags().contains(PTEFlags::U) && pte.readable() => {
        pte.ppn().get_bytes_array()[va.page_offset()] as isize
    }
    _ => -1,
}
```

写入分支只把 `readable()` 换成 `writable()`，其余一致。

### 5.3 计数查询

`TRACE_SYSCALL` 走 `current_syscall_count(id)`，数据源仍是 TCB 中的 `syscall_times: [u32; 500]`。计数仍在 `trap_handler` 的 `UserEnvCall` 分支中、**调用 `syscall()` 之前**完成，保证查询本身也被计入。

## 六、`sys_mmap` 与 `sys_munmap`

### 6.1 从测试反推的校验规则

对比 `ch4_mmap3` 与 `ch4_unmap` 中同一条调用 `mmap(start - len, len + 1, prot)`：前者期望 `-1`，后者期望 `0`。两次调用的参数完全相同，差别只在于**前者 `[0x10000000, 0x10001000)` 已被占用**。由此可以确定：

| 规则 | mmap | munmap |
| --- | --- | --- |
| `start` 页对齐 | 必须 | 必须 |
| `len` 页对齐 | 不要求（向上取整） | 必须 |
| 权限位合法 | `port` 只能是 `R/W/X` 的组合且非 0，出现更高位立即拒绝 | — |
| 区间冲突 | 与已有 area 重叠则拒绝 | 必须精确匹配某个已存在的 area |

`port` 取值的验证来自 `ch4_mmap3` 的三条断言：`port=0` 拒绝、`port|8` 拒绝、`port=3` 通过。

### 6.2 实现

`mmap` 的核心是「先查冲突，再挂 area」：

```rust
with_current_memory_set(|memory_set| {
    if memory_set.area_overlaps(start_vpn, end_vpn) {
        return -1;
    }
    memory_set.insert_framed_area(start_va, end_va, map_perm);
    0
})
```

为此在 `MemorySet` 上补了两个方法：

```rust
pub fn area_overlaps(&self, start_vpn: VirtPageNum, end_vpn: VirtPageNum) -> bool {
    self.areas
        .iter()
        .any(|area| area.vpn_range.get_start() < end_vpn && start_vpn < area.vpn_range.get_end())
}

pub fn unmap_area(&mut self, start_vpn: VirtPageNum, end_vpn: VirtPageNum) -> bool {
    if let Some(index) = self.areas.iter().position(|area| {
        area.vpn_range.get_start() == start_vpn && area.vpn_range.get_end() == end_vpn
    }) {
        let mut area = self.areas.remove(index);
        area.unmap(&mut self.page_table);
        true
    } else {
        false
    }
}
```

`unmap_area` 使用**精确匹配**：只有请求区间与某个 area 的 `vpn_range` 完全一致时才成功。这样 `ch4_unmap2` 里两个非对齐请求在入口即被拒，而 `ch4_unmap`、`ch4_trace1` 的整体释放能正常完成。移除 area 时 `unmap` 会清空 PTE，`data_frames` 中的 `FrameTracker` 随之析构，物理页自动归还帧分配器——不需要手工 `frame_dealloc`。

访问当前任务地址空间通过闭包式接口完成，避免把 `UPSafeCell` 的内部引用泄漏出去：

```rust
fn with_current_memory_set<F, R>(&self, f: F) -> R
where
    F: FnOnce(&mut MemorySet) -> R,
{
    let mut inner = self.inner.exclusive_access();
    let cur = inner.current_task;
    f(&mut inner.tasks[cur].memory_set)
}
```

## 七、思考题

**1. 为什么 `mmap` 允许 `len` 不对齐，而 `munmap` 不允许？**

`mmap` 的语义是「我需要至少 `len` 字节」，向上取整到页边界是自然的语义扩展，不会产生歧义。`munmap` 的语义是「撤销这块映射」，如果 `len` 不对齐，「这块」的边界就不确定：`munmap(start, 4097)` 究竟应该释放 1 页还是 2 页？因此要求调用者给出精确的页对齐区间，避免模糊匹配。

**2. 为什么 `trace_write` 对只读页要返回 -1，而不是像内核写用户缓冲区那样直接写？**

trace 的设计意图是**观察和受控访问**，它应当遵守页表设定的权限模型。如果 trace 可以绕过 `W` 位，那么 mmap 建立的只读映射就形同虚设，权限检查失去意义。ch3 直接 `write_volatile` 的做法在 ch4 下不仅技术上会 PageFault，语义上也不再成立。

**3. 为什么 `unmap_area` 必须精确匹配而不是允许部分释放？**

允许部分释放需要把一个 `MapArea` 拆成两个（区间在中间被挖掉）或裁剪（在两端被削掉）。前者必须新建 area 并重新插入 `Vec`，后者要同步更新 `vpn_range` 与 `VirtualPageNum` 的边界。在 lab 的测试范围内，精确匹配已能满足语义，且不会在 `areas` 中留下区间与实际映射不一致的「幽灵 area」——那类 bug 会在后续 `mmap` 的重叠检查中表现为莫名其妙的 `-1`。

**4. `TimeVal` 跨页时为什么不直接算出物理地址再一次性拷贝？**

可以，但需要手工计算「第一页剩余字节数」，再判断剩余部分是否落在下一页，还要处理 `len` 为 0 的边界。`translated_byte_buffer` 已经把这段逻辑封装成通用的分段翻译，复用它既减少出错面，也让代码与 `sys_write` 的处理方式保持一致。

## 八、运行结果

`make -C ci-user test CHAPTER=4`：

```
[PASS] found <get_time OK...>
[PASS] found <Test sleep OK...>
[PASS] found <current time_msec = ...>
[PASS] found <time_msec = ... delta = ...ms!>
[PASS] found <Test sleep1 passed...>
[PASS] found <string from task trace test>
[PASS] found <Test trace OK...>
[PASS] found <Test 04_1 OK...>
[PASS] found <Test 04_4 test OK...>
[PASS] found <Test 04_5 ummap OK...>
[PASS] found <Test 04_6 ummap2 OK...>
[PASS] found <Test trace_1 OK...>
[PASS] not found <FAIL: T.T>
[PASS] not found <Test sbrk failed!>
[PASS] not found <Should cause error, Test 04_2 fail!>
[PASS] not found <Should cause error, Test 04_3 fail!>

Test passed: 16/16
Report for lab1 found.
Report for lab2 found.
```

其中 `Test 04_2 fail!` 与 `Test 04_3 fail!` 属于「必须不出现」的项：`ch4_mmap1`/`ch4_mmap2` 故意访问未映射的 `0x10000000`，内核按预期打印 PageFault 并杀死该任务，说明缺页异常的处理链路正常。

## 九、代码改动清单

| 文件 | 改动 |
| --- | --- |
| `os/src/config.rs` | 无需改动，`MAX_SYSCALL_NUM = 500` 模板已提供 |
| `os/src/task/task.rs` | TCB 增加 `syscall_times: [u32; MAX_SYSCALL_NUM]`，`TaskControlBlock::new` 中初始化为全 0 |
| `os/src/task/mod.rs` | 新增 `inc_current_syscall_count` / `get_current_syscall_count` / `with_current_memory_set` 及三个对外包装 |
| `os/src/trap/mod.rs` | `UserEnvCall` 分支在 `syscall()` 之前计数 |
| `os/src/mm/memory_set.rs` | 新增 `area_overlaps`、`unmap_area` |
| `os/src/syscall/process.rs` | 实现 `sys_get_time`、`sys_trace`（含 `trace_read`/`trace_write`）、`sys_mmap`、`sys_munmap` |
| `reports/lab1.md` | 从 ch3 分支取回，报告按章节累积提交 |
| `reports/lab2.md` | 本文件 |


