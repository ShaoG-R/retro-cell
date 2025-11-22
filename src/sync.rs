use crate::rt::hint::spin_loop;
use crate::rt::sync::atomic::{AtomicU32, Ordering};

/// === RefCount ===
/// Reference counting with writer waiting support.
/// Optimization: High bit marks waiting Writer to avoid unnecessary wakeups.
///
/// === RefCount ===
/// 支持写入等待的引用计数。
/// 优化：高位标记等待的 Writer 以避免不必要的唤醒。
#[derive(Debug)]
pub(crate) struct RefCount {
    // Bits 0-30: Reference count
    // Bits 0-30: 引用计数

    // Bit 31: WAITING flag (indicates a Writer is waiting in wait_until_zero)
    // Bit 31: WAITING 标记 (表示有 Writer 正在 wait_until_zero)
    state: AtomicU32,
}

const WAITING_BIT: u32 = 1 << 31;
const COUNT_MASK: u32 = !WAITING_BIT;

impl RefCount {
    #[inline(always)]
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    pub(crate) fn retain(&self) {
        // Increment count only, preserving the WAITING bit
        // 仅增加计数，保留 WAITING 位
        self.state.fetch_add(1, Ordering::Acquire);
    }

    #[inline(always)]
    pub(crate) fn release(&self) {
        let prev = self.state.fetch_sub(1, Ordering::Release);

        // If this was the last reader and a writer is waiting, wake it up
        // 若这是最后一个读者且有 Writer 在等待，则唤醒它
        if prev == (1 | WAITING_BIT) {
            self.wake();
        }
    }

    // Writer only: wait for all readers to exit
    // 仅供 Writer 使用：等待所有读者退出
    #[inline(never)]
    pub(crate) fn wait_until_zero(&self) {
        let mut spin_count = 0;
        loop {
            let val = self.state.load(Ordering::Acquire);
            // Fast path: no readers
            // 快速路径：无读者
            if (val & COUNT_MASK) == 0 {
                return;
            }

            // Set WAITING bit if not already set
            // 若未设置 WAITING 位，则尝试设置
            if (val & WAITING_BIT) == 0 {
                // Try CAS: val -> val | WAITING_BIT
                // 尝试 CAS: val -> val | WAITING_BIT
                if self
                    .state
                    .compare_exchange_weak(
                        val,
                        val | WAITING_BIT,
                        Ordering::Relaxed, // CAS failure is fine, just retry // CAS 失败无妨，重试即可
                        Ordering::Relaxed,
                    )
                    .is_err()
                {
                    continue;
                }
            }

            // Re-check in case readers exited while setting the bit
            // 二次检查，防止设置位时读者已退出
            let val_now = self.state.load(Ordering::Acquire);
            if (val_now & COUNT_MASK) == 0 {
                return;
            }

            // Spin briefly before sleeping
            // 睡眠前短暂自旋
            if spin_count < 20 {
                spin_loop();
                spin_count += 1;
                continue;
            }

            // Sleep and wait for wakeup
            // 睡眠等待唤醒
            crate::rt::wait(&self.state, val_now | WAITING_BIT);
        }
    }

    // Reset state for node reuse
    // 重置状态以复用节点
    #[inline(always)]
    pub(crate) fn reset(&self) {
        self.state.store(0, Ordering::Relaxed);
    }

    #[inline(always)]
    fn wake(&self) {
        // Wake the single waiting writer
        // 唤醒唯一的等待写入者
        crate::rt::wake_one(&self.state);
    }

    #[inline(always)]
    pub(crate) fn count(&self) -> u32 {
        self.state.load(Ordering::Acquire) & COUNT_MASK
    }
}

/// === Ticket Notifier ===
/// Ticket-based notifier for global lock waiting.
///
/// === Ticket Notifier ===
/// 用于全局锁等待的票据通知器。
#[derive(Debug)]
pub(crate) struct Notifier {
    inner: AtomicU32,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            inner: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    pub fn ticket(&self) -> u32 {
        self.inner.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn wait_ticket(&self, expected: u32) {
        crate::rt::wait(&self.inner, expected);
    }

    #[inline(always)]
    pub fn advance_and_wake(&self) {
        // Release ordering ensures memory visibility to woken threads
        // Release 序确保内存修改对唤醒线程可见
        self.inner.fetch_add(1, Ordering::Release);
        self.wake_all();
    }

    #[inline(always)]
    fn wake_all(&self) {
        crate::rt::wake_all(&self.inner);
    }
}
