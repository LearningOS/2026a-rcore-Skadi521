# lab4 实验报告：文件系统与硬链接

## 一、实验环境

| 项目 | 版本 / 说明 |
| --- | --- |
| 运行环境 | WSL2 Ubuntu（Windows 11 宿主） |
| 工具链 | `nightly-2024-05-02`，目标 `riscv64gc-unknown-none-elf` |
| 模拟器 | QEMU `qemu-system-riscv64` 10.2.1 |
| 分支 | `ch6`（基于 `origin/ch6`） |
| 评测 | `make -C ci-user test CHAPTER=6` |

## 二、实验目标

ch6 把「外设」升级为「持久化文件系统」：内核挂载 `easy-fs`，通过 virtio-blk 读写位于 `fs.img` 上的文件，`File` trait 的实现从「只有 stdin/stdout」扩展到「磁盘上的 inode」。本章的作业有三项：

| 项目 | 要求 |
| --- | --- |
| `sys_fstat` | 返回文件描述符对应 inode 的 `Stat`（`dev`/`ino`/`mode`/`nlink`） |
| `sys_linkat` | 为已有 inode 建立硬链接，`nlink` 加一 |
| `sys_unlinkat` | 删除目录项，`nlink` 减一，归零时回收 inode 与数据块 |

另外，`check/ch6.py` 的 `EXPECTED_6` 是在 `EXPECTED_5` 的基础上**并上**文件相关断言、再**去掉** `Test set_priority OK!` 构成的。也就是说 ch5 的 spawn / wait / mmap / get_time 等能力在本章仍然要全部可跑，而模板分支是干净的，需要在 ch6 上重新落地一遍。

## 三、为什么必须先改 easy-fs

本章最容易踩空的一点：`Stat` 里的 `nlink`（硬链接数）在仓库自带的 `easy-fs` 中**根本不存在**。

`easy-fs/src/layout.rs` 的 `DiskInode` 原本只有 `size` / `direct[28]` / `indirect1` / `indirect2` / `type_`，`vfs.rs` 的 `Inode` 只有 `find` / `create` / `ls` / `read_at` / `write_at` / `clear`。既没有链接计数字段，也没有「把已有 inode 挂到目录下」和「摘掉目录项」的接口。

因此本章的实现分成两层：

1. **easy-fs 层**：给 `DiskInode` 加 `nlink`，给 `Inode` 加 `link` / `unlink` / `nlink` / `is_dir` / `size` / `inode_id`，并让 `Inode` 记住自己的 `inode_id`；
2. **内核层**：在 `File` trait 上补 `stat()`，实现三个系统调用。

## 四、`DiskInode` 增加链接计数

```rust
pub struct DiskInode {
    pub size: u32,
    pub direct: [u32; INODE_DIRECT_COUNT],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
    /// number of hard links pointing to this inode
    pub nlink: u32,
}
```

`initialize()` 中置 `nlink = 1`：新建目录项本身就是第一条链接。

结构体从 128 字节变成 132 字节，`inodes_per_block` 由 `512 / size_of::<DiskInode>()` 算出，随之从 4 变成 3。这一点不构成兼容问题，因为 `fs.img` 每次 `make` 都由 `easy-fs-fuse` 从零重建（`EasyFileSystem::create` 会按新的 `size_of` 重新计算 `inode_area_blocks`），不存在旧镜像被新内核读取的情况。

## 五、`Inode::link` 与 `Inode::unlink`

### 5.1 link

```rust
pub fn link(&self, name: &str, inode_id: u32) -> bool {
    let mut fs = self.fs.lock();
    // the name must not be used
    if self.read_disk_inode(|root_inode| self.find_inode_id(name, root_inode).is_some()) {
        return false;
    }
    // increase the link count of the target inode
    let (target_block_id, target_block_offset) = fs.get_disk_inode_pos(inode_id);
    get_block_cache(target_block_id as usize, Arc::clone(&self.block_device))
        .lock()
        .modify(target_block_offset, |disk_inode: &mut DiskInode| {
            disk_inode.nlink += 1;
        });
    // append a new directory entry
    self.modify_disk_inode(|root_inode| {
        let file_count = (root_inode.size as usize) / DIRENT_SZ;
        let new_size = (file_count + 1) * DIRENT_SZ;
        self.increase_size(new_size as u32, root_inode, &mut fs);
        let dirent = DirEntry::new(name, inode_id);
        root_inode.write_at(file_count * DIRENT_SZ, dirent.as_bytes(), &self.block_device);
    });
    block_cache_sync_all();
    true
}
```

