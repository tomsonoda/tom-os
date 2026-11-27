//! tom-os port: stand-ins for the parts of `std` this crate uses, built on
//! `core`, `alloc`, `hashbrown` and `libm`, for a kernel without an operating
//! system underneath. Paths that were `std::...` are `crate::nostd::...`.
#![allow(unsafe_code, missing_docs, clippy::all, unused_imports, dead_code)]

pub use alloc::fmt;
pub use core::{any, array, cell, char, cmp, convert, default, f64, hash, hint, iter, marker, mem, num, ops,
               option, ptr, result, slice, str};
pub use alloc::{borrow, boxed, rc, string, vec};

pub mod prelude {
    pub use alloc::borrow::ToOwned;
    pub use alloc::boxed::Box;
    pub use alloc::string::{String, ToString};
    pub use alloc::vec::Vec;
    pub use alloc::{format, vec};
    pub use super::F64Ext as _;
}

pub mod collections {
    pub use alloc::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
    pub type HashMap<K, V, S = hash_map::RandomState> = hashbrown::HashMap<K, V, S>;
    pub type HashSet<K, S = hash_map::RandomState> = hashbrown::HashSet<K, S>;
    pub mod hash_map {
        pub use hashbrown::hash_map::{Entry, IntoIter, Iter, IterMut, Keys, OccupiedEntry, VacantEntry, Values};
        /// Seeded once per boot from the host's clock, like std's per-process keys.
        #[derive(Clone, Debug)]
        pub struct RandomState(u64, u64);
        impl RandomState {
            pub fn new() -> RandomState {
                let k = super::super::host::entropy();
                RandomState(k, k.rotate_left(29) ^ 0x9E37_79B9_7F4A_7C15)
            }
        }
        impl Default for RandomState {
            fn default() -> RandomState {
                RandomState::new()
            }
        }
        impl core::hash::BuildHasher for RandomState {
            type Hasher = DefaultHasher;
            fn build_hasher(&self) -> DefaultHasher {
                DefaultHasher(self.0 ^ 0xcbf2_9ce4_8422_2325, self.1)
            }
        }
        /// A small keyed hasher (FNV-1a mixed with the key, finished with a multiply-xorshift).
        #[derive(Clone, Debug)]
        pub struct DefaultHasher(u64, u64);
        impl core::hash::Hasher for DefaultHasher {
            fn write(&mut self, bytes: &[u8]) {
                for &b in bytes {
                    self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
                }
            }
            fn write_u64(&mut self, v: u64) {
                self.0 = (self.0 ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(27);
            }
            fn write_usize(&mut self, v: usize) {
                self.write_u64(v as u64);
            }
            fn write_u32(&mut self, v: u32) {
                self.write_u64(v as u64);
            }
            fn finish(&self) -> u64 {
                let mut x = self.0 ^ self.1;
                x ^= x >> 32;
                x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
                x ^= x >> 32;
                x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
                x ^ (x >> 32)
            }
        }
    }
}

pub mod sync {
    pub use alloc::sync::{Arc, Weak};
    pub use core::sync::atomic;
    /// The kernel runs this code on one CPU without preemption, so a lock is
    /// only a borrow check.
    pub struct Mutex<T: ?Sized>(core::cell::RefCell<T>);
    unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
    unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
    pub type MutexGuard<'a, T> = core::cell::RefMut<'a, T>;
    #[derive(Debug)]
    pub struct PoisonError;
    pub type LockResult<G> = Result<G, PoisonError>;
    #[derive(Debug)]
    pub enum TryLockError {
        Poisoned(PoisonError),
        WouldBlock,
    }
    impl<T> Mutex<T> {
        pub const fn new(v: T) -> Mutex<T> {
            Mutex(core::cell::RefCell::new(v))
        }
        pub fn into_inner(self) -> LockResult<T> {
            Ok(self.0.into_inner())
        }
    }
    impl<T: ?Sized> Mutex<T> {
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
            Ok(self.0.borrow_mut())
        }
        pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
            self.0.try_borrow_mut().map_err(|_| TryLockError::WouldBlock)
        }
    }
}

