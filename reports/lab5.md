# lab5 实验报告：并发、同步与死锁检测

## 一、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 运行环境 | WSL2 Ubuntu（Windows 11 宿主） |
| 工具链 | `nightly-2024-05-02`，目标 `riscv64gc-unknown-none-elf` |
| 模拟器 | QEMU `qemu-system-riscv64` 10.2.1 |
| 分支 | `ch8`（基于 `origin/ch8`） |
| 评测 | `make -C ci-user test CHAPTER=8` |

## 二、实验目标

ch8 在 ch7 的「进程 + 文件系统」之上引入**进程内多线程**，并把「进程」与「线程」拆成两级：`ProcessControlBlock` 持有地址空间、文件描述符表与各类同步对象，`TaskControlBlock` 表示一个执行流，共享同一地址空间。任务分为两部分：

1. **同步原语的系统调用**：`semaphore_create/down/up`、`mutex_create/lock/unlock`、`condvar_create/signal/wait`、`thread_create/gettid/waittid`；
2. **死锁检测**：`enable_deadlock_detect(enabled)` 打开检测后，若某次请求会使进程陷入永久等待，则不阻塞，直接返回 `-0xdead` 让用户态自行处理。

`check/ch8.py` 的期望集合是 `EXPECTED_6b` 并上 `ch7b_pipetest`、6 个 `ch8b_*` 测例与 3 个死锁测例，共 **25 条断言**。三个死锁测例的语义各自不同：

| 测例 | 期望 |
| --- | --- |
| `ch8_deadlock_mutex1` | 同一线程重复申请已持有的阻塞互斥量，检测到环路 |
| `ch8_deadlock_sem1` | 三个线程按不同顺序申请计数信号量，确实成环，至少有线程返回非 0 |
| `ch8_deadlock_sem2` | 四个线程的申请顺序**不成环**，不应误报，全部返回 0 |

## 三、同步对象与检测状态的存放位置

模板已经把三类同步对象放在 `ProcessControlBlockInner` 中，用 `Vec<Option<Arc<...>>>` 保存并按空位复用 id：

```rust
pub struct ProcessControlBlockInner {
    pub mutex_list: Vec<Option<Arc<dyn Mutex>>>,
    pub semaphore_list: Vec<Option<Arc<Semaphore>>>,
    pub condvar_list: Vec<Option<Arc<Condvar>>>,
    /// deadlock detection state of the threads in this process
    pub deadlock: DeadlockDetect,
}
```

死锁检测状态是**进程级**的：同一进程的线程共享资源，跨进程不共享，因此检测表随 PCB 走，`ProcessControlBlock::new` 与 `fork` 各自新建一份空表。检测标志也保存在这里面，`enable_deadlock_detect` 只影响本进程。

## 四、信号量：资源分配矩阵与安全性检查

### 4.1 为什么不能只看「等待环」

互斥量有唯一持有者，等待关系是一张有向图，找环即可。计数信号量不同：

- 一份资源有多单位，可被多个线程同时持有；
- `down` 的等待对象是「某个信号量还有没有余量」，而不是「某个具体线程」；
- 因此「A 等 B、B 等 A」这种环并不一定出现，死锁表现为**所有人都还差一点、谁也走不动**。

所以信号量这一侧采用**资源分配图 + 银行家算法的安全性检查**：维护三个矩阵/向量，判断「如果批准这次请求，是否还存在一个让所有线程都能完成的顺序」。

### 4.2 数据结构

```rust
pub struct DeadlockDetect {
    enabled: bool,
    /// 每个信号量的剩余可用单位
    avail: Vec<isize>,
    /// alloc[tid][sem]：线程 tid 已持有的 sem 单位数
    alloc: Vec<Vec<isize>>,
    /// request[tid][sem]：线程 tid 正在等待的 sem 单位数
    request: Vec<Vec<isize>>,
    mutex_owner: Vec<Option<usize>>,
    mutex_wait: Vec<Option<usize>>,
}
```

行按 tid 增长（`grow`），列按资源 id 增长（`grow_res`），两者都在访问前补齐，因此 id 复用（信号量被回收后重新创建）不会越界。

### 4.3 记账点

