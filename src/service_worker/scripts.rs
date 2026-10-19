//! Service Workers #update-algorithm / #importscripts-method and HTML
//! #fetch-a-classic-worker-script / #fetch-a-classic-worker-imported-script.
//! Each installed worker retains its own immutable script bytes in RAM.
//! Restarting one never silently loads a different version of an import.
use super::{Error, RegistrationOptions, WorkerState};
use crate::http::{
    CookieContext, CredentialsMode, FetchPolicy, RedirectMode, Request, RequestMode,
};
use crate::referrer_policy::ReferrerPolicy;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use url::Url;

const MAX_SCRIPT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCRIPTS: usize = 512;
const PROFILE_BYTES: usize = 128 * 1024 * 1024;

/// All versions/registrations share the profile's bound. Bytes leave the
/// account only when the last script resource reference is dropped.
#[derive(Default)]
pub(crate) struct Budget(AtomicUsize);
struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
impl Budget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Reservation, Error> {
        self.0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= PROFILE_BYTES)
            })
            .map_err(|_| {
                Error::new(
                    "QuotaExceededError",
                    "Service-worker script memory quota exceeded",
                )
            })?;
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

pub(crate) struct Script {
    pub url: Url,
    pub bytes: Arc<[u8]>,
    pub source: Arc<str>,
    headers: Vec<(String, String)>,
    pub muted_errors: bool,
    _reservation: Reservation,
}
impl Script {
    pub(crate) fn referrer_policy(&self) -> Option<ReferrerPolicy> {
        ReferrerPolicy::from_headers(&self.headers)
    }
    pub(crate) fn new(
        url: Url,
        bytes: Vec<u8>,
        headers: Vec<(String, String)>,
        budget: &Arc<Budget>,
    ) -> Result<Arc<Self>, Error> {
        if bytes.len() > MAX_SCRIPT_BYTES {
            return Err(Error::new(
                "QuotaExceededError",
                "Service-worker script exceeds the memory limit",
            ));
        }
        // HTML requires UTF-8 decode regardless of the Content-Type charset.
        // Encoding's UTF-8 decode strips one leading BOM, never all of them.
        let bytes_without_bom = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        let source = String::from_utf8_lossy(bytes_without_bom);
        let size = bytes.len()
            + source.len()
            + url.as_str().len()
            + headers
                .iter()
                .map(|(name, value)| name.len() + value.len() + 64)
                .sum::<usize>();
        let reservation = budget.reserve(size)?;
        Ok(Arc::new(Self {
            url,
            source: Arc::from(source.as_ref()),
            bytes: Arc::from(bytes),
            headers,
            _reservation: reservation,
            muted_errors: false,
        }))
    }
}

#[derive(Clone)]
pub(crate) struct Environment {
    pub client: Url,
    /// The settings origin, independently of an inherited local creation URL.
    pub origin: Url,
    pub cookies: CookieContext,
    pub referrer_policy: ReferrerPolicy,
}

/// A version survives its JavaScript agent and is reused verbatim on restart.
pub(crate) struct Version {
    pub root: Arc<Script>,
    pub resources: Arc<Resources>,
    pub environment: Environment,
    pub modules: std::collections::BTreeMap<(String, Option<String>), Arc<Script>>,
}

/// No HTTP cache is used by this loader: updates check network bytes, and
/// running/restarted workers use their own resource map below. Both policies
/// are RAM-only; updateViaCache never authorizes persistent storage.
pub(crate) async fn fetch(
    url: Url,
    environment: &Environment,
    top: Option<&RegistrationOptions>,
    budget: &Arc<Budget>,
) -> Result<Arc<Script>, Error> {
    // A stalled import must not indefinitely hold the registration's job queue.
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        fetch_inner(url, environment, top, budget, false, None),
    )
    .await
    .map_err(|_| Error::new("NetworkError", "Service-worker script fetch timed out"))?
}

