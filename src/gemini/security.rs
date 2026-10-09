//! Gemini security pages and the choices they offer (Gemini 0.24.1 §§Client
//! certificates, TLS server certificate validation):
//!
//! * a changed server certificate whose pin has not expired: the user sees
//!   both certificates and decides whether to trust the new one;
//! * status 60/61/62: the server's explanation, the identity that was sent,
//!   and a choice of creating an identity or using an existing one. Its
//!   scope defaults to the request's path and everything below it (§Status
//!   60); the user may direct it to a parent directory or the whole capsule,
//!   since many capsules ask for a certificate on one page (`/join`) and
//!   recognize it everywhere else;
//! * `about:certificates`: every identity with its scopes, and every pinned
//!   capsule certificate, each of which can be withdrawn.
//!
//! Actions are `about:` links. Any page can spell such an address, so each
//! action is honored only against the page that offered it: the current
//! document must be the matching security page, and an identity must be one
//! of the choices that page listed.

use super::{GeminiUrl, identity, tofu};
use crate::doc::{DocLine, Kind, Link};
use crate::tls::x509;

pub const MANAGER: &str = "about:certificates";
const TRUST_PREFIX: &str = "about:gemini-trust/";
const IDENTITY_PREFIX: &str = "about:gemini-identity/";
const MANAGER_PREFIX: &str = "about:certificates/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustWarning {
    pub presented: tofu::Presented,
    pub pinned: tofu::Pin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityChoice {
    pub name: String,
    pub fingerprint: [u8; 32],
    pub summary: String,
    /// Already holds a scope on this host and port.
    pub here: bool,
}

