//! That a save is atomic and loses nothing: permissions, extended attributes
//! and hard links survive it, a symlink is written through rather than over,
//! the replacement is never briefly readable by anybody else, and a
//! destination that is not a regular file is refused.

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
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).expect("mode set");

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
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).expect("mode set");

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

// A name that is bytes rather than text is a Unix fact; Windows file
// names are UTF-16 and cannot carry one.
#[cfg(unix)]
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
