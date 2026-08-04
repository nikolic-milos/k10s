//! The filesystem behind the editor, as a seam.
//!
//! Zed's shape at k10s scale: views hold `Arc<dyn Fs>` and never touch
//! `std::fs`, so every open, save, and directory listing is testable against
//! [`FakeFs`] with no disk and no clocks. The implementation blocks, which is
//! why every caller reaches it from `background_executor().spawn` -- a listing
//! on a network mount is not something a frame can wait for -- and the pure
//! state machines in `files`, `finder` and `editor` are split so the disk work
//! and the state change happen on different threads.
//!
//! Saving preserves the file's identity, because a save is not a replacement,
//! and it stays atomic while doing so: a half-written manifest is worse than a
//! failed save. A symlink is written through rather than over -- including one
//! whose target does not exist yet, which opening creates. An existing file
//! keeps its permission bits, its `user.*` attributes and its POSIX ACLs, all
//! copied onto the replacement before it is renamed into place. `security.*`
//! and `trusted.*` are deliberately not copied: an unprivileged process cannot
//! set them, and the kernel labels a new file from policy exactly as it labels
//! every other file in that directory -- treating their presence as identity to
//! preserve is what turned every save on an SELinux system into a
//! truncate-then-write. Only what a rename genuinely cannot carry falls back to
//! writing in place: a hard-linked file, or attributes that could not be read.
//! A destination that is not a regular file is refused rather than truncated or
//! blocked on. Both paths flush the file, and the rename path flushes the
//! directory entry, before reporting success. The replacement is created with
//! the destination's permissions rather than chmodded afterwards, because a
//! 0600 kubeconfig whose temp file is briefly 0644 has already leaked. Temp
//! names carry a process-unique ticket and do not embed the target's name,
//! since a legal 255-byte name plus a suffix is not a legal name, and a failed
//! write takes its temp file with it.
//!
//! Names travel as `OsString`: a directory entry that is not valid UTF-8 is
//! still a file the user can open, so display goes through `label` and paths
//! are joined from the name itself.
//!
//! Modification state travels as an opaque [`Stamp`] rather than a
//! `SystemTime`: the real implementation derives it from mtime, the fake counts
//! writes, and the editor only ever compares for equality to detect that the
//! disk changed under it.

use std::ffi::OsString;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub type Stamp = u64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DirEntry {
    pub name: OsString,
    pub is_dir: bool,
}

impl DirEntry {
    pub fn new(name: impl Into<OsString>, is_dir: bool) -> DirEntry {
        DirEntry {
            name: name.into(),
            is_dir,
        }
    }

    // What a row shows. Paths are joined from `name`, never from this, so a
    // name that does not survive the conversion still opens.
    pub fn label(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }
}

pub trait Fs: Send + Sync {
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn write(&self, path: &Path, text: &str) -> io::Result<()>;
    fn stamp(&self, path: &Path) -> io::Result<Stamp>;
    fn list_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
    fn is_dir(&self, path: &Path) -> bool;
    fn exists(&self, path: &Path) -> bool;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
}

pub struct RealFs;

// Whether a rename can carry everything the destination is, or whether the only
// way to keep it is to write into the file that is already there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Replace {
    Rename {
        mode: Option<u32>,
        // Attributes a fresh inode does not get for free and the owner is
        // allowed to set: `user.*` and the POSIX ACLs. Copied onto the
        // replacement before it is renamed into place.
        carry: Vec<(OsString, Vec<u8>)>,
    },
    InPlace,
}

// What the path names once symlinks are followed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Destination {
    // A real path, whose existing file (if any) can be inspected and preserved.
    Resolved(PathBuf),
    // A symlink that does not resolve -- its target is missing, or the links
    // loop. Opening it is the only way to reach what it means, and opening is
    // also what creates a missing target; renaming over it would replace the
    // link with a regular file and leave the target uncreated.
    Through(PathBuf),
}

