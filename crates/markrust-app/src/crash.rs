// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bounded, privacy-safe desktop diagnostics. Records use fixed categories,
//! never document contents, paths, URLs, or arbitrary error/panic messages.
//! Native crashes remain the responsibility of the OS crash reporter.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock, TryLockError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const LOG_NAME: &str = "diagnostics.jsonl";
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 4096;
const ARCHIVE_COUNT: usize = 2;

static INSTALL: Once = Once::new();
static LOGGER: OnceLock<Mutex<DiagnosticLogger>> = OnceLock::new();

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenOrigin {
    Launch,
    Finder,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenTarget {
    File,
    Folder,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorCategory {
    NotFound,
    PermissionDenied,
    InvalidData,
    Interrupted,
    IoOther,
    Other,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum DiagnosticEvent {
    ApplicationStarted,
    ApplicationStopped,
    FontsLoadFailed,
    WindowReady,
    OpenStarted {
        origin: OpenOrigin,
        target: OpenTarget,
    },
    OpenSucceeded {
        origin: OpenOrigin,
        target: OpenTarget,
    },
    OpenFailed {
        origin: OpenOrigin,
        target: OpenTarget,
        category: ErrorCategory,
    },
    ExportStarted,
    ExportSucceeded,
    ExportFailed {
        category: ErrorCategory,
    },
    Panic {
        source_file: String,
        line: u32,
        column: u32,
    },
}

#[derive(Serialize)]
struct DiagnosticRecord<'a> {
    schema: u8,
    timestamp_ms: u64,
    session_started_ms: u64,
    pid: u32,
    sequence: u64,
    version: &'static str,
    #[serde(flatten)]
    event: &'a DiagnosticEvent,
}

struct DiagnosticLogger {
    path: PathBuf,
    session_started_ms: u64,
    sequence: u64,
}

impl DiagnosticLogger {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            session_started_ms: timestamp_ms(),
            sequence: 0,
        }
    }

    fn write(&mut self, event: &DiagnosticEvent) -> io::Result<()> {
        self.sequence = self.sequence.saturating_add(1);
        let record = DiagnosticRecord {
            schema: 1,
            timestamp_ms: timestamp_ms(),
            session_started_ms: self.session_started_ms,
            pid: std::process::id(),
            sequence: self.sequence,
            version: env!("CARGO_PKG_VERSION"),
            event,
        };
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');
        append_line(&self.path, &line, MAX_LOG_BYTES)
    }
}

/// Location of the current diagnostic log. Older panics.log data is retained
/// for recovery, but new reports are never appended to that legacy file.
pub fn diagnostics_log_path() -> PathBuf {
    let directory = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join("Library/Logs/MarkRust"))
    } else {
        dirs::data_local_dir().map(|data| data.join("MarkRust"))
    }
    .unwrap_or_else(|| std::env::temp_dir().join(format!("MarkRust-{}", std::process::id())));
    directory.join(LOG_NAME)
}

/// Install the panic hook and prepare the log directory once, before GPUI
/// starts. Logging failures never prevent the editor from launching.
pub fn install_panic_logger() {
    install_panic_logger_to(diagnostics_log_path());
}

fn install_panic_logger_to(path: PathBuf) {
    INSTALL.call_once(|| {
        if let Some(directory) = path.parent() {
            if prepare_directory(directory).is_err() {
                eprintln!("MarkRust diagnostics directory is unavailable");
            }
        }
        let _ = LOGGER.set(Mutex::new(DiagnosticLogger::new(path)));
        std::panic::set_hook(Box::new(|info| {
            record_panic(info);
            // The default hook prints arbitrary panic payloads, which can
            // contain private document text or absolute paths.
            eprintln!("MarkRust encountered a panic; consult its diagnostics log");
        }));
    });
}

pub fn record_application_started() {
    record_event(DiagnosticEvent::ApplicationStarted);
}

pub fn record_application_stopped() {
    record_event(DiagnosticEvent::ApplicationStopped);
}

pub fn record_fonts_load_failed() {
    record_event(DiagnosticEvent::FontsLoadFailed);
}

pub fn record_window_ready() {
    record_event(DiagnosticEvent::WindowReady);
}

pub fn open_target(path: &Path) -> OpenTarget {
    if path.is_dir() {
        OpenTarget::Folder
    } else {
        OpenTarget::File
    }
}