三个要点：

1. **先查重名再改计数**。若中途发现重名返回 `false`，此时还没有修改任何状态，语义干净。
2. **直接复用目标 inode 的编号**，不分配新 inode —— 这正是硬链接与「复制一份文件」的本质区别：两条目录项指向同一个 `inode_id`，共享同一份数据块。
3. **目录本身也是文件**，追加目录项走的还是 `increase_size` + `write_at`，与 `create` 完全同构。

### 5.2 unlink

```rust
pub fn unlink(&self, name: &str) -> bool {
    let mut fs = self.fs.lock();
    // find the directory entry to remove
    let found = self.read_disk_inode(|root_inode| {
        let file_count = (root_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                root_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device),
                DIRENT_SZ,
            );
            if dirent.name() == name {
                return Some((i, dirent.inode_id()));
            }
        }
        None
    });
    let Some((index, inode_id)) = found else { return false; };
    // decrease the link count
    let nlink = /* 目标 inode 的 nlink -= 1 */;
    // the last link is gone: reclaim the data blocks and the inode itself
    if nlink == 0 {
        let data_blocks = /* disk_inode.clear_size(...) */;
        for data_block in data_blocks.into_iter() {
            fs.dealloc_data(data_block);
        }
        fs.dealloc_inode(inode_id);
    }
    // move the last entry into the hole, then shrink the directory
    self.modify_disk_inode(|root_inode| {
        let file_count = (root_inode.size as usize) / DIRENT_SZ;
        if index + 1 != file_count {
            let mut last = DirEntry::empty();
            root_inode.read_at((file_count - 1) * DIRENT_SZ, last.as_bytes_mut(), &self.block_device);
            root_inode.write_at(index * DIRENT_SZ, last.as_bytes(), &self.block_device);
        }
        root_inode.size -= DIRENT_SZ as u32;
    });
    block_cache_sync_all();
    true
}
```

目录项的删除用的是**「末尾填洞」**而不是整体前移：把最后一项搬到被删位置，再把 `size` 减去 `DIRENT_SZ`。目录项是定长 32 字节，顺序对查找没有意义，因此这样可以避免 `O(n)` 的搬移，也不需要额外的数据块回收。

`nlink == 0` 时才真正回收。回收分两步：`clear_size` 归还数据块（含 indirect1/indirect2 这些间接块，它们同样来自 `alloc_data`，所以同样交给 `dealloc_data`），`dealloc_inode` 归还 inode 位图中的那一位。为此在 `EasyFileSystem` 上补了：

```rust
pub fn dealloc_inode(&mut self, inode_id: u32) {
    self.inode_bitmap.dealloc(&self.block_device, inode_id as usize)
}
```

## 六、`sys_fstat`

`fd_table` 的元素是 `Arc<dyn File + Send + Sync>`，内核拿到的是 trait 对象，无法直接调用 `OSInode` 的方法。因此在 `File` 上补了一个带默认实现的方法：

```rust
pub trait File: Send + Sync {
    // ...
    /// the stat of the file, only meaningful for files on disk
    fn stat(&self) -> Stat {
        Stat::empty()
    }
}
```

只有 `OSInode` 覆盖它，`Stdin`/`Stdout` 保持默认（返回全零的 `Stat`），这样不必改动 `stdio.rs`。

```rust
fn stat(&self) -> Stat {
    let inner = self.inner.exclusive_access();
    let inode = &inner.inode;
    Stat {
        dev: 0,
        ino: inode.inode_id() as u64,
        mode: if inode.is_dir() { StatMode::DIR } else { StatMode::FILE },
        nlink: inode.nlink(),
        pad: [0; 7],
    }
}
```

