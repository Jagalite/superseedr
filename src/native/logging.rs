// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use chrono::{NaiveDate, Utc};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use tracing_subscriber::fmt::MakeWriter;

const DEFAULT_BUFFERED_LINES: usize = 128_000;
const LOG_FILE_SUFFIX: &str = "log";

pub(crate) struct LogWorkerGuard {
    sender: Option<SyncSender<LogCommand>>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for LogWorkerGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(LogCommand::Shutdown);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct NonBlockingLogWriter {
    sender: SyncSender<LogCommand>,
}

impl Write for NonBlockingLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        match self.sender.try_send(LogCommand::Write(buf.to_vec())) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(buf.len()),
            Err(TrySendError::Disconnected(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "log worker is not available",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let (sender, receiver) = mpsc::sync_channel(1);
        match self.sender.try_send(LogCommand::Flush(sender)) {
            Ok(()) => receiver.recv().unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "log worker stopped before flushing",
                ))
            }),
            Err(TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "log queue is full; flush was not issued",
            )),
            Err(TrySendError::Disconnected(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "log worker is not available",
            )),
        }
    }
}

impl<'a> MakeWriter<'a> for NonBlockingLogWriter {
    type Writer = NonBlockingLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

enum LogCommand {
    Write(Vec<u8>),
    Flush(SyncSender<io::Result<()>>),
    Shutdown,
}

trait LogDateProvider: Send {
    fn current_date(&self) -> NaiveDate;
}

struct UtcDateProvider;

impl LogDateProvider for UtcDateProvider {
    fn current_date(&self) -> NaiveDate {
        Utc::now().date_naive()
    }
}

struct DailyRollingFileWriter {
    log_dir: PathBuf,
    filename_prefix: String,
    max_log_files: usize,
    date_provider: Box<dyn LogDateProvider>,
    report_errors_to_stderr: bool,
    current_date: Option<NaiveDate>,
    reported_roll_error_date: Option<NaiveDate>,
    file: Option<File>,
}

impl DailyRollingFileWriter {
    fn new_with_error_reporting(
        log_dir: PathBuf,
        filename_prefix: String,
        max_log_files: usize,
        date_provider: Box<dyn LogDateProvider>,
        report_errors_to_stderr: bool,
    ) -> io::Result<Self> {
        let mut writer = Self {
            log_dir,
            filename_prefix,
            max_log_files,
            date_provider,
            report_errors_to_stderr,
            current_date: None,
            reported_roll_error_date: None,
            file: None,
        };
        writer.roll_if_needed()?;
        Ok(writer)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.roll_if_needed()?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is not open"))?;
        file.write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }

    fn roll_if_needed(&mut self) -> io::Result<()> {
        let today = self.date_provider.current_date();
        if self.current_date == Some(today) && self.file.is_some() {
            return Ok(());
        }

        let path = self
            .log_dir
            .join(daily_log_file_name(&self.filename_prefix, today));
        let file = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => file,
            Err(error) if self.file.is_some() => {
                if self.reported_roll_error_date != Some(today) {
                    report_stderr_warning(
                        self.report_errors_to_stderr,
                        &format!(
                            "Could not roll log file to {}; continuing with previous file: {}",
                            path.display(),
                            error
                        ),
                    );
                    self.reported_roll_error_date = Some(today);
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        self.file = Some(file);
        self.current_date = Some(today);
        self.reported_roll_error_date = None;
        self.prune_old_logs();
        Ok(())
    }

    fn prune_old_logs(&self) {
        if self.max_log_files == 0 {
            return;
        }

        if let Err(error) = prune_old_logs(
            &self.log_dir,
            &self.filename_prefix,
            self.max_log_files,
            |path| fs::remove_file(path),
            self.report_errors_to_stderr,
        ) {
            report_stderr_warning(
                self.report_errors_to_stderr,
                &format!(
                    "Error pruning log files in {}: {}",
                    self.log_dir.display(),
                    error
                ),
            );
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "kept as the default-reporting wrapper and covered by logging tests"
    )
)]
pub(crate) fn non_blocking_daily_file_writer(
    log_dir: &Path,
    filename_prefix: &str,
    max_log_files: usize,
) -> io::Result<(NonBlockingLogWriter, LogWorkerGuard)> {
    non_blocking_daily_file_writer_with_stderr_reporting(
        log_dir,
        filename_prefix,
        max_log_files,
        true,
    )
}

pub(crate) fn non_blocking_daily_file_writer_with_stderr_reporting(
    log_dir: &Path,
    filename_prefix: &str,
    max_log_files: usize,
    report_errors_to_stderr: bool,
) -> io::Result<(NonBlockingLogWriter, LogWorkerGuard)> {
    non_blocking_daily_file_writer_with_date_provider_and_error_reporting(
        log_dir,
        filename_prefix,
        max_log_files,
        DEFAULT_BUFFERED_LINES,
        Box::new(UtcDateProvider),
        report_errors_to_stderr,
    )
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "test-only date injection helper for rolling log coverage"
    )
)]
fn non_blocking_daily_file_writer_with_date_provider(
    log_dir: &Path,
    filename_prefix: &str,
    max_log_files: usize,
    buffered_lines: usize,
    date_provider: Box<dyn LogDateProvider>,
) -> io::Result<(NonBlockingLogWriter, LogWorkerGuard)> {
    non_blocking_daily_file_writer_with_date_provider_and_error_reporting(
        log_dir,
        filename_prefix,
        max_log_files,
        buffered_lines,
        date_provider,
        true,
    )
}

fn non_blocking_daily_file_writer_with_date_provider_and_error_reporting(
    log_dir: &Path,
    filename_prefix: &str,
    max_log_files: usize,
    buffered_lines: usize,
    date_provider: Box<dyn LogDateProvider>,
    report_errors_to_stderr: bool,
) -> io::Result<(NonBlockingLogWriter, LogWorkerGuard)> {
    let file_writer = DailyRollingFileWriter::new_with_error_reporting(
        log_dir.to_path_buf(),
        filename_prefix.to_string(),
        max_log_files,
        date_provider,
        report_errors_to_stderr,
    )?;
    let (sender, receiver) = mpsc::sync_channel(buffered_lines);
    let handle = thread::Builder::new()
        .name("superseedr-log-writer".to_string())
        .spawn(move || run_log_worker(file_writer, receiver))
        .map_err(io::Error::other)?;

    Ok((
        NonBlockingLogWriter {
            sender: sender.clone(),
        },
        LogWorkerGuard {
            sender: Some(sender),
            handle: Some(handle),
        },
    ))
}

fn run_log_worker(mut file_writer: DailyRollingFileWriter, receiver: Receiver<LogCommand>) {
    let mut reported_write_error = false;
    let report_errors_to_stderr = file_writer.report_errors_to_stderr;
    while let Ok(command) = receiver.recv() {
        match command {
            LogCommand::Write(bytes) => {
                if let Err(error) = file_writer.write_all(&bytes) {
                    report_log_worker_error(
                        &mut reported_write_error,
                        error,
                        report_errors_to_stderr,
                    );
                }
            }
            LogCommand::Flush(sender) => {
                let _ = sender.send(file_writer.flush());
            }
            LogCommand::Shutdown => {
                if let Err(error) = file_writer.flush() {
                    report_log_worker_error(
                        &mut reported_write_error,
                        error,
                        report_errors_to_stderr,
                    );
                }
                break;
            }
        }
    }
}

fn report_log_worker_error(reported: &mut bool, error: io::Error, report_errors_to_stderr: bool) {
    if !*reported {
        report_stderr_warning(
            report_errors_to_stderr,
            &format!(
                "File logging failed; future log lines may be lost: {}",
                error
            ),
        );
        *reported = true;
    }
}

fn report_stderr_warning(report_to_stderr: bool, message: &str) {
    let _ = write_stderr_warning(report_to_stderr, io::stderr().lock(), message);
}

fn write_stderr_warning<W: Write>(
    report_to_stderr: bool,
    mut writer: W,
    message: &str,
) -> io::Result<()> {
    if report_to_stderr {
        writeln!(writer, "[Warn] {message}")?;
    }
    Ok(())
}

fn daily_log_file_name(filename_prefix: &str, date: NaiveDate) -> String {
    format!(
        "{}.{}.{}",
        filename_prefix,
        date.format("%Y-%m-%d"),
        LOG_FILE_SUFFIX
    )
}

fn matching_log_files(
    log_dir: &Path,
    filename_prefix: &str,
) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    let mut logs = Vec::new();
    let prefix = format!("{filename_prefix}.");
    let suffix = format!(".{LOG_FILE_SUFFIX}");

    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(date_part) = file_name
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(&suffix))
        else {
            continue;
        };
        let Ok(date) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") else {
            continue;
        };
        logs.push((date, entry.path()));
    }

    Ok(logs)
}

