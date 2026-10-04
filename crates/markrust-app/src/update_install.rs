// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A two-phase, same-volume macOS update handoff. No document or recovery path
//! is read here. The current trusted executable, never the download, applies it.

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{ensure, Context, Result};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::{
        ffi::CString,
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        },
        path::{Component, Path, PathBuf},
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    const PLAN_LIMIT: u64 = 64 * 1024;
    const MAX_BUNDLE_FILES: usize = 4096;
    const WAIT_TIMEOUT: Duration = Duration::from_secs(60);

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Identity {
        device: u64,
        inode: u64,
    }

    impl Identity {
        fn read(path: &Path) -> Result<Self> {
            let metadata = fs::symlink_metadata(path)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "symbolic links are not update targets"
            );
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Plan {
        schema: u32,
        directory: PathBuf,
        directory_identity: Identity,
        target: PathBuf,
        target_identity: Identity,
        parent_identity: Identity,
        source_version: String,
        target_version: String,
        original_hash: String,
        candidate_hash: String,
        candidate_identity: Identity,
        helper_hash: String,
        parent_pid: u32,
        token: String,
    }

    /// A prepared update has not changed the installation. Task-owned files are
    /// retained on failure so the receipt and rollback evidence remain available.
    #[derive(Debug)]
    pub struct PreparedInstall {
        plan_path: PathBuf,
        target_path: PathBuf,
        directory: PathBuf,
        target_version: String,
    }

    impl PreparedInstall {
        pub fn target_path(&self) -> &Path {
            &self.target_path
        }
        pub fn version(&self) -> &str {
            &self.target_version
        }
        pub fn receipt_path(&self) -> PathBuf {
            self.directory.join("receipt.json")
        }
        pub fn backup_path(&self) -> PathBuf {
            self.directory.join("Previous-MarkRust.app")
        }
    }

    /// Dropping a handle before commit cancels its helper. After commit, the
    /// application must quit synchronously without accepting further edits.
    #[derive(Debug)]
    pub struct ApplyHandle {
        child: Child,
        plan_path: PathBuf,
        committed: bool,
    }

    impl ApplyHandle {
        pub fn id(&self) -> u32 {
            self.child.id()
        }

        pub fn commit(&mut self) -> Result<()> {
            ensure!(!self.committed, "update handoff was already committed");
            ensure!(
                self.child.try_wait()?.is_none(),
                "update helper exited before handoff"
            );
            let plan = read_plan(&self.plan_path)?;
            ensure!(
                plan.parent_pid == std::process::id(),
                "update belongs to another process"
            );
            write_private(&plan.directory.join("start.gate"), plan.token.as_bytes())?;
            self.committed = true;
            Ok(())
        }

        /// A quit veto must call this before accepting edits again. Killing the
        /// helper while the parent is alive cannot interrupt bundle promotion:
        /// promotion is gated on this parent's process exit.
        pub fn abort(&mut self) -> Result<()> {
            if self.child.try_wait()?.is_none() {
                self.child.kill()?;
                self.child.wait()?;
            }
            self.committed = false;
            let plan = read_plan(&self.plan_path)?;
            let gate = plan.directory.join("start.gate");
            if gate.try_exists()? {
                let _ = read_private(&gate, 64)?;
                fs::remove_file(&gate)?;
                sync_directory(&plan.directory)?;
            }
            Ok(())
        }
    }

    impl Drop for ApplyHandle {
        fn drop(&mut self) {
            if !self.committed {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// Refuse a development binary, arbitrary app target, and symlinked bundle.
    pub fn discover_current_bundle() -> Result<PathBuf> {
        let executable = std::env::current_exe()?;
        safe_absolute_path(&executable)?;
        let macos = executable.parent().context("executable has no parent")?;
        let contents = macos
            .parent()
            .context("executable has no Contents directory")?;
        let bundle = contents.parent().context("executable has no app bundle")?;
        ensure!(
            executable
                .file_name()
                .is_some_and(|name| name == "markrust"),
            "unexpected executable name"
        );
        ensure!(
            macos.file_name().is_some_and(|name| name == "MacOS")
                && contents.file_name().is_some_and(|name| name == "Contents"),
            "updates require an installed macOS app bundle"
        );
        ensure!(
            bundle
                .extension()
                .is_some_and(|extension| extension == "app"),
            "updates require an installed .app bundle"
        );
        validate_bundle(bundle, env!("CARGO_PKG_VERSION"))?;
        Ok(bundle.to_path_buf())
    }

    pub fn prepare_install(
        candidate_bundle: &Path,
        expected_version: &str,
    ) -> Result<PreparedInstall> {
        let target = discover_current_bundle()?;
        let executable = std::env::current_exe()?;
        prepare_at(
            candidate_bundle,
            expected_version,
            &target,
            &executable,
            std::process::id(),
            validate_bundle,
        )
    }

    fn prepare_at(
        candidate: &Path,
        version: &str,
        target: &Path,
        helper_source: &Path,
        parent_pid: u32,
        validate: impl Fn(&Path, &str) -> Result<()>,
    ) -> Result<PreparedInstall> {
        ensure!(
            semver::Version::parse(version)? > semver::Version::parse(env!("CARGO_PKG_VERSION"))?,
            "updates must be newer than the running version"
        );
        safe_absolute_path(candidate)?;
        safe_absolute_path(target)?;
        safe_absolute_path(helper_source)?;
        ensure!(
            candidate != target,
            "candidate must not be the installed bundle"
        );
        ensure!(
            target
                .extension()
                .is_some_and(|extension| extension == "app"),
            "target must be an app bundle"
        );
        validate(target, env!("CARGO_PKG_VERSION"))?;
        validate(candidate, version)?;
        let original_hash = tree_hash(target)?;
        let candidate_before = tree_hash(candidate)?;
        let parent = target
            .parent()
            .context("bundle has no installation directory")?;
        let directory = create_private_directory(parent)?;
        ensure!(
            fs::metadata(&directory)?.dev() == fs::metadata(target)?.dev(),
            "update stage must be on the installation volume"
        );
        let staged_candidate = directory.join("Candidate.app");
        copy_tree(candidate, &staged_candidate)?;
        validate(&staged_candidate, version)?;
        ensure!(
            tree_hash(candidate)? == candidate_before,
            "download changed while being staged"
        );
        ensure!(
            tree_hash(&staged_candidate)? == candidate_before,
            "candidate copy does not match the verified download"
        );
        let helper = directory.join("apply-helper");
        copy_regular(helper_source, &helper, 0o500)?;
        let helper_hash = hash_file(&helper)?;
        ensure!(
            helper_hash == hash_file(helper_source)?,
            "trusted helper changed while being copied"
        );
        let token = random_token()?;
        let plan = Plan {
            schema: 1,
            directory_identity: Identity::read(&directory)?,
            target_identity: Identity::read(target)?,
            parent_identity: Identity::read(parent)?,
            directory: directory.clone(),
            target: target.to_path_buf(),
            source_version: env!("CARGO_PKG_VERSION").to_owned(),
            target_version: version.to_owned(),
            original_hash,
            candidate_hash: tree_hash(&staged_candidate)?,
            candidate_identity: Identity::read(&staged_candidate)?,
            helper_hash,
            parent_pid,
            token,
        };
        ensure!(
            tree_hash(target)? == plan.original_hash,
            "installation changed while preparing its update"
        );
        let plan_path = directory.join("plan.json");
        write_private(&plan_path, &serde_json::to_vec(&plan)?)?;
        sync_directory(&directory)?;
        Ok(PreparedInstall {
            plan_path,
            target_path: target.to_path_buf(),
            directory,
            target_version: version.to_owned(),
        })
    }

    pub fn spawn_apply(prepared: &PreparedInstall) -> Result<ApplyHandle> {
        let plan = read_plan(&prepared.plan_path)?;
        ensure!(
            plan.parent_pid == std::process::id(),
            "update belongs to another process"
        );
        ensure!(
            hash_file(&plan.directory.join("apply-helper"))? == plan.helper_hash,
            "trusted helper has changed"
        );
        write_private(
            &plan.directory.join("spawned.lock"),
            b"one helper per plan\n",
        )?;
        let child = Command::new(plan.directory.join("apply-helper"))
            .arg("--apply-update")
            .arg(&prepared.plan_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("could not start the trusted update helper")?;
        let mut handle = ApplyHandle {
            child,
            plan_path: prepared.plan_path.clone(),
            committed: false,
        };
        // A failed helper must not make the GUI quit. Confirm that it parsed
        // the pinned plan and is waiting behind the gate before returning.
        wait_until(
            Duration::from_secs(5),
            || {
                ensure!(
                    handle.child.try_wait()?.is_none(),
                    "update helper failed before it was ready"
                );
                let ready = plan.directory.join("helper.ready");
                if !ready.try_exists()? {
                    return Ok(false);
                }
                ensure!(
                    read_private(&ready, 64)? == plan.token.as_bytes(),
                    "update helper readiness token does not match"
                );
                Ok(true)
            },
            "update helper did not become ready; application was not closed",
        )?;
        Ok(handle)
    }

    /// Hidden CLI entry point. It accepts only a pinned private plan written by
    /// the running app and cannot be used to select an arbitrary installation.
    pub fn helper_cli(plan_path: &Path) -> Result<()> {
        let plan = read_plan(plan_path)?;
        ensure!(
            std::env::current_exe()? == plan.directory.join("apply-helper"),
            "update must run from its trusted copied helper"
        );
        ensure!(
            plan.source_version == env!("CARGO_PKG_VERSION"),
            "plan source version does not match helper"
        );
        ensure!(
            hash_file(&plan.directory.join("apply-helper"))? == plan.helper_hash,
            "trusted helper has changed"
        );
        let result = (|| {
            write_private(&plan.directory.join("helper.ready"), plan.token.as_bytes())?;
            apply_after_exit(
                &plan,
                WAIT_TIMEOUT,
                || gate_open(&plan),
                || process_alive(plan.parent_pid),
                validate_bundle,
                || {
                    let status = Command::new("/usr/bin/open")
                        .arg("-n")
                        .arg(&plan.target)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()?;
                    ensure!(status.success(), "updated app could not be relaunched");
                    Ok(())
                },
            )
        })();
        let status = if result.is_ok() {
            "installed"
        } else {
            "failed"
        };
        let receipt = serde_json::json!({
            "schema": 1, "status": status, "source_version": plan.source_version,
            "target_version": plan.target_version, "target": plan.target,
            "backup": plan.directory.join("Previous-MarkRust.app"),
            "error": result.as_ref().err().map(|error| format!("{error:#}")),
        });
        write_private(
            &plan.directory.join("receipt.json"),
            &serde_json::to_vec(&receipt)?,
        )?;
        result
    }

    fn apply_after_exit(
        plan: &Plan,
        timeout: Duration,
        committed: impl FnMut() -> Result<bool>,
        mut parent_alive: impl FnMut() -> Result<bool>,
        validate: impl Fn(&Path, &str) -> Result<()>,
        launch: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        wait_until(
            timeout,
            committed,
            "update was not committed before timeout",
        )?;
        wait_until(
            timeout,
            || Ok(!parent_alive()?),
            "application did not quit; installation was not changed",
        )?;
        apply_plan(plan, validate, launch)
    }

    fn apply_plan(
        plan: &Plan,
        validate: impl Fn(&Path, &str) -> Result<()>,
        launch: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        check_plan_paths(plan)?;
        ensure!(
            semver::Version::parse(&plan.target_version)?
                > semver::Version::parse(&plan.source_version)?,
            "plan is not an upgrade"
        );
        let candidate = plan.directory.join("Candidate.app");
        let backup = plan.directory.join("Previous-MarkRust.app");
        ensure!(!backup.try_exists()?, "rollback bundle already exists");
        validate(&plan.target, &plan.source_version)?;
        validate(&candidate, &plan.target_version)?;
        ensure!(
            Identity::read(&plan.target)? == plan.target_identity
                && tree_hash(&plan.target)? == plan.original_hash,
            "another installation replaced the original bundle; refusing to overwrite it"
        );
        ensure!(
            Identity::read(&candidate)? == plan.candidate_identity
                && tree_hash(&candidate)? == plan.candidate_hash,
            "staged update changed; installation was not changed"
        );
        // RENAME_SWAP never leaves the destination absent, including a crash at
        // the promotion point. The previous app remains inside the private stage.
        exchange(&candidate, &plan.target)?;
        let promoted: Result<()> = (|| {
            ensure!(
                Identity::read(&candidate)? == plan.target_identity
                    && tree_hash(&candidate)? == plan.original_hash,
                "installation raced update promotion"
            );
            validate(&plan.target, &plan.target_version)?;
            ensure!(
                tree_hash(&plan.target)? == plan.candidate_hash,
                "promoted update failed integrity validation"
            );
            sync_directory(
                plan.target
                    .parent()
                    .context("missing installation directory")?,
            )?;
            Ok(())
        })();
        if let Err(error) = promoted {
            rollback_exchange(&plan.target, &candidate, &plan.candidate_identity)?;
            return Err(error.context("update was rolled back"));
        }
        if let Err(error) = fs::rename(&candidate, &backup) {
            rollback_exchange(&plan.target, &candidate, &plan.candidate_identity)?;
            return Err(error).context("could not retain rollback bundle; update was rolled back");
        }
        if let Err(error) = sync_directory(&plan.directory) {
            rollback_exchange(&plan.target, &backup, &plan.candidate_identity)?;
            return Err(error.context("rollback directory could not be synchronized; the original application was restored"));
        }
        if let Err(error) = launch() {
            rollback_exchange(&plan.target, &backup, &plan.candidate_identity)?;
            // Both versions remain: original at target, rejected new version in
            // the private backup location, with an explicit failed receipt.
            return Err(error.context("launch failed; the original application was restored"));
        }
        Ok(())
    }

    fn rollback_exchange(
        target: &Path,
        previous: &Path,
        promoted_identity: &Identity,
    ) -> Result<()> {
        ensure!(
            Identity::read(target)? == *promoted_identity,
            "installation changed after promotion; automatic rollback refused to overwrite it"
        );
        exchange(target, previous)?;
        sync_directory(target.parent().context("missing installation directory")?)
    }

    fn exchange(left: &Path, right: &Path) -> Result<()> {
        let left = CString::new(left.as_os_str().as_bytes())?;
        let right = CString::new(right.as_os_str().as_bytes())?;
        // Both C strings live for the call. The paths have been checked and the
        // kernel atomically swaps their directory entries on the same volume.
        let result = unsafe {
            libc::renameatx_np(
                libc::AT_FDCWD,
                left.as_ptr(),
                libc::AT_FDCWD,
                right.as_ptr(),
                libc::RENAME_SWAP,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context(
                "atomic app exchange is unavailable; no fallback replacement was attempted",
            );
        }
        Ok(())
    }

    fn validate_bundle(path: &Path, version: &str) -> Result<()> {
        crate::updater::validate_candidate_bundle(path, version).map_err(anyhow::Error::from)
    }

    fn read_plan(path: &Path) -> Result<Plan> {
        safe_absolute_path(path)?;
        ensure!(
            path.file_name().is_some_and(|name| name == "plan.json"),
            "unexpected update plan name"
        );
        let bytes = read_private(path, PLAN_LIMIT)?;
        let plan: Plan = serde_json::from_slice(&bytes)?;
        ensure!(plan.schema == 1, "unsupported update plan schema");
        ensure!(
            path.parent() == Some(plan.directory.as_path()),
            "plan must be in its own private directory"
        );
        ensure!(
            plan.token.len() == 64 && plan.token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid update gate token"
        );
        ensure!(plan.parent_pid > 1, "invalid parent process");
        check_plan_paths(&plan)?;
        Ok(plan)
    }

    fn check_plan_paths(plan: &Plan) -> Result<()> {
        safe_absolute_path(&plan.directory)?;
        safe_absolute_path(&plan.target)?;
        private_directory(&plan.directory)?;
        ensure!(
            Identity::read(&plan.directory)? == plan.directory_identity,
            "private update directory was replaced"
        );
        ensure!(
            plan.directory
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".MarkRust-update-")),
            "unexpected update directory name"
        );
        ensure!(
            plan.target
                .extension()
                .is_some_and(|extension| extension == "app"),
            "target is not an app bundle"
        );
        let parent = plan.target.parent().context("target has no parent")?;
        ensure!(
            plan.directory.parent() == Some(parent),
            "update is not staged beside its target"
        );
        ensure!(
            Identity::read(parent)? == plan.parent_identity,
            "installation directory was replaced"
        );
        ensure!(
            plan.directory_identity.device == plan.target_identity.device,
            "update and target must be on the same volume"
        );
        Ok(())
    }

    fn safe_absolute_path(path: &Path) -> Result<()> {
        ensure!(path.is_absolute(), "update paths must be absolute");
        let mut current = PathBuf::new();
        for component in path.components() {
            ensure!(
                matches!(component, Component::RootDir | Component::Normal(_)),
                "update paths must not contain parent or current-directory components"
            );
            current.push(component.as_os_str());
            ensure!(
                !fs::symlink_metadata(&current)?.file_type().is_symlink(),
                "symbolic links are not allowed in update paths"
            );
        }
        ensure!(
            fs::canonicalize(path)? == path,
            "update path is not canonical"
        );
        Ok(())
    }

    fn private_directory(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "update directory is not a real directory"
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
            "update directory must be owned by this user with mode 0700"
        );
        Ok(())
    }

    fn create_private_directory(parent: &Path) -> Result<PathBuf> {
        let directory = parent.join(format!(".MarkRust-update-{}", random_token()?));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .context(
                "installation directory is not writable; no elevated privilege workaround is used",
            )?;
        private_directory(&directory)?;
        Ok(directory)
    }

    fn random_token() -> Result<String> {
        let mut bytes = [0; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
        private_directory(path.parent().context("private file has no directory")?)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        sync_directory(path.parent().context("private file has no directory")?)
    }

    fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>> {
        private_directory(path.parent().context("private file has no directory")?)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600,
            "update control file must be private, owned, regular and not hard-linked"
        );
        ensure!(
            metadata.len() <= limit,
            "update control file exceeds its limit"
        );
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= limit,
            "update control file grew beyond its limit"
        );
        Ok(bytes)
    }

    fn gate_open(plan: &Plan) -> Result<bool> {
        let gate = plan.directory.join("start.gate");
        match fs::symlink_metadata(&gate) {
            Ok(_) => Ok(read_private(&gate, 64)? == plan.token.as_bytes()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn wait_until(
        timeout: Duration,
        mut condition: impl FnMut() -> Result<bool>,
        message: &str,
    ) -> Result<()> {
        let started = Instant::now();
        loop {
            if condition()? {
                return Ok(());
            }
            ensure!(started.elapsed() < timeout, "{message}");
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn process_alive(pid: u32) -> Result<bool> {
        let pid = libc::pid_t::try_from(pid)?;
        // Signal zero probes existence without sending a signal.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error.into()),
        }
    }

    fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(source)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "bundle directory must not be a symbolic link"
        );
        ensure!(
            metadata.mode() & 0o022 == 0,
            "bundle directory must not be group/world writable"
        );
        fs::create_dir(destination)?;
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(metadata.mode() & 0o777),
        )?;
        for entry in sorted_entries(source)? {
            let target = destination.join(entry.file_name());
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                copy_tree(&entry.path(), &target)?;
            } else {
                copy_regular(&entry.path(), &target, metadata.mode() & 0o777)?;
            }
        }
        Ok(())
    }

    fn copy_regular(source: &Path, destination: &Path, mode: u32) -> Result<()> {
        let mut source = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        let metadata = source.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "update files must be regular and not hard-linked"
        );
        ensure!(
            mode & 0o022 == 0,
            "update files must not be group/world writable"
        );
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(destination)?;
        std::io::copy(&mut source, &mut target)?;
        target.set_permissions(fs::Permissions::from_mode(mode))?;
        target.sync_all()?;
        Ok(())
    }

    fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        Ok(entries)
    }

    fn hash_file(path: &Path) -> Result<String> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "hash target is not a regular file"
        );
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        Ok(format!("{:x}", hash.finalize()))
    }

    fn tree_hash(root: &Path) -> Result<String> {
        fn visit(root: &Path, path: &Path, hash: &mut Sha256, count: &mut usize) -> Result<()> {
            *count += 1;
            ensure!(
                *count <= MAX_BUNDLE_FILES,
                "bundle contains too many entries"
            );
            let metadata = fs::symlink_metadata(path)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "bundle must not contain symbolic links"
            );
            ensure!(
                metadata.mode() & 0o022 == 0,
                "bundle must not contain group/world writable entries"
            );
            let relative = path.strip_prefix(root)?.as_os_str().as_bytes();
            hash.update((relative.len() as u64).to_be_bytes());
            hash.update(relative);
            hash.update((metadata.mode() & 0o777).to_be_bytes());
            if metadata.is_dir() {
                hash.update(b"directory");
                for entry in sorted_entries(path)? {
                    visit(root, &entry.path(), hash, count)?;
                }
            } else {
                ensure!(
                    metadata.is_file() && metadata.nlink() == 1,
                    "bundle entries must be regular and not hard-linked"
                );
                hash.update(b"file");
                hash.update(metadata.len().to_be_bytes());
                hash.update(hash_file(path)?.as_bytes());
            }
            Ok(())
        }
        let mut hash = Sha256::new();
        visit(root, root, &mut hash, &mut 0)?;
        Ok(format!("{:x}", hash.finalize()))
    }

    fn sync_directory(path: &Path) -> Result<()> {
        File::open(path)?.sync_all().map_err(Into::into)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use anyhow::bail;
        use std::os::unix::fs::symlink;

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                let base = fs::canonicalize(std::env::temp_dir()).unwrap();
                Self(create_private_directory(&base).unwrap())
            }
            fn bundle(&self, name: &str, bytes: &[u8]) -> PathBuf {
                let path = self.0.join(name);
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                let executable = path.join("fixture");
                fs::write(&executable, bytes).unwrap();
                fs::set_permissions(executable, fs::Permissions::from_mode(0o644)).unwrap();
                path
            }
            fn prepared(&self) -> PreparedInstall {
                let target = self.bundle("MarkRust.app", b"original");
                let candidate = self.bundle("Download.app", b"candidate");
                let helper = self.0.join("trusted-helper");
                fs::write(&helper, b"trusted current executable").unwrap();
                fs::set_permissions(&helper, fs::Permissions::from_mode(0o500)).unwrap();
                prepare_at(
                    &candidate,
                    "99.0.0",
                    &target,
                    &helper,
                    std::process::id(),
                    |_, _| Ok(()),
                )
                .unwrap()
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn prepare_does_not_replace_target_and_controls_are_private() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"original");
            assert_eq!(
                fs::metadata(&prepared.plan_path).unwrap().mode() & 0o777,
                0o600
            );
            assert!(!gate_open(&plan).unwrap());
        }

        #[test]
        fn promotion_is_atomic_and_retains_previous_bundle() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            apply_plan(&plan, |_, _| Ok(()), || Ok(())).unwrap();
            assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"candidate");
            assert_eq!(
                fs::read(prepared.backup_path().join("fixture")).unwrap(),
                b"original"
            );
        }

        #[test]
        fn changed_original_is_not_overwritten() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            fs::write(plan.target.join("fixture"), b"another installation").unwrap();
            assert!(apply_plan(&plan, |_, _| Ok(()), || Ok(())).is_err());
            assert_eq!(
                fs::read(plan.target.join("fixture")).unwrap(),
                b"another installation"
            );
        }

        #[test]
        fn changed_candidate_is_not_installed() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            fs::write(plan.directory.join("Candidate.app/fixture"), b"changed").unwrap();
            assert!(apply_plan(&plan, |_, _| Ok(()), || Ok(())).is_err());
            assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"original");
        }

        #[test]
        fn failed_launch_restores_original_without_deleting_either_version() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            assert!(apply_plan(&plan, |_, _| Ok(()), || bail!("injected launch failure")).is_err());
            assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"original");
            assert_eq!(
                fs::read(prepared.backup_path().join("fixture")).unwrap(),
                b"candidate"
            );
        }

        #[test]
        fn promoted_validation_failure_rolls_back() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            assert!(apply_plan(
                &plan,
                |path, version| {
                    if path == plan.target && version == plan.target_version {
                        bail!("injected promoted validation failure");
                    }
                    Ok(())
                },
                || Ok(())
            )
            .is_err());
            assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"original");
            assert_eq!(
                fs::read(plan.directory.join("Candidate.app/fixture")).unwrap(),
                b"candidate"
            );
        }

        #[test]
        fn symlinks_and_public_control_files_are_rejected() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let link = fixture.0.join("linked-plan");
            symlink(&prepared.plan_path, &link).unwrap();
            assert!(safe_absolute_path(&link).is_err());
            fs::set_permissions(&prepared.plan_path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_plan(&prepared.plan_path).is_err());
        }

        #[test]
        fn timeout_and_live_parent_do_not_promote_any_bundle() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            assert!(wait_until(Duration::ZERO, || gate_open(&plan), "not committed").is_err());
            assert!(process_alive(std::process::id()).unwrap());
            assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"original");
        }

        #[test]
        fn committed_update_still_cannot_replace_a_live_parent() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            assert!(apply_after_exit(
                &plan,
                Duration::ZERO,
                || Ok(true),
                || Ok(true),
                |_, _| Ok(()),
                || Ok(())
            )
            .is_err());
            assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"original");
            assert!(!prepared.backup_path().exists());
        }

        #[test]
        fn handoff_promotes_only_after_gate_and_parent_exit() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let plan = read_plan(&prepared.plan_path).unwrap();
            let checked_parent = std::cell::Cell::new(false);
            apply_after_exit(
                &plan,
                Duration::ZERO,
                || Ok(true),
                || {
                    assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"original");
                    checked_parent.set(true);
                    Ok(false)
                },
                |_, _| Ok(()),
                || {
                    assert!(checked_parent.get());
                    assert_eq!(fs::read(plan.target.join("fixture")).unwrap(), b"candidate");
                    Ok(())
                },
            )
            .unwrap();
        }

        #[test]
        fn plan_unknown_fields_are_rejected() {
            let fixture = Fixture::new();
            let prepared = fixture.prepared();
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&prepared.plan_path).unwrap()).unwrap();
            value["alternate_target"] = serde_json::json!("/Applications/Another.app");
            assert!(serde_json::from_value::<Plan>(value).is_err());
        }
    }
}

