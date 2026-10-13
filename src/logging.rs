//! Private, structured diagnostics. Application events use explicit safe fields;
//! dependency logging and argument/configuration dumps are never enabled here.
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};

use chrono::{NaiveDate, Utc};
use fs2::FileExt;
use tracing::{Instrument, Span};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::{EnvFilter, FilterExt, LevelFilter, Targets};
use tracing_subscriber::prelude::*;

const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const MAX_FILES: usize = 200;
const RETENTION_DAYS: i64 = 7;

pub fn directory() -> PathBuf {
    if let Some(path) = std::env::var_os("USAGE_LOG_DIR").filter(|p| !p.is_empty()) {
        return crate::config::resolved_data_path(Path::new(&path));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::home_dir().join(".local/state"));
    base.join("usage/logs")
}

fn process_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

pub struct LoggingGuard {
    _worker: WorkerGuard,
}

/// Keep the guard alive until the command returns, including all error paths.
pub fn init(default_level: &str) -> io::Result<Option<LoggingGuard>> {
    let level = std::env::var("USAGE_LOG").unwrap_or_else(|_| default_level.into());
    if level.trim().eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let filter = EnvFilter::builder()
        .with_regex(false)
        .parse(&level)
        .unwrap_or_else(|_| {
            eprintln!("日志级别配置无效，使用 info");
            EnvFilter::new("info")
        });
    let mut writer = LogWriter::new(directory(), MAX_FILE_BYTES)?;
    if default_level != "warn" {
        // Detect file creation failures before claiming diagnostics are available.
        writer.rotate(Utc::now().date_naive())?;
    }
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(false)
        .finish(writer);
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_writer(writer)
        .with_filter(diagnostic_filter(filter))
        // Never expose third-party HTTP/credential internals, even for USAGE_LOG=trace.
        .with_filter(application_targets());
    tracing_subscriber::registry()
        .with(layer)
        .try_init()
        .map_err(|_| io::Error::other("logging subscriber unavailable"))?;
    Ok(Some(LoggingGuard { _worker: guard }))
}

fn diagnostic_filter<S>(filter: EnvFilter) -> impl tracing_subscriber::layer::Filter<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    // Operation context must survive warn/error and module-only event filters.
    filter.or(tracing_subscriber::filter::filter_fn(|metadata| {
        metadata.is_span()
            && metadata.target() == "usage::logging"
            && metadata.name() == "operation"
    }))
}

fn application_targets() -> Targets {
    Targets::new()
        .with_default(LevelFilter::OFF)
        .with_target("usage", LevelFilter::TRACE)
}

/// One ID exists before any side effects, even if account resolution or spawn fails.
pub(crate) struct Operation {
    id: String,
    span: Span,
    started: Instant,
    stage: &'static str,
    failed: bool,
}