impl Fs for RealFs {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn write(&self, path: &Path, text: &str) -> io::Result<()> {
        let target = match resolve_target(path) {
            // A link the filesystem cannot resolve is followed by opening it,
            // which creates the target it names -- or fails on a loop, which is
            // the honest answer rather than flattening the link into a file.
            Destination::Through(link) => return write_in_place(&link, text),
            Destination::Resolved(target) => target,
        };
        if let Some(parent) = parent_of(&target) {
            std::fs::create_dir_all(parent)?;
        }
        match plan_replacement(&target)? {
            Replace::InPlace => write_in_place(&target, text),
            Replace::Rename { mode, carry } => rename_into_place(&target, text, mode, &carry),
        }
    }

    fn stamp(&self, path: &Path) -> io::Result<Stamp> {
        let modified = std::fs::metadata(path)?.modified()?;
        Ok(modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos() as Stamp)
            .unwrap_or(0))
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            // `file_type` does not follow links, so a symlinked folder reports
            // as neither file nor directory. Asking again through the link is
            // what makes it agree with `is_dir`, and what lets the picker and
            // the tree open it at all.
            let is_dir = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => entry.path().is_dir(),
                Ok(kind) => kind.is_dir(),
                Err(_) => false,
            };
            entries.push(DirEntry {
                name: entry.file_name(),
                is_dir,
            });
        }
        entries.sort_by(|a, b| (!a.is_dir, &a.name).cmp(&(!b.is_dir, &b.name)));
        Ok(entries)
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}

// The directory a path lives in, or None when the path names something in the
// process working directory and there is nothing to create or flush.
fn parent_of(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

// Saving to a symlink means saving to what it points at: replacing the link
// itself is how an editor turns a dotfile into a copy nobody else sees.
fn resolve_target(path: &Path) -> Destination {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => match std::fs::canonicalize(path) {
            Ok(resolved) => Destination::Resolved(resolved),
            Err(_) => Destination::Through(path.to_path_buf()),
        },
        _ => Destination::Resolved(path.to_path_buf()),
    }
}

fn plan_replacement(target: &Path) -> io::Result<Replace> {
    let Ok(metadata) = std::fs::metadata(target) else {
        // Nothing there to preserve, and a rename is the only way to make the
        // file appear whole.
        return Ok(Replace::Rename {
            mode: None,
            carry: Vec::new(),
        });
    };
    if metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "that path is a directory",
        ));
    }
    if !metadata.is_file() {
        // Opening a fifo for writing blocks until somebody reads it, which
        // would hang the save thread and the buffer's queue behind it; a device
        // is not something this editor should be truncating either.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "that path is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.nlink() > 1 {
            // A rename would break the link, and the other names are the whole
            // point of having made it.
            return Ok(Replace::InPlace);
        }
        return match carried_attributes(target) {
            Some(carry) => Ok(Replace::Rename {
                mode: Some(metadata.permissions().mode() & 0o7777),
                carry,
            }),
            // The attributes are there but cannot be read, so a rename would
            // drop something without knowing what. Writing in place keeps them.
            None => Ok(Replace::InPlace),
        };
    }
    #[cfg(not(unix))]
    Ok(Replace::Rename {
        mode: None,
        carry: Vec::new(),
    })
}

// The extended attributes a replacement has to be given explicitly, or None
// when they cannot be read and a rename would therefore lose something unseen.
//
// Only `user.*` and the POSIX ACLs are carried: they belong to the file, the
// owner may set them, and a fresh inode does not inherit them. `security.*` and
// `trusted.*` are deliberately not carried -- an unprivileged process cannot
// set them anyway, and the kernel labels a newly created file from policy,
// which is the same label the directory gives every other file in it. Their
// mere presence must not cost the atomic write: every file on an SELinux system
// carries `security.selinux`, and treating that as identity to preserve turned
// every save into a truncate-then-write.
#[cfg(unix)]
fn carried_attributes(path: &Path) -> Option<Vec<(OsString, Vec<u8>)>> {
    use std::os::unix::ffi::OsStrExt as _;

    let names = match extended_attribute_names(path) {
        Attributes::None => return Some(Vec::new()),
        Attributes::Names(names) => names,
        Attributes::Unreadable => return None,
    };
    // A capability set is identity a rename cannot carry and policy will not
    // restore, so it is the one attribute outside our namespaces that decides
    // the path rather than being left to the kernel.
    if names
        .iter()
        .any(|name| name.as_bytes() == b"security.capability")
    {
        return None;
    }
    let mut carried = Vec::new();
    for name in names {
        let bytes = name.as_bytes();
        if !(bytes.starts_with(b"user.") || bytes.starts_with(b"system.posix_acl_")) {
            continue;
        }
        match read_attribute(path, &name)? {
            // Removed between the listing and the read: nothing to carry.
            None => continue,
            Some(value) => carried.push((name, value)),
        }
    }
    Some(carried)
}

