// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Private, best-effort crash recovery for editor sessions.
//!
//! Recovery is deliberately a bounded safety net, not a second document store:
//! a snapshot may contain at most 16 MiB total, 4 MiB per tab, and 256 total
//! open or archived tab entries. When a snapshot exceeds those limits or local
//! storage is unavailable, MarkRust
//! preserves the live document and exposes a warning, but cannot promise that
//! an abrupt process or power loss will be recoverable.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Bump only for an incompatible on-disk recovery schema.
pub const RECOVERY_VERSION: u32 = 1;
pub const MAX_RECOVERY_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RECOVERY_TAB_BYTES: usize = 4 * 1024 * 1024;
/// A recovery snapshot has one shared entry budget for visible tabs and
/// archived dirty drafts. Keeping it finite bounds malformed local JSON while
/// still allowing large multi-document sessions.
pub const MAX_RECOVERY_TABS: usize = 256;
pub const MAX_RECOVERY_TITLE_BYTES: usize = 4 * 1024;
const SESSION_FILE: &str = "session-v1.json";
const BACKUP_FILE: &str = "session-v1.json.bak";
const WINDOW_DIRECTORY_PREFIX: &str = "window-";
const LEASE_FILE: &str = ".window-owner.lock";
const MAX_RECOVERY_WINDOWS: usize = 64;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryEditorMode {
    Wysiwyg,
    Source,
    Split,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryEditingPane {
    Source,
    Wysiwyg,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySelection {
    pub start: usize,
    pub end: usize,
    pub reversed: bool,
}

impl RecoverySelection {
    pub fn collapsed(offset: usize) -> Self {
        Self {
            start: offset,
            end: offset,
            reversed: false,
        }
    }

    fn validate(&self, content: &str) -> Result<(), RecoveryError> {
        if self.start > content.len()
            || self.end > content.len()
            || !content.is_char_boundary(self.start)
            || !content.is_char_boundary(self.end)
        {
            return Err(RecoveryError::InvalidSnapshot(
                "selection is outside the recovered UTF-8 document".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTab {
    pub path: Option<PathBuf>,
    pub title: String,
    pub mode: RecoveryEditorMode,
    pub editing_pane: RecoveryEditingPane,
    pub source_selection: RecoverySelection,
    pub rich_selection: RecoverySelection,
    /// Current in-memory contents, including unsaved edits.
    pub content: String,
    /// Last known on-disk contents. This remains available when disk changes
    /// while the app was not running, so recovery can avoid an unsafe autosave.
    pub saved_content: String,
    pub dirty: bool,
    /// Buffer-only merge or normalization decisions still require explicit
    /// Save after restart, even when disk matches the reconciled saved base.
    #[serde(default)]
    pub autosave_blocked: bool,
    /// Uncommitted rich-field text, including invalid YAML/URL and visible
    /// materialized IME preedit. It is never published by a private checkpoint.
    #[serde(default)]
    pub widget_draft: Option<RecoveryWidgetDraft>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryWidgetKind {
    BodyComposition,
    CodeInfo,
    ImageAlt,
    ImageProperties,
    FrontmatterTitle,
    FrontmatterDescription,
    FrontmatterTags,
    FrontmatterYaml,
    LinkDestination,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryWidgetDraft {
    pub kind: RecoveryWidgetKind,
    pub original_source: String,
    pub source_range: std::ops::Range<usize>,
    pub draft: String,
    pub selection: std::ops::Range<usize>,
    pub selection_reversed: bool,
}

impl From<markrust_editor::wysiwyg::WidgetDraftSnapshot> for RecoveryWidgetDraft {
    fn from(snapshot: markrust_editor::wysiwyg::WidgetDraftSnapshot) -> Self {
        use markrust_editor::wysiwyg::{FrontmatterField, WidgetDraftKind};
        let kind = match snapshot.kind {
            WidgetDraftKind::BodyComposition => RecoveryWidgetKind::BodyComposition,
            WidgetDraftKind::CodeInfo => RecoveryWidgetKind::CodeInfo,
            WidgetDraftKind::ImageAlt => RecoveryWidgetKind::ImageAlt,
            WidgetDraftKind::ImageProperties => RecoveryWidgetKind::ImageProperties,
            WidgetDraftKind::FrontmatterField(FrontmatterField::Title) => {
                RecoveryWidgetKind::FrontmatterTitle
            }
            WidgetDraftKind::FrontmatterField(FrontmatterField::Description) => {
                RecoveryWidgetKind::FrontmatterDescription
            }
            WidgetDraftKind::FrontmatterField(FrontmatterField::Tags) => {
                RecoveryWidgetKind::FrontmatterTags
            }
            WidgetDraftKind::FrontmatterYaml => RecoveryWidgetKind::FrontmatterYaml,
            WidgetDraftKind::LinkDestination => RecoveryWidgetKind::LinkDestination,
        };
        Self {
            kind,
            original_source: snapshot.original_source,
            source_range: snapshot.source_range,
            draft: snapshot.draft,
            selection: snapshot.selection,
            selection_reversed: snapshot.selection_reversed,
        }
    }
}

impl RecoveryWidgetDraft {
    pub fn editor_snapshot(&self) -> markrust_editor::wysiwyg::WidgetDraftSnapshot {
        use markrust_editor::wysiwyg::{FrontmatterField, WidgetDraftKind, WidgetDraftSnapshot};
        let kind = match self.kind {
            RecoveryWidgetKind::BodyComposition => WidgetDraftKind::BodyComposition,
            RecoveryWidgetKind::CodeInfo => WidgetDraftKind::CodeInfo,
            RecoveryWidgetKind::ImageAlt => WidgetDraftKind::ImageAlt,
            RecoveryWidgetKind::ImageProperties => WidgetDraftKind::ImageProperties,
            RecoveryWidgetKind::FrontmatterTitle => {
                WidgetDraftKind::FrontmatterField(FrontmatterField::Title)
            }
            RecoveryWidgetKind::FrontmatterDescription => {
                WidgetDraftKind::FrontmatterField(FrontmatterField::Description)
            }
            RecoveryWidgetKind::FrontmatterTags => {
                WidgetDraftKind::FrontmatterField(FrontmatterField::Tags)
            }
            RecoveryWidgetKind::FrontmatterYaml => WidgetDraftKind::FrontmatterYaml,
            RecoveryWidgetKind::LinkDestination => WidgetDraftKind::LinkDestination,
        };
        WidgetDraftSnapshot {
            kind,
            original_source: self.original_source.clone(),
            source_range: self.source_range.clone(),
            draft: self.draft.clone(),
            selection: self.selection.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    fn validate(&self) -> Result<(), RecoveryError> {
        if self.original_source.len() > MAX_RECOVERY_TAB_BYTES
            || self.draft.len() > MAX_RECOVERY_TAB_BYTES
        {
            return Err(RecoveryError::Limit(
                "rich-field draft exceeds the per-tab private recovery limit".into(),
            ));
        }
        if self.source_range.start > self.source_range.end
            || self
                .original_source
                .get(self.source_range.clone())
                .is_none()
            || self.selection.start > self.selection.end
            || self.draft.get(self.selection.clone()).is_none()
        {
            return Err(RecoveryError::InvalidSnapshot(
                "rich-field recovery anchor or selection is invalid UTF-8".into(),
            ));
        }
        Ok(())
    }
}

impl RecoveryTab {
    fn validate(&self) -> Result<(), RecoveryError> {
        if self.title.len() > MAX_RECOVERY_TITLE_BYTES {
            return Err(RecoveryError::Limit(format!(
                "tab title exceeds the {MAX_RECOVERY_TITLE_BYTES}-byte recovery limit"
            )));
        }
        if self.content.len() > MAX_RECOVERY_TAB_BYTES
            || self.saved_content.len() > MAX_RECOVERY_TAB_BYTES
        {
            return Err(RecoveryError::Limit(format!(
                "a tab exceeds the {MAX_RECOVERY_TAB_BYTES}-byte recovery limit"
            )));
        }
        self.source_selection.validate(&self.content)?;
        self.rich_selection.validate(&self.content)?;
        if let Some(draft) = &self.widget_draft {
            draft.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySnapshot {
    pub version: u32,
    #[serde(default)]
    pub root: Option<PathBuf>,
    pub active_tab: usize,
    pub tabs: Vec<RecoveryTab>,
    /// Dirty tabs closed during the prior session. Keeping them in the same
    /// bounded entry budget prevents a close gesture from silently discarding
    /// an unsaved buffer.
    #[serde(default)]
    pub archived_tabs: Vec<RecoveryTab>,
}

impl RecoverySnapshot {
    pub fn validate(&self) -> Result<(), RecoveryError> {
        if self.version != RECOVERY_VERSION {
            return Err(RecoveryError::InvalidSnapshot(format!(
                "unsupported recovery snapshot version {}",
                self.version
            )));
        }
        // A stale widget anchor may need its own pathless scratch owner on
        // restore; reserve that entry before an exit relies on the checkpoint.
        let fallback_count = self
            .tabs
            .iter()
            .chain(&self.archived_tabs)
            .filter(|tab| tab.widget_draft.is_some())
            .count();
        let total_tabs = self
            .tabs
            .len()
            .saturating_add(self.archived_tabs.len())
            .saturating_add(fallback_count);
        if total_tabs > MAX_RECOVERY_TABS {
            return Err(RecoveryError::Limit(format!(
                "snapshot has more than {MAX_RECOVERY_TABS} total tab entries"
            )));
        }
        if self.tabs.is_empty() {
            if self.active_tab != 0 {
                return Err(RecoveryError::InvalidSnapshot(
                    "empty recovery snapshot has an active tab".into(),
                ));
            }
        } else if self.active_tab >= self.tabs.len() {
            return Err(RecoveryError::InvalidSnapshot(
                "active tab is outside the recovery snapshot".into(),
            ));
        }
        for tab in self.tabs.iter().chain(&self.archived_tabs) {
            tab.validate()?;
        }
        Ok(())
    }

    /// Validate both structure and serialized size before a close operation
    /// relies on an archived dirty draft being recoverable.
    pub fn validate_for_write(&self) -> Result<(), RecoveryError> {
        let _ = encode_snapshot(self)?;
        Ok(())
    }
}

/// What a startup restore must do for a recovered tab after comparing the
/// snapshot's saved base with the current contents on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreKind {
    /// A clean tab should follow the current file contents rather than replay
    /// stale session metadata over it.
    CurrentDisk,
    /// Reapply the recovered buffer. `autosave_blocked` prevents a recovery
    /// from silently writing over a file that changed while MarkRust was down.
    Recovered {
        disk_changed: bool,
        autosave_blocked: bool,
    },
}

pub fn restore_kind(tab: &RecoveryTab, current_disk: Option<&str>) -> RestoreKind {
    if !tab.dirty && tab.widget_draft.is_none() && tab.content == tab.saved_content {
        // A file that disappeared while the app was down must not be treated
        // as a clean saved tab: a later autosave could recreate it over a new
        // file. Preserve the last buffer as an explicitly-saveable conflict.
        if tab.path.is_some() && current_disk.is_none() {
            return RestoreKind::Recovered {
                disk_changed: true,
                autosave_blocked: true,
            };
        }
        return RestoreKind::CurrentDisk;
    }
    if tab.path.is_none() {
        return RestoreKind::Recovered {
            disk_changed: false,
            autosave_blocked: tab.autosave_blocked,
        };
    }
    match current_disk {
        Some(disk) if disk == tab.saved_content => RestoreKind::Recovered {
            disk_changed: false,
            autosave_blocked: tab.autosave_blocked,
        },
        // The recovered edits are already present on disk. Reopen the disk
        // version cleanly rather than preserving a redundant dirty buffer.
        Some(disk) if disk == tab.content => RestoreKind::CurrentDisk,
        Some(_) | None => RestoreKind::Recovered {
            disk_changed: true,
            autosave_blocked: tab.path.is_some(),
        },
    }
}

/// Read a saved document only while restoring a session. This uses the same
/// hard cap as recovery snapshots so a hostile or unexpectedly large file
/// cannot turn startup recovery into an unbounded read.
pub fn read_text_for_restore(path: &Path) -> Result<Option<String>, RecoveryError> {
    match read_bounded_file(path, false) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| {
            RecoveryError::InvalidSnapshot(format!(
                "recovery target is not valid UTF-8: {}",
                path.display()
            ))
        }),
        Err(RecoveryError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub enum RecoveryError {
    Io(io::Error),
    Json(serde_json::Error),
    Limit(String),
    InvalidSnapshot(String),
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::Limit(message) | Self::InvalidSnapshot(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RecoveryError {}

impl From<io::Error> for RecoveryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for RecoveryError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// A user-visible recovery status. It intentionally holds no document text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryWarning {
    ReadFailed(String),
    RestoredBackup(String),
    WriteFailed(String),
    CapacityReached(String),
    DiskChanged(String),
    PendingWidget(String),
}

/// Result of a sequenced background checkpoint attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryWriteResult {
    Written,
    /// Superseded work or a retired window; no storage mutation occurred.
    SkippedStale,
}

impl RecoveryWarning {
    pub fn message(&self) -> &str {
        match self {
            Self::ReadFailed(message)
            | Self::RestoredBackup(message)
            | Self::WriteFailed(message)
            | Self::CapacityReached(message)
            | Self::DiskChanged(message)
            | Self::PendingWidget(message) => message,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecoveryLoad {
    pub snapshot: Option<RecoverySnapshot>,
    pub warning: Option<RecoveryWarning>,
}

/// Filesystem facade kept independent from GPUI so corruption and restart
/// behavior can be tested without opening a window or touching user data.
#[derive(Debug, Clone)]
pub struct RecoveryStore {
    directory: PathBuf,
    write_lock: Arc<Mutex<()>>,
    latest_generation: Arc<AtomicU64>,
    /// Retirement is shared with delayed checkpoint workers and readers. Its
    /// transition is serialized with writes before the shared lease is closed.
    retired: Arc<AtomicBool>,
    /// Live windows retain one stable owner-lease inode across checkpoints.
    /// Retirement can close its handle even while worker clones remain alive.
    lease: Arc<Mutex<Option<File>>>,
}

impl RecoveryStore {
    /// Compatibility allocation for callers that do not restore app sessions.
    /// It still gives the window an isolated leased store; only startup session
    /// discovery may reopen a persisted owner. Never write under the cwd.
    pub fn production() -> Option<Self> {
        Self::production_fresh().ok()
    }

    fn production_directory() -> Option<PathBuf> {
        dirs::data_local_dir()
            .or_else(dirs::data_dir)
            .or_else(dirs::home_dir)
            .map(|base| base.join("markrust").join("recovery"))
    }

    /// Each live window owns one checkpoint directory. Enumerate it once at
    /// application startup; constructing additional windows must use a fresh
    /// store, not reread the same legacy session into a second writable owner.
    pub fn production_sessions() -> Result<Vec<Self>, RecoveryError> {
        let directory = Self::production_directory().ok_or_else(|| {
            RecoveryError::InvalidSnapshot("No private recovery location is available".into())
        })?;
        Self::prepare_production_directory(&directory)?;
        let mut available = Vec::new();
        for store in Self::sessions_in(&directory)? {
            match store.claim() {
                Ok(store) => available.push(store),
                Err(RecoveryError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                    // Another application process still owns its live edits.
                    // Do not restore or checkpoint over that owner's session.
                }
                Err(error) => return Err(error),
            }
        }
        Ok(available)
    }

    pub fn production_fresh() -> Result<Self, RecoveryError> {
        let directory = Self::production_directory().ok_or_else(|| {
            RecoveryError::InvalidSnapshot("No private recovery location is available".into())
        })?;
        Self::prepare_production_directory(&directory)?;
        Self::fresh_in(&directory)
    }

    fn prepare_production_directory(directory: &Path) -> Result<(), RecoveryError> {
        // The OS-managed data location may legitimately have symlinked system
        // ancestors. The app-owned markrust/recovery components may not.
        if let Some(parent) = directory.parent() {
            Self::new(parent.to_path_buf()).ensure_private_directory()?;
        }
        Self::new(directory.to_path_buf()).ensure_private_directory()
    }

    fn sessions_in(directory: &Path) -> Result<Vec<Self>, RecoveryError> {
        let root = Self::new(directory.to_path_buf());
        root.ensure_private_directory()?;
        let mut sessions = Vec::new();
        // Keep the existing single-window snapshot readable without copying or
        // deleting it. Its window continues owning that legacy directory.
        if root.snapshot_path().try_exists()? || root.backup_path().try_exists()? {
            sessions.push(root.clone());
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(WINDOW_DIRECTORY_PREFIX)
            {
                continue;
            }
            let store = Self::new(entry.path());
            store.ensure_private_directory()?;
            // Empty directories from a failed initial checkpoint own no edits
            // and do not create empty windows on the next launch.
            if store.snapshot_path().try_exists()? || store.backup_path().try_exists()? {
                if sessions.len() >= MAX_RECOVERY_WINDOWS {
                    return Err(RecoveryError::Limit(format!(
                        "private recovery contains more than {MAX_RECOVERY_WINDOWS} window sessions"
                    )));
                }
                sessions.push(store);
            }
        }
        sessions.sort_by(|left, right| left.directory.cmp(&right.directory));
        Ok(sessions)
    }

    fn fresh_in(directory: &Path) -> Result<Self, RecoveryError> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let root = Self::new(directory.to_path_buf());
        root.ensure_private_directory()?;
        if Self::sessions_in(directory)?.len() >= MAX_RECOVERY_WINDOWS {
            return Err(RecoveryError::Limit(format!(
                "private recovery allows at most {MAX_RECOVERY_WINDOWS} window sessions"
            )));
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let store = Self::new(directory.join(format!(
            "{WINDOW_DIRECTORY_PREFIX}{nonce:032x}-{}-{sequence}",
            std::process::id()
        )));
        // create_dir rather than create_dir_all makes allocation exclusive.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&store.directory)?;
        }
        #[cfg(not(unix))]
        fs::create_dir(&store.directory)?;
        store.ensure_private_directory()?;
        sync_directory(directory)?;
        store.claim()
    }

    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            write_lock: Arc::new(Mutex::new(())),
            latest_generation: Arc::new(AtomicU64::new(0)),
            retired: Arc::new(AtomicBool::new(false)),
            lease: Arc::new(Mutex::new(None)),
        }
    }

    fn claim(self) -> Result<Self, RecoveryError> {
        let write_lock = self.write_lock.clone();
        let _write_guard = write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_private_directory()?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lease_path = self.directory.join(LEASE_FILE);
        let file = options.open(&lease_path)?;
        let metadata = file.metadata()?;
        ensure_owned(&metadata, &lease_path)?;
        if !metadata.is_file() {
            return Err(RecoveryError::InvalidSnapshot(
                "recovery owner lease is not a regular file".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.nlink() != 1 {
                return Err(RecoveryError::InvalidSnapshot(
                    "recovery owner lease must not be hard-linked".into(),
                ));
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            // flock has no pointer or lifetime contract. The shared lease
            // below keeps the descriptor open across live worker clones.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        *self
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(file);
        Ok(self)
    }

    #[cfg(test)]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.directory.join(SESSION_FILE)
    }

    pub fn backup_path(&self) -> PathBuf {
        self.directory.join(BACKUP_FILE)
    }

    /// Retire only a durably checkpointed clean closed window. Delete the old
    /// backup first so it cannot resurrect a previously dirty session after
    /// the primary disappears. Never recursively remove a recovery directory.
    pub fn retire_clean_session(&self) -> Result<(), RecoveryError> {
        let _write_guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.retired.load(Ordering::Acquire) {
            return Ok(());
        }
        self.ensure_private_directory()?;
        let snapshot = self.read_snapshot(&self.snapshot_path())?.ok_or_else(|| {
            RecoveryError::InvalidSnapshot(
                "cannot retire a window without a current checkpoint".into(),
            )
        })?;
        if !snapshot.archived_tabs.is_empty()
            || snapshot.tabs.iter().any(|tab| {
                tab.dirty || tab.widget_draft.is_some() || tab.content != tab.saved_content
            })
        {
            return Err(RecoveryError::InvalidSnapshot(
                "cannot retire a window containing unsaved drafts".into(),
            ));
        }
        match fs::remove_file(self.backup_path()) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            result => result?,
        }
        sync_directory(&self.directory)?;
        fs::remove_file(self.snapshot_path())?;
        sync_directory(&self.directory)?;
        // No retained worker may reopen this session after its checkpoints
        // have been durably removed. Set the barrier before closing any handle.
        self.retired.store(true, Ordering::Release);
        #[cfg(not(unix))]
        self.close_lease();
        // Only a durably retired clean window may remove its lease pathname.
        // Live checkpoints must keep the locked inode reachable, otherwise a
        // second process could create and lock a different owner-lease inode.
        // Windows also needs every shared handle closed before directory
        // removal; share-delete alone does not complete file deletion.
        let cleanup = (|| -> Result<(), RecoveryError> {
            match fs::remove_file(self.directory.join(LEASE_FILE)) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                result => result?,
            }
            sync_directory(&self.directory)?;
            if self
                .directory
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(WINDOW_DIRECTORY_PREFIX))
            {
                match fs::remove_dir(&self.directory) {
                    Ok(()) => {
                        if let Some(parent) = self.directory.parent() {
                            sync_directory(parent)?;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        })();
        // Unix keeps the original flock until its pathname and clean window
        // directory have been removed, including when cleanup reports an error.
        #[cfg(unix)]
        self.close_lease();
        cleanup
    }

    pub fn load(&self) -> RecoveryLoad {
        let _write_guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.retired.load(Ordering::Acquire) {
            return RecoveryLoad {
                snapshot: None,
                warning: None,
            };
        }
        if let Err(error) = self.ensure_private_directory() {
            return RecoveryLoad {
                snapshot: None,
                warning: Some(RecoveryWarning::ReadFailed(format!(
                    "Could not access private session recovery storage: {error}"
                ))),
            };
        }
        match self.read_snapshot(&self.snapshot_path()) {
            Ok(Some(snapshot)) => RecoveryLoad {
                snapshot: Some(snapshot),
                warning: None,
            },
            Ok(None) => self.load_backup_after("no primary recovery snapshot was found"),
            Err(primary) => self.load_backup_after(&format!(
                "the primary recovery snapshot could not be read: {primary}"
            )),
        }
    }

    fn load_backup_after(&self, primary_message: &str) -> RecoveryLoad {
        match self.read_snapshot(&self.backup_path()) {
            Ok(Some(snapshot)) => RecoveryLoad {
                snapshot: Some(snapshot),
                warning: Some(RecoveryWarning::RestoredBackup(format!(
                    "Recovered the previous session backup because {primary_message}."
                ))),
            },
            Ok(None) => RecoveryLoad {
                snapshot: None,
                warning: if primary_message == "no primary recovery snapshot was found" {
                    None
                } else {
                    Some(RecoveryWarning::ReadFailed(primary_message.to_string()))
                },
            },
            Err(backup) => RecoveryLoad {
                snapshot: None,
                warning: Some(RecoveryWarning::ReadFailed(format!(
                    "{primary_message}; the backup could not be read: {backup}"
                ))),
            },
        }
    }

    #[allow(dead_code)] // Used by isolated recovery tests and maintenance tools.
    pub fn write(&self, snapshot: &RecoverySnapshot) -> Result<(), RecoveryError> {
        // Debounced tasks can overlap briefly while a previous fsync is in
        // progress. Serializing writes keeps an older snapshot from
        // completing after a newer one and replacing it.
        let _write_guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.write_locked(snapshot)
    }

    /// Record that `generation` is the newest requested checkpoint. This is
    /// intentionally lock-free because it is called from input-driven UI code.
    pub fn note_generation(&self, generation: u64) {
        self.latest_generation
            .fetch_max(generation, Ordering::Release);
    }

    /// Write only if no later checkpoint was requested. The admission test is
    /// inside the write mutex, so a worker delayed before locking cannot commit
    /// an older snapshot after a newer one.
    pub fn write_if_current(
        &self,
        snapshot: &RecoverySnapshot,
        generation: u64,
    ) -> Result<RecoveryWriteResult, RecoveryError> {
        let _write_guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.retired.load(Ordering::Acquire)
            || self.latest_generation.load(Ordering::Acquire) != generation
        {
            return Ok(RecoveryWriteResult::SkippedStale);
        }
        self.write_locked(snapshot)?;
        Ok(RecoveryWriteResult::Written)
    }

    fn write_locked(&self, snapshot: &RecoverySnapshot) -> Result<(), RecoveryError> {
        self.ensure_not_retired()?;
        let encoded = encode_snapshot(snapshot)?;
        self.ensure_private_directory()?;

        let current = self.snapshot_path();
        let temp = self.temp_path("session");
        if let Err(error) = self.write_private_file(&temp, &encoded) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }

        let outcome = (|| {
            // Rotate only a fully parseable, schema-valid primary. If a
            // corrupt primary was already recovered through a valid backup,
            // copying its raw bytes here would destroy that last good backup.
            if let Some(old) = self.valid_current_snapshot_bytes(&current) {
                self.replace_backup(&old)?;
            }
            fs::rename(&temp, &current)?;
            sync_directory(&self.directory)?;
            Ok(())
        })();

        if outcome.is_err() {
            let _ = fs::remove_file(&temp);
        }
        outcome
    }

    fn valid_current_snapshot_bytes(&self, path: &Path) -> Option<Vec<u8>> {
        let bytes = read_bounded(path).ok()?;
        let snapshot: RecoverySnapshot = serde_json::from_slice(&bytes).ok()?;
        snapshot.validate().ok()?;
        Some(bytes)
    }

    fn replace_backup(&self, bytes: &[u8]) -> Result<(), RecoveryError> {
        let backup_temp = self.temp_path("backup");
        let outcome = (|| {
            self.write_private_file(&backup_temp, bytes)?;
            fs::rename(&backup_temp, self.backup_path())?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(&backup_temp);
        }
        outcome
    }

    fn read_snapshot(&self, path: &Path) -> Result<Option<RecoverySnapshot>, RecoveryError> {
        let bytes = match read_bounded(path) {
            Err(RecoveryError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None)
            }
            result => result?,
        };
        let snapshot: RecoverySnapshot = serde_json::from_slice(&bytes)?;
        snapshot.validate()?;
        Ok(Some(snapshot))
    }

    fn ensure_private_directory(&self) -> Result<(), RecoveryError> {
        self.ensure_not_retired()?;
        let mut created = false;
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(RecoveryError::InvalidSnapshot(format!(
                        "recovery directory is not a real directory: {}",
                        self.directory.display()
                    )));
                }
                ensure_owned(&metadata, &self.directory)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(&self.directory)?;
                }
                #[cfg(not(unix))]
                fs::create_dir_all(&self.directory)?;
                created = true;
                let metadata = fs::symlink_metadata(&self.directory)?;
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(RecoveryError::InvalidSnapshot(format!(
                        "recovery directory is not a real directory: {}",
                        self.directory.display()
                    )));
                }
                ensure_owned(&metadata, &self.directory)?;
            }
            Err(error) => return Err(error.into()),
        }
        set_private_directory_permissions(&self.directory)?;
        if created {
            if let Some(parent) = self.directory.parent() {
                sync_directory(parent)?;
            }
        }
        Ok(())
    }

    fn ensure_not_retired(&self) -> Result<(), RecoveryError> {
        if self.retired.load(Ordering::Acquire) {
            return Err(RecoveryError::InvalidSnapshot(
                "cannot reopen a retired recovery session".into(),
            ));
        }
        Ok(())
    }

    fn close_lease(&self) {
        let lease = self
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(lease);
    }

    fn temp_path(&self, label: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.directory.join(format!(
            ".{SESSION_FILE}.{label}.{}.{sequence}.tmp",
            std::process::id()
        ))
    }

    fn write_private_file(&self, path: &Path, bytes: &[u8]) -> Result<(), RecoveryError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        set_private_file_permissions(path)?;
        Ok(())
    }
}

/// Serializer sink that refuses to allocate beyond the on-disk snapshot cap.
/// `serde_json::to_vec` would first allocate an arbitrarily expanded escaped
/// payload and only then let us reject it.
struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "recovery snapshot size overflow",
            ));
        };
        if next_len > self.limit {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "recovery snapshot size limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_snapshot(snapshot: &RecoverySnapshot) -> Result<Vec<u8>, RecoveryError> {
    snapshot.validate()?;
    let mut writer = BoundedWriter::new(MAX_RECOVERY_SNAPSHOT_BYTES);
    let serialization = serde_json::to_writer(&mut writer, snapshot);
    if writer.exceeded {
        return Err(RecoveryError::Limit(format!(
            "session recovery exceeds the {MAX_RECOVERY_SNAPSHOT_BYTES}-byte limit"
        )));
    }
    serialization?;
    Ok(writer.bytes)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, RecoveryError> {
    read_bounded_file(path, true)
}

fn read_bounded_file(path: &Path, private: bool) -> Result<Vec<u8>, RecoveryError> {
    let metadata = if private {
        fs::symlink_metadata(path)?
    } else {
        fs::metadata(path)?
    };
    if !metadata.file_type().is_file() {
        return Err(RecoveryError::InvalidSnapshot(format!(
            "recovery path is not a regular file: {}",
            path.display()
        )));
    }
    if private {
        ensure_owned(&metadata, path)?;
    }
    if metadata.len() > MAX_RECOVERY_SNAPSHOT_BYTES as u64 {
        return Err(RecoveryError::Limit(format!(
            "recovery snapshot exceeds the {MAX_RECOVERY_SNAPSHOT_BYTES}-byte limit"
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Refuse a replaced symlink between symlink_metadata and open.
        if private {
            options.custom_flags(libc::O_NOFOLLOW);
        }
    }
    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    if private {
        ensure_owned(&opened, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if opened.nlink() != 1 {
                return Err(RecoveryError::InvalidSnapshot(
                    "private recovery must not be hard-linked".into(),
                ));
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
    }
    if !opened.is_file() {
        return Err(RecoveryError::InvalidSnapshot(
            "recovery target changed file type while reading".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_RECOVERY_SNAPSHOT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECOVERY_SNAPSHOT_BYTES {
        return Err(RecoveryError::Limit(format!(
            "recovery snapshot exceeds the {MAX_RECOVERY_SNAPSHOT_BYTES}-byte limit"
        )));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn ensure_owned(metadata: &fs::Metadata, path: &Path) -> Result<(), RecoveryError> {
    use std::os::unix::fs::MetadataExt;
    // No pointer or lifetime contract; POSIX geteuid simply returns the
    // effective numeric owner of this process.
    let owner = unsafe { libc::geteuid() };
    if metadata.uid() != owner {
        return Err(RecoveryError::InvalidSnapshot(format!(
            "private recovery path is owned by another user: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_owned(_metadata: &fs::Metadata, _path: &Path) -> Result<(), RecoveryError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), RecoveryError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), RecoveryError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), RecoveryError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), RecoveryError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), RecoveryError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), RecoveryError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "markrust-recovery-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tab(path: Option<PathBuf>, content: &str, saved_content: &str, dirty: bool) -> RecoveryTab {
        RecoveryTab {
            path,
            title: "Untitled".into(),
            mode: RecoveryEditorMode::Split,
            editing_pane: RecoveryEditingPane::Wysiwyg,
            source_selection: RecoverySelection {
                start: 0,
                end: content.len(),
                reversed: true,
            },
            rich_selection: RecoverySelection::collapsed(content.len()),
            content: content.into(),
            saved_content: saved_content.into(),
            dirty,
            autosave_blocked: false,
            widget_draft: None,
        }
    }

    fn one_tab_snapshot(tab: RecoveryTab) -> RecoverySnapshot {
        RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab],
            archived_tabs: vec![],
        }
    }

    #[test]
    fn independent_window_stores_never_replace_each_others_drafts() {
        let temp = TempDir::new("window-isolation");
        let first = RecoveryStore::fresh_in(temp.path()).unwrap();
        let second = RecoveryStore::fresh_in(temp.path()).unwrap();
        first
            .write(&one_tab_snapshot(tab(None, "first", "", true)))
            .unwrap();
        second
            .write(&one_tab_snapshot(tab(None, "second", "", true)))
            .unwrap();
        first
            .write(&one_tab_snapshot(tab(None, "latest first", "", true)))
            .unwrap();
        assert_ne!(first.directory, second.directory);
        assert_eq!(
            first.load().snapshot.unwrap().tabs[0].content,
            "latest first"
        );
        assert_eq!(second.load().snapshot.unwrap().tabs[0].content, "second");
        assert_eq!(RecoveryStore::sessions_in(temp.path()).unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn live_window_lease_cannot_be_claimed_by_a_second_owner() {
        let temp = TempDir::new("exclusive-owner");
        let owner = RecoveryStore::fresh_in(temp.path()).unwrap();
        let directory = owner.directory.clone();
        let retained_clone = owner.clone();
        drop(owner);
        assert!(
            matches!(RecoveryStore::new(directory.clone()).claim(), Err(RecoveryError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(retained_clone);
        assert!(RecoveryStore::new(directory).claim().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn live_window_lease_survives_checkpoints_and_backup_rotations() {
        use std::os::unix::fs::MetadataExt;

        let temp = TempDir::new("checkpoint-exclusive-owner");
        let owner = RecoveryStore::fresh_in(temp.path()).unwrap();
        let directory = owner.directory.clone();
        let lease_path = directory.join(LEASE_FILE);
        let lease_inode = fs::metadata(&lease_path).unwrap().ino();

        for (index, content) in ["first draft", "second draft", "latest draft"]
            .into_iter()
            .enumerate()
        {
            owner
                .write(&one_tab_snapshot(tab(None, content, "", true)))
                .unwrap();
            assert_eq!(fs::metadata(&lease_path).unwrap().ino(), lease_inode);
            assert!(
                matches!(RecoveryStore::new(directory.clone()).claim(), Err(RecoveryError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
            );
            assert_eq!(owner.load().snapshot.unwrap().tabs[0].content, content);
            assert_eq!(owner.backup_path().exists(), index != 0);
        }

        let retained_worker = owner.clone();
        drop(owner);
        assert!(
            matches!(RecoveryStore::new(directory.clone()).claim(), Err(RecoveryError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(retained_worker);

        let restored_owner = RecoveryStore::new(directory).claim().unwrap();
        assert_eq!(fs::metadata(&lease_path).unwrap().ino(), lease_inode);
        assert_eq!(
            restored_owner.load().snapshot.unwrap().tabs[0].content,
            "latest draft"
        );
    }

    #[test]
    fn clean_closed_window_retirement_does_not_resurrect_a_dirty_backup() {
        let temp = TempDir::new("retire-clean");
        for _ in 0..(MAX_RECOVERY_WINDOWS + 2) {
            let store = RecoveryStore::fresh_in(temp.path()).unwrap();
            store
                .write(&one_tab_snapshot(tab(None, "old draft", "", true)))
                .unwrap();
            store
                .write(&one_tab_snapshot(tab(
                    None,
                    "published",
                    "published",
                    false,
                )))
                .unwrap();
            assert!(store.backup_path().exists());
            store.retire_clean_session().unwrap();
        }
        assert!(RecoveryStore::sessions_in(temp.path()).unwrap().is_empty());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn retained_worker_and_reader_clones_cannot_resurrect_a_retired_window() {
        let temp = TempDir::new("retire-retained-clones");
        let store = RecoveryStore::fresh_in(temp.path()).unwrap();
        let directory = store.directory.clone();
        let dirty = one_tab_snapshot(tab(None, "older private draft", "", true));
        store.write(&dirty).unwrap();
        store
            .write(&one_tab_snapshot(tab(None, "saved", "saved", false)))
            .unwrap();
        let worker = store.clone();
        let reader = store.clone();
        store.note_generation(3);
        let (start, delayed) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            delayed.recv().unwrap();
            assert_eq!(
                worker.write_if_current(&dirty, 3).unwrap(),
                RecoveryWriteResult::SkippedStale
            );
            assert!(matches!(
                worker.write(&dirty),
                Err(RecoveryError::InvalidSnapshot(_))
            ));
            let loaded = worker.load();
            assert!(loaded.snapshot.is_none());
            assert!(loaded.warning.is_none());
        });

        store.retire_clean_session().unwrap();
        assert!(store
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none());
        // The clones remain alive while the lease handle is closed and the
        // directory is deleted, which is essential for Windows cleanup.
        assert!(!directory.exists());
        start.send(()).unwrap();
        task.join().unwrap();
        reader.note_generation(4);
        assert_eq!(
            reader
                .write_if_current(&one_tab_snapshot(tab(None, "late", "", true)), 4)
                .unwrap(),
            RecoveryWriteResult::SkippedStale
        );
        assert!(reader.load().snapshot.is_none());
        reader.retire_clean_session().unwrap();
        assert!(reader.claim().is_err());
        assert!(!directory.exists());
        assert!(RecoveryStore::sessions_in(temp.path()).unwrap().is_empty());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn retired_legacy_store_keeps_its_root_empty_without_recreating_a_checkpoint() {
        let temp = TempDir::new("retire-legacy");
        let directory = temp.path().join("legacy-recovery");
        let store = RecoveryStore::new(directory.clone()).claim().unwrap();
        let retained = store.clone();
        let clean = one_tab_snapshot(tab(None, "saved", "saved", false));
        store.write(&clean).unwrap();
        store.retire_clean_session().unwrap();
        assert!(directory.is_dir());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        assert!(retained.write(&clean).is_err());
        let loaded = retained.load();
        assert!(loaded.snapshot.is_none());
        assert!(loaded.warning.is_none());
        assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    }

    #[test]
    fn rejected_dirty_retirement_keeps_the_shared_lease_and_checkpoint_admission() {
        let temp = TempDir::new("retire-dirty-admission");
        let store = RecoveryStore::fresh_in(temp.path()).unwrap();
        let retained = store.clone();
        store
            .write(&one_tab_snapshot(tab(None, "unsaved", "", true)))
            .unwrap();
        assert!(store.retire_clean_session().is_err());
        assert!(!store.retired.load(Ordering::Acquire));
        assert!(store
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some());
        let newer = one_tab_snapshot(tab(None, "new unsaved edits", "", true));
        retained.note_generation(2);
        assert_eq!(
            retained.write_if_current(&newer, 2).unwrap(),
            RecoveryWriteResult::Written
        );
        assert_eq!(store.load().snapshot.unwrap(), newer);
        assert!(store.directory.join(LEASE_FILE).is_file());
    }

    #[test]
    fn dirty_closed_window_and_raw_widget_drafts_cannot_be_retired() {
        let temp = TempDir::new("retain-dirty");
        let store = RecoveryStore::fresh_in(temp.path()).unwrap();
        store
            .write(&one_tab_snapshot(tab(None, "draft", "", true)))
            .unwrap();
        assert!(store.retire_clean_session().is_err());
        assert_eq!(store.load().snapshot.unwrap().tabs[0].content, "draft");
        let mut clean_body = tab(None, "[label](url)", "[label](url)", false);
        clean_body.widget_draft = Some(RecoveryWidgetDraft {
            kind: RecoveryWidgetKind::LinkDestination,
            original_source: clean_body.content.clone(),
            source_range: 8..11,
            draft: "incomplete URL 👩‍🚀".into(),
            selection: 0..0,
            selection_reversed: false,
        });
        let snapshot = one_tab_snapshot(clean_body.clone());
        store.write(&snapshot).unwrap();
        assert!(store.retire_clean_session().is_err());
        assert_eq!(store.load().snapshot.unwrap(), snapshot);
        assert_eq!(
            clean_body
                .widget_draft
                .clone()
                .unwrap()
                .editor_snapshot()
                .raw_draft_text(),
            "incomplete URL 👩‍🚀"
        );
    }

    #[test]
    fn old_recovery_json_without_widget_draft_remains_readable() {
        let original = tab(None, "draft", "", true);
        let mut json = serde_json::to_value(&original).unwrap();
        json.as_object_mut().unwrap().remove("widget_draft");
        assert_eq!(
            serde_json::from_value::<RecoveryTab>(json)
                .unwrap()
                .widget_draft,
            None
        );
    }

    #[test]
    fn display_only_body_composition_round_trips_without_changing_markdown() {
        use markrust_editor::wysiwyg::{WidgetDraftKind, WidgetDraftSnapshot};
        let original = "original body\n";
        let snapshot = WidgetDraftSnapshot {
            kind: WidgetDraftKind::BodyComposition,
            original_source: original.into(),
            source_range: 4..4,
            draft: "👩‍🚀 candidate text".into(),
            selection: 0..0,
            selection_reversed: false,
        };
        let mut body = tab(None, original, original, false);
        body.widget_draft = Some(snapshot.clone().into());
        let encoded = encode_snapshot(&one_tab_snapshot(body)).unwrap();
        let decoded: RecoverySnapshot = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.tabs[0].content, original);
        assert_eq!(
            decoded.tabs[0]
                .widget_draft
                .as_ref()
                .unwrap()
                .editor_snapshot(),
            snapshot
        );
    }

    #[test]
    fn image_properties_round_trip_as_private_draft_not_published_markdown() {
        use markrust_editor::wysiwyg::{WidgetDraftKind, WidgetDraftSnapshot};
        let original = "![old](image.png)";
        let draft =
            "Image draft (edit)\nLocation bytes: 13\nnext.png 😺\nAlternative text:\nnew alt";
        let editor = WidgetDraftSnapshot {
            kind: WidgetDraftKind::ImageProperties,
            original_source: original.into(),
            source_range: 0..original.len(),
            draft: draft.into(),
            selection: 0..draft.len(),
            selection_reversed: false,
        };
        let mut item = tab(None, original, original, false);
        item.widget_draft = Some(editor.clone().into());
        let encoded = encode_snapshot(&one_tab_snapshot(item)).unwrap();
        let decoded: RecoverySnapshot = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.tabs[0].content, original);
        assert_eq!(
            decoded.tabs[0]
                .widget_draft
                .as_ref()
                .unwrap()
                .editor_snapshot(),
            editor
        );
        assert!(matches!(
            restore_kind(&decoded.tabs[0], None),
            RestoreKind::Recovered { .. }
        ));
    }

    #[test]
    fn widget_payload_budget_and_utf8_anchor_are_checked_before_write() {
        let mut item = tab(None, "body", "", true);
        item.widget_draft = Some(RecoveryWidgetDraft {
            kind: RecoveryWidgetKind::FrontmatterYaml,
            original_source: "é".into(),
            source_range: 1..2,
            draft: "raw".into(),
            selection: 0..0,
            selection_reversed: false,
        });
        assert!(matches!(
            one_tab_snapshot(item.clone()).validate_for_write(),
            Err(RecoveryError::InvalidSnapshot(_))
        ));
        let draft = item.widget_draft.as_mut().unwrap();
        draft.source_range = 0..2;
        draft.draft = "x".repeat(MAX_RECOVERY_TAB_BYTES + 1);
        assert!(matches!(
            one_tab_snapshot(item).validate_for_write(),
            Err(RecoveryError::Limit(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_private_snapshot_and_symlinked_owner_lease_are_rejected() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new("private-link-safety");
        let store = RecoveryStore::new(temp.path().join("window-private"));
        store
            .write(&one_tab_snapshot(tab(None, "private", "", true)))
            .unwrap();
        fs::hard_link(store.snapshot_path(), temp.path().join("other-link")).unwrap();
        assert!(store.load().snapshot.is_none());
        let target = temp.path().join("lease-sentinel");
        fs::write(&target, "sentinel").unwrap();
        symlink(&target, store.directory.join(LEASE_FILE)).unwrap();
        assert!(store.claim().is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "sentinel");
    }

    #[test]
    fn restart_round_trip_preserves_unicode_tab_order_and_selections() {
        let temp = TempDir::new("unicode-order");
        let store = RecoveryStore::new(temp.path().join("private-recovery"));
        let first = tab(None, "Привет 👋", "", true);
        let second = tab(
            Some(PathBuf::from("/tmp/日本語.md")),
            "second",
            "base",
            true,
        );
        let snapshot = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: Some(PathBuf::from("/tmp/工作")),
            active_tab: 1,
            tabs: vec![first.clone(), second.clone()],
            archived_tabs: vec![],
        };

        store.write(&snapshot).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.warning, None);
        assert_eq!(loaded.snapshot.unwrap(), snapshot);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(store.directory())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(store.snapshot_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn corrupted_primary_recovers_previous_atomic_backup() {
        let temp = TempDir::new("corruption");
        let store = RecoveryStore::new(temp.path().join("private-recovery"));
        let old = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "old", "", true)],
            archived_tabs: vec![],
        };
        let new = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "new", "", true)],
            archived_tabs: vec![],
        };
        store.write(&old).unwrap();
        store.write(&new).unwrap();
        fs::write(store.snapshot_path(), b"not valid json").unwrap();

        let loaded = store.load();
        assert_eq!(loaded.snapshot.unwrap(), old);
        assert!(matches!(
            loaded.warning,
            Some(RecoveryWarning::RestoredBackup(_))
        ));
    }

    #[test]
    fn changed_disk_blocks_autosave_for_recovered_dirty_file() {
        let saved = "before";
        let recovered = tab(Some(PathBuf::from("/tmp/note.md")), "draft", saved, true);
        assert_eq!(
            restore_kind(&recovered, Some("edited elsewhere")),
            RestoreKind::Recovered {
                disk_changed: true,
                autosave_blocked: true,
            }
        );
        assert_eq!(
            restore_kind(&recovered, Some(saved)),
            RestoreKind::Recovered {
                disk_changed: false,
                autosave_blocked: false,
            }
        );
        assert_eq!(
            restore_kind(&recovered, Some("draft")),
            RestoreKind::CurrentDisk
        );
    }

    #[test]
    fn explicit_save_boundary_survives_checkpoint_and_restart() {
        let temp = TempDir::new("explicit-save-boundary");
        let store = RecoveryStore::new(temp.path().join("private-recovery"));
        let mut blocked = tab(
            Some(temp.path().join("note.md")),
            "mine and merged edits",
            "external disk version",
            true,
        );
        blocked.autosave_blocked = true;
        let snapshot = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![blocked],
            archived_tabs: vec![],
        };
        store.write(&snapshot).unwrap();
        let loaded = store.load().snapshot.unwrap();
        assert!(loaded.tabs[0].autosave_blocked);
        assert_eq!(
            restore_kind(&loaded.tabs[0], Some("external disk version")),
            RestoreKind::Recovered {
                disk_changed: false,
                autosave_blocked: true,
            }
        );
        // If an explicit save already published the buffer, no unsaved draft
        // remains for the block to protect.
        assert_eq!(
            restore_kind(&loaded.tabs[0], Some("mine and merged edits")),
            RestoreKind::CurrentDisk
        );
    }

    #[test]
    fn legacy_recovery_tabs_without_autosave_block_remain_readable() {
        let original = tab(Some(PathBuf::from("/tmp/legacy.md")), "draft", "base", true);
        let mut old_json = serde_json::to_value(&original).unwrap();
        old_json.as_object_mut().unwrap().remove("autosave_blocked");
        let restored: RecoveryTab = serde_json::from_value(old_json).unwrap();
        assert!(!restored.autosave_blocked);
        assert_eq!(restored, original);
        assert_eq!(
            restore_kind(&restored, Some("base")),
            RestoreKind::Recovered {
                disk_changed: false,
                autosave_blocked: false,
            }
        );
        assert_eq!(
            restore_kind(&restored, Some("external")),
            RestoreKind::Recovered {
                disk_changed: true,
                autosave_blocked: true,
            }
        );
    }

    #[test]
    fn pathless_normalization_draft_retains_explicit_save_boundary() {
        let mut normalized = tab(None, "normalized draft", "original draft", true);
        normalized.autosave_blocked = true;
        assert_eq!(
            restore_kind(&normalized, None),
            RestoreKind::Recovered {
                disk_changed: false,
                autosave_blocked: true,
            }
        );
    }

    #[test]
    fn missing_clean_file_becomes_an_explicit_save_conflict() {
        let clean = tab(
            Some(PathBuf::from("/tmp/missing.md")),
            "saved",
            "saved",
            false,
        );
        assert_eq!(
            restore_kind(&clean, None),
            RestoreKind::Recovered {
                disk_changed: true,
                autosave_blocked: true,
            }
        );
    }

    #[test]
    fn stale_checkpoint_cannot_replace_a_newer_requested_generation() {
        let temp = TempDir::new("generation");
        let store = RecoveryStore::new(temp.path().join("private-recovery"));
        let old = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "old", "", true)],
            archived_tabs: vec![],
        };
        let newest = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "newest", "", true)],
            archived_tabs: vec![],
        };

        store.note_generation(2);
        assert_eq!(
            store.write_if_current(&old, 1).unwrap(),
            RecoveryWriteResult::SkippedStale
        );
        assert!(!store.snapshot_path().exists());
        assert_eq!(
            store.write_if_current(&newest, 2).unwrap(),
            RecoveryWriteResult::Written
        );
        assert_eq!(store.load().snapshot.unwrap(), newest);
    }

    #[test]
    fn corrupt_primary_never_rotates_over_the_last_valid_backup() {
        let temp = TempDir::new("preserve-backup");
        let store = RecoveryStore::new(temp.path().join("private-recovery"));
        let oldest = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "oldest", "", true)],
            archived_tabs: vec![],
        };
        let current = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "current", "", true)],
            archived_tabs: vec![],
        };
        let replacement = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "replacement", "", true)],
            archived_tabs: vec![],
        };

        store.write(&oldest).unwrap();
        store.write(&current).unwrap();
        fs::write(store.snapshot_path(), b"corrupt primary").unwrap();
        store.write(&replacement).unwrap();

        assert_eq!(
            store.read_snapshot(&store.backup_path()).unwrap().unwrap(),
            oldest
        );
        assert_eq!(store.load().snapshot.unwrap(), replacement);
    }

    #[test]
    fn combined_open_and_archived_tab_budget_is_enforced() {
        let snapshot = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: (0..MAX_RECOVERY_TABS)
                .map(|_| tab(None, "", "", false))
                .collect(),
            archived_tabs: vec![tab(None, "draft", "", true)],
        };
        assert!(matches!(snapshot.validate(), Err(RecoveryError::Limit(_))));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_recovery_directory_is_rejected_without_chmodding_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = TempDir::new("directory-symlink");
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let recovery_link = temp.path().join("recovery-link");
        symlink(&target, &recovery_link).unwrap();
        let store = RecoveryStore::new(recovery_link);
        let snapshot = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "draft", "", true)],
            archived_tabs: vec![],
        };

        assert!(matches!(
            store.write(&snapshot),
            Err(RecoveryError::InvalidSnapshot(_))
        ));
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn malformed_or_oversized_selection_is_rejected_before_restore() {
        let mut snapshot = RecoverySnapshot {
            version: RECOVERY_VERSION,
            root: None,
            active_tab: 0,
            tabs: vec![tab(None, "é", "", true)],
            archived_tabs: vec![],
        };
        snapshot.tabs[0].source_selection = RecoverySelection {
            start: 1,
            end: 2,
            reversed: false,
        };
        assert!(matches!(
            snapshot.validate(),
            Err(RecoveryError::InvalidSnapshot(_))
        ));
    }
}
