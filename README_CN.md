# RetroCell

一个允许无锁读取并支持回溯（历史）访问的并发数据结构。

[![Crates.io](https://img.shields.io/crates/v/retro-cell.svg)](https://crates.io/crates/retro-cell)
[![Docs.rs](https://docs.rs/retro-cell/badge.svg)](https://docs.rs/retro-cell)
[![License](https://img.shields.io/crates/l/retro-cell)](LICENSE-MIT)

## 特性

- **回溯读取**：读者可以在写入者更新数据时访问数据的上一版本，从而避免阻塞。
- **拥塞控制**：写入者可以检测拥塞（即有活跃的读者），并选择等待读者排空（原地更新）或执行写时复制（COW）更新。
- **无锁读取**：`try_read` 允许非阻塞地尝试访问数据。
- **阻塞读取**：`read` 确保访问到最新数据，必要时会进行阻塞。

## SWMR (单写多读)

`RetroCell` 被设计为 **单写多读 (Single-Writer Multi-Reader, SWMR)** 数据结构。

- 如果你需要 **多写入者 (Multiple Writers)**，必须使用同步原语（如 `Arc<Mutex<RetroCell<T>>>`）来包装 `RetroCell`（或写入句柄）。
- 读取者 (`Reader<T>`) 实现了 `Clone` 和 `Send`/`Sync`，因此可以在线程间自由共享。

## 安装

在 `Cargo.toml` 中添加：

```toml
[dependencies]
retro-cell = "0.1"
```

## 使用指南

### 基本示例

```rust
use retro_cell::RetroCell;
use std::thread;

fn main() {
    let (mut cell, reader) = RetroCell::new(0);

    // 启动一个读者线程
    let r = reader.clone();
    thread::spawn(move || {
        loop {
            let val = r.read();
            println!("读取到的值: {}", *val);
            if *val >= 5 { break; }
        }
    });

    // 更新值
    for i in 1..=5 {
        match cell.try_write() {
            retro_cell::WriteOutcome::InPlace(mut guard) => {
                *guard = i;
            }
            retro_cell::WriteOutcome::Congested(writer) => {
                // 如果拥塞，使用写时复制（COW）以避免阻塞读者
                writer.perform_cow(|v| *v = i);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
```

### 回溯读取

当写入者正在更新数据时，读者可能会被阻塞。读者可以选择读取“先前”版本的数据（如果可用），而不是等待。

```rust
use retro_cell::{RetroCell, ReadResult};

let (mut cell, reader) = RetroCell::new(10);

// 模拟长时间写入
let mut guard = cell.write_in_place();
*guard = 20;

// 尝试读取
match reader.try_read() {
    ReadResult::Success(val) => println!("当前值: {}", *val),
    ReadResult::Blocked(blocked) => {
        if let Some(old_val) = blocked.read_retro() {
             println!("被阻塞，但发现了旧版本: {}", *old_val);
        } else {
             println!("被阻塞，且无旧版本可用。");
             // 选择等待新版本
             let new_val = blocked.wait();
             println!("等待并获取了新版本: {}", *new_val);
        }
    }
}
```

## 性能表现

基准测试运行于 Windows 平台 (Intel Core i9-13900KS)。

### 对比 ArcSwap (读多写少 / COW)
*测试场景：非阻塞写入 (COW) + 非阻塞读取*

| 场景 (写/读) | RetroCell (ns) | ArcSwap (ns) | 提升倍数 |
|--------------|---------------:|-------------:|:--------:|
| 1W / 1R      |             88 |          558 |   ~6.3x  |
| 1W / 4R      |            168 |          558 |   ~3.3x  |
| 2W / 2R      |            240 |        1,043 |   ~4.3x  |
| 4W / 4R      |            574 |        2,046 |   ~3.5x  |

> **结论**: 在类似 RCU 的工作负载下，`RetroCell` 显著快于 `ArcSwap`，尤其是在高并发竞争下。

### 对比 RwLock (阻塞模式)
*测试场景：阻塞写入 (原地更新) + 阻塞读取*

| 场景 (写/读) | RetroCell (ns) | RwLock (ns) | 提升倍数 |
|--------------|---------------:|------------:|:--------:|
| 1W / 1R      |            104 |          53 |   ~0.5x  |
| 1W / 4R      |            305 |         187 |   ~0.6x  |
| 2W / 2R      |            259 |         213 |   ~0.8x  |
| 4W / 4R      |            582 |         523 |   ~0.9x  |

> **结论**: 对于纯粹的阻塞式原地更新，标准 `RwLock` 由于逻辑更简单而更快。`RetroCell` 的优势在于其混合能力（能够在拥塞时退回到 COW 模式以避免阻塞读者）。

## 许可证

本项目采用以下任一许可证进行许可：

 * MIT license ([LICENSE-MIT](LICENSE-MIT) 或 http://opensource.org/licenses/MIT)
 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) 或 http://www.apache.org/licenses/LICENSE-2.0)

由您选择。