#[cfg(target_os = "macos")]
pub use platform::{
    discover_current_bundle, helper_cli, prepare_install, spawn_apply, ApplyHandle, PreparedInstall,
};

#[cfg(not(target_os = "macos"))]
mod unsupported {
    use anyhow::{bail, Result};
    use std::path::{Path, PathBuf};

    #[derive(Debug)]
    pub struct PreparedInstall;
    #[derive(Debug)]
    pub struct ApplyHandle;

    impl PreparedInstall {
        pub fn target_path(&self) -> &Path {
            Path::new("")
        }
        pub fn version(&self) -> &str {
            ""
        }
        pub fn receipt_path(&self) -> PathBuf {
            PathBuf::new()
        }
        pub fn backup_path(&self) -> PathBuf {
            PathBuf::new()
        }
    }
    impl ApplyHandle {
        pub fn id(&self) -> u32 {
            0
        }
        pub fn commit(&mut self) -> Result<()> {
            bail!("automatic installation is currently supported only on macOS")
        }
        pub fn abort(&mut self) -> Result<()> {
            Ok(())
        }
    }
    pub fn discover_current_bundle() -> Result<PathBuf> {
        bail!("automatic installation is currently supported only on macOS")
    }
    pub fn prepare_install(_: &Path, _: &str) -> Result<PreparedInstall> {
        bail!("automatic installation is currently supported only on macOS")
    }
    pub fn spawn_apply(_: &PreparedInstall) -> Result<ApplyHandle> {
        bail!("automatic installation is currently supported only on macOS")
    }
    pub fn helper_cli(_: &Path) -> Result<()> {
        bail!("automatic installation is currently supported only on macOS")
    }
}

#[cfg(not(target_os = "macos"))]
pub use unsupported::{
    discover_current_bundle, helper_cli, prepare_install, spawn_apply, ApplyHandle, PreparedInstall,
};