fn prune_old_logs<F>(
    log_dir: &Path,
    filename_prefix: &str,
    max_log_files: usize,
    remove_file: F,
    report_errors_to_stderr: bool,
) -> io::Result<()>
where
    F: Fn(&Path) -> io::Result<()>,
{
    if max_log_files == 0 {
        return Ok(());
    }

    let mut logs = matching_log_files(log_dir, filename_prefix)?;
    if logs.len() <= max_log_files {
        return Ok(());
    }

    logs.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let remove_count = logs.len() - max_log_files;
    for (_, path) in logs.into_iter().take(remove_count) {
        if let Err(error) = remove_file(&path) {
            report_stderr_warning(
                report_errors_to_stderr,
                &format!(
                    "Failed to remove old log file {}: {}",
                    path.display(),
                    error
                ),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[derive(Clone)]
    struct SharedDateProvider {
        date: Arc<Mutex<NaiveDate>>,
    }

    impl LogDateProvider for SharedDateProvider {
        fn current_date(&self) -> NaiveDate {
            *self.date.lock().unwrap()
        }
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    #[test]
    fn stderr_warning_writer_suppresses_output_when_disabled() {
        let mut output = Vec::new();

        write_stderr_warning(false, &mut output, "hidden warning").expect("write warning");

        assert!(output.is_empty());
    }

    #[test]
    fn stderr_warning_writer_formats_enabled_warning() {
        let mut output = Vec::new();

        write_stderr_warning(true, &mut output, "visible warning").expect("write warning");

        let output = String::from_utf8(output).expect("utf8 warning");
        assert_eq!(output, "[Warn] visible warning\n");
    }

    #[test]
    fn daily_log_file_name_matches_existing_format() {
        assert_eq!(
            daily_log_file_name("app", date(2026, 5, 2)),
            "app.2026-05-02.log"
        );
        assert_eq!(
            daily_log_file_name("cli", date(2026, 5, 2)),
            "cli.2026-05-02.log"
        );
    }

    #[test]
    fn non_blocking_writer_flushes_on_guard_drop() {
        let dir = tempdir().expect("create tempdir");
        let (mut writer, guard) =
            non_blocking_daily_file_writer(dir.path(), "app", 31).expect("create log writer");

        writer.write_all(b"sample log line\n").expect("queue log");
        drop(guard);

        let contents = fs::read_to_string(
            dir.path()
                .join(daily_log_file_name("app", Utc::now().date_naive())),
        )
        .expect("read log file");
        assert!(contents.contains("sample log line"));
    }

    #[test]
    fn daily_writer_rolls_to_next_date() {
        let dir = tempdir().expect("create tempdir");
        let current_date = Arc::new(Mutex::new(date(2026, 5, 1)));
        let provider = SharedDateProvider {
            date: Arc::clone(&current_date),
        };
        let (mut writer, guard) = non_blocking_daily_file_writer_with_date_provider(
            dir.path(),
            "app",
            31,
            16,
            Box::new(provider),
        )
        .expect("create log writer");

        writer.write_all(b"first day\n").expect("queue first day");
        writer.flush().expect("flush first day");
        *current_date.lock().unwrap() = date(2026, 5, 2);
        writer.write_all(b"second day\n").expect("queue second day");
        drop(guard);

        let first =
            fs::read_to_string(dir.path().join("app.2026-05-01.log")).expect("read first log");
        let second =
            fs::read_to_string(dir.path().join("app.2026-05-02.log")).expect("read second log");
        assert!(first.contains("first day"));
        assert!(second.contains("second day"));
    }

    #[test]
    fn retention_prunes_only_old_matching_logs() {
        let dir = tempdir().expect("create tempdir");
        let start = date(2026, 4, 1);
        for offset in 0..35 {
            let log_date = start
                .checked_add_signed(Duration::days(offset))
                .expect("date in range");
            fs::write(
                dir.path().join(daily_log_file_name("app", log_date)),
                format!("old {offset}\n"),
            )
            .expect("seed log");
        }
        fs::write(dir.path().join("app.not-a-date.log"), "keep").expect("seed non-date log");
        fs::write(dir.path().join("other.2026-05-01.log"), "keep").expect("seed other log");

        let current_date = start
            .checked_add_signed(Duration::days(35))
            .expect("date in range");
        let provider = SharedDateProvider {
            date: Arc::new(Mutex::new(current_date)),
        };
        let (_writer, guard) = non_blocking_daily_file_writer_with_date_provider(
            dir.path(),
            "app",
            31,
            16,
            Box::new(provider),
        )
        .expect("create log writer");
        drop(guard);

        let matching = matching_log_files(dir.path(), "app").expect("list matching logs");
        assert_eq!(matching.len(), 31);
        assert!(!dir.path().join("app.2026-04-01.log").exists());
        assert!(!dir.path().join("app.2026-04-05.log").exists());
        assert!(dir
            .path()
            .join(daily_log_file_name("app", current_date))
            .exists());
        assert!(dir.path().join("app.not-a-date.log").exists());
        assert!(dir.path().join("other.2026-05-01.log").exists());
    }

    #[test]
    fn retention_delete_failures_do_not_fail_pruning() {
        let dir = tempdir().expect("create tempdir");
        let start = date(2026, 4, 1);
        for offset in 0..4 {
            let log_date = start
                .checked_add_signed(Duration::days(offset))
                .expect("date in range");
            fs::write(
                dir.path().join(daily_log_file_name("app", log_date)),
                format!("old {offset}\n"),
            )
            .expect("seed log");
        }

        let result = prune_old_logs(
            dir.path(),
            "app",
            2,
            |_path| Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked")),
            false,
        );

        assert!(result.is_ok());
        let matching = matching_log_files(dir.path(), "app").expect("list matching logs");
        assert_eq!(matching.len(), 4);
    }

    #[test]
    fn rollover_open_failure_keeps_previous_file() {
        let dir = tempdir().expect("create tempdir");
        let current_date = Arc::new(Mutex::new(date(2026, 5, 1)));
        let provider = SharedDateProvider {
            date: Arc::clone(&current_date),
        };
        let mut writer = DailyRollingFileWriter::new_with_error_reporting(
            dir.path().to_path_buf(),
            "app".to_string(),
            31,
            Box::new(provider),
            false,
        )
        .expect("create log writer");

        writer.write_all(b"first day\n").expect("write first day");
        writer.flush().expect("flush first day");
        fs::create_dir(dir.path().join("app.2026-05-02.log")).expect("create rollover blocker");

        *current_date.lock().unwrap() = date(2026, 5, 2);
        writer
            .write_all(b"second day stayed on previous file\n")
            .expect("write through rollover failure");
        writer.flush().expect("flush previous file");

        let first =
            fs::read_to_string(dir.path().join("app.2026-05-01.log")).expect("read first log");
        assert!(first.contains("first day"));
        assert!(first.contains("second day stayed on previous file"));

        fs::remove_dir(dir.path().join("app.2026-05-02.log")).expect("remove rollover blocker");
        writer
            .write_all(b"second day recovered\n")
            .expect("write through recovered rollover");
        writer.flush().expect("flush recovered file");

        let second =
            fs::read_to_string(dir.path().join("app.2026-05-02.log")).expect("read second log");
        assert!(second.contains("second day recovered"));
    }

    #[test]
    fn full_queue_drops_without_write_error() {
        let (sender, _receiver) = mpsc::sync_channel(0);
        let mut writer = NonBlockingLogWriter { sender };

        let written = writer.write(b"dropped line").expect("full queue is lossy");

        assert_eq!(written, "dropped line".len());
    }

    #[test]
    fn full_queue_flush_reports_would_block() {
        let (sender, _receiver) = mpsc::sync_channel(0);
        let mut writer = NonBlockingLogWriter { sender };

        let error = writer
            .flush()
            .expect_err("full queue cannot confirm durability");

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn log_writer_reports_open_failure() {
        let dir = tempdir().expect("create tempdir");
        let blocking_file = dir.path().join("not-a-directory");
        fs::write(&blocking_file, "blocking").expect("create blocking file");

        let result = non_blocking_daily_file_writer(&blocking_file, "app", 31);

        assert!(result.is_err(), "file path should not act as a log dir");
    }
}