`ino` 直接用 `inode_id`。`ch6_file2` 要求「同一个 inode 的两条路径 stat 出来 `dev` 与 `ino` 相同」，`inode_id` 天然满足这一点（同一个 `inode_id` 的目录项都在同一个磁盘位置上）。

系统调用侧的关键是**用户指针是虚拟地址**：

```rust
let src = unsafe {
    core::slice::from_raw_parts(&stat as *const Stat as *const u8, core::mem::size_of::<Stat>())
};
let mut copied = 0usize;
for buffer in translated_byte_buffer(token, st as *const u8, src.len()).iter_mut() {
    let len = buffer.len();
    buffer.copy_from_slice(&src[copied..copied + len]);
    copied += len;
}
```

`Stat` 是 80 字节，完全可能跨页，所以走 `translated_byte_buffer` 分段拷贝，而不是用 `translated_refmut` 一次性写。

## 七、`sys_linkat` 与 `sys_unlinkat`

```rust
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    let token = current_user_token();
    let old_name = translated_str(token, old_name);
    let new_name = translated_str(token, new_name);
    let Some(inode) = ROOT_INODE.find(old_name.as_str()) else { return -1; };
    if ROOT_INODE.link(new_name.as_str(), inode.inode_id()) { 0 } else { -1 }
}

pub fn sys_unlinkat(name: *const u8) -> isize {
    let token = current_user_token();
    let name = translated_str(token, name);
    if ROOT_INODE.unlink(name.as_str()) { 0 } else { -1 }
}
```

两个路径都用 `translated_str` 从用户态拷出（路径以 `\0` 结尾，ch6 没有目录层级，一律相对根目录）。`linkat` 的第三个参数 `flags` 在本课程中恒为 0，直接忽略；`AT_FDCWD` 也无需处理，因为只有根目录。

## 八、ch5 能力的移植

ch6 模板把 ch5 的五个系统调用再次清空，需要在 ch6 上重新实现（实现与 lab3 相同，差异只有一处）：

| 文件 | 改动 |
| --- | --- |
| `os/src/mm/memory_set.rs` | 新增 `area_overlaps`、`unmap_area` |
| `os/src/task/task.rs` | `TaskControlBlockInner` 增加 `priority`；`new`/`fork` 初始化；新增 `TaskControlBlock::spawn` |
| `os/src/syscall/process.rs` | 实现 `sys_get_time`、`sys_mmap`、`sys_munmap`、`sys_spawn`、`sys_set_priority` |

差异点在 `sys_spawn`：ch5 通过 `loader::get_app_data_by_name` 从链接进内核的 `link_app.S` 里取 ELF，而 **ch6 没有 `loader` 模块**，程序一律以文件形式躺在 `fs.img` 中。因此 spawn 与 exec 一样走文件系统：

```rust
if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
    let all_data = app_inode.read_all();
    current_task().unwrap().spawn(all_data.as_slice())
} else {
    -1
}
```

另外 `TaskControlBlock::spawn` 在建 TCB 时必须自己填 `fd_table`（stdin/stdout/stderr），因为它不像 `fork` 那样从父进程复制：

```rust
fd_table: vec![
    Some(Arc::new(Stdin)),
    Some(Arc::new(Stdout)),
    Some(Arc::new(Stdout)),
],
```

## 九、运行结果

`make -C ci-user test CHAPTER=6`：

```
[PASS] found <Test file0 OK...>
[PASS] found <Test fstat OK...>
[PASS] found <Test link OK...>
[PASS] found <Test mass open/unlink OK...>
[PASS] found <Test spawn0 OK...>
[PASS] found <Test wait OK...>
[PASS] found <Test waitpid OK...>
... （ch2b ~ ch5b 的 27 项断言全部命中）
[PASS] not found <FAIL: T.T>
[PASS] not found <Test sbrk failed!>

Test passed: 31/31
Report for lab1 found.
Report for lab2 found.
Report for lab3 found.
Report for lab4 found.
```

最终 `ch6b_initproc` 驱动的 `ch6_usertest` 依次跑完 27 个子测例，末尾输出 `ch6 Usertests passed!`，随后 `All applications completed!` 正常收尾。

其中：