#[cfg(unix)]
enum Attributes {
    None,
    Names(Vec<OsString>),
    Unreadable,
}

#[cfg(unix)]
fn extended_attribute_names(path: &Path) -> Attributes {
    use std::os::unix::ffi::OsStrExt as _;
    const MAX_LIST: usize = 64 << 10;

    let mut buffer = vec![0u8; 1024];
    loop {
        match rustix::fs::listxattr(path, &mut buffer[..]) {
            Ok(0) => return Attributes::None,
            Ok(listed) => {
                let names = buffer[..listed]
                    .split(|byte| *byte == 0)
                    .filter(|name| !name.is_empty())
                    .map(|name| std::ffi::OsStr::from_bytes(name).to_os_string())
                    .collect();
                return Attributes::Names(names);
            }
            Err(rustix::io::Errno::RANGE) if buffer.len() < MAX_LIST => {
                buffer.resize(buffer.len() * 4, 0);
            }
            // A filesystem without extended attributes genuinely has none;
            // anything else means we were not allowed to look, which is not the
            // same answer and must not license a rename.
            Err(rustix::io::Errno::NOTSUP | rustix::io::Errno::NODATA) => return Attributes::None,
            Err(_) => return Attributes::Unreadable,
        }
    }
}

#[cfg(unix)]
fn read_attribute(path: &Path, name: &OsString) -> Option<Option<Vec<u8>>> {
    const MAX_VALUE: usize = 64 << 10;
    let mut buffer = vec![0u8; 256];
    loop {
        match rustix::fs::getxattr(path, name.as_os_str(), &mut buffer[..]) {
            Ok(read) => {
                buffer.truncate(read);
                return Some(Some(buffer));
            }
            Err(rustix::io::Errno::RANGE) if buffer.len() < MAX_VALUE => {
                buffer.resize(buffer.len() * 4, 0);
            }
            // Gone between the listing and the read: nothing to carry.
            Err(rustix::io::Errno::NODATA) => return Some(None),
            Err(_) => return None,
        }
    }
}

fn write_in_place(target: &Path, text: &str) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(target)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

fn rename_into_place(
    target: &Path,
    text: &str,
    mode: Option<u32>,
    carry: &[(OsString, Vec<u8>)],
) -> io::Result<()> {
    // The temp name carries a process-unique ticket: two saves of one file must
    // not share a scratch path, or the second write can be renamed into place
    // half-finished. It does not embed the target's name, because a legal
    // 255-byte name plus a suffix is not a legal name. Cleaned up on either
    // failure, because a stray sibling in the user's folder is our litter.
    static TICKET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ticket = TICKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch = format!(".k10s-{}-{ticket}.tmp", std::process::id());
    let temp = match parent_of(target) {
        Some(parent) => parent.join(scratch),
        None => PathBuf::from(scratch),
    };
    let written =
        write_temp(&temp, text, mode, carry).and_then(|()| std::fs::rename(&temp, target));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    // The rename is only durable once the directory entry is, and a manifest
    // that survives the process but not the machine is not saved.
    let directory = parent_of(target).unwrap_or(Path::new("."));
    if let Ok(handle) = std::fs::File::open(directory) {
        let _ = handle.sync_all();
    }
    Ok(())
}

