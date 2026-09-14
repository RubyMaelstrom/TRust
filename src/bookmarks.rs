//! Shared protocol-neutral bookmarks. JSON strings/UTF-8 follow RFC 8259
//! §§4, 7, 8 (RFC Editor snapshot 2026-09-06); opaque Gopher URLs use RFC 4266.
//! All filesystem work runs on a blocking worker, never a frontend event loop.
use crate::doc::{Doc, Link};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_FILE: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 2000;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: u64,
    pub title: String,
    pub url: String,
}
#[derive(Clone, Debug)]
pub enum Operation {
    List(String),
    Add {
        url: String,
        title: String,
        explicit_title: bool,
    },
    Rename {
        id: u64,
        title: String,
    },
    Remove(u64),
    Undo,
}
#[derive(Debug)]
pub struct Completion {
    pub result: Result<Outcome, String>,
}
#[derive(Debug)]
pub struct Outcome {
    pub message: String,
    pub listing: Option<String>,
}
#[derive(Clone, Default)]
pub struct Context {
    pub current: Option<(Link, String)>,
    pub selected: Option<(Link, String)>,
}

pub fn canonical(link: &Link) -> Result<String, String> {
    let address = match link {
        Link::Media(url) => url.to_string(),
        Link::JsClick { href, .. } => href.clone(),
        Link::Form { .. } => return Err("Choose a destination link to bookmark".into()),
        _ => link.to_string(),
    };
    crate::gopher::absolute_link(&address)
        .map(|link| link.to_string())
        .ok_or_else(|| "This item has no persistent, navigable address to bookmark".into())
}
pub fn suggested_title(doc: &Doc) -> String {
    doc.lines
        .iter()
        .find(|line| {
            matches!(line.kind, crate::doc::Kind::Heading(_)) && !line.text.trim().is_empty()
        })
        .map(|line| clean_title(&line.text))
        .unwrap_or_else(|| doc.url.to_string())
}
fn clean_title(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect::<String>()
        .trim()
        .to_string()
}
pub fn operation(command: &str, context: &Context) -> Result<Operation, String> {
    let args = crate::command::quoted_arguments(command)?;
    if args
        .first()
        .is_some_and(|s| s.eq_ignore_ascii_case("bookmarks"))
    {
        return Ok(Operation::List(args[1..].join(" ")));
    }
    match args.get(1).map(String::as_str).unwrap_or("add") {
        "add" | "link" => {
            let selected = args.get(1).is_some_and(|s| s == "link");
            let (link, suggested) = (if selected { &context.selected } else { &context.current }).as_ref().ok_or_else(|| if selected { "No selected link" } else { "No current destination" }.to_string())?;
            let explicit_title = args.len() > 2;
            let title = if explicit_title { args[2..].join(" ") } else { suggested.clone() };
            Ok(Operation::Add { url: canonical(link)?, title: clean_title(&title), explicit_title })
        }
        "rename" if args.len() >= 4 => Ok(Operation::Rename { id: args[2].parse().map_err(|_| "Invalid bookmark ID")?, title: clean_title(&args[3..].join(" ")) }),
        "remove" if args.len() == 3 => Ok(Operation::Remove(args[2].parse().map_err(|_| "Invalid bookmark ID")?)),
        "undo" if args.len() == 2 => Ok(Operation::Undo),
        _ => Err("Use bookmark [add|link] [title], bookmark rename <id> <title>, bookmark remove <id>, bookmark undo, or bookmarks [filter]".into()),
    }
}

pub struct Worker {
    tx: tokio::sync::mpsc::Sender<Operation>,
}
impl Worker {
    #[cfg(test)]
    pub(crate) fn test_channel() -> (Self, tokio::sync::mpsc::Receiver<Operation>) {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        (Self { tx }, rx)
    }

