# RetroCell

A concurrent data structure that allows lock-free reads and supports retroactive (historical) access.

[![Crates.io](https://img.shields.io/crates/v/retro-cell.svg)](https://crates.io/crates/retro-cell)
[![Docs.rs](https://docs.rs/retro-cell/badge.svg)](https://docs.rs/retro-cell)
[![License](https://img.shields.io/crates/l/smr-swap)](LICENSE-MIT)

[中文文档](./README_CN.md)

## Features

- **Retroactive Reading**: Readers can access the previous version of the data while a writer is updating it, avoiding blocks.
- **Congestion Control**: Writers can detect congestion (active readers) and choose between waiting for readers to drain (In-Place update) or performing a Copy-On-Write (COW) update.
- **Lock-Free Reads**: `try_read` allows non-blocking attempts to access data.
- **Blocking Reads**: `read` ensures the latest data is accessed, blocking if necessary.

## SWMR (Single-Writer Multi-Reader)

`RetroCell` is designed as a **Single-Writer Multi-Reader (SWMR)** data structure. 

- If you need **Multiple Writers**, you must wrap the `RetroCell` (or the writer handle) in a synchronization primitive like `Arc<Mutex<RetroCell<T>>>`.
- The readers (`Reader<T>`) are `Clone` and `Send`/`Sync`, so they can be freely shared across threads.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
retro-cell = "0.1"
```

## Usage

### Basic Example

```rust
use retro_cell::RetroCell;
use std::thread;

fn main() {
    let (mut cell, reader) = RetroCell::new(0);

    // Spawn a reader thread
    let r = reader.clone();
    thread::spawn(move || {
        loop {
            let val = r.read();
            println!("Read value: {}", *val);
            if *val >= 5 { break; }
        }
    });

    // Update values
    for i in 1..=5 {
        match cell.try_write() {
            retro_cell::WriteOutcome::InPlace(mut guard) => {
                *guard = i;
            }
            retro_cell::WriteOutcome::Congested(writer) => {
                // If congested, use Copy-On-Write to avoid blocking readers
                writer.perform_cow(|v| *v = i);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
```

### Retroactive Reading

When a writer is updating the data, readers might get blocked. Instead of waiting, they can choose to read the "previous" version of the data if available.

```rust
use retro_cell::{RetroCell, ReadResult};

let (mut cell, reader) = RetroCell::new(10);

// Simulate a long write
let mut guard = cell.write_in_place();
*guard = 20;

// Try to read
match reader.try_read() {
    ReadResult::Success(val) => println!("Current: {}", *val),
    ReadResult::Blocked(blocked) => {
        if let Some(old_val) = blocked.read_retro() {
             println!("Blocked, but found old version: {}", *old_val);
        } else {
             println!("Blocked, and no old version available.");
             // Optionally wait for the new version
             let new_val = blocked.wait();
             println!("Waited for new version: {}", *new_val);
        }
    }
}
```

## Performance

Benchmarks run on an Windows (Intel Core i9-13900KS).

### Comparison vs ArcSwap (Read-Heavy / COW)
*Testing Non-Blocking Write (COW) + Non-Blocking Read*

| Scenario | RetroCell (ns) | ArcSwap (ns) | Speedup |
|----------|---------------:|-------------:|:-------:|
| 1W / 1R  |             88 |          558 |  ~6.3x  |
| 1W / 4R  |            168 |          558 |  ~3.3x  |
| 2W / 2R  |            240 |        1,043 |  ~4.3x  |
| 4W / 4R  |            574 |        2,046 |  ~3.5x  |

> **Result**: RetroCell is significantly faster than `ArcSwap` for RCU-like workloads, especially under contention.

### Comparison vs RwLock (Blocking)
*Testing Blocking Write (In-Place) + Blocking Read*

| Scenario | RetroCell (ns) | RwLock (ns) | Speedup |
|----------|---------------:|------------:|:-------:|
| 1W / 1R  |            104 |          53 |  ~0.5x  |
| 1W / 4R  |            305 |         187 |  ~0.6x  |
| 2W / 2R  |            259 |         213 |  ~0.8x  |
| 4W / 4R  |            582 |         523 |  ~0.9x  |

> **Result**: For purely blocking, in-place updates, standard `RwLock` is faster due to simpler logic. RetroCell's strength lies in its hybrid capabilities (falling back to COW to avoid blocking).

## License

This project is licensed under either of

 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option.
