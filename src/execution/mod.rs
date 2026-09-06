// SPDX-License-Identifier: GPL-3.0-or-later
//! Task ownership is shared; the host supplies the scheduler and clock driver.
#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
pub(crate) use browser::*;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tokio::task::spawn_blocking as spawn_compute;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tokio::{spawn, task::JoinSet};

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod time {
    pub(crate) use tokio::time::*;
}

pub(crate) async fn shutdown_signal() {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    #[cfg(target_arch = "wasm32")]
    std::future::pending::<()>().await;
}

#[cfg(all(target_arch = "wasm32", feature = "browser-contract"))]
pub(crate) mod browser_contract;
