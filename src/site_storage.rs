//! Bookmark permission is separate from cookie/site and Storage/origin access.
//! HTML #obtain-a-site, URL #host-registrable-domain, Storage #storage-keys
//! (local WHATWG snapshots, 2026-09-06). Only HTTP(S) bookmarks grant permission.
//! Disk work is serialized with bookmark edits, off both frontend event loops.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::{Host, Url};

pub(crate) const ORIGIN_QUOTA: usize = 5 * 1024 * 1024;
pub(crate) const PROFILE_QUOTA: usize = 64 * 1024 * 1024;
const MAX_FILE: u64 = 8 * PROFILE_QUOTA as u64;
static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
type Wake = Arc<dyn Fn() + Send + Sync>;
static FRONTEND_WAKE: Mutex<Option<Wake>> = Mutex::new(None);

pub fn set_wake(wake: impl Fn() + Send + Sync + 'static) {
    *FRONTEND_WAKE.lock().unwrap() = Some(Arc::new(wake));
}
fn notify() {
    let wake = FRONTEND_WAKE.lock().unwrap().clone();
    if let Some(wake) = wake {
        wake();
    }
}

/// Only this kind can cross the persistence boundary. The other WebStorage
/// buckets back APIs with independent lifetimes and access rules.
enum StorageKind<'a> {
    Local(&'a str),
    Temporary,
}
fn storage_kind(bucket: &str) -> StorageKind<'_> {
    match bucket.strip_prefix("local:") {
        Some(origin) => StorageKind::Local(origin),
        None => StorageKind::Temporary,
    }
}

/// Scheme-independent permission key. Cookie same-site checks add the scheme.
pub(crate) fn site(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    match url.host()? {
        Host::Domain(host) => Some(
            String::from_utf8_lossy(
                psl::domain(host.as_bytes()).map_or(host.as_bytes(), |d| d.as_bytes()),
            )
            .into_owned(),
        ),
        host => Some(host.to_string()),
    }
}

pub(crate) fn same_site(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme() && site(a).is_some_and(|a| Some(a) == site(b))
}

fn sites(data: &Value) -> Result<HashSet<String>, String> {
    Ok(crate::bookmarks::entries(data)?
        .iter()
        .filter_map(|entry| Url::parse(&entry.url).ok().and_then(|url| site(&url)))
        .collect())
}

#[derive(Clone)]
struct Mutation {
    generation: u64,
    value: Option<Value>,
}
type Pending = BTreeMap<(String, String), Mutation>;
enum Command {
    Wake,
    Flush(mpsc::Sender<Result<(), String>>),
}
struct Service {
    bookmarks: PathBuf,
    policy: Mutex<HashMap<String, u64>>,
    pending: Mutex<Pending>,
    storage: crate::js::WebStorage,
    tx: mpsc::SyncSender<Command>,
    notice: Mutex<Option<String>>,
    disabled: Mutex<Option<String>>,
}