pub mod time {
    pub use core::time::Duration;
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Instant(u64);
    impl Instant {
        pub fn now() -> Instant {
            Instant(super::host::millis())
        }
        pub fn elapsed(&self) -> Duration {
            Duration::from_millis(super::host::millis().saturating_sub(self.0))
        }
        pub fn duration_since(&self, earlier: Instant) -> Duration {
            Duration::from_millis(self.0.saturating_sub(earlier.0))
        }
    }
    impl core::ops::Add<Duration> for Instant {
        type Output = Instant;
        fn add(self, d: Duration) -> Instant {
            Instant(self.0 + d.as_millis() as u64)
        }
    }
    impl core::ops::Sub<Instant> for Instant {
        type Output = Duration;
        fn sub(self, o: Instant) -> Duration {
            self.duration_since(o)
        }
    }
    pub const UNIX_EPOCH: SystemTime = SystemTime(0);
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct SystemTime(u64);
    #[derive(Debug)]
    pub struct SystemTimeError;
    impl SystemTime {
        pub fn now() -> SystemTime {
            SystemTime(super::host::unix_seconds())
        }
        pub fn duration_since(&self, earlier: SystemTime) -> Result<Duration, SystemTimeError> {
            Ok(Duration::from_secs(self.0.saturating_sub(earlier.0)))
        }
    }
}

pub mod io {
    use alloc::vec::Vec;
    #[derive(Debug)]
    pub struct Error;
    pub type Result<T> = core::result::Result<T, Error>;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum SeekFrom {
        Start(u64),
        End(i64),
        Current(i64),
    }
    /// A byte sink, as `std::io::Write`.
    pub trait Write {
        fn write_all(&mut self, buf: &[u8]) -> Result<()>;
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            self.write_all(buf).map(|()| buf.len())
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
        fn write_fmt(&mut self, args: core::fmt::Arguments<'_>) -> Result<()> {
            struct Adapter<'a, W: ?Sized>(&'a mut W);
            impl<W: Write + ?Sized> core::fmt::Write for Adapter<'_, W> {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                    self.0.write_all(s.as_bytes()).map_err(|_| core::fmt::Error)
                }
            }
            core::fmt::write(&mut Adapter(self), args).map_err(|_| Error)
        }
    }
    impl Write for Vec<u8> {
        fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            self.extend_from_slice(buf);
            Ok(())
        }
    }
    impl<W: Write + ?Sized> Write for &mut W {
        fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            (**self).write_all(buf)
        }
    }
    impl Write for &mut [u8] {
        fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            if buf.len() > self.len() {
                return Err(Error);
            }
            let (a, b) = core::mem::take(self).split_at_mut(buf.len());
            a.copy_from_slice(buf);
            *self = b;
            Ok(())
        }
    }
}

pub mod panic {
    pub use core::panic::AssertUnwindSafe;
    /// Panics abort in the kernel, so there is nothing to catch.
    pub fn catch_unwind<F: FnOnce() -> R, R>(f: F) -> Result<R, alloc::boxed::Box<dyn core::any::Any + Send>> {
        Ok(f())
    }
    pub fn resume_unwind(_payload: alloc::boxed::Box<dyn core::any::Any + Send>) -> ! {
        panic!("resume_unwind")
    }
}

pub mod env {
    pub fn var_os(_name: &str) -> Option<alloc::string::String> {
        None
    }
}

pub mod error {
    pub use core::error::Error;
}

pub mod thread {
    /// `thread_local!` values: there is one thread of Lua execution at a time
    /// (the kernel's threads are cooperative), so a plain static serves.
    pub struct LocalKey<T: 'static>(pub core::cell::LazyCell<T, fn() -> T>);
    unsafe impl<T> Sync for LocalKey<T> {}
    impl<T: 'static> LocalKey<T> {
        pub fn with<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
            f(&self.0)
        }
    }
    impl<T: Copy + 'static> LocalKey<core::cell::Cell<T>> {
        pub fn get(&'static self) -> T {
            self.0.get()
        }
        pub fn set(&'static self, v: T) {
            self.0.set(v)
        }
    }
    impl<T: 'static> LocalKey<core::cell::Cell<T>> {
        pub fn replace(&'static self, v: T) -> T {
            self.0.replace(v)
        }
        pub fn take(&'static self) -> T where T: Default {
            self.0.take()
        }
    }
    impl<T: 'static> LocalKey<core::cell::RefCell<T>> {
        pub fn with_borrow<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
            f(&self.0.borrow())
        }
        pub fn with_borrow_mut<R>(&'static self, f: impl FnOnce(&mut T) -> R) -> R {
            f(&mut self.0.borrow_mut())
        }
        pub fn take(&'static self) -> T where T: Default {
            self.0.take()
        }
        pub fn set(&'static self, v: T) {
            *self.0.borrow_mut() = v;
        }
        pub fn replace(&'static self, v: T) -> T {
            self.0.replace(v)
        }
    }
}

