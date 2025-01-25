//! Time.

#![allow(unsafe_code)]

use futures::{FutureExt, Stream};
use std::{
    fmt,
    future::{Future, IntoFuture},
    pin::Pin,
    task::{ready, Context, Poll},
    time::Duration,
};

/// JavaScript sleep wrapper.
mod js {
    use js_sys::Function;
    use std::{
        cell::RefCell,
        fmt,
        future::Future,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll, Waker},
        time::Duration,
    };
    use wasm_bindgen::{prelude::*, JsCast};
    use web_sys::{Window, WorkerGlobalScope};

    /// JavaScript sleep.
    ///
    /// This is not Send + Sync since the underlying callback is bound to a
    /// JavaScript thread.
    pub struct JsSleep {
        inner: Rc<RefCell<JsSleepInner>>,
        timeout_id: i32,
        _callback: Closure<dyn FnMut()>,
    }

    // Implement Send + Sync for target without threads.
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    unsafe impl Send for JsSleep {}
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    unsafe impl Sync for JsSleep {}

    impl fmt::Debug for JsSleep {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.debug_struct("Sleep").field("timeout_id", &self.timeout_id).finish()
        }
    }

    #[derive(Default)]
    struct JsSleepInner {
        fired: bool,
        waker: Option<Waker>,
    }

    impl JsSleep {
        pub(super) fn new(duration: Duration) -> Self {
            let inner = Rc::new(RefCell::new(JsSleepInner::default()));

            let callback = {
                let inner = inner.clone();
                Closure::new(move || {
                    let mut inner = inner.borrow_mut();
                    inner.fired = true;
                    if let Some(waker) = inner.waker.take() {
                        waker.wake();
                    }
                })
            };

            let timeout = duration.as_millis().try_into().expect("sleep duration overflow");
            let timeout_id = Self::register_timeout(callback.as_ref().unchecked_ref(), timeout);

            Self { inner, timeout_id, _callback: callback }
        }

        fn register_timeout(handler: &Function, timeout: i32) -> i32 {
            let global = js_sys::global();

            if let Some(window) = global.dyn_ref::<Window>() {
                window.set_timeout_with_callback_and_timeout_and_arguments_0(handler, timeout).unwrap()
            } else if let Some(worker) = global.dyn_ref::<WorkerGlobalScope>() {
                worker.set_timeout_with_callback_and_timeout_and_arguments_0(handler, timeout).unwrap()
            } else {
                panic!("unsupported JavaScript global: {global:?}");
            }
        }

        fn unregister_timeout(id: i32) {
            let global = js_sys::global();

            if let Some(window) = global.dyn_ref::<Window>() {
                window.clear_timeout_with_handle(id);
            } else if let Some(worker) = global.dyn_ref::<WorkerGlobalScope>() {
                worker.clear_timeout_with_handle(id);
            } else {
                panic!("unsupported JavaScript global: {global:?}");
            }
        }
    }

    impl Future for JsSleep {
        type Output = ();

        /// Waits until the sleep duration has elapsed.
        fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            let mut inner = self.inner.borrow_mut();

            if inner.fired {
                return Poll::Ready(());
            }

            inner.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    impl Drop for JsSleep {
        fn drop(&mut self) {
            let inner = self.inner.borrow_mut();
            if !inner.fired {
                Self::unregister_timeout(self.timeout_id);
            }
        }
    }
}

/// Future for [`sleep`].
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
pub use js::JsSleep as Sleep;

/// Thread-safe sleep.
#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
mod threads {
    use futures::{ready, FutureExt};
    use std::{
        fmt,
        future::Future,
        pin::Pin,
        sync::LazyLock,
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::sync::{mpsc, oneshot};
    use wasm_bindgen_futures::spawn_thread;

    use super::js::JsSleep;

    struct SleepReq {
        duration: Duration,
        wake_tx: oneshot::Sender<()>,
    }

    static SLEEP_TX: LazyLock<mpsc::UnboundedSender<SleepReq>> = LazyLock::new(|| {
        let (sleep_tx, mut sleep_rx) = mpsc::unbounded_channel::<SleepReq>();
        spawn_thread(move || async move {
            while let Some(SleepReq { duration, mut wake_tx }) = sleep_rx.recv().await {
                wasm_bindgen_futures::spawn_local(async move {
                    tokio::select! {
                        () = JsSleep::new(duration) => {
                            let _ = wake_tx.send(());
                        },
                        () = wake_tx.closed() => (),
                    }
                });
            }
        });
        sleep_tx
    });

    /// Thread-safe sleep wrapper.
    pub struct Sleep(oneshot::Receiver<()>);

    impl fmt::Debug for Sleep {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.debug_tuple("Sleep").field(&self.0).finish()
        }
    }

    impl Sleep {
        pub(super) fn new(duration: Duration) -> Self {
            let (wake_tx, wake_rx) = oneshot::channel();
            SLEEP_TX.send(SleepReq { duration, wake_tx }).expect("sleep thread failed");
            Self(wake_rx)
        }
    }