/// Explicit startup keeps tests and headless diagnostics away from the user's profile.
/// Call before constructing a frontend or starting any network/page actor work.
pub fn initialize() -> Result<(), String> {
    if SERVICE.get().is_some() {
        return Ok(());
    }
    let bookmarks = crate::storage::Paths::bookmarks_file()?;
    let (tx, rx) = mpsc::sync_channel(16);
    let service = Arc::new(Service {
        bookmarks,
        policy: Mutex::new(HashMap::new()),
        pending: Mutex::new(BTreeMap::new()),
        storage: Default::default(),
        tx,
        notice: Mutex::new(None),
        disabled: Mutex::new(None),
    });
    let restored = service.restore();
    if let Err(error) = &restored {
        *service.notice.lock().unwrap() = Some(error.clone());
        *service.disabled.lock().unwrap() = Some(error.clone());
    }
    SERVICE
        .set(service.clone())
        .map_err(|_| "Site storage already initialized")?;
    std::thread::Builder::new()
        .name("trust-site-storage".into())
        .spawn(move || {
            let mut expiry: Option<i64> = None;
            loop {
                let command = match expiry {
                    Some(deadline) => rx
                        .recv_timeout(Duration::from_millis((deadline - now()).max(1) as u64))
                        .map(Some),
                    None => rx
                        .recv()
                        .map(Some)
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                let command = match command {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                // One deadline per batch, never a perpetual polling timer.
                let mut replies = Vec::new();
                if let Some(Command::Flush(ref reply)) = command {
                    replies.push(reply.clone());
                }
                if matches!(command, Some(Command::Wake)) {
                    let deadline = std::time::Instant::now() + Duration::from_millis(100);
                    while let Some(wait) =
                        deadline.checked_duration_since(std::time::Instant::now())
                    {
                        match rx.recv_timeout(wait) {
                            Ok(Command::Flush(reply)) => {
                                replies.push(reply);
                                break;
                            }
                            Ok(Command::Wake) => {}
                            Err(_) => break,
                        }
                    }
                }
                let result = service.flush_disk();
                match &result {
                    Ok(next) => expiry = *next,
                    Err(error) => {
                        expiry = None;
                        *service.notice.lock().unwrap() = Some(error.clone());
                        notify();
                    }
                }
                for reply in replies {
                    let _ = reply.send(result.clone().map(|_| ()));
                }
            }
        })
        .map_err(|error| error.to_string())?;
    // The initial wake also arranges the first expiration deadline.
    wake();
    restored
}

pub fn flush() -> Result<(), String> {
    let Some(service) = SERVICE.get() else {
        return Ok(());
    };
    let (tx, rx) = mpsc::channel();
    service
        .tx
        .send(Command::Flush(tx))
        .map_err(|_| "Site storage worker stopped")?;
    rx.recv().map_err(|_| "Site storage worker stopped")?
}

/// Stop accepting late resource/actor writes, then drain every earlier mutation.
pub fn shutdown() -> Result<(), String> {
    if let Some(service) = SERVICE.get() {
        let _policy = service.policy.lock().unwrap();
        STOPPING.store(true, std::sync::atomic::Ordering::Release);
    }
    flush()
}

pub fn take_notice() -> Option<String> {
    SERVICE
        .get()
        .and_then(|service| service.notice.lock().unwrap().take())
}

pub(crate) fn web_storage() -> crate::js::WebStorage {
    SERVICE
        .get()
        .map_or_else(Default::default, |s| s.storage.clone())
}

fn wake() {
    if let Some(s) = SERVICE.get() {
        let _ = s.tx.try_send(Command::Wake);
    }
}

pub(crate) fn mutation(site: String, key: String, value: Option<Value>) {
    let Some(s) = SERVICE.get() else {
        return;
    };
    let policy = s.policy.lock().unwrap();
    if STOPPING.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Some(&generation) = policy.get(&site) else {
        drop(policy);
        wake();
        return;
    };
    s.pending
        .lock()
        .unwrap()
        .insert((site, key), Mutation { generation, value });
    drop(policy);
    wake();
}

pub(crate) fn local_mutation(bucket: &str, key: &str, value: Option<&str>) {
    let StorageKind::Local(origin) = storage_kind(bucket) else {
        return;
    };
    let Some(site) = Url::parse(origin).ok().and_then(|u| site(&u)) else {
        return;
    };
    mutation(
        site,
        local_key(origin, key),
        value.map(|value| json!({"kind":"local", "origin":origin,"key":key,"value":value})),
    );
}

pub(crate) fn local_clear(bucket: &str) {
    let StorageKind::Local(origin) = storage_kind(bucket) else {
        return;
    };
    let Some(site) = Url::parse(origin).ok().and_then(|u| site(&u)) else {
        return;
    };
    let Some(s) = SERVICE.get() else {
        return;
    };
    let policy = s.policy.lock().unwrap();
    if STOPPING.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Some(&generation) = policy.get(&site) else {
        return;
    };
    let mut pending = s.pending.lock().unwrap();
    pending.retain(|(owner, key), _| {
        owner != &site
            || serde_json::from_str::<Value>(key)
                .ok()
                .is_none_or(|v| v[0] != "local" || v[1] != origin)
    });
    pending.insert(
        (site, json!(["clear", origin]).to_string()),
        Mutation {
            generation,
            value: Some(json!({"kind":"clear","origin":origin})),
        },
    );
    drop(pending);
    drop(policy);
    wake();
}

fn local_key(origin: &str, key: &str) -> String {
    json!(["local", origin, key]).to_string()
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(i64::MAX as u128) as i64)
}

fn directory(bookmarks: &Path) -> PathBuf {
    bookmarks.parent().unwrap().join("site-data")
}
fn file_for(bookmarks: &Path, site: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    directory(bookmarks).join(format!(
        "{}.json",
        Sha256::digest(site.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}

fn read_json(path: &Path, default: Value) -> Result<Value, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(default),
        Err(e) => return Err(format!("Cannot read site storage: {e}")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("Site storage exceeds its file limit".into());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid site storage JSON")?;
    if value["version"] != 1 {
        return Err("Unsupported site storage version".into());
    }
    Ok(value)
}

fn write_json(path: &Path, data: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(data).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("Site storage exceeds its file limit".into());
    }
    crate::storage::create_private_dir(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    // A previous crash can leave a temporary file; it is never a restore source.
    let _ = fs::remove_file(&temp);
    let result = (|| -> std::io::Result<()> {
        let mut file = crate::storage::private_file()
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.map_err(|e| format!("Cannot save site storage: {e}"))
}

fn epochs(bookmarks: &Path) -> Result<Value, String> {
    let value = read_json(
        &directory(bookmarks).join("generations.json"),
        json!({"version":1,"sites":{}}),
    )?;
    if !value["sites"]
        .as_object()
        .is_some_and(|m| m.len() <= 100_000 && m.values().all(|v| v.as_u64().is_some()))
    {
        return Err("Invalid site storage generations".into());
    }
    Ok(value)
}
fn generation(epochs: &Value, site: &str) -> u64 {
    epochs["sites"][site].as_u64().unwrap_or(0)
}

fn load_site(bookmarks: &Path, site: &str) -> Result<Value, String> {
    let data = read_json(
        &file_for(bookmarks, site),
        json!({"version":1,"site":site,"records":{}}),
    )?;
    if data["site"] != site || !data["records"].is_object() {
        return Err("Invalid site storage records".into());
    }
    Ok(data)
}

/// Called while the stable bookmark lock is held. Revoke on disk BEFORE the
/// bookmark file is replaced: a crash can lose remembered data, never permission.
pub(crate) fn before_bookmarks_write(
    path: &Path,
    before: &Value,
    after: &Value,
) -> Result<(), String> {
    let old = sites(before)?;
    let new = sites(after)?;
    let removed: Vec<_> = old.difference(&new).collect();
    if removed.is_empty() {
        return Ok(());
    }
    let mut epochs = epochs(path)?;
    for site in &removed {
        epochs["sites"][site.as_str()] = generation(&epochs, site)
            .checked_add(1)
            .ok_or("Site generation exhausted")?
            .into();
    }
    write_json(&directory(path).join("generations.json"), &epochs)?;
    for site in removed {
        match fs::remove_file(file_for(path, site)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "Cannot remove saved site data; bookmark retained: {e}"
                ));
            }
        }
        let stem = file_for(path, site)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for file in fs::read_dir(directory(path)).map_err(|e| e.to_string())? {
            let file = file.map_err(|e| e.to_string())?;
            if file
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{stem}.tmp-"))
            {
                fs::remove_file(file.path()).map_err(|e| e.to_string())?;
            }
        }
    }
    #[cfg(unix)]
    fs::File::open(directory(path))
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Update this process's permission and capture pre-existing RAM state. The
/// caller flushes after releasing the file lock, before reporting completion.
pub(crate) fn after_bookmarks_write(
    path: &Path,
    before: &Value,
    after: &Value,
) -> Result<(), String> {
    let Some(s) = SERVICE.get().filter(|s| s.bookmarks == path) else {
        return Ok(());
    };
    let old = sites(before)?;
    let new = sites(after)?;
    let epochs = epochs(path)?;
    *s.policy.lock().unwrap() = new
        .iter()
        .map(|site| (site.clone(), generation(&epochs, site)))
        .collect();
    let added: HashSet<_> = new.difference(&old).cloned().collect();
    crate::http::persist_cookie_snapshot(&added);
    let storage = s.storage.lock().unwrap();
    for (bucket, values) in storage.iter() {
        if bucket
            .strip_prefix("local:")
            .and_then(|o| Url::parse(o).ok())
            .and_then(|u| site(&u))
            .is_some_and(|site| added.contains(&site))
        {
            for (key, value) in values {
                local_mutation(bucket, key, Some(value));
            }
        }
    }
    Ok(())
}

impl Service {
    fn restore(&self) -> Result<(), String> {
        // Reading an unused profile remains lazy.
        if !self.bookmarks.exists() && !directory(&self.bookmarks).exists() {
            return Ok(());
        }
        let _lock = crate::bookmarks::lock(&self.bookmarks)?;
        let allowed = sites(&crate::bookmarks::read(&self.bookmarks)?)?;
        let epochs = epochs(&self.bookmarks)?;
        self.commit(&allowed, &epochs, &Pending::new())?;
        let mut local: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut total = 0;
        let mut cookies = Vec::new();
        for site in &allowed {
            let data = load_site(&self.bookmarks, site)?;
            for value in data["records"].as_object().unwrap().values() {
                if value["kind"] == "cookie" {
                    if let Some(cookie) = crate::http::restore_cookie_record(value, site) {
                        cookies.push(cookie);
                    }
                } else if value["kind"] == "local" {
                    let (Some(origin), Some(key), Some(value)) = (
                        value["origin"].as_str(),
                        value["key"].as_str(),
                        value["value"].as_str(),
                    ) else {
                        continue;
                    };
                    let Ok(url) = Url::parse(origin) else {
                        continue;
                    };
                    if self::site(&url).as_ref() != Some(site)
                        || url.origin().ascii_serialization() != origin
                    {
                        continue;
                    }
                    let bucket = local.entry(format!("local:{origin}")).or_default();
                    let size = 2 * (key.encode_utf16().count() + value.encode_utf16().count());
                    let used: usize = bucket
                        .iter()
                        .map(|(k, v)| 2 * (k.encode_utf16().count() + v.encode_utf16().count()))
                        .sum();
                    if used + size > ORIGIN_QUOTA || total + size > PROFILE_QUOTA {
                        return Err("Saved localStorage exceeds quota".into());
                    }
                    bucket.insert(key.into(), value.into());
                    total += size;
                }
            }
        }
        *self.storage.lock().unwrap() = local;
        crate::http::restore_cookies(cookies);
        *self.policy.lock().unwrap() = allowed
            .iter()
            .map(|site| (site.clone(), generation(&epochs, site)))
            .collect();
        Ok(())
    }

    fn flush_disk(&self) -> Result<Option<i64>, String> {
        if let Some(error) = self.disabled.lock().unwrap().as_ref() {
            return Err(error.clone());
        }
        if !self.bookmarks.exists() && !directory(&self.bookmarks).exists() {
            return Ok(None);
        }
        let _lock = crate::bookmarks::lock(&self.bookmarks)?;
        let allowed = sites(&crate::bookmarks::read(&self.bookmarks)?)?;
        let epochs = epochs(&self.bookmarks)?;
        // A bookmark added by another process becomes effective on the next
        // storage event here too. Capture existing values under their RAM locks.
        let added: HashSet<_> = {
            let mut policy = self.policy.lock().unwrap();
            let added = allowed
                .iter()
                .filter(|site| !policy.contains_key(*site))
                .cloned()
                .collect();
            *policy = allowed
                .iter()
                .map(|site| (site.clone(), generation(&epochs, site)))
                .collect();
            added
        };
        if SERVICE
            .get()
            .is_some_and(|s| std::ptr::eq(s.as_ref(), self))
            && !added.is_empty()
        {
            crate::http::persist_cookie_snapshot(&added);
            for (bucket, entries) in self.storage.lock().unwrap().iter() {
                if bucket
                    .strip_prefix("local:")
                    .and_then(|o| Url::parse(o).ok())
                    .and_then(|u| site(&u))
                    .is_some_and(|s| added.contains(&s))
                {
                    for (key, value) in entries {
                        local_mutation(bucket, key, Some(value));
                    }
                }
            }
        }
        // Take mutations only after acquiring the disk lock. Recheck generation
        // and bookmark permission even if another running instance changed them.
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        let result = self.commit(&allowed, &epochs, &pending);
        if result.is_err() {
            let mut queued = self.pending.lock().unwrap();
            for (key, value) in pending {
                queued.entry(key).or_insert(value);
            }
        }
        *self.policy.lock().unwrap() = allowed
            .iter()
            .map(|site| (site.clone(), generation(&epochs, site)))
            .collect();
        result
    }

    fn commit(
        &self,
        allowed: &HashSet<String>,
        epochs: &Value,
        pending: &Pending,
    ) -> Result<Option<i64>, String> {
        let directory = directory(&self.bookmarks);
        let mut earliest: Option<i64> = None;
        let allowed_paths: HashSet<_> = allowed
            .iter()
            .map(|site| file_for(&self.bookmarks, site))
            .collect();
        if let Ok(files) = fs::read_dir(&directory) {
            for file in files {
                let file = file.map_err(|e| e.to_string())?;
                if file.file_name() == "generations.json" {
                    continue;
                }
                if file.path().extension().is_some_and(|e| e == "json") {
                    let path = file.path();
                    let owned = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
                    if owned && !allowed_paths.contains(&path) {
                        fs::remove_file(path).map_err(|e| e.to_string())?;
                    }
                } else if file
                    .path()
                    .extension()
                    .is_some_and(|e| e.to_string_lossy().starts_with("tmp-"))
                {
                    fs::remove_file(file.path()).map_err(|e| e.to_string())?;
                }
            }
        }
        let mut total_local = 0usize;
        let mut writes = Vec::new();
        for site in allowed {
            let path = file_for(&self.bookmarks, site);
            let mut data = load_site(&self.bookmarks, site)?;
            let original = data.clone();
            let records = data["records"].as_object_mut().unwrap();
            for ((owner, key), change) in pending {
                if owner != site || change.generation != generation(epochs, site) {
                    continue;
                }
                match &change.value {
                    Some(value) if value["kind"] == "clear" => records.retain(|_, record| {
                        record["kind"] != "local" || record["origin"] != value["origin"]
                    }),
                    Some(value) => {
                        records.insert(key.clone(), value.clone());
                    }
                    None => {
                        records.remove(key);
                    }
                }
            }
            let mut origin_sizes: HashMap<String, usize> = HashMap::new();
            for record in records.values().filter(|v| v["kind"] == "local") {
                let (Some(origin), Some(key), Some(value)) = (
                    record["origin"].as_str(),
                    record["key"].as_str(),
                    record["value"].as_str(),
                ) else {
                    return Err("Invalid localStorage record".into());
                };
                let size = 2 * (key.encode_utf16().count() + value.encode_utf16().count());
                total_local = total_local.saturating_add(size);
                let origin_size = origin_sizes.entry(origin.into()).or_default();
                *origin_size = origin_size.saturating_add(size);
                if *origin_size > ORIGIN_QUOTA || total_local > PROFILE_QUOTA {
                    return Err("Saved localStorage exceeds quota".into());
                }
            }
            records.retain(|_, value| {
                if value["kind"] == "local" {
                    return value["origin"].as_str().is_some_and(|origin| {
                        Url::parse(origin).ok().is_some_and(|url| {
                            self::site(&url).as_ref() == Some(site)
                                && url.origin().ascii_serialization() == origin
                        })
                    });
                }
                if value["kind"] != "cookie"
                    || crate::http::restore_cookie_record(value, site).is_none()
                {
                    return false;
                }
                let expiry = value["expires_at"].as_i64().unwrap();
                earliest = Some(earliest.map_or(expiry, |e| e.min(expiry)));
                true
            });
            if data != original {
                writes.push((path, data));
            }
        }
        for (path, data) in writes {
            if data["records"].as_object().unwrap().is_empty() {
                if path.exists() {
                    fs::remove_file(path).map_err(|e| e.to_string())?;
                }
            } else {
                write_json(&path, &data)?;
            }
        }
        if directory.exists() {
            #[cfg(unix)]
            fs::File::open(&directory)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Ok(earliest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        service: Service,
    }
    impl Fixture {
        fn new() -> Self {
            static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "trust-site-storage-{}-{}",
                std::process::id(),
                ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let (tx, _) = mpsc::sync_channel(16);
            let service = Service {
                bookmarks: root.join("bookmarks.json"),
                policy: Mutex::new(HashMap::new()),
                pending: Mutex::new(BTreeMap::new()),
                storage: Default::default(),
                tx,
                notice: Mutex::new(None),
                disabled: Mutex::new(None),
            };
            Self { root, service }
        }
        fn bookmarks(&self, urls: &[&str]) -> Value {
            let data = json!({"version":1,"bookmarks":urls.iter().enumerate().map(|(i,url)| json!({"id":i+1,"url":url,"title":"Test"})).collect::<Vec<_>>()});
            write_json(&self.service.bookmarks, &data).unwrap();
            data
        }
        fn pending_local(&self, origin: &str, key: &str, value: &str, generation: u64) {
            let site = site(&Url::parse(origin).unwrap()).unwrap();
            self.service.pending.lock().unwrap().insert(
                (site, local_key(origin, key)),
                Mutation {
                    generation,
                    value: Some(json!({"kind":"local","origin":origin,"key":key,"value":value})),
                },
            );
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn permission_uses_psl_private_rules_and_exact_host_fallback() {
        let key = |url| site(&Url::parse(url).unwrap());
        assert_eq!(
            key("http://a.example.co.uk:8000/page"),
            key("https://b.example.co.uk/")
        );
        assert_ne!(key("https://a.github.io/"), key("https://b.github.io/"));
        assert_ne!(
            key("https://a.example.com./"),
            key("https://a.example.com/")
        );
        assert_eq!(
            key("https://bücher.example/"),
            key("http://xn--bcher-kva.example/")
        );
        assert_eq!(key("http://[::1]:8000/"), Some("[::1]".into()));
        assert_eq!(key("http://localhost/"), Some("localhost".into()));
        assert_eq!(key("gemini://example.com/"), None);
        assert!(!same_site(
            &Url::parse("https://a.example.com").unwrap(),
            &Url::parse("http://b.example.com").unwrap()
        ));
    }

    #[test]
    fn saved_local_storage_restores_by_origin_and_ignores_unbookmarked_writes() {
        let _guard = crate::http::COOKIE_TEST_LOCK.lock().unwrap();
        let f = Fixture::new();
        f.bookmarks(&["https://www.example.com/page"]);
        f.pending_local("https://a.example.com", "key", "https", 0);
        f.pending_local("http://a.example.com", "key", "http", 0);
        f.pending_local("https://a.example.com:8443", "key", "port", 0);
        f.pending_local("https://elsewhere.test", "secret", "temporary", 0);
        f.service.flush_disk().unwrap();
        f.service.restore().unwrap();
        let storage = f.service.storage.lock().unwrap();
        assert_eq!(storage["local:https://a.example.com"]["key"], "https");
        assert_eq!(storage["local:http://a.example.com"]["key"], "http");
        assert_eq!(storage["local:https://a.example.com:8443"]["key"], "port");
        assert_eq!(storage.len(), 3);
        assert!(!file_for(&f.service.bookmarks, "elsewhere.test").exists());
    }

    #[test]
    fn last_bookmark_removal_purges_and_invalidates_queued_writes_after_readd() {
        let f = Fixture::new();
        let two = f.bookmarks(&["https://a.example.com/one", "http://b.example.com/two"]);
        f.pending_local("https://a.example.com", "token", "before", 0);
        f.service.flush_disk().unwrap();
        let one = json!({"version":1,"bookmarks":[two["bookmarks"][0].clone()]});
        before_bookmarks_write(&f.service.bookmarks, &two, &one).unwrap();
        assert!(file_for(&f.service.bookmarks, "example.com").exists());
        let empty = json!({"version":1,"bookmarks":[]});
        before_bookmarks_write(&f.service.bookmarks, &one, &empty).unwrap();
        assert!(!file_for(&f.service.bookmarks, "example.com").exists());
        f.pending_local("https://a.example.com", "token", "stale", 0);
        f.bookmarks(&["https://a.example.com/again"]);
        f.service.flush_disk().unwrap();
        assert!(!file_for(&f.service.bookmarks, "example.com").exists());
        f.pending_local("https://a.example.com", "token", "new", 1);
        f.service.flush_disk().unwrap();
        assert!(file_for(&f.service.bookmarks, "example.com").exists());
    }

    #[test]
    fn independent_instances_merge_changes_and_clear_removes_all_origin_keys() {
        let f = Fixture::new();
        let mut second = Fixture::new();
        second.service.bookmarks = f.service.bookmarks.clone();
        f.bookmarks(&["https://example.com"]);
        f.pending_local("https://example.com", "first", "one", 0);
        second.pending_local("https://example.com", "second", "two", 0);
        f.service.flush_disk().unwrap();
        second.service.flush_disk().unwrap();
        let data = load_site(&f.service.bookmarks, "example.com").unwrap();
        assert_eq!(data["records"].as_object().unwrap().len(), 2);
        f.service.pending.lock().unwrap().insert(
            (
                "example.com".into(),
                json!(["clear", "https://example.com"]).to_string(),
            ),
            Mutation {
                generation: 0,
                value: Some(json!({"kind":"clear","origin":"https://example.com"})),
            },
        );
        f.pending_local("https://example.com", "after", "three", 0);
        f.service.flush_disk().unwrap();
        let data = load_site(&f.service.bookmarks, "example.com").unwrap();
        assert_eq!(data["records"].as_object().unwrap().len(), 1);
        assert_eq!(
            data["records"][local_key("https://example.com", "after")]["value"],
            "three"
        );
    }

    #[test]
    fn cookies_keep_absolute_expiration_and_session_cookies_never_restore() {
        let _guard = crate::http::COOKIE_TEST_LOCK.lock().unwrap();
        let f = Fixture::new();
        f.bookmarks(&["https://example.com"]);
        let expiry = now() + 60_000;
        let cookie = json!({"kind":"cookie","name":"auth","value":"value","domain":"example.com","host_only":true,"path":"/account","secure":true,"http_only":true,"same_site":"Strict","created_at":now(),"last_access":now(),"expires_at":expiry});
        let mut session = cookie.clone();
        session["expires_at"] = Value::Null;
        let mut expired = cookie.clone();
        expired["expires_at"] = (now() - 1).into();
        write_json(&file_for(&f.service.bookmarks,"example.com"),&json!({"version":1,"site":"example.com","records":{"persist":cookie,"session":session,"expired":expired}})).unwrap();
        assert_eq!(f.service.flush_disk().unwrap(), Some(expiry));
        let data = load_site(&f.service.bookmarks, "example.com").unwrap();
        assert_eq!(data["records"].as_object().unwrap().len(), 1);
        assert_eq!(data["records"]["persist"]["expires_at"], expiry);
        f.service.restore().unwrap();
        assert_eq!(
            crate::http::cookies_for_js(&Url::parse("https://example.com/account").unwrap()),
            ""
        );
        assert_eq!(
            crate::http::cookies_for_request(&Url::parse("https://example.com/account").unwrap()),
            "auth=value"
        );
        assert_eq!(
            crate::http::cookies_for_request(&Url::parse("http://example.com/account").unwrap()),
            ""
        );
    }

    #[test]
    fn corrupt_or_oversized_storage_does_not_partially_restore() {
        let _guard = crate::http::COOKIE_TEST_LOCK.lock().unwrap();
        let f = Fixture::new();
        f.bookmarks(&["https://example.com"]);
        f.pending_local("https://example.com", "token", "good", 0);
        f.service.flush_disk().unwrap();
        fs::write(file_for(&f.service.bookmarks, "example.com"), b"invalid").unwrap();
        assert!(f.service.restore().is_err());
        assert!(f.service.storage.lock().unwrap().is_empty());
        fs::remove_file(file_for(&f.service.bookmarks, "example.com")).unwrap();
        f.pending_local("https://example.com", "big", &"x".repeat(ORIGIN_QUOTA), 0);
        assert!(f.service.flush_disk().is_err());
        assert!(!file_for(&f.service.bookmarks, "example.com").exists());
    }

    #[cfg(unix)]
    #[test]
    fn site_files_are_private_and_temporary_files_are_removed_on_revocation() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let before = f.bookmarks(&["https://example.com"]);
        f.pending_local("https://example.com", "key", "value", 0);
        f.service.flush_disk().unwrap();
        let path = file_for(&f.service.bookmarks, "example.com");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(directory(&f.service.bookmarks))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let temp = path.with_extension("tmp-12345");
        fs::write(&temp, b"private").unwrap();
        before_bookmarks_write(
            &f.service.bookmarks,
            &before,
            &json!({"version":1,"bookmarks":[]}),
        )
        .unwrap();
        assert!(!path.exists());
        assert!(!temp.exists());
    }
}