#[macro_export]
#[doc(hidden)]
macro_rules! thread_local {
    () => {};
    ($(#[$a:meta])* $v:vis static $name:ident: $t:ty = const { $init:expr }; $($rest:tt)*) => {
        $(#[$a])* $v static $name: $crate::nostd::thread::LocalKey<$t> =
            $crate::nostd::thread::LocalKey(core::cell::LazyCell::new(|| $init));
        $crate::thread_local!($($rest)*);
    };
    ($(#[$a:meta])* $v:vis static $name:ident: $t:ty = const { $init:expr }) => {
        $crate::thread_local!($(#[$a])* $v static $name: $t = const { $init };);
    };
    ($(#[$a:meta])* $v:vis static $name:ident: $t:ty = $init:expr; $($rest:tt)*) => {
        $(#[$a])* $v static $name: $crate::nostd::thread::LocalKey<$t> =
            $crate::nostd::thread::LocalKey(core::cell::LazyCell::new(|| $init));
        $crate::thread_local!($($rest)*);
    };
    ($(#[$a:meta])* $v:vis static $name:ident: $t:ty = $init:expr) => {
        $crate::thread_local!($(#[$a])* $v static $name: $t = $init;);
    };
}

/// What the kernel provides (see rust/src/rt.rs in tom-os).
pub mod host {
    unsafe extern "C" {
        fn time_ms() -> u64;
        fn rtc_unix_time() -> u64;
        fn kernel_entropy() -> u64;
    }
    pub fn millis() -> u64 {
        unsafe { time_ms() }
    }
    pub fn unix_seconds() -> u64 {
        unsafe { rtc_unix_time() }
    }
    pub fn entropy() -> u64 {
        let t = unsafe { kernel_entropy() };
        t ^ (millis() << 32) ^ unix_seconds().rotate_left(17)
    }
}

/// `f64` methods that live in `std` because they need a math library.
pub trait F64Ext {
    fn floor(self) -> f64;
    fn ceil(self) -> f64;
    fn round(self) -> f64;
    fn trunc(self) -> f64;
    fn fract(self) -> f64;
    fn sqrt(self) -> f64;
    fn powf(self, n: f64) -> f64;
    fn powi(self, n: i32) -> f64;
    fn exp(self) -> f64;
    fn exp2(self) -> f64;
    fn ln(self) -> f64;
    fn log2(self) -> f64;
    fn log10(self) -> f64;
    fn log(self, base: f64) -> f64;
    fn sin(self) -> f64;
    fn cos(self) -> f64;
    fn tan(self) -> f64;
    fn asin(self) -> f64;
    fn acos(self) -> f64;
    fn atan(self) -> f64;
    fn atan2(self, x: f64) -> f64;
    fn sinh(self) -> f64;
    fn cosh(self) -> f64;
    fn tanh(self) -> f64;
    fn mul_add(self, a: f64, b: f64) -> f64;
    fn rem_euclid(self, rhs: f64) -> f64;
    fn hypot(self, o: f64) -> f64;
    fn cbrt(self) -> f64;
}
impl F64Ext for f64 {
    fn floor(self) -> f64 { libm::floor(self) }
    fn ceil(self) -> f64 { libm::ceil(self) }
    fn round(self) -> f64 { libm::round(self) }
    fn trunc(self) -> f64 { libm::trunc(self) }
    fn fract(self) -> f64 { self - libm::trunc(self) }
    fn sqrt(self) -> f64 { libm::sqrt(self) }
    fn powf(self, n: f64) -> f64 { libm::pow(self, n) }
    fn powi(self, n: i32) -> f64 { libm::pow(self, n as f64) }
    fn exp(self) -> f64 { libm::exp(self) }
    fn exp2(self) -> f64 { libm::exp2(self) }
    fn ln(self) -> f64 { libm::log(self) }
    fn log2(self) -> f64 { libm::log2(self) }
    fn log10(self) -> f64 { libm::log10(self) }
    fn log(self, base: f64) -> f64 { libm::log(self) / libm::log(base) }
    fn sin(self) -> f64 { libm::sin(self) }
    fn cos(self) -> f64 { libm::cos(self) }
    fn tan(self) -> f64 { libm::tan(self) }
    fn asin(self) -> f64 { libm::asin(self) }
    fn acos(self) -> f64 { libm::acos(self) }
    fn atan(self) -> f64 { libm::atan(self) }
    fn atan2(self, x: f64) -> f64 { libm::atan2(self, x) }
    fn sinh(self) -> f64 { libm::sinh(self) }
    fn cosh(self) -> f64 { libm::cosh(self) }
    fn tanh(self) -> f64 { libm::tanh(self) }
    fn mul_add(self, a: f64, b: f64) -> f64 { self * a + b }
    fn rem_euclid(self, rhs: f64) -> f64 { let r = libm::fmod(self, rhs); if r < 0.0 { r + rhs.abs() } else { r } }
    fn hypot(self, o: f64) -> f64 { libm::hypot(self, o) }
    fn cbrt(self) -> f64 { libm::cbrt(self) }
}