    impl Future for Sleep {
        type Output = ();

        /// Waits until the sleep duration has elapsed.
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            ready!(self.0.poll_unpin(cx)).expect("sleep thread failed");
            Poll::Ready(())
        }
    }

    impl Drop for Sleep {
        fn drop(&mut self) {
            // empty
        }
    }
}

/// Future for [`sleep`].
#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
pub use threads::Sleep;

/// Waits until `duration` has elapsed.
pub fn sleep(duration: Duration) -> Sleep {
    Sleep::new(duration)
}

#[cfg(all(target_family = "wasm", target_os = "wasi"))]
pub use tokio::time::Instant;

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
mod js_instant {
    use std::{
        ops::{Add, Sub},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };
    use wasm_bindgen::JsCast;
    use web_sys::{Window, WorkerGlobalScope};

    /// A measurement of a monotonically nondecreasing clock.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(u64);

    impl Instant {
        /// Returns an instant corresponding to “now”.
        pub fn now() -> Self {
            let global = js_sys::global();

            let now = if let Some(window) = global.dyn_ref::<Window>() {
                let performance = window.performance().unwrap();
                performance.now() + performance.time_origin()
            } else if let Some(worker) = global.dyn_ref::<WorkerGlobalScope>() {
                let performance = worker.performance().unwrap();
                performance.now() + performance.time_origin()
            } else {
                panic!("unsupported JavaScript global: {global:?}");
            } as u64;

            static START: AtomicU64 = AtomicU64::new(0);
            let start = match START.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => now,
                Err(start) => start,
            };
            Self(now - start)
        }

        /// Returns the amount of time elapsed since this instant was created,
        /// or zero duration if this instant is in the future.
        pub fn elapsed(&self) -> Duration {
            Instant::now() - *self
        }

        /// Adds the duration to this instant, if the resulting value is representable.
        pub fn checked_add(&self, duration: Duration) -> Option<Self> {
            let duration_millis = u64::try_from(duration.as_millis()).ok()?;
            let millis = self.0.checked_add(duration_millis)?;
            Some(Self(millis))
        }
    }

    impl Add<Duration> for Instant {
        type Output = Instant;

        fn add(self, rhs: Duration) -> Self::Output {
            Self(self.0 + rhs.as_millis() as u64)
        }
    }

    impl Sub for Instant {
        type Output = Duration;

        fn sub(self, rhs: Self) -> Self::Output {
            if self > rhs {
                Duration::from_millis(self.0 - rhs.0)
            } else {
                Duration::ZERO
            }
        }
    }
}

#[cfg(all(target_family = "wasm", not(target_os = "wasi")))]
pub use js_instant::Instant;

/// Waits until `deadline` is reached.
pub fn sleep_until(deadline: Instant) -> Sleep {
    let now = Instant::now();
    if deadline > now {
        sleep(deadline - now)
    } else {
        sleep(Duration::ZERO)
    }
}

/// A stream that produces an event at a fixed time interval.
#[derive(Debug)]
pub struct IntervalStream {
    period: Duration,
    sleep: Sleep,
}

impl Stream for IntervalStream {
    type Item = Instant;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        ready!(self.as_mut().sleep.poll_unpin(cx));
        self.sleep = sleep(self.period);
        Poll::Ready(Some(Instant::now()))
    }
}

/// Creates a stream that produces an event at the specified time interval.
pub fn interval_stream(period: Duration) -> IntervalStream {
    IntervalStream { period, sleep: sleep(Duration::ZERO) }
}

pub mod error {
    use super::*;

    /// Timeout elapsed.
    #[derive(Debug, Clone)]
    pub struct Elapsed {}

    impl fmt::Display for Elapsed {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "timeout elapsed")
        }
    }

    impl std::error::Error for Elapsed {}

    impl From<Elapsed> for std::io::Error {
        fn from(_err: Elapsed) -> Self {
            std::io::ErrorKind::TimedOut.into()
        }
    }
}

/// Future returned by [`timeout`].
pub struct Timeout<F> {
    sleep: Sleep,
    future: F,
}

impl<F> fmt::Debug for Timeout<F> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Timeout").finish()
    }
}

impl<F> Future for Timeout<F>
where
    F: Future,
{
    type Output = Result<F::Output, error::Elapsed>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let future = unsafe { self.as_mut().map_unchecked_mut(|this| &mut this.future) };
        if let Poll::Ready(res) = future.poll(cx) {
            return Poll::Ready(Ok(res));
        }

        let sleep = unsafe { self.as_mut().map_unchecked_mut(|this| &mut this.sleep) };
        if let Poll::Ready(()) = sleep.poll(cx) {
            return Poll::Ready(Err(error::Elapsed {}));
        }

        Poll::Pending
    }
}

/// Requires a `Future` to complete before the specified duration has elapsed.
pub fn timeout<F>(duration: Duration, future: F) -> Timeout<F::IntoFuture>
where
    F: IntoFuture,
{
    Timeout { sleep: sleep(duration), future: future.into_future() }
}