    pub fn start(
        runtime: &tokio::runtime::Handle,
        complete: impl Fn(Completion) + Send + 'static,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        runtime.spawn(async move {
            let mut removed = None;
            while let Some(operation) = rx.recv().await {
                let result = tokio::task::spawn_blocking(move || {
                    let result = crate::storage::Paths::bookmarks_file()
                        .and_then(|path| apply(&path, operation, &mut removed));
                    (result, removed)
                })
                .await;
                let outcome = match result {
                    Ok((result, undo)) => {
                        removed = undo;
                        result
                    }
                    Err(e) => {
                        removed = None;
                        Err(format!("Bookmark worker failed: {e}"))
                    }
                };
                complete(Completion { result: outcome });
            }
        });
        Self { tx }
    }
    pub fn submit(&self, op: Operation) -> Result<(), String> {
        self.tx
            .try_send(op)
            .map_err(|_| "Bookmark queue is busy; try again shortly".into())
    }
}

pub(crate) fn entries(data: &Value) -> Result<Vec<Entry>, String> {
    if data.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("Unsupported bookmarks file version; file left untouched".into());
    }
    let values = data
        .get("bookmarks")
        .and_then(Value::as_array)
        .ok_or("Invalid bookmarks array")?;
    if values.len() > MAX_ENTRIES {
        return Err("Bookmarks file exceeds 2000 entries".into());
    }
    let mut seen = std::collections::HashSet::new();
    values
        .iter()
        .map(|v| {
            let id = v
                .get("id")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0 && seen.insert(*id))
                .ok_or("Invalid or duplicate bookmark ID")?;
            let title = v
                .get("title")
                .and_then(Value::as_str)
                .ok_or("Invalid bookmark title")?;
            let url = v
                .get("url")
                .and_then(Value::as_str)
                .ok_or("Invalid bookmark URL")?;
            if title.len() > 4096 || url.len() > 32768 || url.chars().any(char::is_control) {
                return Err("Bookmark field exceeds limits or contains controls".into());
            }
            Ok(Entry {
                id,
                title: title.into(),
                url: url.into(),
            })
        })
        .collect()
}
pub(crate) fn read(path: &Path) -> Result<Value, String> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"version":1,"next_id":1,"bookmarks":[]}));
        }
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("Bookmarks file exceeds 8 MiB; file left untouched".into());
    }
    let data = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Cannot parse {}: {e}; file left untouched", path.display()))?;
    entries(&data)?;
    Ok(data)
}
fn write(path: &Path, data: &Value) -> Result<(), String> {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let bytes = serde_json::to_vec_pretty(data).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("Bookmarks would exceed 8 MiB; file left untouched".into());
    }
    let temp = path.with_extension(format!(
        "json.tmp-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = crate::storage::private_file()
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|e| format!("Cannot save {}: {e}", path.display()))
}
pub(crate) fn lock(path: &Path) -> Result<fs::File, String> {
    crate::storage::create_private_dir(path.parent().ok_or("Invalid bookmark path")?)
        .map_err(|e| e.to_string())?;
    // Lock a stable inode, not the JSON inode that atomic replacement changes.
    // OS ownership releases the lock on process exit, including a crash.
    let lock = crate::storage::private_file()
        .read(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(format!("Bookmark store is locked or unavailable: {e}")),
        }
    }
    Ok(lock)
}

