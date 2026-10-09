//! File API (local w3c/FileAPI@1341d26) #url-model: the blob URL store.
//!
//! The user agent's blob URL store maps blob URLs (keys without a fragment) to blob URL
//! entries: a Blob's byte sequence and the environment settings object that created the URL.
//! A page and all of its agents — its Windows and frames on the page actor, its dedicated,
//! nested and shared workers — share one store, so a URL minted in any of them resolves in
//! the others (#blob-url-resolve), subject to #partitioningOfBlobUrls: only environments
//! whose storage key equals the creator's obtain the Blob (#blob-url-obtain-object), and only
//! they may revoke it (#dfn-revokeObjectURL). A navigation obtains it without that check.
//!
//! An entry keeps the Blob's leaves: in-memory bytes, shared and immutable, or byte ranges of
//! files the user selected, which stay unread until the entry is consumed (#lifeTime does not
//! keep a file open; the File API's snapshot state still applies to every read). Entries leave
//! the store when revoked, when their environment's Document unloads (the page's store goes
//! away with it, and a frame's Window takes its own entries along when its realm is released),
//! and when their worker ends (the hook #lifeTime asks for).

use crate::file_select::{FileRange, ReadError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One leaf of a stored Blob's byte sequence.
#[derive(Clone, Debug)]
pub enum BlobPart {
    Bytes(Arc<[u8]>),
    File(FileRange),
}

impl BlobPart {
    pub fn len(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::File(range) => range.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The environment settings object that created an entry: an agent and one of its realms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Environment {
    pub agent: u64,
    pub realm: u64,
}

/// A blob URL entry (#blob-url-entry): the Blob's byte sequence and type, with its
/// environment and that environment's origin and storage key.
#[derive(Debug)]
pub struct BlobUrlEntry {
    pub parts: Vec<BlobPart>,
    pub content_type: String,
    /// The ASCII serialization of the environment's origin (URL #concept-url-origin of the
    /// blob URL while the entry exists).
    pub origin: String,
    /// The environment's storage key for non-storage purposes (Storage #obtain-a-storage-key):
    /// an origin's serialization, or a unique identity for an opaque origin.
    pub storage_key: String,
    pub environment: Environment,
    /// The referrer policy of the environment's policy container when the URL was created.
    /// HTML #creating-a-policy-container-from-a-fetch-response clones the environment's
    /// container for a blob URL's Document or worker; TRust keeps the policy the entry was
    /// created under rather than following later `<meta name=referrer>` changes.
    pub referrer_policy: crate::referrer_policy::ReferrerPolicy,
}

impl BlobUrlEntry {
    pub fn size(&self) -> u64 {
        self.parts.iter().map(BlobPart::len).sum()
    }

    /// The whole byte sequence; selected-file ranges are read now, and their snapshot state
    /// must still hold (#snapshot-state).
    pub fn read(&self) -> Result<Vec<u8>, ReadError> {
        let size = self.size();
        if size > crate::file_select::MAX_READ_BYTES {
            return Err(ReadError::NotReadable(format!(
                "the blob is larger than the {} MiB read limit",
                crate::file_select::MAX_READ_BYTES / (1024 * 1024)
            )));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        for part in &self.parts {
            match part {
                BlobPart::Bytes(chunk) => bytes.extend_from_slice(chunk),
                BlobPart::File(range) => bytes.extend_from_slice(&range.read()?),
            }
        }
        Ok(bytes)
    }

    fn retained_bytes(&self) -> usize {
        self.parts
            .iter()
            .map(|part| match part {
                BlobPart::Bytes(bytes) => bytes.len(),
                BlobPart::File(range) => range.retained_bytes(),
            })
            .sum::<usize>()
            + self.content_type.len()
            + self.origin.len()
            + self.storage_key.len()
            + std::mem::size_of::<Self>()
    }
}

/// One page's blob URL store, shared by every agent of the page.
#[derive(Debug, Default)]
pub struct BlobUrlStore {
    entries: Mutex<HashMap<String, Arc<BlobUrlEntry>>>,
}

/// The store key of a URL string: its serialization without the fragment (#blob-url-resolve).
pub fn key(url: &str) -> &str {
    url.split_once('#').map_or(url, |(key, _)| key)
}

impl BlobUrlStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<BlobUrlEntry>>> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// #add-an-entry: `url` is a freshly generated blob URL (#unicodeBlobURL).
    pub fn add(&self, url: String, entry: BlobUrlEntry) {
        self.lock().insert(url, Arc::new(entry));
    }

    /// #blob-url-resolve: the entry of `url`, ignoring its fragment.
    pub fn resolve(&self, url: &str) -> Option<Arc<BlobUrlEntry>> {
        self.lock().get(key(url)).cloned()
    }

    /// #blob-url-resolve followed by #blob-url-obtain-object for an environment with
    /// `storage_key` (#check-for-same-partition-blob-url-usage), or for a top-level
    /// navigation (`None`), which is not partitioned.
    pub fn obtain(&self, url: &str, storage_key: Option<&str>) -> Option<Arc<BlobUrlEntry>> {
        self.resolve(url)
            .filter(|entry| storage_key.is_none_or(|key| entry.storage_key == key))
    }

    /// #dfn-revokeObjectURL steps 3-7 for an environment with `storage_key`. The entry is
    /// URL's blob URL entry (resolved without the fragment); it is removed by `url`'s whole
    /// serialization (#removeTheEntry), as the store's keys never have a fragment.
    pub fn revoke(&self, url: &str, storage_key: &str) {
        let mut entries = self.lock();
        let authorized = entries
            .get(key(url))
            .is_some_and(|entry| entry.storage_key == storage_key);
        if authorized {
            entries.remove(url);
        }
    }

    /// #lifeTime: the entries an environment created go away with it. `realm` None removes
    /// every entry of the agent (a worker that ended).
    pub fn remove_environment(&self, agent: u64, realm: Option<u64>) {
        self.lock().retain(|_, entry| {
            entry.environment.agent != agent
                || realm.is_some_and(|realm| entry.environment.realm != realm)
        });
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Approximate heap bytes the entries hold.
    pub fn retained_bytes(&self) -> usize {
        let entries = self.lock();
        entries
            .iter()
            .map(|(url, entry)| url.len() + entry.retained_bytes())
            .sum()
    }

    /// Add an entry of `bytes` from an unnamed environment of `url`'s origin, for tests.
    #[cfg(test)]
    pub(crate) fn add_bytes(&self, url: &str, bytes: Vec<u8>, content_type: &str) {
        let origin = url::Url::parse(url)
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_default();
        self.add(
            url.to_string(),
            BlobUrlEntry {
                parts: vec![BlobPart::Bytes(Arc::from(bytes))],
                content_type: content_type.to_string(),
                origin: origin.clone(),
                storage_key: origin,
                environment: Environment { agent: 0, realm: 0 },
                referrer_policy: Default::default(),
            },
        );
    }

    /// The URLs and entries, for diagnostics.
    pub fn snapshot(&self) -> Vec<(String, Arc<BlobUrlEntry>)> {
        self.lock()
            .iter()
            .map(|(url, entry)| (url.clone(), entry.clone()))
            .collect()
    }
}

/// Removes an agent's entries when it ends (a worker; File API #lifeTime's worker hook).
#[derive(Debug)]
pub struct EnvironmentRelease {
    pub store: Arc<BlobUrlStore>,
    pub agent: u64,
}

impl Drop for EnvironmentRelease {
    fn drop(&mut self) {
        self.store.remove_environment(self.agent, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(bytes: &[u8], key: &str, agent: u64, realm: u64) -> BlobUrlEntry {
        BlobUrlEntry {
            parts: vec![BlobPart::Bytes(Arc::from(bytes))],
            content_type: String::from("text/plain"),
            origin: String::from(key),
            storage_key: String::from(key),
            environment: Environment { agent, realm },
            referrer_policy: Default::default(),
        }
    }

    #[test]
    fn entries_resolve_without_fragments_and_respect_partitions() {
        let store = BlobUrlStore::default();
        let url = String::from("blob:https://a.test/0f6a2f8e-1c39-4c55-9a6e-6c51f3d1e7aa");
        store.add(url.clone(), entry(b"abc", "https://a.test", 1, 0));
        assert_eq!(
            store.resolve(&format!("{url}#x")).unwrap().read().unwrap(),
            b"abc"
        );
        assert!(store.obtain(&url, Some("https://a.test")).is_some());
        assert!(store.obtain(&url, Some("https://b.test")).is_none());
        assert!(
            store.obtain(&url, None).is_some(),
            "navigations are not partitioned"
        );
        store.revoke(&url, "https://b.test");
        assert!(
            store.resolve(&url).is_some(),
            "another partition cannot revoke"
        );
        store.revoke(&format!("{url}#x"), "https://a.test");
        assert!(
            store.resolve(&url).is_some(),
            "#removeTheEntry serializes the fragment"
        );
        store.revoke(&url, "https://a.test");
        assert!(store.resolve(&url).is_none());
    }

    #[test]
    fn environments_take_their_entries_along() {
        let store = Arc::new(BlobUrlStore::default());
        store.add(String::from("blob:x/1"), entry(b"1", "x", 1, 0));
        store.add(String::from("blob:x/2"), entry(b"2", "x", 1, 7));
        store.add(String::from("blob:x/3"), entry(b"3", "x", 2, 0));
        store.remove_environment(1, Some(7));
        assert!(store.resolve("blob:x/2").is_none() && store.resolve("blob:x/1").is_some());
        drop(EnvironmentRelease {
            store: store.clone(),
            agent: 2,
        });
        assert!(store.resolve("blob:x/3").is_none() && store.len() == 1);
    }
}
