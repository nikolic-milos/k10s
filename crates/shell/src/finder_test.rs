use std::path::{Path, PathBuf};

use crate::fs::{DirEntry, Fs};

use crate::finder::*;
use crate::fs::fake::FakeFs;

#[cfg(windows)]
macro_rules! native_path {
    ($path:literal) => {
        concat!("C:", $path)
    };
}

#[cfg(not(windows))]
macro_rules! native_path {
    ($path:literal) => {
        $path
    };
}

// The two steps the view takes, in the order it takes them: read the
// directory only when the typed path names a new one, then filter.
fn refresh(state: &mut PickerState, fs: &dyn Fs) {
    if let Some(dir) = state.begin_listing() {
        let listed = fs.list_dir(Path::new(&dir));
        state.listed(&dir, listed);
    }
    state.refilter();
}

// A filesystem that counts directory listings and metadata probes.
struct Counting {
    inner: FakeFs,
    listings: std::sync::atomic::AtomicUsize,
    probes: std::sync::atomic::AtomicUsize,
}

impl Fs for Counting {
    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        self.inner.read_to_string(path)
    }
    fn write(&self, path: &Path, text: &str) -> std::io::Result<()> {
        self.inner.write(path, text)
    }
    fn stamp(&self, path: &Path) -> std::io::Result<crate::fs::Stamp> {
        self.inner.stamp(path)
    }
    fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        self.listings
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.list_dir(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.exists(path)
    }
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.inner.canonicalize(path)
    }
}

fn workspace_fs() -> FakeFs {
    FakeFs::with_files(&[
        (native_path!("/work/deploy.yaml"), "a"),
        (native_path!("/work/svc.yaml"), "b"),
        (native_path!("/work/overlays/prod/patch.yaml"), "c"),
        (native_path!("/work/.git/config"), "d"),
        (native_path!("/work/target/out.bin"), "e"),
        (native_path!("/work/README.md"), "f"),
    ])
}

#[test]
fn the_picker_filters_completes_and_descends() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    refresh(&mut state, &fs);
    assert!(state.matches.len() >= 5, "everything lists: {state:?}");
    state.input.push_str("ov");
    refresh(&mut state, &fs);
    let (_, segment) = state.split();
    assert_eq!(segment, "ov");
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Descend(native_path!("/work/overlays/").to_string()),
        "enter descends into the matched folder"
    );
    state.input = native_path!("/work/overlays/").to_string();
    refresh(&mut state, &fs);
    state.complete_selected();
    assert_eq!(
        state.input,
        native_path!("/work/overlays/prod/"),
        "tab completes with a slash"
    );
    state.input.push_str("patch");
    refresh(&mut state, &fs);
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!(
            "/work/overlays/prod/patch.yaml"
        )))
    );
}

#[test]
fn folder_mode_confirms_this_folder_through_its_own_row() {
    let fs = workspace_fs();
    let mut state = PickerState::new(
        Path::new(native_path!("/work/overlays")),
        PickerMode::OpenFolder,
    );
    refresh(&mut state, &fs);
    assert_eq!(
        state.entries[0].label(),
        HERE,
        "the folder being looked at is the first row"
    );
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/work/overlays"))),
        "enter on the dot row opens the folder itself"
    );
    state.selected = 1;
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/work/overlays/prod"))),
        "enter on a child folder opens it, never descends, in folder mode"
    );
}

#[test]
fn folder_mode_refuses_a_file_and_file_mode_descends_into_folders() {
    let fs = workspace_fs();
    let mut folder = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFolder);
    folder.input = native_path!("/work/READ").to_string();
    refresh(&mut folder, &fs);
    assert!(
        matches!(folder.confirm(&fs), PickerAction::Reject(note) if note.contains("pick a folder")),
        "a file cannot answer an open-folder request"
    );
    let mut file = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    file.input = native_path!("/work/ove").to_string();
    refresh(&mut file, &fs);
    assert_eq!(
        file.confirm(&fs),
        PickerAction::Descend(native_path!("/work/overlays/").to_string())
    );
}