| 事件 | 记账动作 |
| --- | --- |
| `semaphore_create(n)` | `register_sem(id, n)`：`avail[id] = n`，清空该列 |
| `semaphore_down(id)` 进入 | `set_request(tid, id, 1)` |
| `down` 判定不安全 | 撤销 request，返回 `-0xdead` |
| `down` 成功返回 | `set_request(..., 0)` + `on_acquire`：`alloc += 1`，`avail -= 1` |
| `semaphore_up(id)` | `on_release`：`avail += 1`，`alloc` 减一 |

关键点：**请求必须在阻塞之前登记**。线程在 `sem.down()` 里被挂起时，`request` 已经写在表里，其它线程随后做检查时看到的才是完整的等待关系；`down` 返回后再把 `request` 清零并计入 `alloc`。

`up` 的系统调用只带信号量 id，不携带「谁归还的」，因此 `on_release` 先按调用者扣减，调用者没有持有该资源时再任选一个持有者扣减。这使 `alloc` 在语义上是「本进程对该资源的持有总量」，足以支撑安全性判断。

### 4.4 安全性检查

```rust
let mut work = self.avail.clone();
loop {
    let mut progressed = false;
    for tid in 0..nthread {
        if finish[tid] { continue; }
        if request[tid] 全部 <= work {          // 这个线程的需求可被满足
            finish[tid] = true;
            progressed = true;
            work += alloc[tid];                 // 它完成后归还全部持有量
        }
    }
    if !progressed { break; }
}
// 仍有「未完成且 request 非零」的线程 → 不安全
```

判定为不安全时返回 `DEADLOCK_ERR`（`-0xdead`），与用户库 `try_sem_down` 里比较的常量一致。

用安全性检查而不是精确的环检测，带来两点性质：

- **不会误报真正的可解场景**：`sem2` 中四个线程的申请顺序彼此错开，只要存在一个完成顺序，检查就会通过；
- **是保守的**：它判断的是「批准后系统是否仍存在安全序列」，而不是「此刻是否已经死锁」。对本章的三个测例而言，这一判据的粒度正好——`sem1` 因为每个线程都还缺一份被他人持有的资源而不安全，`sem2` 则始终安全。

## 五、互斥量：wait-for 链

阻塞互斥量有唯一持有者，用两条链描述等待关系：

- `mutex_owner[mid]`：谁持有；
- `mutex_wait[tid]`：线程正在等哪个互斥量。

判定从目标互斥量的持有者出发，沿「持有者 → 它正在等的互斥量 → 该互斥量的持有者」前进，若回到申请者本身则成环：

```rust
let mut owner = self.mutex_owner[mid]?;
for _ in 0..MAX_CHAIN {
    if owner == tid { return true; }
    let next = self.mutex_wait[owner]?;
    owner = self.mutex_owner[next]?;
}
```

`MAX_CHAIN = 64` 是对状态损坏的兜底上界。`mutex1` 的「同一线程重复加锁」在这一模型下退化为 `owner == tid` 立即命中，无需真正入队等待。

记账点：`lock` 前 `set_mutex_wait(tid, Some(mid))`，加锁成功返回后清空并 `on_mutex_acquire`；`unlock` 时 `on_mutex_release`。`condvar_wait` 会临时释放互斥量，因此在 `wait` 前后同步更新「释放」与「重新持有」两条记录，避免把条件变量等待误判成持锁等待。自旋互斥量（`MutexSpin`）不会阻塞，不参与这套模型。

## 六、系统调用接入点

| 系统调用 | 与检测的交互 |
| --- | --- |
| `enable_deadlock_detect` | 设置进程的 `enabled` 标志 |
| `thread_create` | `reset_thread(new_tid)`：回收的 tid 不能继承前任的分配记录 |
| `semaphore_create` | `register_sem(id, n)` |
| `semaphore_down` | 先登记 request → 检查 → 阻塞或拒绝；成功后转入 alloc |
| `semaphore_up` | `on_release` 归还一份 |
| `mutex_create` | `register_mutex(id)`，初始无持有者 |
| `mutex_lock` | 检查回边 → 登记 wait → 加锁 → 转入 owner |
| `mutex_unlock` | `on_mutex_release` |
| `condvar_wait` | 释放/重新持有互斥量时同步更新记录 |

