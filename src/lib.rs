//! # RetroCell
//!
//! A concurrent data structure that allows lock-free reads and supports retroactive (historical) access.
//!
//! 一个允许无锁读取并支持回溯（历史）访问的并发数据结构。
//!
//! ## Features
//!
//! - **Retroactive Reading**: Readers can access the previous version during writes to avoid waiting.
//! - **Congestion Control**: Writers can detect congestion and choose to wait or force an update.
//!
//! ## 特性
//!
//! - **回溯读取**：读者可以在写入时读取先前版本以避免等待。
//! - **拥塞控制**：写入者可以检测拥塞并选择等待或强制更新。

mod reader;
mod rt;
mod shared;
mod sync;
mod utils;
mod writer;

// Re-export reader types
// 导出读取器类型
pub use reader::{BlockedReader, ReadResult, Reader, Ref};
// Re-export writer types
// 导出写入器类型
pub use writer::{CongestedWriter, InPlaceGuard, RetroCell, WriteOutcome};