#[test]
fn save_mode_takes_the_typed_path_and_open_mode_rejects_it() {
    let fs = workspace_fs();
    let mut save = PickerState::new(Path::new(native_path!("/work")), PickerMode::Save);
    save.input = native_path!("/work/new-manifest.yaml").to_string();
    refresh(&mut save, &fs);
    assert_eq!(
        save.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/work/new-manifest.yaml"))),
        "save-as trusts the typed name"
    );
    let mut open = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    open.input = native_path!("/work/nope.yaml").to_string();
    refresh(&mut open, &fs);
    assert!(
        matches!(open.confirm(&fs), PickerAction::Reject(_)),
        "open mode refuses a path that does not exist"
    );
}

#[test]
fn parent_walks_up_one_directory() {
    let fs = workspace_fs();
    let mut state = PickerState::new(
        Path::new(native_path!("/work/overlays/prod")),
        PickerMode::OpenFile,
    );
    refresh(&mut state, &fs);
    state.parent();
    assert_eq!(state.input, native_path!("/work/overlays/"));
    state.parent();
    assert_eq!(state.input, native_path!("/work/"));
}

#[test]
fn ctrl_up_drops_a_half_typed_name_before_it_leaves_the_folder() {
    let fs = workspace_fs();
    let mut state = PickerState::new(
        Path::new(native_path!("/work/overlays")),
        PickerMode::OpenFile,
    );
    state.input = native_path!("/work/overlays/pat").to_string();
    refresh(&mut state, &fs);
    state.parent();
    assert_eq!(
        state.input,
        native_path!("/work/overlays/"),
        "the typo goes, the folder stays"
    );
    state.parent();
    assert_eq!(state.input, native_path!("/work/"));
}

#[test]
fn save_mode_never_confirms_a_highlighted_file_the_user_did_not_type() {
    let fs = workspace_fs();
    let mut save = PickerState::new(Path::new(native_path!("/work")), PickerMode::Save);
    refresh(&mut save, &fs);
    assert!(
        matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("file name")),
        "enter straight after opening save-as must not overwrite row zero"
    );
    save.input = native_path!("/work/deploy.yaml").to_string();
    refresh(&mut save, &fs);
    assert_eq!(
        save.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/work/deploy.yaml"))),
        "a fully typed name is still the answer"
    );
}

#[test]
fn typing_inside_a_listed_directory_costs_no_second_listing() {
    // The picker used to read the folder on every character, which is a
    // syscall per keystroke and a freeze on anything but a local disk.
    let fs = Counting {
        inner: workspace_fs(),
        listings: std::sync::atomic::AtomicUsize::new(0),
        probes: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    refresh(&mut state, &fs);
    let after_open = fs.listings.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(after_open, 1, "opening reads the folder once");
    for character in "over".chars() {
        state.input.push(character);
        refresh(&mut state, &fs);
    }
    assert_eq!(
        fs.listings.load(std::sync::atomic::Ordering::Relaxed),
        after_open,
        "four keystrokes inside one folder read nothing"
    );
    assert!(
        state
            .matches
            .iter()
            .any(|index| state.entries[*index].label() == "overlays"),
        "and the filter still narrows: {state:?}"
    );
    state.input.push('/');
    refresh(&mut state, &fs);
    assert_eq!(
        fs.listings.load(std::sync::atomic::Ordering::Relaxed),
        after_open + 1,
        "crossing into a new folder reads that one"
    );
}

#[test]
fn a_listing_that_arrives_late_never_replaces_the_folder_being_looked_at() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    let dir = state.begin_listing().expect("a folder to read");
    let listed = fs.list_dir(Path::new(&dir));
    // The user moved on while that was in flight.
    state.input = native_path!("/work/overlays/").to_string();
    assert!(
        !state.listed(&dir, listed),
        "the stale answer is dropped rather than shown under the new path"
    );
    assert!(state.entries.is_empty());
}

#[test]
fn a_second_answer_for_the_folder_on_screen_keeps_the_highlighted_row() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    let first = state.begin_listing().expect("a folder to read");
    let slow = fs.list_dir(Path::new(&first));
    // Into a folder and straight back out, so two listings of /work/ are in
    // flight at once.
    state.input = native_path!("/work/overlays/").to_string();
    let _inner = state.begin_listing().expect("the folder stepped into");
    state.input = native_path!("/work/").to_string();
    let again = state.begin_listing().expect("stepping back asks again");
    assert!(state.listed(&again, fs.list_dir(Path::new(&again))));
    state.refilter();
    state.selected = 3;
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/work/README.md"))),
        "the row the user arrowed to"
    );
    let adopted = state.listed(&first, slow);
    if adopted {
        // What the view does with an answer it accepts.
        state.refilter();
    }
    assert!(
        !adopted,
        "the older request finishing late is not news about this folder"
    );
    assert_eq!(state.selected, 3, "so the highlighted row survives it");
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/work/README.md"))),
        "and enter opens what the eye is on, not row zero"
    );
}