fn write_temp(
    temp: &Path,
    text: &str,
    mode: Option<u32>,
    carry: &[(OsString, Vec<u8>)],
) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // The permissions are set as the file is created, not after it is written:
    // a 0600 kubeconfig whose replacement is briefly 0644 has already shown its
    // contents to every local user, and the final chmod hides that it did. A
    // destination we know nothing about starts private rather than open.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode.unwrap_or(0o600));
    }
    let mut file = options.open(temp)?;
    #[cfg(unix)]
    for (name, value) in carry {
        if let Err(error) = rustix::fs::fsetxattr(
            &file,
            name.as_os_str(),
            value,
            rustix::fs::XattrFlags::empty(),
        ) {
            return Err(io::Error::from(error));
        }
    }
    #[cfg(not(unix))]
    let _ = (mode, carry);
    file.write_all(text.as_bytes())?;
    // A destination with no mode of its own is a new file, and a new file takes
    // the umask the process was given.
    #[cfg(unix)]
    if mode.is_none() {
        use std::os::unix::fs::PermissionsExt as _;
        let umask = 0o666 & !current_umask();
        file.set_permissions(std::fs::Permissions::from_mode(umask))?;
    }
    file.sync_all()
}

// The process umask, read without changing it: `umask` has no getter, so the
// value is sampled once and remembered.
#[cfg(unix)]
fn current_umask() -> u32 {
    use std::sync::OnceLock;
    static UMASK: OnceLock<u32> = OnceLock::new();
    *UMASK.get_or_init(|| {
        let mode = rustix::process::umask(rustix::fs::Mode::empty());
        rustix::process::umask(mode);
        mode.bits()
    })
}