- `ch6_file1` 验证 `fstat(fd).nlink == 1` 且 `mode == StatMode::FILE`；
- `ch6_file2` 依次造出 4 条链接（`nlink` 从 1 → 2 → 4），删除原名后读 `lname0` 仍能读到原内容，`ino` 不变、`nlink` 降到 3，全部删除后回到 1；
- `ch6_file3` 做 10 轮「创建 → 写 50 次 → 关闭 → unlink → 再打开应失败」，用来压测 inode 与数据块的回收路径。

## 十、写路径与块缓存的设计取舍

`ch6_file3` 是整套测例中唯一的重 I/O 项：每轮写 50 次、每次约 2.7 KB，10 轮累计约 1.375 MB。在 QEMU 上每次块设备访问都要走一次 virtio 请求，成本远高于内存操作，因此这里的优化目标是**减少设备访问次数**，而不是减少 CPU 计算。据此有三处设计：

1. **整块覆盖时不读回旧内容**。若 `DiskInode::write_at` 对每个数据块都先 `get_block_cache`（触发一次读盘）再修改，当写入段恰好覆盖整块时这次读盘是多余的。为此在块缓存中加入 `BlockCache::new_uninit` 与 `get_block_cache_no_load`：命中缓存时复用原条目，未命中则直接给出置脏的空缓冲。连续写 2.7 KB 涉及的 6 个块中有 4–5 个属于整块覆盖。
2. **释放数据块只清位图**。`dealloc_data` 原本要读一次、把 512 字节清零、再写回；而被释放块的内容按其可见范围（由 `size` 决定）不会再次被读到，重新分配的块也一定先写后读。因此只清位图即可，一次 `unlink` 因此省去约 270 次块读写。
3. **批量写不在每块之后同步**。`Inode::write_at` 不再逐次调用 `block_cache_sync_all()`，脏块留到换出时或后续元数据操作（`create` / `link` / `unlink`）同步时统一写回，避免「每写一块就刷一次设备」的放大效应；同时把块缓存容量从 16 块提高到 128 块，使位图块与 inode 块常驻内存，不再反复读盘。

这三处都只改变块设备的访问次数，不改变文件系统的对外语义。

## 十一、代码改动清单

| 文件 | 改动 |
| --- | --- |
| `easy-fs/src/layout.rs` | `DiskInode` 增加 `nlink`，`initialize` 置 1；`write_at` 对整块覆盖走免读路径 |
| `easy-fs/src/efs.rs` | 新增 `dealloc_inode`；`root_inode` 传入 `inode_id = 0`；`dealloc_data` 只清位图 |
| `easy-fs/src/vfs.rs` | `Inode` 记住 `inode_id`；新增 `inode_id` / `is_dir` / `size` / `nlink` 访问器与 `link` / `unlink`；`write_at` 去掉逐次 `sync_all` |
| `easy-fs/src/block_cache.rs` | 新增 `new_uninit` / `get_block_cache_no_load`；缓存容量 16 → 128 |
| `easy-fs/src/lib.rs` | 导出 `get_block_cache_no_load` |
| `os/src/fs/mod.rs` | `File` trait 增加 `stat()` 默认实现；`Stat::empty()`；导出 `ROOT_INODE` |
| `os/src/fs/inode.rs` | `OSInode` 实现 `stat()`；`ROOT_INODE` 补文档注释 |
| `os/src/syscall/fs.rs` | 实现 `sys_fstat`、`sys_linkat`、`sys_unlinkat` |
| `os/src/mm/memory_set.rs` | 新增 `area_overlaps`、`unmap_area`（自 ch5 移植） |
| `os/src/task/task.rs` | `priority` 字段；`spawn`（含 `fd_table` 初始化）（自 ch5 移植） |
| `os/src/syscall/process.rs` | `sys_get_time` / `sys_mmap` / `sys_munmap` / `sys_spawn` / `sys_set_priority`（自 ch5 移植，`spawn` 改为读文件） |
| `reports/lab1.md`、`lab2.md`、`lab3.md` | 从 ch5 分支取回，报告按章节累积提交 |
| `reports/lab4.md` | 本文件 |
