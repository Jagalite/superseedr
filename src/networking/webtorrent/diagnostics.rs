// SPDX-License-Identifier: GPL-3.0-or-later
//! Opt-in, synchronous diagnostic output for synthetic connectivity investigations.
//! This intentionally measures event order rather than uninstrumented performance.
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{LazyLock, Mutex},
    time::Instant,
};

struct Trace {
    started: Instant,
    output: Mutex<(File, u64)>,
}
static TRACE: LazyLock<Option<Trace>> = LazyLock::new(|| {
    let path = std::env::var_os("SUPERSEEDR_RTC_TRACE")?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => Some(Trace {
            started: Instant::now(),
            output: Mutex::new((file, 0)),
        }),
        Err(error) => {
            eprintln!(
                "Cannot create RTC trace {}: {error}",
                std::path::Path::new(&path).display()
            );
            None
        }
    }
});

pub(crate) fn record(event: &str, fields: impl FnOnce() -> serde_json::Value) {
    let Some(trace) = TRACE.as_ref() else {
        return;
    };
    let mut output = trace.output.lock().expect("RTC trace writer");
    let row = serde_json::json!({
        "seq": output.1, "elapsed_us": trace.started.elapsed().as_micros(),
        "event": event, "fields": fields(),
    });
    output.1 += 1;
    if let Err(error) = writeln!(output.0, "{row}") {
        eprintln!("RTC trace write failed: {error}");
    }
}
