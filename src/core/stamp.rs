//! Once-per-instance guard for restore-on-start.
//!
//! The stamp lives in `$XDG_RUNTIME_DIR/hypr-persist` and is never written
//! under `/tmp`. A missing runtime directory skips auto-restore.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::store::{ExclusiveError, PrivateDir};
use crate::models::SessionFile;

const MAX_INSTANCE_KEY_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceKey(String);

impl InstanceKey {
    pub fn parse(raw: &str) -> Result<Self, ()> {
        if raw.is_empty() || raw.len() > MAX_INSTANCE_KEY_LEN {
            return Err(());
        }
        if !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(());
        }
        Ok(Self(raw.to_string()))
    }

    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    RestoreOnStartOff,
    Disabled,
    AlreadyStamped,
    TooManyPreexisting,
    NoSession,
    StampUnwritable,
    NoInstanceKey,
    NoRuntimeDir,
}

pub struct ScheduledRestore {
    pub name: String,
    pub session: SessionFile,
    pub instance: InstanceKey,
    dir: PrivateDir,
    leaf: String,
}

impl ScheduledRestore {
    /// Claim this instance's stamp. An existing leaf, even an empty one left
    /// by a crash, is [`SkipReason::AlreadyStamped`] and is not replaced.
    pub fn burn(&self) -> Result<(), SkipReason> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let body = format!("v1\n{}\n{now}\n", self.name);
        match self.dir.create_exclusive(&self.leaf, body.as_bytes()) {
            Ok(()) => Ok(()),
            Err(ExclusiveError::Exists) => Err(SkipReason::AlreadyStamped),
            Err(ExclusiveError::Failed(err)) => {
                tracing::warn!("could not claim restore stamp: {err:#}");
                Err(SkipReason::StampUnwritable)
            }
        }
    }

    pub fn stamp_path(&self) -> PathBuf {
        self.dir.path().join(&self.leaf)
    }
}

pub enum AutoRestore {
    Schedule(ScheduledRestore),
    Skip(SkipReason),
}

pub struct AutoRestoreInput<'a> {
    pub restore_on_start: bool,
    pub state_root: &'a Path,
    pub runtime_dir: Option<&'a Path>,
    pub signature: Option<&'a str>,
    pub boot_id: Option<&'a str>,
    pub preexisting_windows: usize,
    pub max_preexisting_windows: u32,
    pub target: Option<(&'a str, &'a SessionFile)>,
}

pub fn decide_auto_restore(input: &AutoRestoreInput<'_>) -> AutoRestore {
    if !input.restore_on_start {
        return AutoRestore::Skip(SkipReason::RestoreOnStartOff);
    }
    if is_disabled(input.state_root) {
        return AutoRestore::Skip(SkipReason::Disabled);
    }
    let Some(runtime) = input.runtime_dir else {
        return AutoRestore::Skip(SkipReason::NoRuntimeDir);
    };
    if !runtime.exists() {
        return AutoRestore::Skip(SkipReason::NoRuntimeDir);
    }
    if !runtime.is_dir() {
        return AutoRestore::Skip(SkipReason::StampUnwritable);
    }
    let Ok(instance) = instance_key(input.signature, input.boot_id) else {
        return AutoRestore::Skip(SkipReason::NoInstanceKey);
    };
    let Ok(dir) = PrivateDir::open(&runtime.join("hypr-persist"), false) else {
        return AutoRestore::Skip(SkipReason::StampUnwritable);
    };
    let leaf = format!(".{}.stamp", instance.as_str());
    match dir.read_regular(&leaf) {
        Ok(Some(_)) => return AutoRestore::Skip(SkipReason::AlreadyStamped),
        Ok(None) => {}
        Err(_) => return AutoRestore::Skip(SkipReason::StampUnwritable),
    }
    if input.preexisting_windows
        > usize::try_from(input.max_preexisting_windows).unwrap_or(usize::MAX)
    {
        return AutoRestore::Skip(SkipReason::TooManyPreexisting);
    }
    let Some((name, session)) = input.target else {
        return AutoRestore::Skip(SkipReason::NoSession);
    };
    AutoRestore::Schedule(ScheduledRestore {
        name: name.to_string(),
        session: session.clone(),
        instance,
        dir,
        leaf,
    })
}

