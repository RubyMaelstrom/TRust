//! The terminal frontend's file picker: the prompt HTML #show-the-picker,-if-applicable asks a
//! user agent to display for a File Upload control (local whatwg/html `e5071a20`). It is a
//! keyboard-driven directory browser with a path entry; rendering lives in `ui.rs`.
//!
//! * Without `multiple` exactly one file is chosen; with it, Space or Insert marks files across
//!   directories and Enter (or Ctrl-O) confirms the marks.
//! * `webkitdirectory` (File and Directory Entries API #html-forms) lists directories and chooses
//!   the current one.
//! * HTML #attr-input-accept: "User agents should prevent the user from selecting files that are
//!   not accepted"; files the filter excludes (and hidden files) are listed only after Ctrl-T.
//! * The path entry takes typed or pasted paths (absolute, `~/`, or relative to the listed
//!   directory); Tab completes its last component.
//! * Esc dismisses the prompt, which fires `cancel` at the control.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::file_select::AcceptFilter;
use crate::js::FilePickerRequest;

/// At most this many entries of one directory are listed.
pub const MAX_LISTED_ENTRIES: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// `..`, the parent directory.
    Parent,
    /// Choose the listed directory (a `webkitdirectory` control).
    ChooseDirectory,
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: Option<u64>,
}

/// The result of one key or click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Chosen(Vec<PathBuf>),
    Dismissed,
}