pub(crate) async fn fetch_module(
    url: Url,
    environment: &Environment,
    budget: &Arc<Budget>,
    kind: Option<&str>,
) -> Result<Arc<Script>, Error> {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        fetch_inner(url, environment, None, budget, true, kind),
    )
    .await
    .map_err(|_| Error::new("NetworkError", "Module fetch timed out"))?
}

async fn fetch_inner(
    url: Url,
    environment: &Environment,
    top: Option<&RegistrationOptions>,
    budget: &Arc<Budget>,
    module: bool,
    module_type: Option<&str>,
) -> Result<Arc<Script>, Error> {
    if !matches!(url.scheme(), "http" | "https")
        || !crate::http::subresource_allowed(&environment.origin, &url)
    {
        return Err(Error::new(
            "NetworkError",
            "Service-worker script URL cannot be fetched",
        ));
    }
    let mut request = Request::subresource(
        url,
        &environment.origin,
        if top.is_some() {
            "serviceworker"
        } else {
            "script"
        },
        None,
    );
    request.cookie_context = Some(environment.cookies.clone());
    crate::http::set_request_referrer(
        &mut request,
        Some(&environment.client),
        environment.referrer_policy,
    );
    if top.is_some() {
        request.redirect = RedirectMode::Error;
        request
            .headers
            .push(("Service-Worker".into(), "script".into()));
        request.fetch_policy = Some(FetchPolicy {
            origin: environment.origin.clone(),
            mode: RequestMode::SameOrigin,
            credentials: CredentialsMode::SameOrigin,
        });
        crate::http::set_fetch_metadata(
            &mut request,
            &environment.origin,
            "serviceworker",
            "same-origin",
        );
    } else {
        request.fetch_policy = Some(FetchPolicy {
            origin: environment.origin.clone(),
            mode: if module {
                RequestMode::Cors
            } else {
                RequestMode::NoCors
            },
            credentials: CredentialsMode::SameOrigin,
        });
        if module {
            request
                .headers
                .push(("Service-Worker".into(), "script".into()));
            crate::http::set_fetch_metadata(
                &mut request,
                &environment.origin,
                "serviceworker",
                "cors",
            );
        }
    }
    let (mut response, mut stream) = crate::http::fetch_script_streaming(&request)
        .await
        .map_err(|error| Error::new("NetworkError", error.message))?;
    let muted_errors = response
        .internal
        .as_ref()
        .is_some_and(|internal| internal.kind == "opaque");
    response.unfilter();
    if if module {
        !crate::http::module_response_allowed(response.status, &response.content_type, module_type)
    } else {
        !crate::http::is_javascript_mime_type(&response.content_type)
    } {
        return Err(Error::new(
            if top.is_some() {
                "SecurityError"
            } else {
                "NetworkError"
            },
            "Service-worker script does not have a JavaScript MIME type",
        ));
    }
    if !(200..300).contains(&response.status) || matches!(response.status, 204 | 205) {
        return Err(Error::new(
            "NetworkError",
            "Service-worker script did not return a successful response",
        ));
    }
    if let Some(options) = top {
        let allowed = crate::http::header_list_get(&response.headers, "service-worker-allowed");
        options.check_max_scope(allowed.as_deref())?;
    }
    if let Some(stream) = stream.as_mut() {
        while let Some(piece) = stream
            .next()
            .await
            .map_err(|message| Error::new("NetworkError", message))?
        {
            if response.body.len().saturating_add(piece.len()) > MAX_SCRIPT_BYTES {
                return Err(Error::new(
                    "QuotaExceededError",
                    "Service-worker script exceeds the memory limit",
                ));
            }
            response.body.extend_from_slice(&piece);
        }
    }
    let mut script = Script::new(response.url, response.body, response.headers, budget)?;
    Arc::get_mut(&mut script)
        .expect("new script is not shared yet")
        .muted_errors = muted_errors;
    Ok(script)
}

