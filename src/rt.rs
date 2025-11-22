#[cfg(not(feature = "loom"))]
pub(crate) use std::hint;
#[cfg(not(feature = "loom"))]
pub(crate) use std::sync;
#[cfg(not(feature = "loom"))]
pub(crate) use std::thread;

#[cfg(feature = "loom")]
pub(crate) use loom::hint;
#[cfg(feature = "loom")]
pub(crate) use loom::sync;
#[cfg(feature = "loom")]
pub(crate) use loom::thread;

#[cfg(not(feature = "loom"))]
#[inline(always)]
pub(crate) fn wait(atomic: &sync::atomic::AtomicU32, expected: u32) {
    atomic_wait::wait(atomic, expected);
}

#[cfg(not(feature = "loom"))]
#[inline(always)]
pub(crate) fn wake_one(atomic: &sync::atomic::AtomicU32) {
    atomic_wait::wake_one(atomic);
}

#[cfg(not(feature = "loom"))]
#[inline(always)]
pub(crate) fn wake_all(atomic: &sync::atomic::AtomicU32) {
    atomic_wait::wake_all(atomic);
}

#[cfg(feature = "loom")]
#[inline(always)]
pub(crate) fn wait(_atomic: &sync::atomic::AtomicU32, _expected: u32) {
    crate::rt::thread::yield_now();
}

#[cfg(feature = "loom")]
#[inline(always)]
pub(crate) fn wake_one(_atomic: &sync::atomic::AtomicU32) {}

#[cfg(feature = "loom")]
#[inline(always)]
pub(crate) fn wake_all(_atomic: &sync::atomic::AtomicU32) {}
