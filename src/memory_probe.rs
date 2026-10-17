// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Explicit opt-in diagnostics for the experimental memory-profile build only.
//! The sampler perturbs allocations; use matching instrumentation for comparisons.
use std::{
    fs::OpenOptions,
    io::Write,
    time::{Duration, Instant},
};

pub(crate) fn start() -> std::io::Result<()> {
    let Some(path) = std::env::var_os("SUPERSEEDR_MEMORY_PROBE_FILE") else {
        return Ok(());
    };
    // Never overwrite an existing measurement or application file.
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    let relief_after = std::env::var("SUPERSEEDR_MEMORY_RELIEF_AFTER_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    std::thread::Builder::new()
        .name("memory-probe".into())
        .spawn(move || {
            let start = Instant::now();
            let mut relieved = false;
            loop {
                let elapsed = start.elapsed().as_secs_f64();
                let before = crate::memory_profile::snapshot();
                #[allow(unused_mut)]
                let mut released: Option<usize> = None;
                if !relieved && relief_after.is_some_and(|seconds| elapsed >= seconds as f64) {
                    relieved = true;
                    #[cfg(target_os = "macos")]
                    {
                        unsafe extern "C" {
                            fn malloc_zone_pressure_relief(
                                zone: *mut std::ffi::c_void,
                                goal: usize,
                            ) -> usize;
                        }
                        // SAFETY: Apple's malloc.h specifies NULL for all zones and
                        // zero goal for maximal release of unused allocator pages.
                        released =
                            Some(unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) });
                    }
                }
                let record = serde_json::json!({"elapsed":elapsed,"rust_heap":before,
                "pressure_relief_bytes":released});
                if serde_json::to_writer(&mut output, &record).is_err()
                    || output.write_all(b"\n").is_err()
                {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })?;
    Ok(())
}