type Resource = Result<Arc<Script>, Error>;
struct Map {
    // Insertion order matters when the update algorithm refetches imports.
    entries: Vec<(Url, Resource)>,
    used: HashSet<Url>,
    classic_imported: bool,
}

pub(crate) struct Resources(Mutex<Map>);
impl Resources {
    pub(crate) fn new(root: Url, script: Arc<Script>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(Map {
            entries: vec![(root.clone(), Ok(script))],
            used: HashSet::from([root]),
            classic_imported: false,
        })))
    }
    pub(crate) fn snapshot(&self) -> Vec<(Url, Resource)> {
        self.0.lock().unwrap().entries.clone()
    }
    pub(crate) fn classic_imported(&self) -> bool {
        self.0.lock().unwrap().classic_imported
    }
    /// The updater may seed a failed import response as well as good bytes.
    /// A failure is ignored by byte comparison, but remains a failure if
    /// the new script actually imports that URL during installation.
    pub(crate) fn seed(&self, url: Url, response: Resource) -> Result<(), Error> {
        let mut map = self.0.lock().unwrap();
        if let Some((_, old)) = map.entries.iter_mut().find(|(key, _)| key == &url) {
            *old = response;
        } else {
            if map.entries.len() >= MAX_SCRIPTS {
                return Err(Error::new(
                    "QuotaExceededError",
                    "Too many service-worker scripts",
                ));
            }
            map.entries.push((url, response));
        }
        Ok(())
    }
    /// Return a stored response, or permission to fetch a new import while
    /// parsed/installing. Installed workers may only use their saved map.
    pub(crate) fn import(&self, url: &Url, state: WorkerState) -> Option<Resource> {
        let mut map = self.0.lock().unwrap();
        let installing = matches!(state, WorkerState::Parsed | WorkerState::Installing);
        if let Some(response) = map
            .entries
            .iter()
            .find(|(key, _)| key == url)
            .map(|(_, response)| response.clone())
        {
            if installing {
                map.used.insert(url.clone());
            }
            return Some(response);
        }
        (!installing).then(|| {
            Err(Error::new(
                "NetworkError",
                "An installed service worker cannot import a new script URL",
            ))
        })
    }
    pub(crate) fn imported(&self, url: Url, script: Arc<Script>) -> Result<(), Error> {
        self.seed(url.clone(), Ok(script))?;
        let mut map = self.0.lock().unwrap();
        map.used.insert(url);
        map.classic_imported = true;
        Ok(())
    }
    pub(crate) fn finish_install(&self) {
        let mut map = self.0.lock().unwrap();
        let used = map.used.clone();
        map.entries.retain(|(url, _)| used.contains(url));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn script(budget: &Arc<Budget>, path: &str, source: &[u8]) -> Arc<Script> {
        Script::new(
            Url::parse(&format!("https://sw.test/{path}")).unwrap(),
            source.to_vec(),
            Vec::new(),
            budget,
        )
        .unwrap()
    }
    #[test]
    fn resource_maps_keep_versions_and_forbid_new_imports_after_installation() {
        let budget = Arc::new(Budget::default());
        let root = script(&budget, "sw.js", b"importScripts('a.js')");
        let resources = Resources::new(root.url.clone(), root);
        let a = script(&budget, "a.js", b"old");
        let a_url = a.url.clone();
        assert!(resources.import(&a.url, WorkerState::Parsed).is_none());
        resources.imported(a.url.clone(), a.clone()).unwrap();
        let unused = script(&budget, "unused.js", b"unused");
        resources
            .seed(unused.url.clone(), Ok(unused.clone()))
            .unwrap();
        assert!(resources.classic_imported());
        resources.finish_install();
        assert_eq!(resources.snapshot().len(), 2);
        assert_eq!(
            &*resources
                .import(&a_url, WorkerState::Activated)
                .unwrap()
                .unwrap()
                .bytes,
            b"old"
        );
        assert!(
            resources
                .import(&unused.url, WorkerState::Activated)
                .unwrap()
                .is_err()
        );
        let update = Resources::new(a_url.clone(), script(&budget, "a.js", b"new"));
        assert_eq!(
            &*update
                .import(&a_url, WorkerState::Parsed)
                .unwrap()
                .unwrap()
                .bytes,
            b"new"
        );
        assert_eq!(
            &*resources
                .import(&a_url, WorkerState::Activated)
                .unwrap()
                .unwrap()
                .bytes,
            b"old"
        );
        drop((update, resources, a, unused));
        assert_eq!(budget.0.load(Ordering::Acquire), 0);
    }
    #[test]
    fn source_decoding_strips_one_utf8_bom_and_memory_is_released() {
        let budget = Arc::new(Budget::default());
        let value = script(&budget, "utf8.js", b"\xef\xbb\xbf\xef\xbb\xbf\xff");
        assert_eq!(&*value.source, "\u{feff}\u{fffd}");
        assert!(budget.0.load(Ordering::Acquire) > 0);
        assert!(budget.reserve(PROFILE_BYTES).is_err());
        drop(value);
        assert_eq!(budget.0.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn top_script_checks_mime_scope_redirects_and_browser_owned_headers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let mut heads = Vec::new();
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                loop {
                    let mut byte = [0];
                    if socket.read(&mut byte).await.unwrap() == 0 {
                        break;
                    }
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
                let reply = if head.starts_with("get /redirect ") {
                    "HTTP/1.1 302 Found\r\nLocation: /must-not-fetch\r\nContent-Length: 100000\r\n\r\n"
                } else if head.starts_with("get /mime ") {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if head.starts_with("get /scripts/scope ") {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=iso-8859-1\r\nService-Worker-Allowed: /\r\nContent-Length: 2\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\né"
                };
                socket.write_all(reply.as_bytes()).await.unwrap();
                if head.starts_with("get /redirect ") {
                    // Keep its advertised body incomplete. Redirect mode
                    // Error must reject at the head without waiting for it.
                    tokio::spawn(async move {
                        let mut byte = [0];
                        let _ = socket.read(&mut byte).await;
                    });
                }
                heads.push(head);
            }
            heads
        });
        let environment = Environment {
            client: base.clone(),
            origin: base.clone(),
            cookies: CookieContext::subresource(&base),
            referrer_policy: ReferrerPolicy::default(),
        };
        let budget = Arc::new(Budget::default());
        for (path, expected) in [
            ("redirect", Some("NetworkError")),
            ("mime", Some("SecurityError")),
            ("scripts/scope", Some("SecurityError")),
            ("scripts/ok", None),
        ] {
            let options = RegistrationOptions::parse(
                &base,
                &base.origin(),
                path,
                Some("/"),
                super::super::WorkerType::Classic,
                super::super::UpdateViaCache::None,
            )
            .unwrap();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                fetch(
                    options.script.clone(),
                    &environment,
                    Some(&options),
                    &budget,
                ),
            )
            .await
            .expect("script fetch must finish without reading the redirect body");
            match (expected, result) {
                (Some(expected), Err(error)) => assert_eq!(error.name, expected),
                (None, Ok(script)) => {
                    assert_eq!(&*script.source, "é");
                    assert_eq!(script.referrer_policy(), Some(ReferrerPolicy::NoReferrer));
                }
                (expected, result) => panic!(
                    "unexpected script result for {path}: expected={expected:?}, error={:?}",
                    result.err()
                ),
            }
        }
        for head in server.await.unwrap() {
            assert!(head.contains("service-worker: script\r\n"), "{head}");
            assert!(head.contains("sec-fetch-dest: serviceworker\r\n"), "{head}");
            assert!(head.contains("sec-fetch-mode: same-origin\r\n"), "{head}");
        }
    }
}
