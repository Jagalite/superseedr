// SPDX-License-Identifier: GPL-3.0-or-later
use std::future::Future;
use tokio::task::JoinHandle;

pub(crate) use tokio::task::spawn_local as spawn;

/// The browser host must continuously drive a LocalSet until its children drain.
pub(crate) struct JoinSet<T>(tokio::task::JoinSet<T>);
impl<T: 'static> JoinSet<T> {
    pub(crate) fn new() -> Self {
        Self(tokio::task::JoinSet::new())
    }
    pub(crate) fn spawn<F>(&mut self, future: F) -> tokio::task::AbortHandle
    where
        F: Future<Output = T> + 'static,
    {
        self.0.spawn_local(future)
    }
}
impl<T> std::ops::Deref for JoinSet<T> {
    type Target = tokio::task::JoinSet<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T> std::ops::DerefMut for JoinSet<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Execute a bounded piece/hash job on the application worker, yielding to the
/// browser before starting. As on native, cancellation cannot interrupt a running
/// synchronous hash; callers retain their existing stale-result checks.
pub(crate) fn spawn_compute<F, T>(work: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + 'static,
    T: 'static,
{
    spawn(async move {
        time::sleep(std::time::Duration::ZERO).await;
        work()
    })
}

pub(crate) mod time {
    pub(crate) use std::time::Duration;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::sync::oneshot;
    use wasm_bindgen::{closure::Closure, prelude::*};
    pub(crate) use web_time::Instant;

    #[wasm_bindgen(
        inline_js = "export function startTimer(callback, ms) { return setTimeout(callback, ms); } export function cancelTimer(id) { clearTimeout(id); }"
    )]
    extern "C" {
        #[wasm_bindgen(js_name = startTimer)]
        fn start_timer(callback: &js_sys::Function, ms: f64) -> i32;
        #[wasm_bindgen(js_name = cancelTimer)]
        fn cancel_timer(id: i32);
    }
    pub(crate) struct Sleep {
        deadline: Instant,
        timer: Option<(i32, Closure<dyn FnMut()>, oneshot::Receiver<()>)>,
    }
    pub(crate) fn sleep(duration: Duration) -> Sleep {
        sleep_until(Instant::now() + duration)
    }
    pub(crate) fn sleep_until(deadline: Instant) -> Sleep {
        Sleep {
            deadline,
            timer: None,
        }
    }
    impl Sleep {
        fn cancel(&mut self) {
            if let Some((id, _, _)) = self.timer.take() {
                cancel_timer(id);
            }
        }
    }
    impl Drop for Sleep {
        fn drop(&mut self) {
            self.cancel();
        }
    }
    impl Future for Sleep {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            // Even a zero-duration sleep yields to the browser event loop.
            loop {
                if self.timer.is_none() {
                    let (tx, rx) = oneshot::channel();
                    let mut tx = Some(tx);
                    let callback = Closure::wrap(Box::new(move || {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(());
                        }
                    }) as Box<dyn FnMut()>);
                    let ms = self
                        .deadline
                        .saturating_duration_since(Instant::now())
                        .as_secs_f64()
                        * 1000.0;
                    let id = start_timer(
                        callback.as_ref().unchecked_ref(),
                        ms.ceil().min(i32::MAX as f64),
                    );
                    self.timer = Some((id, callback, rx));
                }
                let (_, _, rx) = self.timer.as_mut().unwrap();
                if Pin::new(rx).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                self.cancel();
                if Instant::now() >= self.deadline {
                    return Poll::Ready(());
                }
            }
        }
    }
    #[derive(Debug)]
    pub(crate) struct Elapsed;
    impl std::fmt::Display for Elapsed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }
    impl std::error::Error for Elapsed {}
    pub(crate) async fn timeout<F: Future>(
        duration: Duration,
        future: F,
    ) -> Result<F::Output, Elapsed> {
        tokio::select! { biased; output = future => Ok(output), _ = sleep(duration) => Err(Elapsed) }
    }
    #[derive(Clone, Copy)]
    pub(crate) enum MissedTickBehavior {
        Burst,
        Delay,
        Skip,
    }
    pub(crate) struct Interval {
        next: Instant,
        period: Duration,
        behavior: MissedTickBehavior,
    }
    pub(crate) fn interval(period: Duration) -> Interval {
        assert!(!period.is_zero(), "interval period must be nonzero");
        Interval {
            next: Instant::now(),
            period,
            behavior: MissedTickBehavior::Burst,
        }
    }
    impl Interval {
        pub(crate) fn reset(&mut self) {
            self.next = Instant::now() + self.period;
        }
        pub(crate) fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
            self.behavior = behavior;
        }
        pub(crate) async fn tick(&mut self) -> Instant {
            let scheduled = self.next;
            if scheduled > Instant::now() {
                sleep_until(scheduled).await;
            }
            let now = Instant::now();
            self.next = if now.saturating_duration_since(scheduled) > Duration::from_millis(5) {
                match self.behavior {
                    MissedTickBehavior::Burst => scheduled + self.period,
                    MissedTickBehavior::Delay => now + self.period,
                    MissedTickBehavior::Skip => {
                        let remainder =
                            now.duration_since(scheduled).as_nanos() % self.period.as_nanos();
                        now + (self.period
                            - Duration::new(
                                (remainder / 1_000_000_000) as u64,
                                (remainder % 1_000_000_000) as u32,
                            ))
                    }
                }
            } else {
                scheduled + self.period
            };
            scheduled
        }
    }
}
