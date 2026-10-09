//! Files the user selects for a page: file pickers and file drops.
//!
//! Local authoritative snapshots consulted (downloaded 2026-09-06):
//! WHATWG HTML `e5071a20c8569d8a3ec02ed27dd01b948773f850` (#file-upload-state-(type=file),
//! #attr-input-accept, #show-the-picker,-if-applicable, #drag-and-drop-processing-model),
//! W3C File API `1341d2687a5dbfc1fa6168e4e45a1c6afb0d6fb4` (#file-section, #snapshot-state,
//! #file-type-guidelines, #dfn-error-codes) and the WICG File and Directory Entries API
//! `3d02b95365186b23d03061caea147e6551700c8e` (#html-forms, #names-paths).
//!
//! * A File Upload control represents a list of *selected files*, each with a filename (without
//!   path components), a type and a body. Only files the user chose through a frontend picker or
//!   a drop are ever registered here, and the page reaches them only through opaque ids handed to
//!   its platform prelude, never through paths.
//! * File API: a `File` that refers to a file on disk records a *snapshot state* when it is
//!   created; reading it later fails with `NotReadableError` when the file no longer matches.
//!   TRust's snapshot is the file's length, modification time and (on Unix) device and inode.
//!   Bodies are read lazily, in bounded ranges, so selecting a large file costs no memory until
//!   the page reads or uploads it; a single read is capped by the HTTP body limit.
//! * Entries API: `webkitdirectory` selects every file below the chosen directory; each file's
//!   relative path starts with (and includes) that directory's name.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest byte range one read may materialize: the same 512 MiB ceiling TRust applies to HTTP
/// response bodies and local-file navigation (`http::MAX_BODY`, `file::MAX_FILE_BODY`). A form
/// upload is assembled in memory before it is sent, so its whole body observes this bound too.
pub const MAX_READ_BYTES: u64 = 512 * 1024 * 1024;

/// A directory selection is expanded eagerly into its files (Entries API #html-forms). These
/// bounds keep a mistaken choice such as `/` from walking the whole filesystem.
pub const MAX_DIRECTORY_FILES: usize = 10_000;
pub const MAX_DIRECTORY_DEPTH: usize = 64;

/// At most this many distinct files are registered for one page's lifetime.
pub const MAX_REGISTERED_FILES: usize = 100_000;

/// The state of a file on disk when the user selected it (File API #snapshot-state).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Snapshot {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    directory: bool,
}

impl Snapshot {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Self {
            len: if metadata.is_dir() { 0 } else { metadata.len() },
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            directory: metadata.is_dir(),
        }
    }
}

/// One file the user chose, described for the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedFile {
    pub path: PathBuf,
    /// HTML #file-upload-state: the filename, without path components.
    pub name: String,
    /// Entries API #dom-file-webkitrelativepath; empty outside a directory selection.
    pub relative_path: String,
    /// File API #file-type-guidelines: a lowercase parsable MIME type, or "" when unknown.
    pub mime: String,
    pub size: u64,
    /// File API #dfn-lastModified, in milliseconds since the Unix epoch, when the platform knows.
    pub last_modified: Option<i64>,
    /// A dropped directory is a File item (Entries API #html-data) whose body is empty and
    /// cannot be read as a file.
    pub directory: bool,
    pub snapshot: Snapshot,
}

/// HTML #file-upload-state: "Filenames must not contain path components", which are the parts
/// separated by U+005C REVERSE SOLIDUS. Native separators are already gone from a file name.
pub fn file_name_of(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rsplit_once('\\') {
        Some((_, last)) => last.to_string(),
        None => name,
    }
}

