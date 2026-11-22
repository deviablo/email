// SPDX-License-Identifier: GPL-3.0-or-later

//! Network and keyring work runs on a small Tokio runtime next to the GTK
//! main loop. Results come back to the UI through `glib::spawn_future_local`.

use std::future::Future;
use std::sync::LazyLock;

use tokio::runtime::Runtime;

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("e-mail-worker")
        .enable_all()
        .build()
        .expect("failed to start the background runtime")
});

pub fn handle() -> tokio::runtime::Handle {
    RUNTIME.handle().clone()
}

/// Runs `future` in the background and awaits its result on the GTK side.
pub async fn run<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    RUNTIME
        .spawn(future)
        .await
        .expect("background task panicked")
}