impl IdentityChoice {
    pub fn of(identity: &identity::Identity, url: &GeminiUrl) -> Self {
        Self {
            name: identity.name.clone(),
            fingerprint: identity.fingerprint,
            summary: identity.summary(),
            here: identity
                .scopes
                .iter()
                .any(|s| s.host == url.host && s.port == url.port),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateRequest {
    pub status: u8,
    pub meta: String,
    /// The identity TLS presented for this request.
    pub sent: Option<IdentityChoice>,
    /// An identity was scoped here, but the server never asked for one.
    pub unrequested: bool,
    /// The connection used TLS 1.2, which sends client certificates in the
    /// clear (Gemini §Minimum version requirements: clients SHOULD warn).
    pub tls12: bool,
    pub choices: Vec<IdentityChoice>,
    /// Identity files that could not be read.
    pub problems: Vec<String>,
    /// Where an identity chosen here would be sent, narrowest first.
    pub scopes: Vec<GeminiUrl>,
    /// The user's choice among `scopes`; 0 is the specification's default.
    pub scope: usize,
}

impl CertificateRequest {
    pub fn chosen_scope(&self) -> Option<&GeminiUrl> {
        self.scopes.get(self.scope)
    }
}

impl CertificateRequest {
    /// Gather the choices for a 6x answer to `url`.
    pub fn new(
        url: &GeminiUrl,
        status: u8,
        meta: &str,
        sent: Option<&identity::Identity>,
        unrequested: bool,
        tls12: bool,
    ) -> Self {
        let (identities, problems) = identity::list();
        let mut choices: Vec<IdentityChoice> = identities
            .iter()
            .map(|identity| IdentityChoice::of(identity, url))
            .collect();
        // Identities already used on this capsule first, then by name.
        choices.sort_by(|a, b| b.here.cmp(&a.here).then_with(|| a.name.cmp(&b.name)));
        choices.truncate(64);
        Self {
            status,
            meta: meta.into(),
            sent: sent.map(|identity| IdentityChoice::of(identity, url)),
            unrequested,
            tls12,
            choices,
            problems,
            scopes: identity::scope_options(url),
            scope: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Security {
    Changed(Box<TrustWarning>),
    Certificate(Box<CertificateRequest>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    TrustChanged,
    /// Choose the scope for this page's identity (an index into its options).
    Scope(usize),
    NewIdentity,
    UseIdentity([u8; 32]),
    ForgetScope([u8; 32], GeminiUrl),
    /// Send this identity to the whole capsule instead of one scope.
    WidenScope([u8; 32], GeminiUrl),
    Delete([u8; 32]),
    ConfirmDelete([u8; 32]),
    ForgetPin(String),
}

fn encode(value: &str) -> String {
    super::encode_query(value)
}

fn decode(value: &str) -> Option<String> {
    crate::dict::decode(value).ok()
}

impl Action {
    pub fn link(&self) -> Link {
        let hex = tofu::fingerprint_hex;
        Link::External(match self {
            Self::TrustChanged => format!("{TRUST_PREFIX}accept"),
            Self::Scope(n) => format!("{IDENTITY_PREFIX}scope/{n}"),
            Self::NewIdentity => format!("{IDENTITY_PREFIX}new"),
            Self::UseIdentity(fp) => format!("{IDENTITY_PREFIX}use/{}", hex(fp)),
            Self::ForgetScope(fp, scope) => format!(
                "{MANAGER_PREFIX}forget-scope/{}/{}",
                hex(fp),
                encode(&scope.to_string())
            ),
            Self::WidenScope(fp, scope) => format!(
                "{MANAGER_PREFIX}widen-scope/{}/{}",
                hex(fp),
                encode(&scope.to_string())
            ),
            Self::Delete(fp) => format!("{MANAGER_PREFIX}delete/{}", hex(fp)),
            Self::ConfirmDelete(fp) => format!("{MANAGER_PREFIX}confirm-delete/{}", hex(fp)),
            Self::ForgetPin(key) => format!("{MANAGER_PREFIX}forget-pin/{}", encode(key)),
        })
    }

    pub fn from_link(link: &Link) -> Option<Self> {
        let Link::External(value) = link else {
            return None;
        };
        if value == &format!("{TRUST_PREFIX}accept") {
            return Some(Self::TrustChanged);
        }
        if let Some(rest) = value.strip_prefix(IDENTITY_PREFIX) {
            return match rest.split_once('/') {
                None if rest == "new" => Some(Self::NewIdentity),
                Some(("use", fp)) => tofu::parse_fingerprint(fp).map(Self::UseIdentity),
                Some(("scope", n))
                    if !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    n.parse().ok().map(Self::Scope)
                }
                _ => None,
            };
        }
        let rest = value.strip_prefix(MANAGER_PREFIX)?;
        let (name, rest) = rest.split_once('/')?;
        match name {
            "forget-scope" | "widen-scope" => {
                let (fp, scope) = rest.split_once('/')?;
                let scope = GeminiUrl::parse(&decode(scope)?)?;
                let fp = tofu::parse_fingerprint(fp)?;
                Some(if name == "forget-scope" {
                    Self::ForgetScope(fp, scope)
                } else {
                    Self::WidenScope(fp, scope)
                })
            }
            "delete" => tofu::parse_fingerprint(rest).map(Self::Delete),
            "confirm-delete" => tofu::parse_fingerprint(rest).map(Self::ConfirmDelete),
            "forget-pin" => decode(rest)
                .filter(|key| !key.is_empty() && !key.contains(char::is_control))
                .map(Self::ForgetPin),
            _ => None,
        }
    }
}

/// What the frontend must do after an action.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Request this page again (the decision changes what TLS presents).
    Navigate(Link),
    /// The user chose the scope option at this index; the frontend records
    /// it with [`choose_scope`] on the current page and redraws it.
    Scope(usize),
    /// Ask for a new identity's name.
    Prompt(crate::prompt::Prompt),
    /// Show the manager (again), at this address.
    Manager(String),
}

/// The page an action was activated on.
pub struct Context<'a> {
    /// The current document's address.
    pub address: &'a Link,
    /// The security state the current Gemini page carries, if any.
    pub security: Option<&'a Security>,
}

pub fn apply(action: &Action, context: &Context<'_>) -> Result<Outcome, String> {
    let gemini = match context.address {
        Link::Gemini(url) => Some(url),
        _ => None,
    };
    let manager = |address: &str| matches!(context.address, Link::External(a) if a == address);
    match action {
        Action::TrustChanged => match (gemini, context.security) {
            (Some(url), Some(Security::Changed(warning))) => {
                tofu::trust(&warning.presented, x509::now())?;
                Ok(Outcome::Navigate(Link::Gemini(url.clone())))
            }
            _ => Err("No changed capsule certificate is awaiting a decision here.".into()),
        },
        Action::Scope(n) => match context.security {
            Some(Security::Certificate(request)) if *n < request.scopes.len() => {
                Ok(Outcome::Scope(*n))
            }
            _ => Err("This page did not offer that scope.".into()),
        },
        Action::NewIdentity => match (gemini, context.security) {
            (Some(url), Some(Security::Certificate(request))) => {
                let scope = request
                    .chosen_scope()
                    .cloned()
                    .unwrap_or_else(|| identity::scope_of(url));
                Ok(Outcome::Prompt(crate::prompt::Prompt::identity_name(
                    url, &scope,
                )))
            }
            _ => Err("This page did not ask for a client certificate.".into()),
        },
        Action::UseIdentity(fingerprint) => match (gemini, context.security) {
            (Some(url), Some(Security::Certificate(request)))
                if request
                    .choices
                    .iter()
                    .any(|c| &c.fingerprint == fingerprint) =>
            {
                let chosen = identity::find(fingerprint)?;
                let scope = request
                    .chosen_scope()
                    .cloned()
                    .unwrap_or_else(|| identity::scope_of(url));
                identity::authorize(&chosen, &scope)?;
                Ok(Outcome::Navigate(Link::Gemini(url.clone())))
            }
            _ => Err("That identity was not offered for this page.".into()),
        },
        Action::ForgetScope(fingerprint, scope) if manager(MANAGER) => {
            identity::forget_scope(fingerprint, scope)?;
            Ok(Outcome::Manager(MANAGER.into()))
        }
        Action::WidenScope(fingerprint, scope) if manager(MANAGER) => {
            let capsule = GeminiUrl::new(scope.host.clone(), scope.port, "/");
            identity::change_scope(fingerprint, scope, &capsule)?;
            Ok(Outcome::Manager(MANAGER.into()))
        }
        Action::Delete(fingerprint) if manager(MANAGER) => {
            identity::find(fingerprint)?;
            Ok(Outcome::Manager(format!(
                "{MANAGER}?delete={}",
                tofu::fingerprint_hex(fingerprint)
            )))
        }
        Action::ConfirmDelete(fingerprint)
            if manager(&format!(
                "{MANAGER}?delete={}",
                tofu::fingerprint_hex(fingerprint)
            )) =>
        {
            identity::delete(fingerprint)?;
            Ok(Outcome::Manager(MANAGER.into()))
        }
        Action::ForgetPin(key) if manager(MANAGER) => {
            tofu::forget(key)?;
            Ok(Outcome::Manager(MANAGER.into()))
        }
        _ => Err("Open the certificate manager to change identities and pins.".into()),
    }
}

/// Record the user's scope choice on a certificate page.
pub fn choose_scope(security: &mut Security, index: usize) -> bool {
    match security {
        Security::Certificate(request) if index < request.scopes.len() => {
            request.scope = index;
            true
        }
        _ => false,
    }
}

/// What the `identity` command did.
pub struct CommandResult {
    pub status: String,
    /// The current page should be requested again under its new identity.
    pub reload: bool,
}

pub const IDENTITY_USAGE: &str =
    "usage: identity [capsule [name] | page | revoke] — on a Gemini page";

/// `identity`: show which identity is sent for the current Gemini page;
/// `identity capsule [name]` sends the identity used on this capsule to all
/// of it; `identity page` narrows the identity used here to this page and
/// below; `identity revoke` stops sending it here. Each is the user's
/// explicit direction (Gemini §Status 60).
pub fn identity_command(
    arguments: &str,
    current: Option<&GeminiUrl>,
) -> Result<CommandResult, String> {
    let Some(url) = current else {
        return Err("Open a Gemini page first; certificates opens the identity manager.".into());
    };
    let mut words = arguments.split_whitespace();
    let verb = words.next().unwrap_or("");
    let name = words.collect::<Vec<_>>().join(" ");
    let here = identity::select(url)?;
    match verb {
        "" | "status" => Ok(CommandResult {
            status: match &here {
                Some(identity) => format!(
                    "Identity “{}” is sent to {} (scope {} and below).",
                    identity.name,
                    url,
                    identity
                        .scope_for(url)
                        .map_or_else(String::new, ToString::to_string)
                ),
                None => format!(
                    "No identity is sent to {url}. identity capsule sends this capsule's identity everywhere on it; certificates manages identities."
                ),
            },
            reload: false,
        }),
        "capsule" | "widen" => {
            let capsule = GeminiUrl::new(url.host.clone(), url.port, "/");
            let (identities, _) = identity::list();
            let candidates: Vec<_> = identities
                .into_iter()
                .filter(|i| {
                    i.scopes
                        .iter()
                        .any(|s| s.host == url.host && s.port == url.port)
                        && (name.is_empty() || i.name == name)
                })
                .collect();
            let chosen = match candidates.as_slice() {
                [one] => one,
                [] if name.is_empty() => {
                    return Err(format!(
                        "No identity is used on {capsule} yet; open the page that asks for a certificate first."
                    ));
                }
                [] => return Err(format!("No identity named “{name}” is used on {capsule}.")),
                many => {
                    return Err(format!(
                        "Several identities are used on {capsule}: {}. Use identity capsule <name>.",
                        many.iter()
                            .map(|i| i.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            };
            let scope = chosen
                .scopes
                .iter()
                .find(|s| s.host == url.host && s.port == url.port)
                .cloned()
                .expect("filtered above");
            identity::change_scope(&chosen.fingerprint, &scope, &capsule)?;
            Ok(CommandResult {
                status: format!(
                    "Identity “{}” is now sent to every page of {capsule}.",
                    chosen.name
                ),
                reload: true,
            })
        }
        "page" | "narrow" => {
            let identity = here.ok_or_else(|| format!("No identity is sent to {url}."))?;
            let scope = identity
                .scope_for(url)
                .cloned()
                .ok_or("The identity's scopes changed; try again.")?;
            let page = identity::scope_of(url);
            identity::change_scope(&identity.fingerprint, &scope, &page)?;
            Ok(CommandResult {
                status: format!(
                    "Identity “{}” is now sent only to {page} and below.",
                    identity.name
                ),
                reload: false,
            })
        }
        "revoke" | "off" => match identity::revoke(url)? {
            Some((name, scope)) => Ok(CommandResult {
                status: format!("Identity “{name}” is no longer sent to {scope}."),
                reload: true,
            }),
            None => Err(format!("No identity is sent to {url}.")),
        },
        _ => Err(IDENTITY_USAGE.into()),
    }
}

fn line(kind: Kind, text: impl Into<String>, link: Option<Link>) -> DocLine {
    DocLine {
        kind,
        text: text.into(),
        link,
    }
}

/// The page for a security state at `url`.
pub fn lines(security: &Security) -> Vec<DocLine> {
    let mut out = Vec::new();
    let now = x509::now();
    match security {
        Security::Changed(warning) => {
            out.push(line(Kind::Heading(1), "Capsule certificate changed", None));
            let pinned_until = warning
                .pinned
                .not_after
                .map_or_else(|| "an unknown date".to_string(), x509::date);
            out.push(line(
                Kind::Error,
                format!(
                    "{} presented a different certificate from the one TRust pinned on {}, which remains valid until {pinned_until}. This can mean someone is intercepting the connection, or that the capsule replaced its certificate early. Nothing was sent to the server.",
                    warning.presented.key,
                    x509::date(warning.pinned.since),
                ),
                None,
            ));
            out.push(line(Kind::Heading(2), "Pinned certificate", None));
            out.push(line(
                Kind::Text,
                format!(
                    "SHA-256 {}",
                    tofu::fingerprint_text(&warning.pinned.fingerprint)
                ),
                None,
            ));
            out.push(line(Kind::Heading(2), "Presented certificate", None));
            for fact in warning.presented.summary(now) {
                out.push(line(Kind::Text, fact, None));
            }
            out.push(line(Kind::Text, "", None));
            out.push(line(
                Kind::GemLink,
                "Trust the new certificate and continue",
                Some(Action::TrustChanged.link()),
            ));
            out.push(line(
                Kind::GemLink,
                "Manage capsule certificates",
                Some(Link::External(MANAGER.into())),
            ));
        }
        Security::Certificate(request) => {
            out.push(line(
                Kind::Heading(1),
                format!(
                    "{} ({})",
                    super::status_name(request.status),
                    request.status
                ),
                None,
            ));
            if !request.meta.is_empty() {
                out.push(line(Kind::Info, request.meta.clone(), None));
            }
            match (&request.sent, request.status) {
                (Some(sent), 61) => out.push(line(
                    Kind::Error,
                    format!(
                        "The capsule does not accept identity “{}” for this page. Choose or create a different identity.",
                        sent.name
                    ),
                    None,
                )),
                (Some(sent), 62) => out.push(line(
                    Kind::Error,
                    format!(
                        "The capsule says identity “{}” is not a valid certificate (for example, outside its validity dates). Choose or create a different identity.",
                        sent.name
                    ),
                    None,
                )),
                (Some(sent), _) => out.push(line(
                    Kind::Error,
                    format!(
                        "Identity “{}” was sent and not accepted. Choose or create a different identity.",
                        sent.name
                    ),
                    None,
                )),
                (None, _) if request.unrequested => out.push(line(
                    Kind::Error,
                    "An identity is chosen for this page, but the server never asked for a certificate during TLS, so none could be sent.",
                    None,
                )),
                (None, _) => {}
            }
            out.push(line(Kind::Heading(2), "Where the identity is sent", None));
            let last = request.scopes.len().saturating_sub(1);
            for (index, scope) in request.scopes.iter().enumerate() {
                let text = if index == 0 {
                    format!("{scope} and every page below it (recommended)")
                } else if index == last && scope.path == "/" {
                    format!("The whole capsule: every page of {scope}")
                } else {
                    format!("{scope} and every page below it")
                };
                out.push(line(
                    Kind::GemLink,
                    format!(
                        "({}) {text}",
                        if index == request.scope { "•" } else { " " }
                    ),
                    Some(Action::Scope(index).link()),
                ));
            }
            out.push(line(
                Kind::Info,
                "Capsules that ask for a certificate on one page (such as a join page) often recognize it everywhere: choose the whole capsule for those. Other hosts and ports never receive it.",
                None,
            ));
            if request.tls12 {
                out.push(line(
                    Kind::Error,
                    "This capsule uses TLS 1.2, which sends client certificates unencrypted: anyone on the network path can see which identity you use.",
                    None,
                ));
            }
            out.push(line(Kind::Text, "", None));
            out.push(line(
                Kind::GemLink,
                "Create a new identity…",
                Some(Action::NewIdentity.link()),
            ));
            for choice in &request.choices {
                if request
                    .sent
                    .as_ref()
                    .is_some_and(|sent| sent.fingerprint == choice.fingerprint)
                {
                    continue;
                }
                out.push(line(
                    Kind::GemLink,
                    format!("Use “{}” — {}", choice.name, choice.summary),
                    Some(Action::UseIdentity(choice.fingerprint).link()),
                ));
            }
            for problem in &request.problems {
                out.push(line(
                    Kind::Error,
                    format!("Unusable identity file: {problem}"),
                    None,
                ));
            }
            out.push(line(
                Kind::GemLink,
                "Manage identities",
                Some(Link::External(MANAGER.into())),
            ));
        }
    }
    out
}

fn gemtext_text(text: &str) -> String {
    // Keep generated lines from being read as Gemtext markup.
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    if text.starts_with(['#', '*', '>', '=', '`']) {
        format!(" {text}")
    } else {
        text
    }
}

/// The `about:certificates` page (Gemtext). `delete` asks to confirm the
/// deletion of one identity.
pub fn manager(address: &str) -> String {
    let confirm = address
        .strip_prefix(&format!("{MANAGER}?delete="))
        .and_then(tofu::parse_fingerprint);
    let mut page = String::from("# Gemini certificates\n\n");
    let (identities, problems) = identity::list();
    if let Some(fingerprint) = confirm
        && let Some(identity) = identities.iter().find(|i| i.fingerprint == fingerprint)
    {
        page.push_str(&format!(
            "## Delete identity “{}”?\n\nIts private key is erased. Capsules that know you by this certificate cannot recognize you again, and this cannot be undone.\n\n=> {} Delete “{}” permanently\n=> {MANAGER} Keep it\n\n",
            gemtext_text(&identity.name),
            Action::ConfirmDelete(fingerprint).link(),
            gemtext_text(&identity.name),
        ));
    }
    page.push_str("## Client identities\n\nIdentities are offered when a capsule asks for a client certificate (status 60). Each is sent only where you chose it: a host, port and path subtree.\n\n");
    if identities.is_empty() {
        page.push_str("No identities yet.\n\n");
    }
    for identity in &identities {
        page.push_str(&format!("### {}\n", gemtext_text(&identity.name)));
        page.push_str(&format!(
            "SHA-256 {}\n",
            tofu::fingerprint_text(&identity.fingerprint)
        ));
        if let Some(certificate) = &identity.certificate
            && let (Some(from), Some(to)) = (certificate.not_before, certificate.not_after)
        {
            page.push_str(&format!(
                "Valid {} to {}\n",
                x509::date(from),
                x509::date(to)
            ));
        }
        page.push_str(&format!(
            "File {}\n",
            gemtext_text(&identity.path.display().to_string())
        ));
        if identity.scopes.is_empty() {
            page.push_str("Not used for any capsule.\n");
        }
        for scope in &identity.scopes {
            page.push_str(&format!("* Sent to {scope} and every page below it\n"));
            if scope.path != "/" {
                page.push_str(&format!(
                    "=> {} Send it to the whole capsule {}\n",
                    Action::WidenScope(identity.fingerprint, scope.clone()).link(),
                    GeminiUrl::new(scope.host.clone(), scope.port, "/")
                ));
            }
            page.push_str(&format!(
                "=> {} Stop sending it to {}\n",
                Action::ForgetScope(identity.fingerprint, scope.clone()).link(),
                scope
            ));
        }
        page.push_str(&format!(
            "=> {} Delete this identity…\n\n",
            Action::Delete(identity.fingerprint).link()
        ));
    }
    for problem in problems {
        page.push_str(&format!("* Unusable file: {}\n", gemtext_text(&problem)));
    }
    if let Some(dir) = identity::directory() {
        page.push_str(&format!(
            "\nPEM files placed in {} (certificate and private key) appear here and are offered on request.\n",
            gemtext_text(&dir.display().to_string())
        ));
    }
    page.push_str("\n## Capsule certificates\n\nTRust trusts a capsule's certificate the first time it connects and warns if a different one appears before the pinned certificate expires (Gemini TOFU).\n\n");
    let pins = tofu::pins();
    if pins.is_empty() {
        page.push_str("No capsules pinned yet.\n");
    }
    for (key, pin) in pins {
        page.push_str(&format!(
            "=> {} Forget {} (SHA-256 {}…, {}trusted since {})\n",
            Action::ForgetPin(key.clone()).link(),
            gemtext_text(&key),
            &tofu::fingerprint_text(&pin.fingerprint)[..23],
            pin.not_after
                .map_or_else(String::new, |t| format!("valid until {}, ", x509::date(t))),
            x509::date(pin.since)
        ));
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_roundtrip_and_reject_lookalikes() {
        let fp = [0xab; 32];
        let scope = GeminiUrl::new("e.test", 1966, "/a b/");
        for action in [
            Action::TrustChanged,
            Action::Scope(2),
            Action::NewIdentity,
            Action::UseIdentity(fp),
            Action::WidenScope(fp, GeminiUrl::parse("gemini://e.test/join").unwrap()),
            Action::ForgetScope(fp, GeminiUrl::parse("gemini://e.test:1966/a%20b/").unwrap()),
            Action::Delete(fp),
            Action::ConfirmDelete(fp),
            Action::ForgetPin("[::1]:1965".into()),
        ] {
            assert_eq!(Action::from_link(&action.link()), Some(action));
        }
        let _ = scope;
        for bad in [
            "about:gemini-trust/accept/now",
            "about:gemini-identity/use/abc",
            "about:gemini-identity/news",
            "about:certificates/delete/zz",
            "about:certificates/forget-pin/",
        ] {
            assert_eq!(
                Action::from_link(&Link::External(bad.into())),
                None,
                "{bad}"
            );
        }
    }

    #[test]
    fn actions_only_apply_to_the_page_that_offered_them() {
        let url = GeminiUrl::new("e.test", 1965, "/private");
        let other = Link::Gemini(url.clone());
        // A remote page linking to an action has no security state.
        let plain = Context {
            address: &other,
            security: None,
        };
        for action in [
            Action::TrustChanged,
            Action::NewIdentity,
            Action::UseIdentity([1; 32]),
            Action::Delete([1; 32]),
            Action::ConfirmDelete([1; 32]),
            Action::ForgetPin("e.test:1965".into()),
        ] {
            assert!(apply(&action, &plain).is_err(), "{action:?}");
        }
        // Identities not listed by the page cannot be chosen through it.
        let request = Security::Certificate(Box::new(CertificateRequest {
            status: 60,
            meta: String::new(),
            sent: None,
            unrequested: false,
            tls12: false,
            choices: Vec::new(),
            problems: Vec::new(),
            scopes: identity::scope_options(&url),
            scope: 0,
        }));
        let context = Context {
            address: &other,
            security: Some(&request),
        };
        assert!(apply(&Action::UseIdentity([2; 32]), &context).is_err());
        assert!(matches!(
            apply(&Action::NewIdentity, &context),
            Ok(Outcome::Prompt(_))
        ));
        // Deleting needs the confirmation page for that same identity.
        let manager = Link::External(format!("{MANAGER}?delete={}", "00".repeat(32)));
        let context = Context {
            address: &manager,
            security: None,
        };
        assert!(apply(&Action::ConfirmDelete([1; 32]), &context).is_err());
    }
}