impl Operation {
    pub(crate) fn new(name: &'static str) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let span = tracing::info_span!(
            "operation", operation = name, operation_id = %id,
            process_id = process_id(), pid = std::process::id(),
            profile_id = tracing::field::Empty, revision = tracing::field::Empty,
            auth_mode = tracing::field::Empty, command = tracing::field::Empty,
            run_id = tracing::field::Empty
        );
        span.in_scope(|| tracing::info!(event = "operation_started"));
        Self {
            id,
            span,
            started: Instant::now(),
            stage: "start",
            failed: false,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn span(&self) -> Span {
        self.span.clone()
    }

    pub(crate) fn account(&self, id: &str, revision: i64, mode: &'static str) {
        if let Ok(id) = uuid::Uuid::parse_str(id) {
            self.span.record("profile_id", id.to_string());
        }
        self.span.record("revision", revision);
        self.span.record("auth_mode", mode);
    }

    pub(crate) fn link_run(&self, id: &str) {
        if let Ok(id) = uuid::Uuid::parse_str(id) {
            self.span.record("run_id", id.to_string());
        }
    }

    pub(crate) fn begin(&mut self, stage: &'static str) {
        self.stage = stage;
        self.span
            .in_scope(|| tracing::debug!(event = "stage_started", stage));
    }

    pub(crate) fn step<T, E>(
        &mut self,
        stage: &'static str,
        action: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        self.begin(stage);
        let started = Instant::now();
        let result = self.span.in_scope(action);
        self.span.in_scope(|| {
            if result.is_ok() {
                tracing::debug!(
                    event = "stage_completed",
                    stage,
                    elapsed_ms = started.elapsed().as_millis() as u64
                );
            } else {
                tracing::error!(
                    event = "stage_failed",
                    stage,
                    elapsed_ms = started.elapsed().as_millis() as u64
                );
            }
        });
        result
    }

    pub(crate) fn fail(&mut self, stage: &'static str) {
        self.stage = stage;
        self.failed = true;
    }

    pub(crate) fn finish(&self, successful: bool) {
        let successful = successful && !self.failed;
        self.span.in_scope(|| {
            if successful {
                tracing::info!(
                    event = "operation_completed",
                    elapsed_ms = self.started.elapsed().as_millis() as u64
                );
            } else {
                tracing::error!(
                    event = "operation_failed",
                    stage = self.stage,
                    elapsed_ms = self.started.elapsed().as_millis() as u64
                );
            }
        });
    }
}

pub(crate) fn operation<T, E>(
    name: &'static str,
    action: impl FnOnce(&mut Operation) -> Result<T, E>,
) -> Result<T, E> {
    let mut op = Operation::new(name);
    let result = op.span().in_scope(|| action(&mut op));
    op.finish(result.is_ok());
    result
}

pub(crate) async fn instrument<T>(span: Span, future: impl std::future::Future<Output = T>) -> T {
    future.instrument(span).await
}

struct LogFile {
    path: PathBuf,
    date: NaiveDate,
    modified: SystemTime,
    bytes: u64,
}

fn files(directory: &Path) -> io::Result<Vec<LogFile>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .and_then(|s| s.strip_prefix("usage-"))
            .and_then(|s| s.strip_suffix(".jsonl"))
        else {
            continue;
        };
        let mut parts = name.splitn(3, '-');
        let Some(date) = parts
            .next()
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y%m%d").ok())
        else {
            continue;
        };
        if parts.next().and_then(|s| s.parse::<u32>().ok()).is_none() {
            continue;
        }
        let Some((id, part)) = parts.next().and_then(|s| s.rsplit_once('-')) else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_err() || part.parse::<u32>().is_err() {
            continue;
        }
        // Never follow symlinks or remove files belonging to another application.
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() {
            continue;
        }
        files.push(LogFile {
            path: entry.path(),
            date,
            modified: metadata.modified()?,
            bytes: metadata.len(),
        });
    }
    files.sort_by_key(|f| (f.date, f.modified, f.path.clone()));
    Ok(files)
}

fn prune(directory: &Path) {
    let Ok(files) = files(directory) else {
        return;
    };
    let mut bytes: u64 = files.iter().map(|f| f.bytes).sum();
    let mut count = files.len();
    let oldest = Utc::now().date_naive() - chrono::Duration::days(RETENTION_DAYS);
    for entry in files {
        if entry.date >= oldest && bytes <= MAX_TOTAL_BYTES && count <= MAX_FILES {
            continue;
        }
        let Ok(file) = OpenOptions::new().write(true).open(&entry.path) else {
            continue;
        };
        if FileExt::try_lock_exclusive(&file).is_err() {
            continue;
        }
        if fs::remove_file(&entry.path).is_ok() {
            bytes = bytes.saturating_sub(entry.bytes);
            count = count.saturating_sub(1);
        }
    }
}

/// Only file lifecycle policy is application-specific; tracing formats events and
/// tracing-appender owns the bounded background queue. Files remain locked while live.
struct LogWriter {
    directory: PathBuf,
    session: uuid::Uuid,
    part: u32,
    file: Option<File>,
    date: NaiveDate,
    written: u64,
    limit: u64,
}

impl LogWriter {
    fn new(directory: PathBuf, limit: u64) -> io::Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        // Existing parent/directory permissions are not changed.
        builder.create(&directory)?;
        prune(&directory);
        Ok(Self {
            directory,
            session: uuid::Uuid::new_v4(),
            part: 0,
            file: None,
            date: Utc::now().date_naive(),
            written: 0,
            limit,
        })
    }

    fn rotate(&mut self, date: NaiveDate) -> io::Result<()> {
        let path = self.directory.join(format!(
            "usage-{}-{}-{}-{:04}.jsonl",
            date.format("%Y%m%d"),
            std::process::id(),
            self.session,
            self.part
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        FileExt::try_lock_exclusive(&file)?;
        self.file = Some(file);
        self.date = date;
        self.written = 0;
        self.part += 1;
        prune(&self.directory);
        Ok(())
    }
}

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let date = Utc::now().date_naive();
        if self.file.is_none()
            || self.date != date
            || (self.written > 0 && self.written + bytes.len() as u64 > self.limit)
        {
            self.rotate(date)?;
        }
        // Complete the formatted event before rotating; partial OS writes cannot split it.
        self.file
            .as_mut()
            .expect("opened log file")
            .write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?;
        }
        Ok(())
    }
}

