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
//! blocked on. Both paths flush the file before reporting success, and the
//! rename path then asks for the directory entry too -- best effort, because
//! the file is already in place by then and a directory that will not open or
//! will not sync is not a save that failed. The replacement is created with
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
    // that survives the process but not the machine is not saved. Asked for
    // rather than required: the file is in place by now, so a directory this
    // process cannot open or a filesystem that will not sync one is not a
    // reason to tell the editor its save failed.
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

// The process umask, sampled once and remembered. `umask` has no getter, and
// the way around that -- set it to zero and set it back -- is a process-wide
// window in which every other thread's `open` and `creat` inherits mask 0, so
// the kernel is asked instead wherever it will answer. Only a system with no
// such answer pays the poke.
#[cfg(unix)]
fn current_umask() -> u32 {
    use std::sync::OnceLock;
    static UMASK: OnceLock<u32> = OnceLock::new();
    *UMASK.get_or_init(|| {
        if let Some(mask) = std::fs::read_to_string("/proc/self/status")
            .ok()
            .as_deref()
            .and_then(umask_in_status)
        {
            return mask;
        }
        let mode = rustix::process::umask(rustix::fs::Mode::empty());
        rustix::process::umask(mode);
        // `RawMode` is u32 on Linux and u16 on Apple and the BSDs; widen it
        // here so the mask arithmetic stays one type on every platform.
        #[allow(clippy::useless_conversion)]
        u32::from(mode.bits())
    })
}

// The `Umask:` line of a Linux process status, which is octal. Anything else
// is a kernel that does not publish it, and answers nothing rather than a mask
// that would silently open up every file this process creates.
#[cfg(unix)]
fn umask_in_status(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .and_then(|mask| u32::from_str_radix(mask.trim(), 8).ok())
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
#[path = "fs_test.rs"]
mod tests;