pub fn record_open_started(origin: OpenOrigin, target: OpenTarget) {
    record_event(DiagnosticEvent::OpenStarted { origin, target });
}

pub fn record_open_succeeded(origin: OpenOrigin, target: OpenTarget) {
    record_event(DiagnosticEvent::OpenSucceeded { origin, target });
}

pub fn record_open_failed(origin: OpenOrigin, target: OpenTarget, error: &anyhow::Error) {
    record_event(DiagnosticEvent::OpenFailed {
        origin,
        target,
        category: error_category(error),
    });
}

pub fn record_export_started() {
    record_event(DiagnosticEvent::ExportStarted);
}

pub fn record_export_succeeded() {
    record_event(DiagnosticEvent::ExportSucceeded);
}

pub fn record_export_failed(error: &anyhow::Error) {
    record_event(DiagnosticEvent::ExportFailed {
        category: error_category(error),
    });
}

fn error_category(error: &anyhow::Error) -> ErrorCategory {
    let Some(io_error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<io::Error>())
    else {
        return ErrorCategory::Other;
    };
    match io_error.kind() {
        io::ErrorKind::NotFound => ErrorCategory::NotFound,
        io::ErrorKind::PermissionDenied => ErrorCategory::PermissionDenied,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => ErrorCategory::InvalidData,
        io::ErrorKind::Interrupted => ErrorCategory::Interrupted,
        _ => ErrorCategory::IoOther,
    }
}

fn record_event(event: DiagnosticEvent) {
    let Some(logger) = LOGGER.get() else { return };
    let mut logger = match logger.lock() {
        Ok(logger) => logger,
        Err(poisoned) => poisoned.into_inner(),
    };
    let _ = logger.write(&event);
}

fn record_panic(info: &PanicHookInfo<'_>) {
    let Some(logger) = LOGGER.get() else { return };
    let (source_file, line, column) = match info.location() {
        Some(location) => (
            safe_source_file(location.file()),
            location.line(),
            location.column(),
        ),
        None => ("unknown".to_string(), 0, 0),
    };
    let event = DiagnosticEvent::Panic {
        source_file,
        line,
        column,
    };
    // A panic can occur while an ordinary event owns the mutex. Never block
    // or recurse through the logger from the hook.
    let mut guard = match logger.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    let _ = guard.write(&event);
}

fn safe_source_file(path: &str) -> String {
    let basename = path.rsplit(['/', '\\']).next().unwrap_or("unknown");
    let clean: String = basename
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "unknown".to_string()
    } else {
        clean
    }
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn prepare_directory(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    if !fs::symlink_metadata(directory)?.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "diagnostics directory is not a regular directory",
        ));
    }
    #[cfg(unix)]
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;

    // Preserve old evidence, but close the privacy hole in older installs
    // and in diagnostic archives inherited from previous app versions.
    secure_existing_regular_file(&directory.join("panics.log"))?;
    let active = directory.join(LOG_NAME);
    secure_existing_regular_file(&active)?;
    for number in 1..=ARCHIVE_COUNT {
        secure_existing_regular_file(&archive_path(&active, number))?;
    }
    Ok(())
}

fn secure_existing_regular_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn append_line(path: &Path, line: &[u8], maximum: u64) -> io::Result<()> {
    if line.len() > MAX_RECORD_BYTES || line.len() as u64 > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "diagnostic record exceeds size limit",
        ));
    }
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "diagnostic path has no parent")
    })?;
    prepare_directory(directory)?;
    ensure_regular_file_or_absent(path)?;
    if fs::metadata(path)
        .map(|metadata| metadata.len() + line.len() as u64 > maximum)
        .unwrap_or(false)
    {
        rotate(path)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(line)?;
    file.sync_data()
}

fn archive_path(path: &Path, number: usize) -> PathBuf {
    path.with_file_name(format!("{LOG_NAME}.{number}"))
}

