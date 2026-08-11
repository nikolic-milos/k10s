use std::path::{Path, PathBuf};

use crate::fs::{DirEntry, Fs};

use crate::finder::*;
use crate::fs::fake::FakeFs;

// The two steps the view takes, in the order it takes them: read the
// directory only when the typed path names a new one, then filter.
fn refresh(state: &mut PickerState, fs: &dyn Fs) {
    if let Some(dir) = state.begin_listing() {
        let listed = if dir.is_empty() {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "type an absolute path",
            ))
        } else {
            fs.list_dir(Path::new(&dir))
        };
        state.listed(&dir, listed);
    }
    state.refilter();
}

// A filesystem that counts how often it was asked to list something.
struct Counting {
    inner: FakeFs,
    listings: std::sync::atomic::AtomicUsize,
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
        self.inner.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.inner.canonicalize(path)
    }
}

fn workspace_fs() -> FakeFs {
    FakeFs::with_files(&[
        ("/work/deploy.yaml", "a"),
        ("/work/svc.yaml", "b"),
        ("/work/overlays/prod/patch.yaml", "c"),
        ("/work/.git/config", "d"),
        ("/work/target/out.bin", "e"),
        ("/work/README.md", "f"),
    ])
}

#[test]
fn the_picker_filters_completes_and_descends() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    refresh(&mut state, &fs);
    assert!(state.matches.len() >= 5, "everything lists: {state:?}");
    state.input.push_str("ov");
    refresh(&mut state, &fs);
    let (_, segment) = state.split();
    assert_eq!(segment, "ov");
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Descend("/work/overlays/".to_string()),
        "enter descends into the matched folder"
    );
    state.input = "/work/overlays/".to_string();
    refresh(&mut state, &fs);
    state.complete_selected();
    assert_eq!(
        state.input, "/work/overlays/prod/",
        "tab completes with a slash"
    );
    state.input.push_str("patch");
    refresh(&mut state, &fs);
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from("/work/overlays/prod/patch.yaml"))
    );
}

#[test]
fn folder_mode_confirms_this_folder_through_its_own_row() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work/overlays"), PickerMode::OpenFolder);
    refresh(&mut state, &fs);
    assert_eq!(
        state.entries[0].label(),
        HERE,
        "the folder being looked at is the first row"
    );
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from("/work/overlays")),
        "enter on the dot row opens the folder itself"
    );
    state.selected = 1;
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from("/work/overlays/prod")),
        "enter on a child folder opens it, never descends, in folder mode"
    );
}

#[test]
fn folder_mode_refuses_a_file_and_file_mode_descends_into_folders() {
    let fs = workspace_fs();
    let mut folder = PickerState::new(Path::new("/work"), PickerMode::OpenFolder);
    folder.input = "/work/READ".to_string();
    refresh(&mut folder, &fs);
    assert!(
        matches!(folder.confirm(&fs), PickerAction::Reject(note) if note.contains("pick a folder")),
        "a file cannot answer an open-folder request"
    );
    let mut file = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    file.input = "/work/ove".to_string();
    refresh(&mut file, &fs);
    assert_eq!(
        file.confirm(&fs),
        PickerAction::Descend("/work/overlays/".to_string())
    );
}

#[test]
fn save_mode_takes_the_typed_path_and_open_mode_rejects_it() {
    let fs = workspace_fs();
    let mut save = PickerState::new(Path::new("/work"), PickerMode::Save);
    save.input = "/work/new-manifest.yaml".to_string();
    refresh(&mut save, &fs);
    assert_eq!(
        save.confirm(&fs),
        PickerAction::Open(PathBuf::from("/work/new-manifest.yaml")),
        "save-as trusts the typed name"
    );
    let mut open = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    open.input = "/work/nope.yaml".to_string();
    refresh(&mut open, &fs);
    assert!(
        matches!(open.confirm(&fs), PickerAction::Reject(_)),
        "open mode refuses a path that does not exist"
    );
}

#[test]
fn parent_walks_up_one_directory() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work/overlays/prod"), PickerMode::OpenFile);
    refresh(&mut state, &fs);
    state.parent();
    assert_eq!(state.input, "/work/overlays/");
    state.parent();
    assert_eq!(state.input, "/work/");
}

#[test]
fn ctrl_up_drops_a_half_typed_name_before_it_leaves_the_folder() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work/overlays"), PickerMode::OpenFile);
    state.input = "/work/overlays/pat".to_string();
    refresh(&mut state, &fs);
    state.parent();
    assert_eq!(
        state.input, "/work/overlays/",
        "the typo goes, the folder stays"
    );
    state.parent();
    assert_eq!(state.input, "/work/");
}

#[test]
fn save_mode_never_confirms_a_highlighted_file_the_user_did_not_type() {
    let fs = workspace_fs();
    let mut save = PickerState::new(Path::new("/work"), PickerMode::Save);
    refresh(&mut save, &fs);
    assert!(
        matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("file name")),
        "enter straight after opening save-as must not overwrite row zero"
    );
    save.input = "/work/deploy.yaml".to_string();
    refresh(&mut save, &fs);
    assert_eq!(
        save.confirm(&fs),
        PickerAction::Open(PathBuf::from("/work/deploy.yaml")),
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
    };
    let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
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
    let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    let dir = state.begin_listing().expect("a folder to read");
    let listed = fs.list_dir(Path::new(&dir));
    // The user moved on while that was in flight.
    state.input = "/work/overlays/".to_string();
    assert!(
        !state.listed(&dir, listed),
        "the stale answer is dropped rather than shown under the new path"
    );
    assert!(state.entries.is_empty());
}

