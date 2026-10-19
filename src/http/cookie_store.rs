//! Cookie Store #query-cookies-algorithm, #set-cookie-algorithm and
//! #process-changes (WHATWG 54559c0c, local snapshot 2026-09-06).
//! Structured attributes enter the same jar as HTTP/document.cookie without
//! being serialized into a header, so a semicolon in Path is not an attribute
//! injection. Only the existing bookmark-gated Cookie::persist writes to disk.
//! CHIPS keys follow privacycg/CHIPS fab89df5; partitioned cookies stay in RAM.

use super::*;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak, mpsc};

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct Item {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct Changes {
    pub changed: Vec<Item>,
    pub deleted: Vec<Item>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Init {
    pub name: String,
    pub value: String,
    pub expires: Option<f64>,
    pub domain: Option<String>,
    pub path: String,
    pub same_site: String,
    pub partitioned: bool,
    pub max_age: Option<f64>,
}

fn partition(context: &CookieContext) -> Option<(String, bool)> {
    context
        .top_level_site
        .clone()
        .map(|site| (site, context.cross_site_ancestor))
}

fn visible(cookie: &Cookie, url: &Url, context: &CookieContext) -> bool {
    cookies_enabled()
        && context.same_site(url)
        && matches!(url.scheme(), "http" | "https")
        && !cookie.http_only
        && (!cookie.secure || cookie_connection_is_secure(url))
        && cookie_domain_match(url.host_str().unwrap_or_default(), cookie)
        && (cookie.host_only || !public_suffix(&cookie.domain))
        && cookie_path_matches(url.path(), &cookie.path)
        && (cookie.partition.is_none() || cookie.partition == partition(context))
}

fn item(cookie: &Cookie, deleted: bool) -> Item {
    Item {
        name: cookie.name.clone(),
        value: (!deleted).then(|| cookie.value.clone()),
    }
}

pub(crate) fn query(url: &Url, context: &CookieContext, name: Option<&str>) -> Vec<Item> {
    let mut jar = COOKIE_JAR.lock().unwrap();
    let now = cookie_now_ms();
    evict_expired_cookies(&mut jar, now);
    let name = name.map(|name| name.trim_matches([' ', '\t']));
    let mut matching: Vec<_> = jar
        .iter_mut()
        .filter(|cookie| {
            visible(cookie, url, context) && name.is_none_or(|name| cookie.name == name)
        })
        .collect();
    matching.sort_by_key(|cookie| (std::cmp::Reverse(cookie.path.len()), cookie.created_at));
    matching
        .into_iter()
        .map(|cookie| {
            cookie.last_access = now;
            item(cookie, false)
        })
        .collect()
}

pub(crate) fn set(url: &Url, context: &CookieContext, init: Init) -> Result<(), &'static str> {
    let now = cookie_now_ms();
    let cookie = prepare(url, context, init, now)?;
    // Cookie Store #set-cookie-algorithm returns success even when the
    // cookie storage model rejects the write (e.g. an HttpOnly collision or
    // the user's cookie policy). Never broaden the third-party exception.
    if context.same_site(url)
        && let Some(cookie) = cookie
    {
        let root = cookie.path == "/";
        insert_cookie_at(
            &mut COOKIE_JAR.lock().unwrap(),
            url,
            cookie,
            true,
            root,
            now,
        );
    }
    Ok(())
}

fn prepare(
    url: &Url,
    context: &CookieContext,
    init: Init,
    now: i64,
) -> Result<Option<Cookie>, &'static str> {
    let name = init.name.trim_matches([' ', '\t']).to_string();
    let value = init.value.trim_matches([' ', '\t']).to_string();
    let forbidden = |s: &str| {
        s.bytes()
            .any(|b| b == b';' || b < 0x20 && b != b'\t' || b == 0x7f)
    };
    let lower = name.to_ascii_lowercase();
    let lower_value = value.to_ascii_lowercase();
    if forbidden(&name)
        || forbidden(&value)
        || name.contains('=')
        || name.is_empty()
            && (value.is_empty()
                || value.contains('=')
                || ["__host-", "__http-", "__secure-"]
                    .iter()
                    .any(|p| lower_value.starts_with(p)))
        || lower.starts_with("__http-")
        || lower.starts_with("__host-http-")
        || name.len() + value.len() > 4096
    {
        return Err("Invalid cookie name or value");
    }
    let host = url.host_str().ok_or("Cookie URL has no host")?;
    let (domain, host_only) = match init.domain {
        None => (host.to_string(), true),
        Some(domain) => {
            if domain.starts_with('.') || lower.starts_with("__host-") {
                return Err("Invalid cookie domain");
            }
            let parsed = url::Host::parse(&domain)
                .map_err(|_| "Invalid cookie domain")?
                .to_string();
            if parsed.len() > 1024
                || !domain_matches(host, &parsed)
                || parsed != host && public_suffix(&parsed)
            {
                return Err("Cookie domain is not a registrable suffix of the host");
            }
            let host_only = public_suffix(&parsed);
            (parsed, host_only)
        }
    };
    let path = if init.path.is_empty() {
        default_cookie_path(url)
    } else {
        init.path
    };
    if !path.starts_with('/') || path.len() > 1024 || lower.starts_with("__host-") && path != "/" {
        return Err("Invalid cookie path");
    }
    if init.expires.is_some() && init.max_age.is_some() {
        return Err("expires and maxAge cannot both be specified");
    }
    let limit = now.saturating_add(COOKIE_MAX_AGE_SECS * 1000);
    let expires_at = if let Some(expires) = init.expires {
        if !expires.is_finite() {
            return Err("Invalid cookie expiry");
        }
        Some((((expires / 1000.).round() * 1000.) as i64).min(limit))
    } else {
        init.max_age.map(|age| {
            if age <= 0. {
                i64::MIN
            } else {
                now.saturating_add((age.min(COOKIE_MAX_AGE_SECS as f64) * 1000.) as i64)
            }
        })
    };
    let same_site = match init.same_site.as_str() {
        "strict" => CookieSameSite::Strict,
        "lax" => CookieSameSite::Lax,
        "none" => CookieSameSite::None,
        _ => return Err("Invalid SameSite value"),
    };
    let partition = if init.partitioned {
        let Some(partition) = partition(context) else {
            return Ok(None);
        };
        Some(partition)
    } else {
        None
    };
    Ok(Some(Cookie {
        name,
        value,
        domain,
        host_only,
        path,
        secure: true,
        http_only: false,
        expires_at,
        same_site,
        created_at: now,
        last_access: now,
        partition,
    }))
}