fn rotate(path: &Path) -> io::Result<()> {
    for number in 1..=ARCHIVE_COUNT {
        ensure_regular_file_or_absent(&archive_path(path, number))?;
    }
    let oldest = archive_path(path, ARCHIVE_COUNT);
    if oldest.exists() {
        fs::remove_file(oldest)?;
    }
    for number in (1..ARCHIVE_COUNT).rev() {
        let from = archive_path(path, number);
        if from.exists() {
            fs::rename(from, archive_path(path, number + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, archive_path(path, 1))?;
    }
    Ok(())
}

fn ensure_regular_file_or_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "diagnostic target is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn test_directory(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "markrust-diagnostics-test-{}-{}-{suffix}",
            std::process::id(),
            timestamp_ms()
        ))
    }

    #[test]
    fn diagnostic_path_is_absolute() {
        let path = diagnostics_log_path();
        assert!(path.is_absolute());
        assert!(path.ends_with(LOG_NAME));
    }

    #[test]
    fn categorization_does_not_emit_error_text() {
        let error = anyhow::Error::new(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private document /Users/person/secret.md",
        ));
        let event = DiagnosticEvent::OpenFailed {
            origin: OpenOrigin::Launch,
            target: OpenTarget::File,
            category: error_category(&error),
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains("permission_denied"));
        assert!(!line.contains("secret"));
        assert!(!line.contains("/Users"));
        assert_eq!(safe_source_file("/Users/person/private.rs"), "private.rs");
    }

    #[test]
    fn files_rotate_and_permissions_are_private() {
        let directory = test_directory("rotate");
        let path = directory.join(LOG_NAME);
        for _ in 0..8 {
            append_line(&path, b"{\"event\":\"test\"}\n", 50).unwrap();
        }
        assert!(path.exists());
        assert!(archive_path(&path, 1).exists());
        assert!(archive_path(&path, 2).exists());
        assert!(fs::metadata(&path).unwrap().len() <= 50);
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_log_is_preserved_and_made_private() {
        let directory = test_directory("legacy");
        fs::create_dir_all(&directory).unwrap();
        let legacy = directory.join("panics.log");
        fs::write(&legacy, b"old evidence").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o644)).unwrap();
        prepare_directory(&directory).unwrap();
        assert_eq!(fs::read(&legacy).unwrap(), b"old evidence");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&legacy).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn existing_archive_permissions_are_tightened_without_erasing_evidence() {
        let directory = test_directory("archive-permissions");
        fs::create_dir_all(&directory).unwrap();
        let active = directory.join(LOG_NAME);
        for path in [
            active.clone(),
            archive_path(&active, 1),
            archive_path(&active, 2),
        ] {
            fs::write(&path, b"old evidence\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        prepare_directory(&directory).unwrap();
        for path in [
            active.clone(),
            archive_path(&active, 1),
            archive_path(&active, 2),
        ] {
            assert_eq!(fs::read(&path).unwrap(), b"old evidence\n");
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rotation_keeps_the_newest_records_in_order() {
        let directory = test_directory("rotation-order");
        let active = directory.join(LOG_NAME);
        for value in 1..=8 {
            append_line(&active, format!("{value}\n").as_bytes(), 4).unwrap();
        }
        assert_eq!(fs::read(&active).unwrap(), b"7\n8\n");
        assert_eq!(fs::read(archive_path(&active, 1)).unwrap(), b"5\n6\n");
        assert_eq!(fs::read(archive_path(&active, 2)).unwrap(), b"3\n4\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unavailable_directory_fails_without_panicking() {
        let directory = test_directory("unavailable");
        fs::write(&directory, b"not a directory").unwrap();
        let result = append_line(&directory.join(LOG_NAME), b"{}\n", 100);
        assert!(result.is_err());
        fs::remove_file(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_log_target_is_rejected() {
        let directory = test_directory("symlink");
        fs::create_dir_all(&directory).unwrap();
        let outside = test_directory("outside");
        fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, directory.join(LOG_NAME)).unwrap();
        assert!(append_line(&directory.join(LOG_NAME), b"{}\n", 100).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        fs::remove_dir_all(directory).unwrap();
        fs::remove_file(outside).unwrap();
    }

    #[test]
    fn panic_hook_redacts_payload_in_child_process() {
        const CHILD_DIRECTORY: &str = "MARKRUST_PANIC_TEST_DIRECTORY";
        if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
            install_panic_logger_to(PathBuf::from(directory).join(LOG_NAME));
            panic!("secret document text at /Users/person/secret.md");
        }
        let directory = test_directory("panic-child");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crash::tests::panic_hook_redacts_payload_in_child_process",
            ])
            .env(CHILD_DIRECTORY, &directory)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let log = fs::read_to_string(directory.join(LOG_NAME)).unwrap();
        assert!(log.contains("\"event\":\"panic\""));
        assert!(!log.contains("secret document"));
        assert!(!log.contains("/Users/person"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("secret document"));
        fs::remove_dir_all(directory).unwrap();
    }
}
