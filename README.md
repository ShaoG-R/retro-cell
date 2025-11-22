# RetroCell

A concurrent data structure that allows lock-free reads and supports retroactive (historical) access.

[![Crates.io](https://img.shields.io/crates/v/retro-cell.svg)](https://crates.io/crates/retro-cell)
[![Docs.rs](https://docs.rs/retro-cell/badge.svg)](https://docs.rs/retro-cell)
[![License](https://img.shields.io/crates/l/smr-swap)](LICENSE-MIT)

## Features

- **Retroactive Reading**: Readers can access the previous version of the data while a writer is updating it, avoiding blocks.
- **Congestion Control**: Writers can detect congestion (active readers) and choose between waiting for readers to drain (In-Place update) or performing a Copy-On-Write (COW) update.
- **Lock-Free Reads**: `try_read` allows non-blocking attempts to access data.
- **Blocking Reads**: `read` ensures the latest data is accessed, blocking if necessary.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
retro-cell = "0.1.0"
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

## License

This project is licensed under either of

 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option.