所有对检测表的访问都在 `process.inner_exclusive_access()` 的作用域内完成，并且**在可能阻塞的 `sem.down()` / `mutex.lock()` 之前释放该借用**，避免内核在持有 PCB 借用的情况下切换任务。

## 七、支撑性改动：`sys_get_time`

ch8 分支的 `sys_get_time` 在模板中是返回 `-1` 的桩。而用户库的 `sleep` 并不走 `sys_sleep`：

```rust
pub fn sleep(period_ms: usize) {
    let start = get_time();
    while get_time() < start + period_ms as isize {
        sys_yield();
    }
}
```

`get_time` 由 `sys_get_time` 填充 `TimeVal` 得到。若该调用恒返回 `-1`，`start + period` 也为 `-1 + period`，条件恒成立，调用者会无限 `yield` 下去——任何使用 `sleep` 的测例（例如死锁测例用来错开线程启动的那段等待）都无法推进。因此本章必须把它补齐：

```rust
let us = get_time_us();
let time = TimeVal { sec: us / 1_000_000, usec: us % 1_000_000 };
let mut buffers = translated_byte_buffer(token, ts as *const u8, len);
```

`TimeVal` 是两个 `usize`（16 字节），起始地址可能跨页，因此用 `translated_byte_buffer` 拿到逐页的物理可写切片后分两段拷贝，而不是直接 `translated_refmut`。

## 八、运行结果

```
Test passed: 25/25
```

其中死锁相关的三条：

```
deadlock test mutex 1 OK!
deadlock test semaphore 1 OK!      // sem1：检测到死锁，有子线程返回非 0
deadlock test semaphore 2 OK!      // sem2：未误报，所有子线程返回 0
```

其余 `ch2b`/`ch3b`/`ch5b`/`ch6b`/`ch7b`/`ch8b` 测例全部通过，说明多线程改造没有破坏既有的进程、文件系统与管道语义。

## 九、设计取舍与限制

1. **检测范围限于进程内**。同步对象本就由 PCB 持有，跨进程共享需要另行设计（本章无此需求）。`fork` 出来的子进程得到一份独立的空检测表。
2. **信号量采用安全性检查，是保守判据**。它回答的是「批准该请求后系统是否仍然安全」，而不是「此刻是否已经死锁」，因此不会漏报永久等待，但也可能拒绝某些「再等一会儿就能拿到」的请求。对本章测例，这一粒度与预期一致。
3. **`mutex_lock` 的检查在阻塞之前完成**，但检查与真正入队之间没有别的线程插入（单核、内核态不可抢占），因此不存在「检查通过后状态又变化」的窗口。
4. **`up` 不携带持有者身份**，归还记在调用者名下，调用者未持有时任选持有者；`alloc` 因此表示总量而非精确归属，安全性判断只依赖总量。
5. **自旋互斥量不参与检测**，它不进入等待队列，不存在永久阻塞。

## 十、代码改动清单

| 文件 | 改动 |
| --- | --- |
| `os/src/sync/deadlock.rs` | 新增：`DeadlockDetect`（avail/alloc/request 矩阵、mutex owner/wait 链）、`DEADLOCK_ERR = -0xdead`、安全性检查与回边判定 |
| `os/src/sync/mod.rs` | 导出 `DeadlockDetect`、`DEADLOCK_ERR` |
| `os/src/task/process.rs` | `ProcessControlBlockInner` 增加 `deadlock` 字段；`new` 与 `fork` 各自初始化 |
| `os/src/task/mod.rs` | 新增 `current_tid()`，供同步系统调用取当前线程号 |
| `os/src/syscall/thread.rs` | `thread_create` 末尾 `reset_thread(new_tid)`，清理被复用 tid 的记录 |
| `os/src/syscall/sync.rs` | `enable_deadlock_detect` 落地；`semaphore_*`、`mutex_*`、`condvar_wait` 接入记账与判定；拒绝时返回 `-0xdead` |
| `os/src/syscall/process.rs` | 实现 `sys_get_time`：`get_time_us` 填 `TimeVal`，按页拷贝到用户地址 |