#[derive(Debug)]
pub struct FilePicker {
    pub request: u64,
    pub multiple: bool,
    pub directory: bool,
    pub accept: AcceptFilter,
    /// List hidden files and files the accept filter excludes.
    pub show_all: bool,
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub highlight: usize,
    /// First listed row, kept in view of `highlight` by the renderer.
    pub scroll: usize,
    /// Marked files, in marking order (`multiple` only).
    pub marked: Vec<PathBuf>,
    /// The path entry and its cursor, in characters.
    pub input: String,
    pub cursor: usize,
    /// A notice for the status row: an error, or completion candidates.
    pub message: Option<String>,
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

fn home() -> Option<PathBuf> {
    #[cfg(windows)]
    let name = "USERPROFILE";
    #[cfg(not(windows))]
    let name = "HOME";
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

impl FilePicker {
    /// A prompt for `request`, listing `start` (or the home directory when it is unusable).
    pub fn new(request: &FilePickerRequest, start: Option<PathBuf>) -> Self {
        let dir = start
            .filter(|dir| dir.is_dir())
            .or_else(home)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut picker = Self {
            request: request.request,
            multiple: request.multiple && !request.directory,
            directory: request.directory,
            accept: AcceptFilter::parse(&request.accept),
            show_all: false,
            dir,
            entries: Vec::new(),
            highlight: 0,
            scroll: 0,
            marked: Vec::new(),
            input: String::new(),
            cursor: 0,
            message: None,
        };
        picker.reload();
        picker
    }

    /// The title the prompt shows.
    pub fn title(&self) -> &'static str {
        if self.directory {
            "CHOOSE FOLDER"
        } else if self.multiple {
            "CHOOSE FILES"
        } else {
            "CHOOSE FILE"
        }
    }

    /// What the listing currently shows, for the header row.
    pub fn filter_label(&self) -> String {
        if self.directory {
            String::from("folders")
        } else if self.show_all || self.accept.is_empty() {
            String::from("all files")
        } else {
            self.accept.source().trim().to_string()
        }
    }

    /// The key hints the prompt shows.
    pub fn hints(&self) -> String {
        let mut hints = vec!["↑↓ move", "Enter open"];
        if self.multiple {
            hints.push("Space mark");
            hints.push("Ctrl-O confirm");
        } else if self.directory {
            hints.push("Ctrl-O choose folder");
        }
        hints.push("Tab complete");
        hints.push(if self.show_all {
            "Ctrl-T filter"
        } else {
            "Ctrl-T all files"
        });
        hints.push("Esc cancel");
        hints.join(" · ")
    }

    /// List `dir` again: `..`, the choose-this-folder action, directories, then files, each in
    /// case-insensitive name order.
    pub fn reload(&mut self) {
        let previous = self
            .entries
            .get(self.highlight)
            .map(|entry| entry.path.clone());
        self.entries.clear();
        if let Some(parent) = self.dir.parent() {
            self.entries.push(Entry {
                name: String::from(".."),
                path: parent.to_path_buf(),
                kind: EntryKind::Parent,
                size: None,
            });
        }
        if self.directory {
            self.entries.push(Entry {
                name: String::from("[ Choose this folder ]"),
                path: self.dir.clone(),
                kind: EntryKind::ChooseDirectory,
                size: None,
            });
        }
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        match std::fs::read_dir(&self.dir) {
            Ok(read) => {
                for entry in read.filter_map(Result::ok).take(MAX_LISTED_ENTRIES) {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if is_hidden(&name) && !self.show_all {
                        continue;
                    }
                    let path = entry.path();
                    let Ok(metadata) = std::fs::metadata(&path) else {
                        continue;
                    };
                    if metadata.is_dir() {
                        dirs.push(Entry {
                            name,
                            path,
                            kind: EntryKind::Directory,
                            size: None,
                        });
                    } else if metadata.is_file()
                        && !self.directory
                        && (self.show_all || self.accept.accepts(&name))
                    {
                        files.push(Entry {
                            name,
                            path,
                            kind: EntryKind::File,
                            size: Some(metadata.len()),
                        });
                    }
                }
            }
            Err(error) => {
                self.message = Some(format!("Cannot list {}: {error}", self.dir.display()));
            }
        }
        let order = |a: &Entry, b: &Entry| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        };
        dirs.sort_by(order);
        files.sort_by(order);
        self.entries.extend(dirs);
        self.entries.extend(files);
        self.highlight = previous
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| {
                // Start on the first real entry rather than `..`.
                usize::from(self.entries.len() > 1 && self.entries[0].kind == EntryKind::Parent)
            });
        self.scroll = self.scroll.min(self.highlight);
    }

    fn enter_directory(&mut self, dir: PathBuf) {
        let from = self.dir.clone();
        self.dir = dir;
        self.entries.clear();
        self.highlight = 0;
        self.scroll = 0;
        self.message = None;
        self.reload();
        // Going up keeps the directory just left highlighted.
        if from.parent() == Some(self.dir.as_path())
            && let Some(index) = self
                .entries
                .iter()
                .position(|entry| entry.kind == EntryKind::Directory && entry.path == from)
        {
            self.highlight = index;
        }
    }

    pub fn is_marked(&self, path: &Path) -> bool {
        self.marked.iter().any(|marked| marked == path)
    }

    fn toggle_mark(&mut self) {
        let Some(entry) = self.entries.get(self.highlight) else {
            return;
        };
        if !self.multiple || entry.kind != EntryKind::File {
            return;
        }
        let path = entry.path.clone();
        if let Some(index) = self.marked.iter().position(|marked| *marked == path) {
            self.marked.remove(index);
        } else {
            self.marked.push(path);
        }
        self.highlight = (self.highlight + 1).min(self.entries.len().saturating_sub(1));
    }

    /// Choose `file`: alone, or with the marked files when some are marked.
    fn choose_file(&mut self, file: PathBuf) -> Outcome {
        if self.multiple && !self.marked.is_empty() {
            let mut chosen = self.marked.clone();
            if !chosen.contains(&file) {
                chosen.push(file);
            }
            return Outcome::Chosen(chosen);
        }
        Outcome::Chosen(vec![file])
    }

    /// Ctrl-O: confirm the marks, the folder, or the highlighted file.
    fn confirm(&mut self) -> Outcome {
        if self.directory {
            return Outcome::Chosen(vec![self.dir.clone()]);
        }
        if self.multiple && !self.marked.is_empty() {
            return Outcome::Chosen(self.marked.clone());
        }
        match self.entries.get(self.highlight) {
            Some(entry) if entry.kind == EntryKind::File => {
                Outcome::Chosen(vec![entry.path.clone()])
            }
            _ => {
                self.message = Some(String::from("Highlight a file to choose it."));
                Outcome::Pending
            }
        }
    }

    /// Enter on, or a click of, entry `index`.
    pub fn activate(&mut self, index: usize) -> Outcome {
        let Some(entry) = self.entries.get(index).cloned() else {
            return Outcome::Pending;
        };
        self.highlight = index;
        match entry.kind {
            EntryKind::Parent | EntryKind::Directory => {
                self.enter_directory(entry.path);
                Outcome::Pending
            }
            EntryKind::ChooseDirectory => Outcome::Chosen(vec![entry.path]),
            EntryKind::File => self.choose_file(entry.path),
        }
    }

    /// A click on entry `index`: marks files when several may be chosen.
    pub fn click(&mut self, index: usize) -> Outcome {
        if self.multiple
            && self
                .entries
                .get(index)
                .is_some_and(|entry| entry.kind == EntryKind::File)
        {
            self.highlight = index;
            let path = self.entries[index].path.clone();
            if let Some(position) = self.marked.iter().position(|marked| *marked == path) {
                self.marked.remove(position);
            } else {
                self.marked.push(path);
            }
            return Outcome::Pending;
        }
        self.activate(index)
    }

    /// The typed path, resolved against the listed directory.
    fn typed_path(&self, text: &str) -> PathBuf {
        let text = text.trim_end_matches(['\r', '\n']);
        if text == "~" {
            return home().unwrap_or_else(|| self.dir.clone());
        }
        if let Some(rest) = text.strip_prefix("~/")
            && let Some(home) = home()
        {
            return home.join(rest);
        }
        let path = Path::new(text);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.dir.join(path)
        }
    }

    fn open_typed(&mut self) -> Outcome {
        let path = self.typed_path(&self.input.clone());
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                self.input.clear();
                self.cursor = 0;
                self.enter_directory(path);
                Outcome::Pending
            }
            Ok(metadata) if metadata.is_file() && !self.directory => {
                self.input.clear();
                self.cursor = 0;
                self.choose_file(path)
            }
            Ok(_) if self.directory => {
                self.message = Some(String::from("Choose a folder."));
                Outcome::Pending
            }
            Ok(_) => {
                self.message = Some(format!("{} is not a regular file.", path.display()));
                Outcome::Pending
            }
            Err(error) => {
                self.message = Some(format!("{}: {error}", path.display()));
                Outcome::Pending
            }
        }
    }

    /// Tab: complete the entry's last path component.
    pub fn complete(&mut self) {
        let (head, partial) = match self.input.rfind(std::path::is_separator) {
            Some(index) => {
                let (head, rest) = self.input.split_at(index + 1);
                (head.to_string(), rest.to_string())
            }
            None => (String::new(), self.input.clone()),
        };
        let base = if head.is_empty() {
            self.dir.clone()
        } else {
            self.typed_path(&head)
        };
        let Ok(read) = std::fs::read_dir(&base) else {
            self.message = Some(format!("Cannot list {}", base.display()));
            return;
        };
        let mut candidates: Vec<(String, bool)> = read
            .filter_map(Result::ok)
            .take(MAX_LISTED_ENTRIES)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with(&partial) || (is_hidden(&name) && !partial.starts_with('.')) {
                    return None;
                }
                let dir = std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir());
                (dir || !self.directory).then_some((name, dir))
            })
            .collect();
        candidates.sort();
        match candidates.as_slice() {
            [] => self.message = Some(String::from("No completions.")),
            [(name, dir)] => {
                self.input = format!("{head}{name}{}", if *dir { "/" } else { "" });
                self.message = None;
            }
            many => {
                let first = &many[0].0;
                let common = many.iter().fold(first.len(), |length, (name, _)| {
                    first
                        .char_indices()
                        .zip(name.chars())
                        .take_while(|((_, a), b)| a == b)
                        .last()
                        .map_or(0, |((index, a), _)| index + a.len_utf8())
                        .min(length)
                });
                self.input = format!("{head}{}", &first[..common]);
                let shown: Vec<String> = many
                    .iter()
                    .take(8)
                    .map(|(name, dir)| format!("{name}{}", if *dir { "/" } else { "" }))
                    .collect();
                self.message = Some(format!(
                    "{}{}",
                    shown.join("  "),
                    if many.len() > shown.len() {
                        "  …"
                    } else {
                        ""
                    }
                ));
            }
        }
        self.cursor = self.input.chars().count();
    }

    fn byte_cursor(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map_or(self.input.len(), |(index, _)| index)
    }

    /// Insert pasted or typed text into the path entry.
    pub fn insert(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        let at = self.byte_cursor();
        self.input.insert_str(at, &text);
        self.cursor += text.chars().count();
        self.message = None;
    }

    fn move_highlight(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        self.highlight = self.highlight.saturating_add_signed(delta).min(last);
    }

    /// One key press. `page` is the number of listed rows on screen.
    pub fn key(&mut self, key: KeyEvent, page: usize) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let editing = !self.input.is_empty();
        let page = page.max(1) as isize;
        match key.code {
            KeyCode::Esc => return Outcome::Dismissed,
            KeyCode::Enter if editing => return self.open_typed(),
            KeyCode::Enter => return self.activate(self.highlight),
            KeyCode::Char('o' | 'O') if ctrl => return self.confirm(),
            KeyCode::Char('t' | 'T') if ctrl => {
                self.show_all = !self.show_all;
                self.reload();
            }
            KeyCode::Char('u' | 'U') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Tab => self.complete(),
            KeyCode::Up => self.move_highlight(-1),
            KeyCode::Down => self.move_highlight(1),
            KeyCode::PageUp => self.move_highlight(-page),
            KeyCode::PageDown => self.move_highlight(page),
            KeyCode::Home if editing => self.cursor = 0,
            KeyCode::End if editing => self.cursor = self.input.chars().count(),
            KeyCode::Home => self.highlight = 0,
            KeyCode::End => self.highlight = self.entries.len().saturating_sub(1),
            KeyCode::Left if editing => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right if editing => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
            }
            KeyCode::Left => {
                if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
                    self.enter_directory(parent);
                }
            }
            KeyCode::Right => {
                if self
                    .entries
                    .get(self.highlight)
                    .is_some_and(|entry| entry.kind == EntryKind::Directory)
                {
                    return self.activate(self.highlight);
                }
            }
            KeyCode::Backspace if editing => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = self.byte_cursor();
                    self.input.remove(at);
                }
            }
            KeyCode::Backspace => {
                if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
                    self.enter_directory(parent);
                }
            }
            KeyCode::Delete if editing => {
                if self.cursor < self.input.chars().count() {
                    let at = self.byte_cursor();
                    self.input.remove(at);
                }
            }
            KeyCode::Insert => self.toggle_mark(),
            KeyCode::Char(' ') if !editing && self.multiple => self.toggle_mark(),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(&c.to_string());
            }
            _ => {}
        }
        Outcome::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(accept: &str, multiple: bool, directory: bool) -> FilePickerRequest {
        FilePickerRequest {
            request: 7,
            accept: accept.into(),
            multiple,
            directory,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("photos")).unwrap();
        std::fs::write(dir.path().join("photos/cat.png"), b"png").unwrap();
        std::fs::write(dir.path().join("photos/dog.JPG"), b"jpg").unwrap();
        std::fs::write(dir.path().join("photos/notes.txt"), b"txt").unwrap();
        std::fs::write(dir.path().join("photos/.hidden.png"), b"x").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"readme").unwrap();
        dir
    }

    fn names(picker: &FilePicker) -> Vec<&str> {
        picker
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }

    #[test]
    fn accept_filters_listing_until_all_files_are_shown() {
        let dir = fixture();
        let mut picker = FilePicker::new(
            &request("image/*", false, false),
            Some(dir.path().join("photos")),
        );
        assert_eq!(names(&picker), ["..", "cat.png", "dog.JPG"]);
        assert_eq!(picker.highlight, 1, "the first real entry is highlighted");
        assert_eq!(picker.key(ctrl('t'), 10), Outcome::Pending);
        assert_eq!(
            names(&picker),
            ["..", ".hidden.png", "cat.png", "dog.JPG", "notes.txt"]
        );
        assert_eq!(picker.filter_label(), "all files");
    }

    #[test]
    fn single_choice_navigation_and_dismissal() {
        let dir = fixture();
        let mut picker = FilePicker::new(&request("", false, false), Some(dir.path().into()));
        assert_eq!(names(&picker), ["..", "photos", "readme.txt"]);
        assert_eq!(picker.key(key(KeyCode::Enter), 10), Outcome::Pending);
        assert_eq!(picker.dir, dir.path().join("photos"));
        assert_eq!(picker.key(key(KeyCode::Backspace), 10), Outcome::Pending);
        assert_eq!(picker.dir, dir.path());
        assert_eq!(
            picker.entries[picker.highlight].name, "photos",
            "the left directory stays highlighted"
        );
        picker.key(key(KeyCode::Down), 10);
        assert_eq!(
            picker.key(key(KeyCode::Enter), 10),
            Outcome::Chosen(vec![dir.path().join("readme.txt")])
        );
        assert_eq!(picker.key(key(KeyCode::Esc), 10), Outcome::Dismissed);
    }

    #[test]
    fn multiple_marks_span_directories() {
        let dir = fixture();
        let mut picker = FilePicker::new(&request("", true, false), Some(dir.path().into()));
        picker.key(key(KeyCode::End), 10);
        picker.key(key(KeyCode::Char(' ')), 10);
        assert!(picker.is_marked(&dir.path().join("readme.txt")));
        picker.key(key(KeyCode::Home), 10);
        picker.key(key(KeyCode::Down), 10);
        picker.key(key(KeyCode::Enter), 10);
        assert_eq!(picker.dir, dir.path().join("photos"));
        assert_eq!(picker.entries[picker.highlight].name, "cat.png");
        picker.key(key(KeyCode::Insert), 10);
        assert_eq!(
            picker.key(ctrl('o'), 10),
            Outcome::Chosen(vec![
                dir.path().join("readme.txt"),
                dir.path().join("photos/cat.png")
            ])
        );
    }

    #[test]
    fn typed_paths_complete_and_open() {
        let dir = fixture();
        let mut picker = FilePicker::new(&request("", false, false), Some(dir.path().into()));
        for c in "ph".chars() {
            picker.key(key(KeyCode::Char(c)), 10);
        }
        picker.key(key(KeyCode::Tab), 10);
        assert_eq!(picker.input, "photos/");
        picker.key(key(KeyCode::Char('c')), 10);
        picker.key(key(KeyCode::Tab), 10);
        assert_eq!(picker.input, "photos/cat.png");
        assert_eq!(
            picker.key(key(KeyCode::Enter), 10),
            Outcome::Chosen(vec![dir.path().join("photos/cat.png")])
        );
        picker.insert("photos/");
        picker.key(key(KeyCode::Tab), 10);
        assert_eq!(picker.input, "photos/");
        assert!(picker.message.as_deref().unwrap().contains("cat.png"));
        picker.key(key(KeyCode::Enter), 10);
        assert_eq!(picker.dir, dir.path().join("photos"));
        assert!(picker.input.is_empty());
    }

    #[test]
    fn directory_mode_lists_and_chooses_folders() {
        let dir = fixture();
        let mut picker = FilePicker::new(&request("", true, true), Some(dir.path().into()));
        assert!(!picker.multiple);
        assert_eq!(names(&picker), ["..", "[ Choose this folder ]", "photos"]);
        picker.key(key(KeyCode::End), 10);
        picker.key(key(KeyCode::Enter), 10);
        assert_eq!(
            picker.key(ctrl('o'), 10),
            Outcome::Chosen(vec![dir.path().join("photos")])
        );
        assert_eq!(picker.title(), "CHOOSE FOLDER");
    }
}