#[test]
fn stepping_back_out_of_a_folder_lists_the_one_left_behind_again() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    refresh(&mut state, &fs);
    assert!(!state.matches.is_empty());
    state.input = native_path!("/work/overlays/").to_string();
    let _inner = state.begin_listing().expect("the folder stepped into");
    state.input = native_path!("/work/").to_string();
    refresh(&mut state, &fs);
    assert!(
        !state.matches.is_empty(),
        "the rows dropped on the way in have to come back: {state:?}"
    );
}

// A name that is bytes rather than text is a Unix fact; Windows file
// names are UTF-16 and cannot carry one.
#[cfg(unix)]
#[test]
fn a_file_whose_name_is_not_utf8_still_opens() {
    use std::os::unix::ffi::OsStrExt as _;
    // A real name carrying a byte that is not UTF-8: the row can only show
    // it lossily, so a path built from the row names nothing.
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.yaml");
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    let dir = state.begin_listing().expect("a folder to read");
    assert!(state.listed(&dir, Ok(vec![DirEntry::new(name, false)])));
    state.refilter();
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(Path::new(native_path!("/work")).join(name)),
        "the highlighted row opens the file it names"
    );
    state.complete_selected();
    state.refilter();
    assert!(
        state.input.contains('\u{fffd}'),
        "tab can only type the label: {}",
        state.input
    );
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(Path::new(native_path!("/work")).join(name)),
        "and a name tab-completed lossily still opens the real file"
    );
    // Nothing highlighted, so the answer comes from the typed name, which
    // is the row's label and has to resolve back to the row's name.
    state.matches.clear();
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(Path::new(native_path!("/work")).join(name))
    );
}

#[cfg(unix)]
#[test]
fn a_folder_whose_name_is_not_utf8_is_refused_rather_than_stepped_into_lossily() {
    use std::os::unix::ffi::OsStrExt as _;
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9");
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    let dir = state.begin_listing().expect("a folder to read");
    assert!(state.listed(&dir, Ok(vec![DirEntry::new(name, true)])));
    state.refilter();

    let refused = state.confirm(&fs);
    assert!(
        matches!(&refused, PickerAction::Reject(note) if note.contains("not text")),
        "stepping in would list a path spelled with replacement characters: {refused:?}"
    );

    let typed = state.input.clone();
    state.complete_selected();
    assert_eq!(state.input, typed, "and tab must not type it either");
    assert!(
        state
            .note
            .as_deref()
            .is_some_and(|note| note.contains("not text")),
        "the row says why: {:?}",
        state.note
    );

    let mut folder = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFolder);
    let dir = folder.begin_listing().expect("a folder to read");
    assert!(folder.listed(&dir, Ok(vec![DirEntry::new(name, true)])));
    folder.refilter();
    folder.selected = folder
        .matches
        .iter()
        .position(|index| folder.entries[*index].name == name)
        .expect("the folder row");
    assert_eq!(
        folder.confirm(&fs),
        PickerAction::Open(Path::new(native_path!("/work")).join(name))
    );
}

#[test]
fn a_path_that_exists_opens_while_its_folder_is_still_being_listed() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    // Asked for and not back yet -- seconds of it, on a network mount --
    // so the rows are silent about the typed name rather than authoritative.
    let _asked = state.begin_listing().expect("a folder to read");
    state.input = native_path!("/work/deploy.yaml").to_string();
    assert!(!state.listing_is_authoritative());
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/work/deploy.yaml"))),
        "a file that is sitting there is not 'no such path'"
    );
    let mut folder = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFolder);
    let _asked = folder.begin_listing().expect("a folder to read");
    folder.input = native_path!("/work/overlays").to_string();
    assert_eq!(
        folder.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/work/overlays"))),
        "and neither is a folder that is"
    );
}