fn apply(
    path: &Path,
    operation: Operation,
    removed: &mut Option<Value>,
) -> Result<Outcome, String> {
    if let Operation::List(filter) = &operation {
        return Ok(Outcome {
            message: "Bookmarks".into(),
            listing: Some(listing(&entries(&read(path)?)?, filter)),
        });
    }
    let store_lock = lock(path)?;
    let mut data = read(path)?;
    let before = data.clone();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut new_removed = removed.clone();
    let message = match operation {
        Operation::Add {
            url,
            title,
            explicit_title,
        } => {
            let canonical_url = crate::gopher::absolute_link(&url)
                .ok_or("Unsupported bookmark URL")?
                .to_string();
            let existing = entries(&data)?
                .iter()
                .position(|entry| entry.url == canonical_url);
            if let Some(i) = existing {
                let item = &mut data["bookmarks"][i];
                if explicit_title {
                    if title.is_empty() {
                        return Err("Bookmark title is empty".into());
                    }
                    item["title"] = title.into();
                    item["updated"] = now.into();
                }
                format!("Bookmark #{} already saved", item["id"])
            } else {
                let next = data
                    .get("next_id")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(
                        entries(&data)?
                            .iter()
                            .map(|e| e.id)
                            .max()
                            .unwrap_or(0)
                            .checked_add(1)
                            .ok_or("Bookmark IDs exhausted")?,
                    );
                data["next_id"] = next.checked_add(1).ok_or("Bookmark IDs exhausted")?.into();
                let items = data["bookmarks"].as_array_mut().unwrap();
                if items.len() >= MAX_ENTRIES {
                    return Err("Bookmark limit reached (2000)".into());
                }
                items.push(json!({"id":next,"url":canonical_url,"title":if title.is_empty() { url } else { title },"created":now,"updated":now}));
                format!("Bookmarked #{} · Alt+B opens bookmarks", next)
            }
        }
        Operation::Rename { id, title } => {
            if title.is_empty() {
                return Err("Bookmark title is empty".into());
            }
            let item = data["bookmarks"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|v| v["id"].as_u64() == Some(id))
                .ok_or("Bookmark not found")?;
            item["title"] = title.into();
            item["updated"] = now.into();
            format!("Renamed bookmark #{id}")
        }
        Operation::Remove(id) => {
            let items = data["bookmarks"].as_array_mut().unwrap();
            let i = items
                .iter()
                .position(|v| v["id"].as_u64() == Some(id))
                .ok_or("Bookmark not found")?;
            new_removed = Some(items.remove(i));
            format!("Removed bookmark #{id} · bookmark undo restores it")
        }
        Operation::Undo => {
            let entry = new_removed
                .take()
                .ok_or("No bookmark removal to undo in this session")?;
            let items = data["bookmarks"].as_array_mut().unwrap();
            if items.len() >= MAX_ENTRIES
                || items
                    .iter()
                    .any(|v| v["id"] == entry["id"] || v["url"] == entry["url"])
            {
                return Err("Cannot restore: bookmark already exists or store is full".into());
            }
            items.push(entry);
            "Bookmark restored".into()
        }
        Operation::List(_) => unreachable!(),
    };
    entries(&data)?;
    crate::site_storage::before_bookmarks_write(path, &before, &data)?;
    write(path, &data)?;
    *removed = new_removed;
    crate::site_storage::after_bookmarks_write(path, &before, &data)?;
    drop(store_lock);
    crate::site_storage::flush()?;
    Ok(Outcome {
        message,
        listing: None,
    })
}
pub fn listing(entries: &[Entry], filter: &str) -> String {
    let filter = filter.to_lowercase();
    let mut body = "# Bookmarks\n\nCtrl+B saves the current destination · Alt+B opens this list.\nIn Telnet, use bookmark and bookmarks; remote keys stay available.\nCommands: bookmark link [title] · bookmark rename <id> <title> · bookmark remove <id> · bookmark undo\nFilter: bookmarks <text>\n\n".to_string();
    let mut count = 0;
    for entry in entries.iter().filter(|e| {
        filter.is_empty()
            || e.title.to_lowercase().contains(&filter)
            || e.url.to_lowercase().contains(&filter)
    }) {
        count += 1;
        // Only render navigable links. Never interpret JSON titles as markup.
        let title = clean_title(&entry.title);
        if let Some(link) = crate::gopher::absolute_link(&entry.url) {
            body.push_str(&format!("=> {link} #{} · {title}\n", entry.id));
        } else {
            body.push_str(&format!("#{} · {title} (unsupported address)\n", entry.id));
        }
    }
    if count == 0 {
        body.push_str(if entries.is_empty() {
            "No bookmarks yet.\n"
        } else {
            "No matching bookmarks.\n"
        });
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "trust-bookmarks-test-{}-{}",
                std::process::id(),
                ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> std::path::PathBuf {
            self.0.join("data/bookmarks.json")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn add(url: &str, title: &str) -> Operation {
        Operation::Add {
            url: url.into(),
            title: title.into(),
            explicit_title: false,
        }
    }
    #[test]
    fn lists_lazily_without_creating_directories() {
        let f = Fixture::new();
        let result = apply(&f.path(), Operation::List("".into()), &mut None).unwrap();
        assert!(result.listing.unwrap().contains("No bookmarks yet"));
        assert!(!f.path().parent().unwrap().exists());
    }
    #[test]
    fn all_protocols_round_trip_across_restart_and_duplicate_adds() {
        let f = Fixture::new();
        for url in [
            "gopher://e/7/s%09rust%20lang",
            "gophers://e/7/s%09rust%20lang",
            "https://e/path?q=1#part",
            "http://e/path",
            "gemini://e/path",
            "finger://e/alice",
            "whois://e/domain",
            "dict://e/d:hello",
            "telnet://[::1]:23",
            "telnets://e:992",
        ] {
            let target = crate::gopher::absolute_link(url).unwrap();
            let url = canonical(&target).unwrap();
            apply(&f.path(), add(&url, "first title"), &mut None).unwrap();
            apply(&f.path(), add(&url, "replacement suggestion"), &mut None).unwrap();
            let reloaded = entries(&read(&f.path()).unwrap()).unwrap();
            let saved = reloaded.iter().find(|entry| entry.url == url).unwrap();
            assert_eq!(saved.title, "first title");
            assert_eq!(
                canonical(&crate::gopher::absolute_link(&saved.url).unwrap()).unwrap(),
                url
            );
        }
        assert_eq!(entries(&read(&f.path()).unwrap()).unwrap().len(), 10);
    }
    #[test]
    fn edits_and_undo_preserve_concurrent_additions_and_unknown_fields() {
        let f = Fixture::new();
        let mut undo = None;
        apply(&f.path(), add("gopher://e/1", "menu"), &mut undo).unwrap();
        let mut original = read(&f.path()).unwrap();
        original["extension"] = json!({"future":true});
        write(&f.path(), &original).unwrap();
        apply(
            &f.path(),
            Operation::Rename {
                id: 1,
                title: "Books".into(),
            },
            &mut undo,
        )
        .unwrap();
        apply(&f.path(), Operation::Remove(1), &mut undo).unwrap();
        apply(&f.path(), add("https://e/new", "New"), &mut None).unwrap();
        apply(&f.path(), Operation::Undo, &mut undo).unwrap();
        let data = read(&f.path()).unwrap();
        let saved = entries(&data).unwrap();
        assert_eq!(saved.len(), 2);
        assert!(saved.iter().any(|e| e.title == "Books"));
        assert_eq!(data["extension"], original["extension"]);
        let filtered = listing(&saved, "BOOKS");
        assert!(filtered.contains("Books"));
        assert!(!filtered.contains("https://e/new"));
    }
    #[test]
    fn concurrent_instances_do_not_lose_updates() {
        let f = Fixture::new();
        let path = f.path();
        let threads: Vec<_> = (0..12)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    apply(
                        &path,
                        add(&format!("gopher://e/0/file{i}"), &format!("File {i}")),
                        &mut None,
                    )
                    .unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let saved = entries(&read(&path).unwrap()).unwrap();
        assert_eq!(saved.len(), 12);
        assert_eq!(
            saved
                .iter()
                .map(|e| e.id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            12
        );
    }
    #[test]
    fn malformed_and_future_files_are_never_overwritten() {
        let f = Fixture::new();
        fs::create_dir_all(f.path().parent().unwrap()).unwrap();
        for bytes in [b"{broken".as_slice(), br#"{"version":2,"bookmarks":[]}"#] {
            fs::write(f.path(), bytes).unwrap();
            assert!(apply(&f.path(), add("https://e", "E"), &mut None).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
        }
    }
    #[test]
    fn titles_cannot_inject_local_page_markup_and_search_urls_stay_exact() {
        let url = "gopher://e/7/s%09rust%20lang";
        let rendered = listing(
            &[Entry {
                id: 1,
                title: "title\n=> https://evil.test injected".into(),
                url: url.into(),
            }],
            "",
        );
        assert_eq!(rendered.lines().filter(|l| l.starts_with("=>")).count(), 1);
        assert!(rendered.contains(url));
    }
    #[cfg(unix)]
    #[test]
    fn new_storage_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        apply(&f.path(), add("https://e", "E"), &mut None).unwrap();
        assert_eq!(
            fs::metadata(f.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(f.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