#[test]
fn a_second_answer_for_the_folder_on_screen_keeps_the_highlighted_row() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    let first = state.begin_listing().expect("a folder to read");
    let slow = fs.list_dir(Path::new(&first));
    // Into a folder and straight back out, so two listings of /work/ are in
    // flight at once.
    state.input = "/work/overlays/".to_string();
    let _inner = state.begin_listing().expect("the folder stepped into");
    state.input = "/work/".to_string();
    let again = state.begin_listing().expect("stepping back asks again");
    assert!(state.listed(&again, fs.list_dir(Path::new(&again))));
    state.refilter();
    state.selected = 3;
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from("/work/README.md")),
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
        PickerAction::Open(PathBuf::from("/work/README.md")),
        "and enter opens what the eye is on, not row zero"
    );
}

#[test]
fn stepping_back_out_of_a_folder_lists_the_one_left_behind_again() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    refresh(&mut state, &fs);
    assert!(!state.matches.is_empty());
    state.input = "/work/overlays/".to_string();
    let _inner = state.begin_listing().expect("the folder stepped into");
    state.input = "/work/".to_string();
    refresh(&mut state, &fs);
    assert!(
        !state.matches.is_empty(),
        "the rows dropped on the way in have to come back: {state:?}"
    );
}

#[test]
fn a_file_whose_name_is_not_utf8_still_opens() {
    use std::os::unix::ffi::OsStrExt as _;
    // A real name carrying a byte that is not UTF-8: the row can only show
    // it lossily, so a path built from the row names nothing.
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.yaml");
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    let dir = state.begin_listing().expect("a folder to read");
    assert!(state.listed(&dir, Ok(vec![DirEntry::new(name, false)])));
    state.refilter();
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(Path::new("/work").join(name)),
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
        PickerAction::Open(Path::new("/work").join(name)),
        "and a name tab-completed lossily still opens the real file"
    );
    // Nothing highlighted, so the answer comes from the typed name, which
    // is the row's label and has to resolve back to the row's name.
    state.matches.clear();
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(Path::new("/work").join(name))
    );
}

#[test]
fn a_path_that_exists_opens_while_its_folder_is_still_being_listed() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    // Asked for and not back yet -- seconds of it, on a network mount --
    // so the rows are silent about the typed name rather than authoritative.
    let _asked = state.begin_listing().expect("a folder to read");
    state.input = "/work/deploy.yaml".to_string();
    assert!(!state.listing_is_authoritative());
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from("/work/deploy.yaml")),
        "a file that is sitting there is not 'no such path'"
    );
    let mut folder = PickerState::new(Path::new("/work"), PickerMode::OpenFolder);
    let _asked = folder.begin_listing().expect("a folder to read");
    folder.input = "/work/overlays".to_string();
    assert_eq!(
        folder.confirm(&fs),
        PickerAction::Open(PathBuf::from("/work/overlays")),
        "and neither is a folder that is"
    );
}

#[test]
fn a_file_inside_a_folder_that_will_not_list_still_opens() {
    // A directory that is searchable but not listable, mode 0311: the
    // listing fails and the files inside it still read.
    let fs = FakeFs::with_files(&[("/locked/deploy.yaml", "a")]);
    let mut state = PickerState::new(Path::new("/locked"), PickerMode::OpenFile);
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
    state.input = "/locked/deploy.yaml".to_string();
    assert_eq!(
        state.confirm(&fs),
        PickerAction::Open(PathBuf::from("/locked/deploy.yaml"))
    );
    assert!(
        state.note.is_some(),
        "and the folder's own error is still on screen"
    );
}

#[test]
fn save_refuses_a_folder_as_its_target() {
    let fs = workspace_fs();
    let mut save = PickerState::new(Path::new("/work"), PickerMode::Save);
    let _asked = save.begin_listing().expect("a folder to read");
    save.input = "/work/overlays".to_string();
    assert!(
        matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("folder")),
        "accepting a directory only moves 'is a directory' into the save"
    );
}

#[test]
fn a_bare_name_is_refused_rather_than_written_beside_the_process() {
    let fs = workspace_fs();
    let mut save = PickerState::new(Path::new("/work"), PickerMode::Save);
    save.input = "notes.yaml".to_string();
    refresh(&mut save, &fs);
    assert!(
        matches!(save.confirm(&fs), PickerAction::Reject(note) if note.contains("absolute")),
        "a name with no directory would land in the process working directory"
    );
    let mut open = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    open.input = "deploy.yaml".to_string();
    refresh(&mut open, &fs);
    assert!(
        matches!(open.confirm(&fs), PickerAction::Reject(note) if note.contains("absolute")),
        "and opening one is not resolved against it either"
    );
}

#[test]
fn a_rejected_enter_is_forgotten_by_the_next_keystroke() {
    let fs = workspace_fs();
    let mut state = PickerState::new(Path::new("/work"), PickerMode::OpenFile);
    state.input = "/work/nope.yaml".to_string();
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
    let mut missing = PickerState::new(Path::new("/nowhere"), PickerMode::OpenFile);
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
    let mut deep = String::from("/work");
    for level in 0..(MAX_SCAN_DEPTH + 2) {
        deep.push_str(&format!("/d{level}"));
    }
    deep.push_str("/buried.yaml");
    let fs = FakeFs::with_files(&[("/work/top.yaml", "t"), (deep.as_str(), "b")]);
    let scan = scan_root(&fs, Path::new("/work"));
    assert!(scan.files.iter().any(|file| file.label == "top.yaml"));
    assert!(
        scan.capped,
        "a subtree dropped for depth is a truncation the modal must state"
    );
}

#[test]
fn the_scan_skips_ignored_directories_and_states_its_cap() {
    let fs = workspace_fs();
    let scan = scan_root(&fs, Path::new("/work"));
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