#[test]
fn a_file_inside_a_folder_that_will_not_list_still_opens() {
    // A directory that is searchable but not listable, mode 0311: the
    // listing fails and the files inside it still read.
    let fs = FakeFs::with_files(&[(native_path!("/locked/deploy.yaml"), "a")]);
    let mut state = PickerState::new(Path::new(native_path!("/locked")), PickerMode::OpenFile);
    let dir = state.begin_listing().expect("a folder to read");
    assert!(state.listed(
        &dir,
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    ));
    state.refilter();
    assert!(
        !state.listing_is_authoritative(),
        "a listing that failed is not the folder's contents"
    );
    state.input = native_path!("/locked/deploy.yaml").to_string();
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from(native_path!("/locked/deploy.yaml")))
    );
    assert!(
        state.note.is_some(),
        "and the folder's own error is still on screen"
    );
}

#[test]
fn save_refuses_a_folder_as_its_target() {
    let fs = workspace_fs();
    let mut save = PickerState::new(Path::new(native_path!("/work")), PickerMode::Save);
    let _asked = save.begin_listing().expect("a folder to read");
    save.input = native_path!("/work/overlays").to_string();
    assert!(
        matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("folder")),
        "accepting a directory only moves 'is a directory' into the save"
    );
}

#[test]
fn a_bare_name_is_refused_rather_than_written_beside_the_process() {
    let fs = workspace_fs();
    let mut save = PickerState::new(Path::new(native_path!("/work")), PickerMode::Save);
    save.input = "notes.yaml".to_string();
    refresh(&mut save, &fs);
    assert!(
        matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("absolute")),
        "a name with no directory would land in the process working directory"
    );
    let mut open = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    open.input = "deploy.yaml".to_string();
    refresh(&mut open, &fs);
    assert!(
        matches!(open.confirm(&fs), PickerAction::Reject(note) if note.contains("absolute")),
        "and opening one is not resolved against it either"
    );
}

#[test]
fn a_native_empty_root_lists_and_confirms_without_becoming_drive_relative() {
    let fs = FakeFs::default();
    let root = Path::new(native_path!("/"));
    assert!(root.is_absolute());
    assert!(root.parent().is_none());
    assert!(fs.exists(root));
    assert!(fs.is_dir(root));
    assert!(fs.list_dir(root).expect("the native empty root").is_empty());
    let missing = Path::new(native_path!("/missing"));
    assert!(!fs.exists(missing));
    assert!(!fs.is_dir(missing));
    assert!(fs.list_dir(missing).is_err());
    let drive_relative = Path::new("C:");
    assert!(!fs.exists(drive_relative));
    assert!(!fs.is_dir(drive_relative));
    assert!(fs.list_dir(drive_relative).is_err());
    let mut state = PickerState::new(root, PickerMode::OpenFolder);
    refresh(&mut state, &fs);
    assert!(state.listing_is_authoritative());
    assert_eq!(state.confirm(&fs), PickerAction::Open(root.to_path_buf()));
}

#[test]
fn relative_picker_inputs_never_dispatch_a_listing_or_keep_old_rows() {
    let fs = Counting {
        inner: workspace_fs(),
        listings: std::sync::atomic::AtomicUsize::new(0),
        probes: std::sync::atomic::AtomicUsize::new(0),
    };
    for mode in [
        PickerMode::OpenFile,
        PickerMode::OpenFolder,
        PickerMode::Save,
    ] {
        for input in [
            "notes.yaml",
            "work/deploy.yaml",
            "./work/deploy.yaml",
            "../work/deploy.yaml",
            "C:work/deploy.yaml",
            "\\work\\deploy.yaml",
            "work/",
        ] {
            let mut state = PickerState::new(Path::new(native_path!("/work")), mode);
            refresh(&mut state, &fs);
            assert!(!state.matches.is_empty());
            state.selected = 1;
            let listings = fs.listings.load(std::sync::atomic::Ordering::Relaxed);
            state.input = input.into();
            assert!(state.begin_listing().is_none(), "{input}: {state:?}");
            assert!(state.entries.is_empty());
            assert!(state.matches.is_empty());
            assert_eq!(state.selected, 0);
            assert!(!state.listing_is_authoritative());
            assert_eq!(state.note.as_deref(), Some("type an absolute path"));
            assert!(!state.listed(
                native_path!("/work/"),
                Ok(vec![DirEntry::new("late.yaml", false)])
            ));
            let note_buffer = state.note.as_ref().unwrap().as_ptr();
            state.input.push('x');
            refresh(&mut state, &fs);
            assert_eq!(state.note.as_ref().unwrap().as_ptr(), note_buffer);
            assert_eq!(
                fs.listings.load(std::sync::atomic::Ordering::Relaxed),
                listings,
                "an invalid path never reaches the listing seam"
            );
            assert!(matches!(
                state.begin_confirm(),
                Some(Confirmation::Ready(PickerAction::Reject(note)))
                    if note == "type an absolute path"
            ));
            assert_eq!(fs.probes.load(std::sync::atomic::Ordering::Relaxed), 0);
            state.input = native_path!("/work/").into();
            assert_eq!(
                state.begin_listing().as_deref(),
                Some(native_path!("/work/"))
            );
        }
    }
}