type Callback = Arc<dyn Fn(Changes) + Send + Sync>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Filter {
    pub name: Option<String>,
    pub url: Url,
}
struct Observer {
    url: Url,
    context: CookieContext,
    callback: Callback,
    filters: Option<Vec<Filter>>,
}
impl Observer {
    fn observes(&self, cookie: &Cookie) -> bool {
        visible(cookie, &self.url, &self.context)
            && self.filters.as_ref().is_none_or(|filters| {
                filters.iter().any(|filter| {
                    filter.name.as_ref().is_none_or(|name| name == &cookie.name)
                        && visible(cookie, &filter.url, &self.context)
                })
            })
    }
}
static OBSERVERS: LazyLock<Mutex<Vec<Weak<Observer>>>> = LazyLock::new(Mutex::default);
static EXPIRY_WAKE: OnceLock<mpsc::SyncSender<()>> = OnceLock::new();

/// The callback must only queue work: jar mutations deliver it under the jar
/// lock. Owning this handle keeps the subscription alive, without keeping a
/// Window or service-worker registration alive through the global list.
pub(crate) struct Subscription {
    _observer: Arc<Observer>,
}

pub(crate) fn subscribe(url: Url, context: CookieContext, callback: Callback) -> Subscription {
    subscribe_filtered(url, context, None, callback)
}

/// Cookie Store #process-changes intersects observable changes at the
/// registration scope with its subscription URLs and names. One observer
/// performs that union so overlapping subscriptions deliver each change once.
pub(crate) fn subscribe_filtered(
    url: Url,
    context: CookieContext,
    filters: Option<Vec<Filter>>,
    callback: Callback,
) -> Subscription {
    let observer = Arc::new(Observer {
        url,
        context,
        callback,
        filters,
    });
    OBSERVERS.lock().unwrap().push(Arc::downgrade(&observer));
    let wake = EXPIRY_WAKE.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("trust-cookie-expiry".into())
            .stack_size(512 * 1024)
            .spawn(move || expiry_loop(rx))
            .expect("cookie expiry thread");
        tx
    });
    let _ = wake.try_send(());
    Subscription {
        _observer: observer,
    }
}