fn matches_operation(value: &serde_json::Value, id: &str) -> bool {
    let matches = |span: &serde_json::Value| {
        ["operation_id", "run_id"]
            .iter()
            .any(|field| span.get(field).and_then(|s| s.as_str()) == Some(id))
    };
    value.get("span").is_some_and(matches)
        || value
            .get("spans")
            .and_then(|s| s.as_array())
            .is_some_and(|spans| spans.iter().any(matches))
}

pub(crate) fn recent(lines: usize, operation: Option<&str>) -> io::Result<Vec<String>> {
    let mut files = files(&directory())?;
    if operation.is_none() {
        files = files.into_iter().rev().take(1).collect();
    }
    let mut tail = VecDeque::new();
    for entry in files {
        let file = match File::open(entry.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(id) = operation
                && !matches_operation(&value, id)
            {
                continue;
            }
            tail.push_back(line);
            if tail.len() > lines {
                tail.pop_front();
            }
        }
    }
    Ok(tail.into_iter().collect())
}

/// Notifications run in separate processes; merge their events by timestamp,
/// rather than selecting whichever process last created a log file.
pub(crate) fn recent_bark(lines: usize, run: Option<&str>) -> io::Result<Vec<String>> {
    let mut tail = std::collections::BTreeMap::new();
    let mut sequence = 0u64;
    for entry in files(&directory())? {
        let file = match File::open(entry.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for line in BufReader::new(file).lines() {
            let line = line?;
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let is_bark = value["fields"]["event"]
                .as_str()
                .is_some_and(|event| event.starts_with("bark_"))
                || value["span"]["operation"] == "codex.bark_notify";
            if !is_bark || run.is_some_and(|id| !matches_operation(&value, id)) {
                continue;
            }
            sequence += 1;
            tail.insert(
                (
                    value["timestamp"].as_str().unwrap_or("").to_string(),
                    sequence,
                ),
                line,
            );
            if tail.len() > lines {
                tail.pop_first();
            }
        }
    }
    Ok(tail.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("usage-logging-test-{}", uuid::Uuid::new_v4())))
        }
        fn named_file(&self, date: NaiveDate) -> PathBuf {
            self.0.join(format!(
                "usage-{}-123-{}-0000.jsonl",
                date.format("%Y%m%d"),
                uuid::Uuid::new_v4()
            ))
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rotation_keeps_whole_events_and_changes_file_at_utc_day_boundary() {
        let dir = Directory::new();
        let mut writer = LogWriter::new(dir.0.clone(), 20).unwrap();
        assert!(files(&dir.0).unwrap().is_empty());
        let yesterday = Utc::now().date_naive() - chrono::Duration::days(1);
        writer.rotate(yesterday).unwrap();
        writer
            .file
            .as_mut()
            .unwrap()
            .write_all(b"{\"old\":true}\n")
            .unwrap();
        writer
            .write_all(b"{\"message\":\"larger than limit\"}\n")
            .unwrap();
        writer.write_all(b"{\"next\":true}\n").unwrap();
        writer.flush().unwrap();
        let files = files(&dir.0).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].date, yesterday);
        let mut count = 0;
        for file in files {
            for line in fs::read_to_string(file.path).unwrap().lines() {
                serde_json::from_str::<serde_json::Value>(line).unwrap();
                count += 1;
            }
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn retention_removes_old_inactive_logs_and_preserves_locked_and_unrelated_files() {
        let dir = Directory::new();
        let _writer = LogWriter::new(dir.0.clone(), MAX_FILE_BYTES).unwrap();
        let old = Utc::now().date_naive() - chrono::Duration::days(10);
        let inactive = dir.named_file(old);
        fs::write(&inactive, b"inactive").unwrap();
        let active = dir.named_file(old);
        let file = File::create(&active).unwrap();
        FileExt::try_lock_exclusive(&file).unwrap();
        let unrelated = dir.0.join("usage-other-program.jsonl");
        fs::write(&unrelated, b"untouched").unwrap();
        #[cfg(unix)]
        let link = {
            let link = dir.named_file(old);
            std::os::unix::fs::symlink(&unrelated, &link).unwrap();
            link
        };
        prune(&dir.0);
        assert!(!inactive.exists());
        assert!(active.exists());
        assert_eq!(fs::read(&unrelated).unwrap(), b"untouched");
        #[cfg(unix)]
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        drop(file);
        prune(&dir.0);
        assert!(!active.exists());
    }

    #[test]
    fn retention_enforces_size_and_count_without_deleting_live_writer() {
        let dir = Directory::new();
        let mut writer = LogWriter::new(dir.0.clone(), MAX_FILE_BYTES).unwrap();
        writer.write_all(b"{\"live\":true}\n").unwrap();
        let today = Utc::now().date_naive();
        let huge = dir.named_file(today);
        File::create(&huge)
            .unwrap()
            .set_len(MAX_TOTAL_BYTES + 1)
            .unwrap();
        prune(&dir.0);
        assert!(!huge.exists());
        for _ in 0..MAX_FILES + 10 {
            fs::write(dir.named_file(today), []).unwrap();
        }
        prune(&dir.0);
        assert_eq!(files(&dir.0).unwrap().len(), MAX_FILES);
        assert!(
            files(&dir.0)
                .unwrap()
                .iter()
                .any(|f| fs::read(&f.path).unwrap() == b"{\"live\":true}\n")
        );
    }

    #[test]
    fn trace_filter_excludes_dependency_events_and_keeps_nested_operation_context() {
        let dir = Directory::new();
        let writer = LogWriter::new(dir.0.clone(), MAX_FILE_BYTES).unwrap();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::sync::Mutex::new(writer))
                .with_filter(application_targets()),
        );
        tracing::subscriber::with_default(subscriber, || {
            operation("test.outer", |outer| {
                outer.account("12345678-1234-1234-1234-123456789abc", 2, "byok");
                operation("test.inner", |inner| {
                    inner.begin("safe_stage");
                    tracing::error!(target: "reqwest", authorization = "private-dependency-token", "raw request");
                    tracing::trace!(event = "safe_app_event");
                    Ok::<_, ()>(())
                })
            }).unwrap();
        });
        let logs = files(&dir.0)
            .unwrap()
            .into_iter()
            .map(|f| fs::read_to_string(f.path).unwrap())
            .collect::<String>();
        assert!(!logs.contains("private-dependency-token"));
        let events: Vec<serde_json::Value> = logs
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let event = events
            .iter()
            .find(|e| e["fields"]["event"] == "safe_app_event")
            .unwrap();
        assert_eq!(event["spans"][0]["operation"], "test.outer");
        assert_eq!(event["spans"][0]["revision"], 2);
        assert_eq!(event["span"]["operation"], "test.inner");
        assert!(
            event["span"]["operation_id"]
                .as_str()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        );
    }

    #[test]
    fn module_event_filter_keeps_operation_spans_without_enabling_extra_events() {
        let dir = Directory::new();
        let writer = LogWriter::new(dir.0.clone(), MAX_FILE_BYTES).unwrap();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::sync::Mutex::new(writer))
                .with_filter(diagnostic_filter(EnvFilter::new(
                    "usage::codex_manager=error",
                )))
                .with_filter(application_targets()),
        );
        tracing::subscriber::with_default(subscriber, || {
            operation("test.filtered", |op| {
                op.account("12345678-1234-1234-1234-123456789abc", 1, "byok");
                tracing::info!(target: "usage::codex_manager", event = "excluded_info");
                tracing::error!(target: "usage::codex_manager", event = "included_error");
                Ok::<_, ()>(())
            })
            .unwrap();
        });
        let logs = files(&dir.0)
            .unwrap()
            .into_iter()
            .map(|f| fs::read_to_string(f.path).unwrap())
            .collect::<String>();
        let events: Vec<serde_json::Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event"], "included_error");
        assert_eq!(events[0]["span"]["operation"], "test.filtered");
        assert_eq!(
            events[0]["span"]["profile_id"],
            "12345678-1234-1234-1234-123456789abc"
        );
    }

    #[cfg(unix)]
    #[test]
    fn creating_private_log_directory_preserves_existing_parent_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Directory::new();
        fs::create_dir(&dir.0).unwrap();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();
        let mut writer = LogWriter::new(dir.0.join("logs"), MAX_FILE_BYTES).unwrap();
        writer.write_all(b"{}\n").unwrap();
        assert_eq!(
            fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(dir.0.join("logs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