#[test]
fn relative_confirmation_is_refused_before_cached_rows_or_metadata_checks() {
    let fs = Counting {
        inner: FakeFs::with_files(&[("work/deploy.yaml", "relative file")]),
        listings: std::sync::atomic::AtomicUsize::new(0),
        probes: std::sync::atomic::AtomicUsize::new(0),
    };
    for mode in [
        PickerMode::OpenFile,
        PickerMode::OpenFolder,
        PickerMode::Save,
    ] {
        for input in [
            "work/deploy.yaml",
            "./work/deploy.yaml",
            "../work/deploy.yaml",
            "C:work/deploy.yaml",
            "\\work\\deploy.yaml",
        ] {
            for cached in [false, true] {
                let mut state = PickerState::new(Path::new(native_path!("/work")), mode);
                state.input = input.into();
                if cached {
                    state.entries.push(DirEntry::new("deploy.yaml", false));
                    state.matches.push(0);
                }
                assert!(matches!(
                    state.begin_confirm(),
                    Some(Confirmation::Ready(PickerAction::Reject(note)))
                        if note == "type an absolute path"
                ));
                assert_eq!(
                    state.confirm(&fs),
                    PickerAction::Reject("type an absolute path".into())
                );
            }
        }
    }
    assert_eq!(fs.listings.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(fs.probes.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn a_rejected_enter_is_forgotten_by_the_next_keystroke() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new(native_path!("/work")), PickerMode::OpenFile);
    state.input = native_path!("/work/nope.yaml").to_string();
    refresh(&mut state, &fs);
    let PickerAction::Reject(note) = state.confirm(&fs) else {
        panic!("a path that is not there is refused");
    };
    state.note = Some(note);
    state.input.push('x');
    refresh(&mut state, &fs);
    assert_eq!(
        state.note, None,
        "the answer to the last enter is not about this keystroke"
    );
    let mut missing = PickerState::new(Path::new(native_path!("/nowhere")), PickerMode::OpenFile);
    refresh(&mut missing, &fs);
    let listing_error = missing.note.clone();
    assert!(
        listing_error.is_some(),
        "a folder that will not list says so"
    );
    missing.input.push('a');
    refresh(&mut missing, &fs);
    assert_eq!(
        missing.note, listing_error,
        "and keeps saying so while the user types inside it"
    );
}

#[test]
fn the_scan_states_a_depth_truncation_and_counts_unreadable_folders() {
    let mut deep = String::from(native_path!("/work"));
    for level in 0..(MAX_SCAN_DEPTH + 2) {
        deep.push_str(&format!("/d{level}"));
    }
    deep.push_str("/buried.yaml");
    let fs = FakeFs::with_files(&[(native_path!("/work/top.yaml"), "t"), (deep.as_str(), "b")]);
    let scan = scan_root(&fs, Path::new(native_path!("/work")));
    assert!(scan.files.iter().any(|file| file.label == "top.yaml"));
    assert!(
        !scan.files.iter().any(|file| file.label.contains("buried")),
        "the subtree past the cap is the one that is not there: {:?}",
        scan.files
    );
    assert!(
        scan.capped,
        "a subtree dropped for depth is a truncation the modal must state"
    );
}

#[test]
fn the_scan_skips_ignored_directories_and_states_its_cap() {
    let fs = workspace_fs();
    let scan = scan_root(&fs, Path::new(native_path!("/work")));
    assert!(scan.files.iter().any(|file| file.label == "deploy.yaml"));
    assert!(
        scan.files
            .iter()
            .any(|file| file.label == "overlays/prod/patch.yaml"
                && file.path == Path::new("overlays/prod/patch.yaml"))
    );
    assert!(
        !scan
            .files
            .iter()
            .any(|file| file.label.contains(".git") || file.label.contains("target")),
        "ignored directories stay out: {:?}",
        scan.files
    );
    assert!(!scan.capped);
}

// Metadata probes wait for the test's release and refuse the state thread.
// Listing still completes normally, so an empty snapshot can age before Enter.
struct BlockingProbe {
    inner: FakeFs,
    state_thread: std::thread::ThreadId,
    probes: std::sync::atomic::AtomicUsize,
    expected_path: std::sync::atomic::AtomicUsize,
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl BlockingProbe {
    fn new() -> (
        std::sync::Arc<Self>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, arrival) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        (
            std::sync::Arc::new(Self {
                inner: FakeFs::default(),
                state_thread: std::thread::current().id(),
                probes: std::sync::atomic::AtomicUsize::new(0),
                expected_path: std::sync::atomic::AtomicUsize::new(0),
                entered,
                release: std::sync::Mutex::new(released),
            }),
            arrival,
            release,
        )
    }

    fn probes(&self) -> usize {
        self.probes.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn probe(&self, path: &Path) {
        assert_ne!(std::thread::current().id(), self.state_thread);
        assert_eq!(
            path.to_str().expect("text probe path").as_ptr() as usize,
            self.expected_path
                .load(std::sync::atomic::Ordering::Relaxed),
            "metadata borrows the captured input instead of allocating a path"
        );
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Fs for BlockingProbe {
    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        self.inner.read_to_string(path)
    }
    fn write(&self, path: &Path, text: &str) -> std::io::Result<()> {
        self.inner.write(path, text)
    }
    fn stamp(&self, path: &Path) -> std::io::Result<crate::fs::Stamp> {
        self.inner.stamp(path)
    }
    fn list_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        self.inner.list_dir(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        self.probe(path);
        self.inner.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.probe(path);
        self.entered.send(()).expect("probe entered");
        self.release
            .lock()
            .expect("release lock")
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("probe released");
        self.inner.exists(path)
    }
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.inner.canonicalize(path)
    }
}

fn pending_check(state: &mut PickerState) -> PathCheck {
    let Some(Confirmation::Check(check)) = state.begin_confirm() else {
        panic!("typed uncached path needs a background check: {state:?}");
    };
    check
}

fn start_check(
    fs: std::sync::Arc<BlockingProbe>,
    check: PathCheck,
) -> std::thread::JoinHandle<(PathCheck, PathStatus)> {
    fs.expected_path.store(
        check.input_buffer() as usize,
        std::sync::atomic::Ordering::Relaxed,
    );
    std::thread::spawn(move || {
        let status = check.run(fs.as_ref());
        (check, status)
    })
}

fn wait_for_probe(entered: &std::sync::mpsc::Receiver<()>) {
    entered
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("worker entered metadata probe");
}

#[test]
fn typed_enter_rechecks_an_empty_snapshot_off_the_state_thread() {
    let cases = [
        (
            PickerMode::OpenFile,
            Some(native_path!("/new")),
            PickerAction::Open(PathBuf::from(native_path!("/new"))),
        ),
        (
            PickerMode::OpenFile,
            Some(native_path!("/new/child")),
            PickerAction::Descend(native_path!("/new/").into()),
        ),
        (
            PickerMode::OpenFolder,
            Some(native_path!("/new/child")),
            PickerAction::Open(PathBuf::from(native_path!("/new"))),
        ),
        (
            PickerMode::Save,
            Some(native_path!("/new/child")),
            PickerAction::Reject(format!(
                "{} is a folder; type a file name",
                native_path!("/new")
            )),
        ),
        (
            PickerMode::Save,
            None,
            PickerAction::Open(PathBuf::from(native_path!("/new"))),
        ),
        (
            PickerMode::OpenFile,
            None,
            PickerAction::Reject(format!("no such path: {}", native_path!("/new"))),
        ),
        (
            PickerMode::OpenFolder,
            None,
            PickerAction::Reject(format!("no such folder: {}", native_path!("/new"))),
        ),
    ];
    for (mode, created, expected) in cases {
        let (fs, entered, release) = BlockingProbe::new();
        let mut state = PickerState::new(Path::new(native_path!("/")), mode);
        refresh(&mut state, fs.as_ref());
        assert!(state.listing_is_authoritative());
        if let Some(created) = created {
            fs.inner.touch(created, "new after listing");
        }
        // Neither editing nor planning Enter may perform metadata IO.
        state.input = native_path!("/new").into();
        refresh(&mut state, fs.as_ref());
        assert!(state.matches.is_empty());
        let check = pending_check(&mut state);
        assert_eq!(fs.probes(), 0);
        assert_eq!(state.note.as_deref(), Some("Checking path..."));
        let worker = start_check(fs.clone(), check);
        wait_for_probe(&entered);
        assert!(
            state.begin_confirm().is_none(),
            "duplicate Enter stays bounded"
        );
        assert_eq!(fs.probes(), 1, "only the existing worker is blocked");
        release.send(()).expect("release check");
        let (check, status) = worker.join().expect("check worker");
        assert_eq!(fs.probes(), if created.is_some() { 2 } else { 1 });
        assert_eq!(state.finish_confirm(check, status), Some(expected));
        assert_eq!(state.note, None);
    }
}

#[test]
fn pending_and_failed_listings_use_the_same_off_thread_confirm() {
    for failed in [false, true] {
        let (fs, entered, release) = BlockingProbe::new();
        fs.inner.touch(native_path!("/locked/real.yaml"), "file");
        let mut state = PickerState::new(Path::new(native_path!("/locked")), PickerMode::OpenFile);
        let dir = state.begin_listing().expect("listing request");
        if failed {
            assert!(state.listed(
                &dir,
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            ));
        }
        state.input = native_path!("/locked/real.yaml").into();
        assert!(state.begin_listing().is_none());
        state.refilter();
        let check = pending_check(&mut state);
        assert_eq!(fs.probes(), 0);
        let worker = start_check(fs.clone(), check);
        wait_for_probe(&entered);
        release.send(()).expect("release check");
        let (check, status) = worker.join().expect("check worker");
        assert_eq!(
            state.finish_confirm(check, status),
            Some(PickerAction::Open(PathBuf::from(native_path!(
                "/locked/real.yaml"
            ))))
        );
    }
}

#[test]
fn edits_away_and_back_cannot_adopt_an_old_confirm_or_clear_a_new_one() {
    let (fs, entered, release) = BlockingProbe::new();
    let mut state = PickerState::new(Path::new(native_path!("/")), PickerMode::OpenFile);
    refresh(&mut state, fs.as_ref());
    fs.inner.touch(native_path!("/a"), "file");
    state.input = native_path!("/a").into();
    refresh(&mut state, fs.as_ref());
    let worker = start_check(fs.clone(), pending_check(&mut state));
    wait_for_probe(&entered);
    for input in [native_path!("/b"), native_path!("/a")] {
        state.input = input.into();
        refresh(&mut state, fs.as_ref());
        assert!(state.begin_confirm().is_none());
    }
    assert_eq!(fs.probes(), 1);
    release.send(()).expect("release stale check");
    let (old, status) = worker.join().expect("old worker");
    let duplicate = old.duplicate();
    assert_eq!(state.finish_confirm(old, status), None);
    let check = pending_check(&mut state);
    assert_eq!(
        state.finish_confirm(duplicate, status),
        None,
        "duplicate old delivery cannot consume the new confirmation"
    );
    assert!(state.begin_confirm().is_none());
    let worker = start_check(fs.clone(), check);
    wait_for_probe(&entered);
    release.send(()).expect("release current check");
    let (check, status) = worker.join().expect("current worker");
    assert_eq!(
        state.finish_confirm(check, status),
        Some(PickerAction::Open(PathBuf::from(native_path!("/a"))))
    );
}

#[test]
fn changing_selection_or_mode_invalidates_a_pending_confirm() {
    for change_mode in [false, true] {
        let (fs, entered, release) = BlockingProbe::new();
        let mut state = PickerState::new(Path::new(native_path!("/")), PickerMode::OpenFile);
        refresh(&mut state, fs.as_ref());
        fs.inner.touch(native_path!("/a"), "file");
        state.input = native_path!("/a").into();
        refresh(&mut state, fs.as_ref());
        let worker = start_check(fs.clone(), pending_check(&mut state));
        wait_for_probe(&entered);
        if change_mode {
            state.mode = PickerMode::Save;
        } else {
            state.selected += 1;
        }
        release.send(()).expect("release check");
        let (check, status) = worker.join().expect("check worker");
        assert_eq!(state.finish_confirm(check, status), None);
    }
}

#[test]
fn closing_a_picker_discards_a_reply_without_claiming_to_abort_its_syscall() {
    let (fs, entered, release) = BlockingProbe::new();
    let mut state = PickerState::new(Path::new(native_path!("/")), PickerMode::OpenFile);
    refresh(&mut state, fs.as_ref());
    fs.inner.touch(native_path!("/a"), "file");
    state.input = native_path!("/a").into();
    refresh(&mut state, fs.as_ref());
    let worker = start_check(fs.clone(), pending_check(&mut state));
    wait_for_probe(&entered);
    state.close();
    assert!(state.begin_confirm().is_none());
    assert_eq!(
        fs.probes(),
        1,
        "the blocked worker has not been interrupted"
    );
    release.send(()).expect("release closed check");
    let (check, status) = worker.join().expect("check worker");
    assert_eq!(fs.probes(), 2);
    assert_eq!(state.finish_confirm(check, status), None);
    assert!(state.begin_confirm().is_none());
}

#[test]
fn cached_selection_and_typing_do_not_probe_metadata() {
    let (fs, _entered, _release) = BlockingProbe::new();
    fs.inner.touch(native_path!("/file.yaml"), "file");
    let mut state = PickerState::new(Path::new(native_path!("/")), PickerMode::OpenFile);
    refresh(&mut state, fs.as_ref());
    for ch in "file".chars() {
        state.input.push(ch);
        refresh(&mut state, fs.as_ref());
    }
    assert!(matches!(
        state.begin_confirm(),
        Some(Confirmation::Ready(PickerAction::Open(path))) if path == Path::new(native_path!("/file.yaml"))
    ));
    assert_eq!(fs.probes(), 0);
}

#[test]
fn a_stale_worker_does_not_block_a_cached_selection_or_start_another_check() {
    let (fs, entered, release) = BlockingProbe::new();
    fs.inner.touch(native_path!("/cached.yaml"), "cached row");
    let mut state = PickerState::new(Path::new(native_path!("/")), PickerMode::OpenFile);
    refresh(&mut state, fs.as_ref());
    fs.inner.touch(native_path!("/later"), "after listing");
    state.input = native_path!("/later").into();
    refresh(&mut state, fs.as_ref());
    let worker = start_check(fs.clone(), pending_check(&mut state));
    wait_for_probe(&entered);
    state.input = native_path!("/cached").into();
    refresh(&mut state, fs.as_ref());
    assert!(matches!(
        state.begin_confirm(),
        Some(Confirmation::Ready(PickerAction::Open(path))) if path == Path::new(native_path!("/cached.yaml"))
    ));
    assert_eq!(fs.probes(), 1, "the pure cached answer does not probe");
    state.input = native_path!("/another").into();
    refresh(&mut state, fs.as_ref());
    assert!(
        state.begin_confirm().is_none(),
        "the old physical worker stays owned"
    );
    assert_eq!(fs.probes(), 1);
    release.send(()).expect("release stale check");
    let (check, status) = worker.join().expect("check worker");
    assert_eq!(state.finish_confirm(check, status), None);
    assert!(matches!(
        state.begin_confirm(),
        Some(Confirmation::Check(_))
    ));
}

#[cfg(unix)]
#[test]
fn accepted_open_reuses_the_checked_input_buffer() {
    use std::os::unix::ffi::OsStrExt as _;

    let fs = FakeFs::with_files(&[(native_path!("/file.yaml"), "file")]);
    let mut state = PickerState::new(Path::new(native_path!("/")), PickerMode::OpenFile);
    let _dir = state.begin_listing().expect("listing request");
    state.input = native_path!("/file.yaml").into();
    let check = pending_check(&mut state);
    let input_buffer = check.input_buffer();
    let status = check.run(&fs);
    let Some(PickerAction::Open(path)) = state.finish_confirm(check, status) else {
        panic!("current existing file opens");
    };
    assert_eq!(path, Path::new(native_path!("/file.yaml")));
    assert_eq!(path.as_os_str().as_bytes().as_ptr(), input_buffer);
}