fn observers() -> Vec<Arc<Observer>> {
    let mut entries = OBSERVERS.lock().unwrap();
    let mut result = Vec::new();
    entries.retain(|entry| {
        if let Some(entry) = entry.upgrade() {
            result.push(entry);
            true
        } else {
            false
        }
    });
    result
}

pub(super) fn publish(cookie: &Cookie, deleted: bool) {
    for observer in observers() {
        if !observer.observes(cookie) {
            continue;
        }
        let mut changes = Changes::default();
        if deleted {
            changes.deleted.push(item(cookie, true));
        } else {
            changes.changed.push(item(cookie, false));
        }
        (observer.callback)(changes);
    }
    if let Some(wake) = EXPIRY_WAKE.get() {
        let _ = wake.try_send(());
    }
}

fn expiry_loop(rx: mpsc::Receiver<()>) {
    loop {
        // Lock order: release the observer registry before taking the jar.
        let observers = observers();
        let deadline = if observers.is_empty() {
            None
        } else {
            let mut jar = COOKIE_JAR.lock().unwrap();
            evict_expired_cookies(&mut jar, cookie_now_ms());
            jar.iter()
                .filter(|cookie| observers.iter().any(|o| o.observes(cookie)))
                .filter_map(|cookie| cookie.expires_at)
                .min()
        };
        drop(observers);
        match deadline {
            Some(deadline) => match rx.recv_timeout(Duration::from_millis(
                deadline.saturating_sub(cookie_now_ms()).max(1) as u64,
            )) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            },
            None => {
                if rx.recv().is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(name: &str, value: &str) -> Init {
        Init {
            name: name.into(),
            value: value.into(),
            expires: None,
            domain: None,
            path: "/".into(),
            same_site: "strict".into(),
            partitioned: false,
            max_age: None,
        }
    }

    #[test]
    fn structured_attributes_and_prefixes_cannot_inject_cookie_attributes() {
        let url = Url::parse("https://cookie-store-native.test/dir/page").unwrap();
        let context = CookieContext::subresource(&url);
        for (name, value) in [
            ("a=b", "c"),
            ("a", "b;c"),
            ("", ""),
            ("", "a=b"),
            ("__HTTP-secret", "1"),
            ("__Host-Http-secret", "1"),
            ("a", "b\n"),
            ("", "__secure-x"),
        ] {
            assert!(prepare(&url, &context, options(name, value), 0).is_err());
        }
        let mut init = options("name", "value");
        init.path = "/; HttpOnly; Domain=evil.test".into();
        let cookie = prepare(&url, &context, init, 0).unwrap().unwrap();
        assert_eq!(cookie.path, "/; HttpOnly; Domain=evil.test");
        assert_eq!(cookie.domain, "cookie-store-native.test");
        assert!(!cookie.http_only);
        let mut init = options("__Host-id", "v");
        init.domain = Some("cookie-store-native.test".into());
        assert!(prepare(&url, &context, init, 0).is_err());
        let mut init = options("id", "v");
        init.domain = Some("test".into());
        assert!(prepare(&url, &context, init, 0).is_err());
        let mut init = options("id", "v");
        init.expires = Some(2000.);
        init.max_age = Some(1.);
        assert!(prepare(&url, &context, init, 0).is_err());
        let cookie = prepare(&url, &context, options(" \tid ", " value\t "), 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            (cookie.name.as_str(), cookie.value.as_str()),
            ("id", "value")
        );
    }

    #[test]
    fn cookie_store_shares_http_jar_and_observes_only_script_visible_changes() {
        let _lock = COOKIE_TEST_LOCK.lock().unwrap();
        let url = Url::parse("https://cookie-store-events.test/a/page").unwrap();
        let context = CookieContext::subresource(&url);
        let changes = Arc::new(Mutex::new(Vec::new()));
        let output = changes.clone();
        let _subscription = subscribe(
            url.clone(),
            context.clone(),
            Arc::new(move |c| output.lock().unwrap().push(c)),
        );
        store_cookie(&url, "secret=v; Secure; HttpOnly; Path=/", false);
        set(&url, &context, options("secret", "changed")).unwrap();
        assert!(query(&url, &context, Some("secret")).is_empty());
        assert!(changes.lock().unwrap().is_empty());
        set(&url, &context, options("visible", "one")).unwrap();
        store_cookie(&url, "visible=two; Secure; Path=/", false);
        assert_eq!(
            query(&url, &context, Some("visible"))[0].value.as_deref(),
            Some("two")
        );
        let mut init = options("visible", "");
        init.max_age = Some(0.);
        set(&url, &context, init).unwrap();
        let changes = changes.lock().unwrap();
        assert_eq!(changes.len(), 3);
        assert!(
            changes[1].deleted.is_empty(),
            "replacement has no deletion event"
        );
        assert_eq!(changes[2].deleted[0].name, "visible");
        assert!(changes[2].deleted[0].value.is_none());
    }

    #[test]
    fn cookie_store_partition_identity_and_policy_remain_separate() {
        let _lock = COOKIE_TEST_LOCK.lock().unwrap();
        let url = Url::parse("https://cookie-store-partition.test/").unwrap();
        let context = CookieContext::subresource(&url);
        set(&url, &context, options("key", "ordinary")).unwrap();
        let mut init = options("key", "partitioned");
        init.partitioned = true;
        set(&url, &context, init).unwrap();
        assert_eq!(query(&url, &context, Some("key")).len(), 2);
        let mut init = options("key", "");
        init.partitioned = true;
        init.max_age = Some(0.);
        set(&url, &context, init).unwrap();
        assert_eq!(
            query(&url, &context, Some("key"))[0].value.as_deref(),
            Some("ordinary")
        );
        let mut third_party = context.clone();
        third_party.cross_site_ancestor = true;
        third_party.top_level_site = Some("https://another.test".into());
        assert!(query(&url, &third_party, None).is_empty());
        set(&url, &third_party, options("blocked", "value")).unwrap();
        assert!(query(&url, &context, Some("blocked")).is_empty());
    }

    #[test]
    fn cookie_store_expiry_delivers_a_deletion_without_a_read_or_poll() {
        let _lock = COOKIE_TEST_LOCK.lock().unwrap();
        let url = Url::parse("https://cookie-store-expiry.test/").unwrap();
        let context = CookieContext::subresource(&url);
        let (tx, rx) = mpsc::channel();
        let _subscription = subscribe(
            url.clone(),
            context.clone(),
            Arc::new(move |changes| {
                let _ = tx.send(changes);
            }),
        );
        let mut init = options("short", "value");
        init.max_age = Some(1.);
        set(&url, &context, init).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap().changed[0].name,
            "short"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap().deleted[0].name,
            "short"
        );
    }

    #[test]
    fn cookie_store_reads_http_partitioned_cookies_only_in_their_partition() {
        let _lock = COOKIE_TEST_LOCK.lock().unwrap();
        let url = Url::parse("https://cookie-store-http-partition.test/").unwrap();
        let context = CookieContext::subresource(&url);
        store_cookie(&url, "shared=ordinary; Secure; Path=/", false);
        store_cookie(
            &url,
            "shared=partitioned; Partitioned; Secure; Path=/",
            false,
        );
        assert_eq!(query(&url, &context, Some("shared")).len(), 2);
        let mut request = Request::get(url.clone());
        let mut other = context.clone();
        other.top_level_site = Some("https://different.test".into());
        request.cookie_context = Some(other);
        assert_eq!(request_cookies(&request), "shared=ordinary");
        request.cookie_context = Some(context);
        let header = request_cookies(&request);
        assert!(header.contains("shared=ordinary") && header.contains("shared=partitioned"));
    }

    #[test]
    fn cookie_store_secure_cookies_work_on_trustworthy_loopback_connections() {
        let _lock = COOKIE_TEST_LOCK.lock().unwrap();
        for url in ["http://127.0.0.1/", "http://[::1]/", "http://localhost/"] {
            let url = Url::parse(url).unwrap();
            let context = CookieContext::subresource(&url);
            set(&url, &context, options("cookieStoreLoopback", "value")).unwrap();
            assert_eq!(
                query(&url, &context, Some("cookieStoreLoopback"))[0]
                    .value
                    .as_deref(),
                Some("value")
            );
            assert!(cookies_for_request(&url).contains("cookieStoreLoopback=value"));
        }
        let url = Url::parse("http://cookie-store-untrusted.test/").unwrap();
        let context = CookieContext::subresource(&url);
        set(&url, &context, options("blockedSecure", "value")).unwrap();
        assert!(query(&url, &context, Some("blockedSecure")).is_empty());
    }
}
