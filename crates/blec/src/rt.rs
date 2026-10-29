//! Lets the public API run on any executor, not just tokio.
//!
//! The handler spawns tasks and uses tokio timers, and btleplug needs a tokio
//! runtime as well (on Linux bluez spawns its D-Bus task and spawns again
//! whenever one of its message streams is dropped). Callers that are not
//! running inside tokio get a small runtime owned by blec instead: every poll
//! of a [`compat`] future enters it, so whatever the future spawns or starts
//! a timer on lives there, and that runtime's thread wakes the caller's
//! executor. Inside a tokio runtime nothing changes.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use once_cell::sync::Lazy;
use tokio::runtime::{EnterGuard, Handle};

/// Started on first use, so apps that run on tokio never create it.
static FALLBACK: Lazy<Handle> = Lazy::new(|| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build the blec runtime");
    let handle = runtime.handle().clone();
    std::thread::Builder::new()
        .name("blec-runtime".into())
        .spawn(move || runtime.block_on(std::future::pending::<()>()))
        .expect("failed to start the blec runtime thread");
    handle
});

/// Enters the fallback runtime unless the caller already runs inside tokio.
fn enter() -> Option<EnterGuard<'static>> {
    Handle::try_current().is_err().then(|| FALLBACK.enter())
}

/// Runs `fut` inside a tokio runtime context, see the module docs.
pub(crate) fn compat<F: Future>(fut: F) -> Compat<F> {
    Compat {
        fut: Some(Box::pin(fut)),
    }
}

pub(crate) struct Compat<F> {
    /// Dropped as soon as it completes, so its drop also runs inside the runtime.
    fut: Option<Pin<Box<F>>>,
}

impl<F: Future> Future for Compat<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _guard = enter();
        let fut = self
            .fut
            .as_mut()
            .expect("blec future polled after completion");
        let output = std::task::ready!(fut.as_mut().poll(cx));
        self.fut = None;
        Poll::Ready(output)
    }
}

impl<F> Drop for Compat<F> {
    fn drop(&mut self) {
        // a future cancelled mid-flight can still own btleplug streams
        if self.fut.is_some() {
            let _guard = enter();
            self.fut = None;
        }
    }
}