pub struct ObservedStamp {
    pub session: String,
    pub instance: String,
}

/// Read this instance's stamp without creating or repairing the runtime dir.
pub fn observed_stamp(
    runtime_dir: Option<&Path>,
    signature: Option<&str>,
    boot_id: Option<&str>,
) -> Option<ObservedStamp> {
    let runtime = runtime_dir?;
    if !runtime.is_dir() {
        return None;
    }
    let key = instance_key(signature, boot_id).ok()?;
    let dir = PrivateDir::open(&runtime.join("hypr-persist"), true).ok()?;
    let leaf = format!(".{}.stamp", key.as_str());
    let bytes = dir.read_regular(&leaf).ok()??;
    let text = String::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "v1" {
        return None;
    }
    let session = lines.next()?.to_string();
    if session.is_empty() {
        return None;
    }
    Some(ObservedStamp {
        session,
        instance: key.as_str().to_string(),
    })
}

fn instance_key(signature: Option<&str>, boot_id: Option<&str>) -> Result<InstanceKey, ()> {
    for candidate in [signature, boot_id].into_iter().flatten() {
        if let Ok(key) = InstanceKey::parse(candidate.trim()) {
            return Ok(key);
        }
    }
    Err(())
}

pub fn is_disabled(state_root: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(state_root.join("disabled")),
        Ok(meta) if meta.file_type().is_symlink() || meta.is_file()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionFile {
        SessionFile::default()
    }

    fn input<'a>(
        state_root: &'a Path,
        runtime: Option<&'a Path>,
        target: Option<(&'a str, &'a SessionFile)>,
    ) -> AutoRestoreInput<'a> {
        AutoRestoreInput {
            restore_on_start: true,
            state_root,
            runtime_dir: runtime,
            signature: Some("instance1"),
            boot_id: None,
            preexisting_windows: 0,
            max_preexisting_windows: 3,
            target,
        }
    }

    #[test]
    fn skip_when_restore_on_start_is_off() {
        let root = tempfile::tempdir().unwrap();
        let mut decided = input(root.path(), None, None);
        decided.restore_on_start = false;
        assert_eq!(
            matches_skip(&decide_auto_restore(&decided)),
            Some(SkipReason::RestoreOnStartOff)
        );
    }

    #[test]
    fn skip_when_disabled_file_or_symlink() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("disabled"), b"").unwrap();
        let session = session();
        assert_eq!(
            matches_skip(&decide_auto_restore(&input(
                root.path(),
                None,
                Some(("last", &session))
            ))),
            Some(SkipReason::Disabled)
        );

        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/hostname", root.path().join("disabled")).unwrap();
        assert_eq!(
            matches_skip(&decide_auto_restore(&input(root.path(), None, None))),
            Some(SkipReason::Disabled)
        );
        assert!(!root.path().join("etc").exists());
    }

    #[test]
    fn skip_when_runtime_dir_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let session = session();
        assert_eq!(
            matches_skip(&decide_auto_restore(&input(
                root.path(),
                None,
                Some(("last", &session))
            ))),
            Some(SkipReason::NoRuntimeDir)
        );
        let missing = root.path().join("no-such-runtime");
        assert_eq!(
            matches_skip(&decide_auto_restore(&input(
                root.path(),
                Some(&missing),
                Some(("last", &session))
            ))),
            Some(SkipReason::NoRuntimeDir)
        );
    }

    #[test]
    fn skip_when_instance_key_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let mut decided = input(root.path(), Some(runtime.path()), None);
        decided.signature = None;
        decided.boot_id = None;
        assert_eq!(
            matches_skip(&decide_auto_restore(&decided)),
            Some(SkipReason::NoInstanceKey)
        );
    }

    #[test]
    fn skip_when_already_stamped() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let session = session();
        let decided = input(root.path(), Some(runtime.path()), Some(("last", &session)));
        let plan = expect_schedule(decide_auto_restore(&decided));
        plan.burn().unwrap();
        assert_eq!(
            matches_skip(&decide_auto_restore(&decided)),
            Some(SkipReason::AlreadyStamped)
        );
    }

    #[test]
    fn skip_when_too_many_preexisting_windows() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let session = session();
        let mut decided = input(root.path(), Some(runtime.path()), Some(("last", &session)));
        decided.preexisting_windows = 4;
        assert_eq!(
            matches_skip(&decide_auto_restore(&decided)),
            Some(SkipReason::TooManyPreexisting)
        );
        decided.preexisting_windows = 3;
        assert!(matches!(
            decide_auto_restore(&decided),
            AutoRestore::Schedule(_)
        ));
    }

    #[test]
    fn skip_when_no_session() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let decided = input(root.path(), Some(runtime.path()), None);
        assert_eq!(
            matches_skip(&decide_auto_restore(&decided)),
            Some(SkipReason::NoSession)
        );
    }

    #[test]
    fn skip_when_stamp_dir_is_unwritable() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::NamedTempFile::new().unwrap();
        let session = session();
        assert_eq!(
            matches_skip(&decide_auto_restore(&input(
                root.path(),
                Some(runtime.path()),
                Some(("last", &session))
            ))),
            Some(SkipReason::StampUnwritable)
        );
    }

    #[test]
    fn burn_creates_0600_file_and_second_decide_is_stamped() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let session = session();
        let decided = input(root.path(), Some(runtime.path()), Some(("last", &session)));
        let plan = expect_schedule(decide_auto_restore(&decided));
        plan.burn().unwrap();
        let meta = std::fs::metadata(plan.stamp_path()).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
        assert_eq!(mode & 0o777, 0o600);
        let body = std::fs::read_to_string(plan.stamp_path()).unwrap();
        assert!(body.starts_with("v1\nlast\n"), "{body}");
        assert!(body.ends_with('\n'));
        assert_eq!(
            matches_skip(&decide_auto_restore(&decided)),
            Some(SkipReason::AlreadyStamped)
        );
    }

    #[test]
    fn second_burn_does_not_replace_an_existing_stamp() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let session = session();
        let decided = input(root.path(), Some(runtime.path()), Some(("last", &session)));
        let first = expect_schedule(decide_auto_restore(&decided));
        let second = expect_schedule(decide_auto_restore(&decided));
        first.burn().unwrap();
        std::fs::write(first.stamp_path(), b"sentinel").unwrap();
        assert_eq!(second.burn(), Err(SkipReason::AlreadyStamped));
        assert_eq!(std::fs::read(second.stamp_path()).unwrap(), b"sentinel");
    }

    #[test]
    fn empty_stamp_is_already_claimed() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let session = session();
        let decided = input(root.path(), Some(runtime.path()), Some(("last", &session)));
        let plan = expect_schedule(decide_auto_restore(&decided));
        std::fs::write(plan.stamp_path(), b"").unwrap();
        assert_eq!(plan.burn(), Err(SkipReason::AlreadyStamped));
        assert_eq!(std::fs::read(plan.stamp_path()).unwrap(), b"");
    }

    #[test]
    fn signature_missing_falls_back_to_boot_id() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let session = session();
        let mut decided = input(root.path(), Some(runtime.path()), Some(("last", &session)));
        decided.signature = None;
        decided.boot_id = Some("01234567-89ab-cdef-0123-456789abcdef");
        let plan = expect_schedule(decide_auto_restore(&decided));
        assert_eq!(plan.instance.as_str(), decided.boot_id.unwrap());

        decided.signature = Some("bad/signature");
        let plan = expect_schedule(decide_auto_restore(&decided));
        assert_eq!(plan.instance.as_str(), decided.boot_id.unwrap());
    }

    #[test]
    fn slash_in_key_rejected() {
        assert!(InstanceKey::parse("bad/signature").is_err());
        assert!(InstanceKey::parse("ok-key_1.2").is_ok());
        assert!(InstanceKey::parse(&"a".repeat(MAX_INSTANCE_KEY_LEN + 1)).is_err());
    }

    fn matches_skip(decision: &AutoRestore) -> Option<SkipReason> {
        match decision {
            AutoRestore::Skip(reason) => Some(*reason),
            AutoRestore::Schedule(_) => None,
        }
    }

    fn expect_schedule(decision: AutoRestore) -> ScheduledRestore {
        match decision {
            AutoRestore::Schedule(plan) => plan,
            AutoRestore::Skip(reason) => panic!("expected schedule, skipped {reason:?}"),
        }
    }
}