fn millis_since_epoch(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

impl SelectedFile {
    /// Describe `path` as it is now. `relative_path` is the Entries API relative path, if any.
    pub fn from_path(path: &Path, relative_path: String) -> Result<Self, String> {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        let name = file_name_of(path);
        let directory = metadata.is_dir();
        Ok(Self {
            path: path.to_path_buf(),
            mime: if directory {
                String::new()
            } else {
                mime_type_for_name(&name).unwrap_or_default().to_string()
            },
            name,
            relative_path,
            size: if directory { 0 } else { metadata.len() },
            last_modified: metadata.modified().ok().and_then(millis_since_epoch),
            directory,
            snapshot: Snapshot::of(&metadata),
        })
    }
}

/// Turn the paths a frontend returned into the selected files.
///
/// For a `webkitdirectory` control each chosen directory contributes every file below it, as if
/// all of them had been selected (Entries API #html-forms), in a stable name order. Otherwise
/// each path must be a regular file; a dropped directory is kept as one directory item when
/// `keep_directories` is set (HTML DnD), and rejected otherwise.
pub fn selection_from_paths(
    paths: &[PathBuf],
    directory: bool,
    keep_directories: bool,
) -> Result<Vec<SelectedFile>, String> {
    let mut out = Vec::new();
    for path in paths {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if directory && metadata.is_dir() {
            let root = file_name_of(path);
            expand_directory(path, &root, 0, &mut out)?;
        } else if metadata.is_dir() && !keep_directories {
            return Err(format!("{} is a directory", path.display()));
        } else {
            out.push(SelectedFile::from_path(path, String::new())?);
        }
    }
    Ok(out)
}

fn expand_directory(
    dir: &Path,
    relative: &str,
    depth: usize,
    out: &mut Vec<SelectedFile>,
) -> Result<(), String> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(format!(
            "{} is nested more than {MAX_DIRECTORY_DEPTH} directories deep",
            dir.display()
        ));
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot list {}: {error}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let Ok(link) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let child = format!("{relative}/{}", file_name_of(&path));
        if metadata.is_dir() {
            // A symbolic link to a directory could form a cycle; only real
            // directories are descended.
            if !link.file_type().is_symlink() {
                expand_directory(&path, &child, depth + 1, out)?;
            }
        } else if metadata.is_file() {
            if out.len() >= MAX_DIRECTORY_FILES {
                return Err(format!(
                    "the directory holds more than {MAX_DIRECTORY_FILES} files"
                ));
            }
            out.push(SelectedFile::from_path(&path, child)?);
        }
    }
    Ok(())
}

/// One parsed token of HTML #attr-input-accept.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AcceptToken {
    /// `audio/*`, `video/*` or `image/*`: the MIME type's top-level type.
    AnyOf(&'static str),
    /// A valid MIME type string with no parameters, lowercased.
    Mime(String),
    /// A string starting with ".", lowercased.
    Extension(String),
}

/// HTML #attr-input-accept: the file types a File Upload control hints it accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcceptFilter {
    tokens: Vec<AcceptToken>,
    source: String,
}