#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use super::{DirEntry, Fs, Stamp};
    use std::collections::BTreeMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeFs {
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        files: BTreeMap<PathBuf, (String, Stamp)>,
        writes: Stamp,
        // Writes that have been asked for but not completed, so a test can
        // finish them in whatever order it likes.
        held: Option<Vec<(PathBuf, String)>>,
    }

    impl FakeFs {
        pub fn with_files(files: &[(&str, &str)]) -> FakeFs {
            let fake = FakeFs::default();
            {
                let mut state = fake.state.lock().expect("fake fs lock");
                for (path, text) in files {
                    state.writes += 1;
                    let stamp = state.writes;
                    state
                        .files
                        .insert(PathBuf::from(path), ((*text).to_string(), stamp));
                }
            }
            fake
        }

        // The disk changing under the editor, without an editor save.
        pub fn touch(&self, path: &str, text: &str) {
            let mut state = self.state.lock().expect("fake fs lock");
            state.writes += 1;
            let stamp = state.writes;
            state
                .files
                .insert(PathBuf::from(path), (text.to_string(), stamp));
        }

        // Stop completing writes, so a test can decide the order they land in.
        pub fn hold_writes(&self) {
            let mut state = self.state.lock().expect("fake fs lock");
            state.held = Some(Vec::new());
        }

        pub fn held_writes(&self) -> Vec<(PathBuf, String)> {
            let state = self.state.lock().expect("fake fs lock");
            state.held.clone().unwrap_or_default()
        }

        // Complete one held write by the order it was requested in, letting a
        // test land a later save before an earlier one.
        pub fn release_write(&self, index: usize) {
            let mut state = self.state.lock().expect("fake fs lock");
            let Some(held) = state.held.as_mut() else {
                return;
            };
            if index >= held.len() {
                return;
            }
            let (path, text) = held.remove(index);
            state.writes += 1;
            let stamp = state.writes;
            state.files.insert(path, (text, stamp));
        }
    }

    impl Fs for FakeFs {
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            let state = self.state.lock().expect("fake fs lock");
            state
                .files
                .get(path)
                .map(|(text, _)| text.clone())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
        }

        fn write(&self, path: &Path, text: &str) -> io::Result<()> {
            let mut state = self.state.lock().expect("fake fs lock");
            if let Some(held) = state.held.as_mut() {
                held.push((path.to_path_buf(), text.to_string()));
                return Ok(());
            }
            state.writes += 1;
            let stamp = state.writes;
            state
                .files
                .insert(path.to_path_buf(), (text.to_string(), stamp));
            Ok(())
        }

        fn stamp(&self, path: &Path) -> io::Result<Stamp> {
            let state = self.state.lock().expect("fake fs lock");
            state
                .files
                .get(path)
                .map(|(_, stamp)| *stamp)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
        }

        fn list_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
            let state = self.state.lock().expect("fake fs lock");
            // Every answer comes from the map already held here: the lock is
            // not reentrant, so reaching back through `self.is_dir` deadlocks.
            let is_directory = path.as_os_str() == "/"
                || state.files.keys().any(|file| {
                    file.strip_prefix(path)
                        .is_ok_and(|rest| !rest.as_os_str().is_empty())
                });
            let mut names: Vec<DirEntry> = Vec::new();
            for file in state.files.keys() {
                let Ok(rest) = file.strip_prefix(path) else {
                    continue;
                };
                let mut parts = rest.components();
                let Some(first) = parts.next() else {
                    continue;
                };
                let name = first.as_os_str().to_os_string();
                let is_dir = parts.next().is_some();
                if !names.iter().any(|entry| entry.name == name) {
                    names.push(DirEntry { name, is_dir });
                }
            }
            if names.is_empty() && !is_directory {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such directory"));
            }
            names.sort_by(|a, b| (!a.is_dir, &a.name).cmp(&(!b.is_dir, &b.name)));
            Ok(names)
        }

        fn is_dir(&self, path: &Path) -> bool {
            let state = self.state.lock().expect("fake fs lock");
            path.as_os_str() == "/"
                || state.files.keys().any(|file| {
                    file.strip_prefix(path)
                        .is_ok_and(|rest| !rest.as_os_str().is_empty())
                })
        }

        fn exists(&self, path: &Path) -> bool {
            {
                let state = self.state.lock().expect("fake fs lock");
                if state.files.contains_key(path) {
                    return true;
                }
            }
            self.is_dir(path)
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            Ok(path.to_path_buf())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeFs;
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("k10s-fs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a temp directory");
        root
    }

    // The real implementation is what ships, so its atomic-write contract is
    // tested against a real directory rather than trusted.
    #[test]
    fn the_real_fs_writes_atomically_and_leaves_no_scratch_behind() {
        let root = scratch("atomic");
        let fs = RealFs;
        let nested = root.join("deep/inner/settings.json");

        fs.write(&nested, "{\"a\": 1}")
            .expect("a write creates parents");
        assert_eq!(fs.read_to_string(&nested).expect("readable"), "{\"a\": 1}");
        let first = fs.stamp(&nested).expect("stamped");

        fs.write(&nested, "{\"a\": 2}").expect("a rewrite replaces");
        assert_eq!(fs.read_to_string(&nested).expect("readable"), "{\"a\": 2}");
        assert!(
            fs.stamp(&nested).expect("stamped") >= first,
            "the stamp moves with the write"
        );

        let siblings = fs
            .list_dir(nested.parent().expect("has a parent"))
            .expect("listable");
        assert_eq!(
            siblings.len(),
            1,
            "no .tmp scratch file survives a save: {siblings:?}"
        );
        assert_eq!(siblings[0].label(), "settings.json");

        assert!(fs.is_dir(&root) && !fs.is_dir(&nested));
        assert!(fs.exists(&nested) && !fs.exists(&root.join("nope")));
        assert!(fs.read_to_string(&root.join("nope")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_keeps_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("mode");
        let fs = RealFs;
        let secret = root.join("kubeconfig");
        std::fs::write(&secret, "old").expect("seeded");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
            .expect("mode set");

        fs.write(&secret, "new").expect("written");

        let mode = std::fs::metadata(&secret)
            .expect("still there")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "a private file must not come back world-readable"
        );
        assert_eq!(fs.read_to_string(&secret).expect("readable"), "new");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_writes_through_a_symlink_instead_of_over_it() {
        let root = scratch("symlink");
        let fs = RealFs;
        let real = root.join("real.yaml");
        let link = root.join("link.yaml");
        std::fs::write(&real, "old").expect("seeded");
        std::os::unix::fs::symlink(&real, &link).expect("linked");

        fs.write(&link, "new").expect("written");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("still there")
                .file_type()
                .is_symlink(),
            "the link is still a link, not a copy of the file"
        );
        assert_eq!(std::fs::read_to_string(&real).expect("readable"), "new");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_whose_target_is_missing_is_written_through_not_replaced() {
        // The dotfile shape: `config` points into a store directory whose file
        // has not been created yet. Renaming over the link makes it a private
        // copy nobody else sees, which is the bug write-through exists to avoid.
        let root = scratch("dangling");
        let fs = RealFs;
        let store = root.join("store");
        std::fs::create_dir_all(&store).expect("store");
        let target = store.join("config");
        let link = root.join("config");
        std::os::unix::fs::symlink(&target, &link).expect("linked");

        fs.write(&link, "new").expect("written");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("still there")
                .file_type()
                .is_symlink(),
            "the link is still a link"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("the target now exists"),
            "new"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_that_is_not_a_regular_file_is_refused() {
        // Opening a fifo for writing blocks until somebody reads it, which would
        // hang the save thread and every save queued behind it.
        let root = scratch("special");
        let fs = RealFs;
        let folder = root.join("a-folder");
        std::fs::create_dir_all(&folder).expect("folder");
        assert_eq!(
            fs.write(&folder, "x").map_err(|error| error.kind()),
            Err(io::ErrorKind::IsADirectory)
        );

        let fifo = root.join("a-fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if made {
            assert_eq!(
                fs.write(&fifo, "x").map_err(|error| error.kind()),
                Err(io::ErrorKind::InvalidInput),
                "a fifo is refused rather than blocked on"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_as_long_as_the_filesystem_allows_still_saves() {
        // The scratch name used to be the target's name plus a suffix, so any
        // name within about twenty bytes of the limit could not be written at
        // all -- the editor reported "File name too long" for the file on screen.
        let root = scratch("longname");
        let fs = RealFs;
        let long = "n".repeat(255);
        let path = root.join(&long);
        fs.write(&path, "text").expect("a legal name saves");
        assert_eq!(fs.read_to_string(&path).expect("readable"), "text");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn an_extended_attribute_rides_along_and_the_save_stays_atomic() {
        // Every file on an SELinux system carries `security.selinux`, so
        // treating any attribute as a reason to abandon the rename made every
        // save non-atomic. The ones that belong to the file are copied instead.
        let root = scratch("xattr");
        let fs = RealFs;
        let path = root.join("manifest.yaml");
        std::fs::write(&path, "old").expect("seeded");
        let set = rustix::fs::setxattr(
            &path,
            std::ffi::OsStr::new("user.k10s-test"),
            b"kept",
            rustix::fs::XattrFlags::empty(),
        );
        if set.is_err() {
            // The filesystem under the temp directory does not support them.
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        fs.write(&path, "new").expect("written");

        assert_eq!(fs.read_to_string(&path).expect("readable"), "new");
        let mut value = [0u8; 16];
        let read = rustix::fs::getxattr(&path, std::ffi::OsStr::new("user.k10s-test"), &mut value)
            .expect("the attribute survived the replacement");
        assert_eq!(&value[..read], b"kept");
        let siblings = fs.list_dir(&root).expect("listable");
        assert_eq!(siblings.len(), 1, "and no scratch file survived");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn the_replacement_is_never_briefly_readable_by_anybody_else() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("mode-window");
        let secret = root.join("kubeconfig");
        std::fs::write(&secret, "old").expect("seeded");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
            .expect("mode set");

        // Watch the directory while a large save runs: the temp file must never
        // be observable with any group or other bits, which is what setting the
        // mode after the write used to allow.
        let watched = root.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let watcher = std::thread::spawn(move || {
            let mut widest = 0u32;
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(entries) = std::fs::read_dir(&watched) {
                    for entry in entries.flatten() {
                        if entry.file_name().to_string_lossy().contains(".k10s-")
                            && let Ok(metadata) = entry.metadata()
                        {
                            widest |= metadata.permissions().mode() & 0o077;
                        }
                    }
                }
            }
            widest
        });

        RealFs
            .write(&secret, &"x".repeat(8 << 20))
            .expect("written");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let observed = watcher.join().expect("the watcher finished");

        assert_eq!(
            observed, 0,
            "the temp file was visible to other users at mode {observed:o}"
        );
        assert_eq!(
            std::fs::metadata(&secret)
                .expect("still there")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_folder_lists_as_a_folder() {
        // `file_type` does not follow links, so a symlinked directory used to
        // list as a file -- and then neither the picker nor the tree would open
        // it, while `is_dir` on the same path said it was a directory.
        let root = scratch("linkdir");
        let fs = RealFs;
        let real = root.join("real");
        std::fs::create_dir_all(real.join("inner")).expect("tree");
        std::os::unix::fs::symlink(&real, root.join("linked")).expect("linked");

        let entries = fs.list_dir(&root).expect("listable");
        let linked = entries
            .iter()
            .find(|entry| entry.label() == "linked")
            .expect("the link is listed");
        assert!(
            linked.is_dir,
            "and it agrees with is_dir: {}",
            fs.is_dir(&root.join("linked"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_keeps_a_hard_linked_file_hard_linked() {
        use std::os::unix::fs::MetadataExt as _;
        let root = scratch("hardlink");
        let fs = RealFs;
        let one = root.join("one.yaml");
        let two = root.join("two.yaml");
        std::fs::write(&one, "old").expect("seeded");
        std::fs::hard_link(&one, &two).expect("linked");

        fs.write(&one, "new").expect("written");

        let left = std::fs::metadata(&one).expect("still there");
        let right = std::fs::metadata(&two).expect("still there");
        assert_eq!(
            left.ino(),
            right.ino(),
            "a rename would have broken the link"
        );
        assert_eq!(std::fs::read_to_string(&two).expect("readable"), "new");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_fake_fs_lists_directories_with_folders_first() {
        let fs = FakeFs::with_files(&[
            ("/work/b.yaml", "b"),
            ("/work/sub/deep.yaml", "d"),
            ("/work/a.yaml", "a"),
        ]);
        let entries = fs.list_dir(Path::new("/work")).expect("listing works");
        let names: Vec<(String, bool)> = entries
            .iter()
            .map(|entry| (entry.label(), entry.is_dir))
            .collect();
        assert_eq!(
            names,
            [
                ("sub".to_string(), true),
                ("a.yaml".to_string(), false),
                ("b.yaml".to_string(), false)
            ]
        );
    }

    #[test]
    fn a_name_that_is_not_utf8_still_names_a_file_that_opens() {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::OsStr::from_bytes(b"broken-\xff.yaml");
        let entry = DirEntry::new(name, false);
        assert!(
            entry.label().contains('\u{fffd}'),
            "the label is lossy on purpose: {}",
            entry.label()
        );
        assert_eq!(
            Path::new("/work").join(&entry.name),
            PathBuf::from("/work").join(name),
            "the path is joined from the name, so the file is still openable"
        );
    }

    #[test]
    fn stamps_move_only_when_the_file_changes() {
        let fs = FakeFs::with_files(&[("/work/a.yaml", "one")]);
        let first = fs.stamp(Path::new("/work/a.yaml")).expect("stamped");
        assert_eq!(
            fs.stamp(Path::new("/work/a.yaml")).expect("stamped"),
            first,
            "reading does not move the stamp"
        );
        fs.touch("/work/a.yaml", "two");
        assert_ne!(fs.stamp(Path::new("/work/a.yaml")).expect("stamped"), first);
    }

    #[test]
    fn held_writes_land_in_whatever_order_the_test_chooses() {
        let fs = FakeFs::with_files(&[("/work/a.yaml", "one")]);
        fs.hold_writes();
        fs.write(Path::new("/work/a.yaml"), "second").expect("held");
        fs.write(Path::new("/work/a.yaml"), "third").expect("held");
        assert_eq!(
            fs.read_to_string(Path::new("/work/a.yaml")).expect("read"),
            "one",
            "nothing has landed yet"
        );
        fs.release_write(1);
        fs.release_write(0);
        assert_eq!(
            fs.read_to_string(Path::new("/work/a.yaml")).expect("read"),
            "second",
            "the older write landed last, which is the hazard a queue prevents"
        );
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let fs = FakeFs::default();
        assert!(fs.read_to_string(Path::new("/nope")).is_err());
        assert!(fs.stamp(Path::new("/nope")).is_err());
        assert!(!fs.exists(Path::new("/nope")));
    }
}
