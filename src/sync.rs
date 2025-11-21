use atomic_wait::{wait, wake_all, wake_one};
use std::hint::spin_loop;
use std::sync::atomic::{AtomicU32, Ordering};

// === RefCount (新实现: 针对节点生命周期的专用轻量级通知器) ===
// 优化点：利用高位标记是否有 Writer 在等待，避免 Reader 无意义的 wake 系统调用。
#[derive(Debug)]
pub(crate) struct RefCount {
    // Bits 0-30: 引用计数
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
        // 仅增加计数，不触碰 Waiting 位
        self.state.fetch_add(1, Ordering::Acquire);
    }

    #[inline(always)]
    pub(crate) fn release(&self) {
        // 优化关键：fetch_sub 返回旧值
        let prev = self.state.fetch_sub(1, Ordering::Release);
        // 如果旧值是 (1 | WAITING_BIT)，说明这是最后一个 Reader，且有 Writer 在等
        if prev == (1 | WAITING_BIT) {
            self.wake();
        }
    }

    // 仅供 Writer 使用，等待读者归零
    #[inline(never)] // 冷路径，防止内联膨胀
    pub(crate) fn wait_until_zero(&self) {
        let mut spin_count = 0;
        loop {
            let val = self.state.load(Ordering::Acquire);
            // 没有任何读者，直接返回
            if (val & COUNT_MASK) == 0 {
                return;
            }

            // 如果没有设置 Waiting 位，尝试设置它
            if (val & WAITING_BIT) == 0 {
                // 尝试 CAS: val -> val | WAITING_BIT
                if self
                    .state
                    .compare_exchange_weak(
                        val,
                        val | WAITING_BIT,
                        Ordering::Relaxed, // CAS 失败重试即可，无需强顺序
                        Ordering::Relaxed,
                    )
                    .is_err()
                {
                    continue; // CAS 失败，重读状态
                }
            }

            // 再次检查（防止在设置位之后 reader 刚好走光）
            let val_now = self.state.load(Ordering::Acquire);
            if (val_now & COUNT_MASK) == 0 {
                return;
            }

            // 自旋优化：先 spin 几次，不要立即 syscall
            if spin_count < 20 {
                spin_loop();
                spin_count += 1;
                continue;
            }

            // 确实需要睡眠，等待唤醒
            wait(&self.state, val_now | WAITING_BIT);
        }
    }

    // 重置状态（当节点被回收复用时）
    #[inline(always)]
    pub(crate) fn reset(&self) {
        self.state.store(0, Ordering::Relaxed);
    }

    #[inline(always)]
    fn wake(&self) {
        let ptr = &self.state as *const AtomicU32;
        // 只需要唤醒一个等待者（唯一的 Writer）
        wake_one(ptr);
    }

    #[inline(always)]
    pub(crate) fn count(&self) -> u32 {
        self.state.load(Ordering::Acquire) & COUNT_MASK
    }
}

// === Ticket Notifier (保持原有逻辑，用于全局锁等待) ===
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
        wait(&self.inner, expected);
    }

    #[inline(always)]
    pub fn advance_and_wake(&self) {
        // 使用 Release 确保之前的内存修改对醒来的线程可见
        self.inner.fetch_add(1, Ordering::Release);
        self.wake_all();
    }

    #[inline(always)]
    fn wake_all(&self) {
        let ptr = &self.inner as *const AtomicU32;
        wake_all(ptr);
    }
}