fn is_mime_token(value: &str) -> bool {
    // MIME Sniffing #http-token-code-point.
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

impl AcceptFilter {
    /// "To obtain the list of tokens from the attribute, the user agent must split the
    /// attribute value on commas" (Infra #split-on-commas strips ASCII whitespace from each
    /// token). Tokens that are none of the allowed forms carry no hint and are ignored.
    pub fn parse(accept: &str) -> Self {
        let mut tokens = Vec::new();
        for token in accept.split(',') {
            let token = token.trim_matches(|c: char| c.is_ascii_whitespace());
            let lower = token.to_ascii_lowercase();
            let parsed = match lower.as_str() {
                "audio/*" => AcceptToken::AnyOf("audio"),
                "video/*" => AcceptToken::AnyOf("video"),
                "image/*" => AcceptToken::AnyOf("image"),
                _ if lower.starts_with('.') && lower.len() > 1 => AcceptToken::Extension(lower),
                _ => match lower.split_once('/') {
                    Some((kind, subtype)) if is_mime_token(kind) && is_mime_token(subtype) => {
                        AcceptToken::Mime(lower)
                    }
                    _ => continue,
                },
            };
            if !tokens.contains(&parsed) {
                tokens.push(parsed);
            }
        }
        Self {
            tokens,
            source: accept.to_string(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The attribute value this filter came from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether a file of this name matches one of the tokens. Without tokens every file does.
    pub fn accepts(&self, name: &str) -> bool {
        if self.tokens.is_empty() {
            return true;
        }
        let lower = name.to_ascii_lowercase();
        let mime = mime_type_for_name(name);
        self.tokens.iter().any(|token| match token {
            AcceptToken::Extension(extension) => {
                lower.len() > extension.len() && lower.ends_with(extension.as_str())
            }
            AcceptToken::AnyOf(kind) => mime.is_some_and(|mime| {
                mime.split_once('/')
                    .is_some_and(|(top, _)| top.eq_ignore_ascii_case(kind))
            }),
            AcceptToken::Mime(wanted) => mime.is_some_and(|mime| mime == wanted),
        })
    }

    /// File-name extensions (without the dot) that express this filter for a native dialog,
    /// or `None` when a token has no known extension and the filter cannot be expressed.
    pub fn extensions(&self) -> Option<Vec<String>> {
        fn push(out: &mut Vec<String>, extension: &str) {
            if !out.iter().any(|known| known == extension) {
                out.push(extension.to_string());
            }
        }
        let mut out: Vec<String> = Vec::new();
        for token in &self.tokens {
            match token {
                AcceptToken::Extension(extension) => push(&mut out, &extension[1..]),
                AcceptToken::AnyOf(kind) => {
                    for (extension, mime) in MIME_TYPES {
                        if mime.split_once('/').is_some_and(|(top, _)| top == *kind) {
                            push(&mut out, extension);
                        }
                    }
                }
                AcceptToken::Mime(wanted) => {
                    let mut known = false;
                    for (extension, mime) in MIME_TYPES {
                        if mime == wanted {
                            push(&mut out, extension);
                            known = true;
                        }
                    }
                    if !known {
                        return None;
                    }
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

/// Common file-name extensions and the MIME type of files carrying them. File API
/// #file-type-guidelines: the type is ASCII lowercase, parsable, without a charset parameter
/// for text/plain, and decided without inspecting the contents. Native platforms map
/// extensions the same way (shared-mime-info globs, the Windows registry).
const MIME_TYPES: &[(&str, &str)] = &[
    ("7z", "application/x-7z-compressed"),
    ("aac", "audio/aac"),
    ("apng", "image/apng"),
    ("avif", "image/avif"),
    ("avi", "video/x-msvideo"),
    ("bmp", "image/bmp"),
    ("bz2", "application/x-bzip2"),
    ("css", "text/css"),
    ("csv", "text/csv"),
    ("doc", "application/msword"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    ("epub", "application/epub+zip"),
    ("flac", "audio/flac"),
    ("gif", "image/gif"),
    ("gz", "application/gzip"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("htm", "text/html"),
    ("html", "text/html"),
    ("ico", "image/vnd.microsoft.icon"),
    ("ics", "text/calendar"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("js", "text/javascript"),
    ("json", "application/json"),
    ("jxl", "image/jxl"),
    ("m4a", "audio/mp4"),
    ("m4v", "video/mp4"),
    ("md", "text/markdown"),
    ("mid", "audio/midi"),
    ("midi", "audio/midi"),
    ("mjs", "text/javascript"),
    ("mkv", "video/x-matroska"),
    ("mov", "video/quicktime"),
    ("mp3", "audio/mpeg"),
    ("mp4", "video/mp4"),
    ("mpeg", "video/mpeg"),
    ("mpg", "video/mpeg"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("oga", "audio/ogg"),
    ("ogg", "audio/ogg"),
    ("ogv", "video/ogg"),
    ("opus", "audio/ogg"),
    ("otf", "font/otf"),
    ("pdf", "application/pdf"),
    ("png", "image/png"),
    ("ppt", "application/vnd.ms-powerpoint"),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("rar", "application/vnd.rar"),
    ("rtf", "application/rtf"),
    ("svg", "image/svg+xml"),
    ("tar", "application/x-tar"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("ttf", "font/ttf"),
    ("txt", "text/plain"),
    ("wasm", "application/wasm"),
    ("wav", "audio/wav"),
    ("weba", "audio/webm"),
    ("webm", "video/webm"),
    ("webp", "image/webp"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("xhtml", "application/xhtml+xml"),
    ("xls", "application/vnd.ms-excel"),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("xml", "application/xml"),
    ("xz", "application/x-xz"),
    ("zip", "application/zip"),
    ("zst", "application/zstd"),
];

/// The MIME type a file of this name has, if its extension is known.
pub fn mime_type_for_name(name: &str) -> Option<&'static str> {
    let (_, extension) = name.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();
    MIME_TYPES
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, mime)| *mime)
}

/// Read bytes `start..end` (clamped to the snapshot length) of the file at `path`, failing when
/// it no longer matches `snapshot` (File API #snapshot-state).
fn read_snapshot(
    path: &Path,
    snapshot: &Snapshot,
    start: u64,
    end: u64,
) -> Result<Vec<u8>, ReadError> {
    let end = end.min(snapshot.len);
    let start = start.min(end);
    if end - start > MAX_READ_BYTES {
        return Err(ReadError::NotReadable(format!(
            "reading more than {} MiB of a file at once is not supported",
            MAX_READ_BYTES / (1024 * 1024)
        )));
    }
    let name = file_name_of(path);
    if snapshot.directory {
        return Err(ReadError::NotReadable(format!("{name} is a directory")));
    }
    let mut handle = std::fs::File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ReadError::NotFound(format!("{name} no longer exists")),
        _ => ReadError::NotReadable(format!("{name} cannot be read: {error}")),
    })?;
    let metadata = handle
        .metadata()
        .map_err(|error| ReadError::NotReadable(format!("{name} cannot be read: {error}")))?;
    if Snapshot::of(&metadata) != *snapshot {
        return Err(ReadError::NotReadable(format!(
            "{name} changed after it was selected"
        )));
    }
    handle
        .seek(SeekFrom::Start(start))
        .map_err(|error| ReadError::NotReadable(error.to_string()))?;
    let mut bytes = Vec::with_capacity((end - start) as usize);
    handle
        .take(end - start)
        .read_to_end(&mut bytes)
        .map_err(|error| ReadError::NotReadable(error.to_string()))?;
    if bytes.len() as u64 != end - start {
        return Err(ReadError::NotReadable(format!(
            "{name} changed after it was selected"
        )));
    }
    Ok(bytes)
}

impl SelectedFile {
    /// The file's whole body as it was when selected.
    pub fn read(&self) -> Result<Vec<u8>, ReadError> {
        if self.directory {
            return Ok(Vec::new());
        }
        read_snapshot(&self.path, &self.snapshot, 0, self.size)
    }
}

/// Why a registered file could not be read (File API #dfn-error-codes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// `NotFoundError`: the file no longer exists.
    NotFound(String),
    /// `NotReadableError`: the snapshot state differs from the file on disk, the file cannot
    /// be read, or the range exceeds [`MAX_READ_BYTES`].
    NotReadable(String),
}

impl ReadError {
    /// The `DOMException` name the File API assigns to this failure.
    pub fn dom_exception_name(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "NotFoundError",
            Self::NotReadable(_) => "NotReadableError",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(message) | Self::NotReadable(message) => message,
        }
    }
}

#[derive(Debug)]
struct Registered {
    path: PathBuf,
    snapshot: Snapshot,
}

/// The files one page's user has selected, by the opaque id the page holds. Selecting the
/// same file again with an unchanged snapshot yields the same id.
#[derive(Debug, Default)]
pub struct UserFiles {
    files: Vec<Registered>,
    ids: HashMap<(PathBuf, Snapshot), u64>,
}

/// A byte range of a file the user selected, with the snapshot state taken when it was
/// selected. It carries no open handle and no bytes: the File API's blob URL store keeps such
/// ranges for a selected file's Blob, and any agent that obtains the entry reads them lazily
/// (`read`) or makes them its own `UserFiles` entries (`UserFiles::register_range`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRange {
    path: PathBuf,
    snapshot: Snapshot,
    start: u64,
    end: u64,
}

impl FileRange {
    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The range's bytes, read now; fails like [`UserFiles::read`].
    pub fn read(&self) -> Result<Vec<u8>, ReadError> {
        read_snapshot(&self.path, &self.snapshot, self.start, self.end)
    }

    /// Approximate heap bytes held for the range.
    pub fn retained_bytes(&self) -> usize {
        2 * self.path.as_os_str().len() + size_of::<Self>()
    }
}

/// One piece of a request body whose file parts are read only when the body is assembled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodySegment {
    Bytes(Vec<u8>),
    File { id: u64, start: u64, end: u64 },
}

impl UserFiles {
    /// Make `file` readable by the page; returns its id.
    pub fn register(&mut self, file: &SelectedFile) -> Result<u64, String> {
        let key = (file.path.clone(), file.snapshot);
        if let Some(&id) = self.ids.get(&key) {
            return Ok(id);
        }
        if self.files.len() >= MAX_REGISTERED_FILES {
            return Err(format!(
                "more than {MAX_REGISTERED_FILES} files were selected on this page"
            ));
        }
        self.files.push(Registered {
            path: file.path.clone(),
            snapshot: file.snapshot,
        });
        let id = self.files.len() as u64;
        self.ids.insert(key, id);
        Ok(id)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Approximate heap bytes held for the registered files.
    pub fn retained_bytes(&self) -> usize {
        self.files
            .iter()
            .map(|file| 2 * file.path.as_os_str().len() + 2 * size_of::<Registered>())
            .sum()
    }

    /// The path of a registered directory item (a dropped directory), for expanding it.
    pub fn directory_path(&self, id: u64) -> Option<PathBuf> {
        self.entry(id)
            .filter(|file| file.snapshot.directory)
            .map(|file| file.path.clone())
    }

    /// The registered length of file `id`.
    pub fn size(&self, id: u64) -> Option<u64> {
        self.entry(id).map(|file| file.snapshot.len)
    }

    fn entry(&self, id: u64) -> Option<&Registered> {
        usize::try_from(id)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| self.files.get(index))
    }

    /// Bytes `start..end` (clamped to the snapshot length) of file `id` as a [`FileRange`],
    /// without reading them.
    pub fn range(&self, id: u64, start: u64, end: u64) -> Option<FileRange> {
        let file = self.entry(id)?;
        let end = end.min(file.snapshot.len);
        Some(FileRange {
            path: file.path.clone(),
            snapshot: file.snapshot,
            start: start.min(end),
            end,
        })
    }

    /// Make the file of `range` readable here too; returns its id. A range from another
    /// agent's registry names a file its user selected under the same snapshot.
    pub fn register_range(&mut self, range: &FileRange) -> Result<u64, String> {
        let key = (range.path.clone(), range.snapshot);
        if let Some(&id) = self.ids.get(&key) {
            return Ok(id);
        }
        if self.files.len() >= MAX_REGISTERED_FILES {
            return Err(format!(
                "more than {MAX_REGISTERED_FILES} files were selected on this page"
            ));
        }
        self.files.push(Registered {
            path: range.path.clone(),
            snapshot: range.snapshot,
        });
        let id = self.files.len() as u64;
        self.ids.insert(key, id);
        Ok(id)
    }

    /// Read bytes `start..end` (clamped to the snapshot length) of file `id`, failing when the
    /// file on disk no longer matches the snapshot taken when it was selected.
    pub fn read(&self, id: u64, start: u64, end: u64) -> Result<Vec<u8>, ReadError> {
        let file = self
            .entry(id)
            .ok_or_else(|| ReadError::NotReadable(String::from("unknown file")))?;
        read_snapshot(&file.path, &file.snapshot, start, end)
    }

    /// Concatenate a request body, reading its file segments now and keeping the whole body
    /// within `limit` bytes.
    pub fn assemble(&self, segments: &[BodySegment], limit: u64) -> Result<Vec<u8>, ReadError> {
        let mut total = 0u64;
        for segment in segments {
            total = total.saturating_add(match segment {
                BodySegment::Bytes(bytes) => bytes.len() as u64,
                BodySegment::File { id, start, end } => {
                    let size = self.size(*id).unwrap_or(0);
                    end.min(&size).saturating_sub(*start.min(&size))
                }
            });
        }
        if total > limit {
            return Err(ReadError::NotReadable(format!(
                "the upload is larger than the {} MiB request limit",
                limit / (1024 * 1024)
            )));
        }
        let mut body = Vec::with_capacity(total as usize);
        for segment in segments {
            match segment {
                BodySegment::Bytes(bytes) => body.extend_from_slice(bytes),
                BodySegment::File { id, start, end } => {
                    body.extend_from_slice(&self.read(*id, *start, *end)?)
                }
            }
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_tokens_follow_the_html_grammar() {
        // HTML #attr-input-accept: comma-separated, ASCII case-insensitive
        // audio/*, video/*, image/*, MIME types without parameters, and
        // extensions; other tokens are not hints.
        let filter =
            AcceptFilter::parse(" IMAGE/* , .PDF,text/plain;charset=x, bogus,,application/zip ");
        assert!(filter.accepts("cat.JPG"));
        assert!(filter.accepts("Report.pdf"));
        assert!(filter.accepts("bundle.zip"));
        assert!(
            !filter.accepts("notes.txt"),
            "a parameterized type is invalid"
        );
        assert!(!filter.accepts("song.mp3"));
        assert!(
            !filter.accepts(".pdf"),
            "an extension needs a name before it"
        );
        assert!(AcceptFilter::parse("").accepts("anything"));
        assert!(AcceptFilter::parse(" , bogus").is_empty());
        let extensions = AcceptFilter::parse(".tar.gz, audio/*")
            .extensions()
            .unwrap();
        assert_eq!(extensions[0], "tar.gz");
        assert!(extensions.iter().any(|extension| extension == "mp3"));
        assert_eq!(
            AcceptFilter::parse("application/x-unknown").extensions(),
            None
        );
        assert!(AcceptFilter::parse(".tar.gz").accepts("a.TAR.GZ"));
    }

    #[test]
    fn file_types_follow_the_file_api_guidelines() {
        assert_eq!(mime_type_for_name("A.PNG"), Some("image/png"));
        assert_eq!(mime_type_for_name("notes.txt"), Some("text/plain"));
        assert_eq!(mime_type_for_name("Makefile"), None);
        for (_, mime) in MIME_TYPES {
            assert_eq!(*mime, mime.to_ascii_lowercase());
            assert!(!mime.contains(';'));
        }
        assert_eq!(file_name_of(Path::new("/tmp/a\\b\\c.txt")), "c.txt");
    }

    #[test]
    fn reads_are_lazy_ranged_and_fail_after_the_snapshot_changes() {
        // File API #snapshot-state / NotReadableError.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello world.txt");
        std::fs::write(&path, b"hello world").unwrap();
        let file = SelectedFile::from_path(&path, String::new()).unwrap();
        assert_eq!(
            (file.name.as_str(), file.mime.as_str(), file.size),
            ("hello world.txt", "text/plain", 11)
        );
        assert!(file.last_modified.is_some());
        let mut files = UserFiles::default();
        let id = files.register(&file).unwrap();
        assert_eq!(files.register(&file).unwrap(), id, "same file, same id");
        assert_eq!(files.read(id, 6, 100).unwrap(), b"world");
        assert_eq!(files.read(id, 9, 3).unwrap(), b"");
        assert_eq!(
            files
                .assemble(
                    &[
                        BodySegment::Bytes(b"<".to_vec()),
                        BodySegment::File {
                            id,
                            start: 0,
                            end: 5
                        },
                        BodySegment::Bytes(b">".to_vec()),
                    ],
                    1024
                )
                .unwrap(),
            b"<hello>"
        );
        assert!(
            files
                .assemble(
                    &[BodySegment::File {
                        id,
                        start: 0,
                        end: 11
                    }],
                    4
                )
                .is_err()
        );
        assert!(matches!(
            files.read(99, 0, 1),
            Err(ReadError::NotReadable(_))
        ));
        std::fs::write(&path, b"HELLO WORLD!").unwrap();
        assert!(matches!(
            files.read(id, 0, 5),
            Err(ReadError::NotReadable(_))
        ));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            files.read(id, 0, 5).unwrap_err().dom_exception_name(),
            "NotFoundError"
        );
    }

    #[test]
    fn directory_selections_expand_with_relative_paths() {
        // Entries API #html-forms: the webkitRelativePath starts with (and
        // includes) the selected directory.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("to_upload");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/1.txt"), b"1").unwrap();
        std::fs::write(root.join("a/b/2.txt"), b"2").unwrap();
        std::fs::write(root.join("a/3.txt"), b"3").unwrap();
        std::fs::write(dir.path().join("not_uploaded.txt"), b"x").unwrap();
        let files = selection_from_paths(std::slice::from_ref(&root), true, false).unwrap();
        let described: Vec<_> = files
            .iter()
            .map(|file| (file.name.as_str(), file.relative_path.as_str()))
            .collect();
        assert_eq!(
            described,
            [
                ("3.txt", "to_upload/a/3.txt"),
                ("1.txt", "to_upload/a/b/1.txt"),
                ("2.txt", "to_upload/a/b/2.txt"),
            ]
        );
        assert!(selection_from_paths(std::slice::from_ref(&root), false, false).is_err());
        let dropped = selection_from_paths(&[root], false, true).unwrap();
        assert!(dropped[0].directory && dropped[0].size == 0 && dropped[0].mime.is_empty());
    }
}
