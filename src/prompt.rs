//! One-line input that browser chrome collects for a document or protocol:
//! Gemini input (status 10/11), a new Gemini identity's name, a Gopher+ ASK
//! answer, and DICT credentials. The terminal and desktop frontends both
//! present it in their command editor. A prompt never holds what the user
//! types; secrets are masked, and kept out of history and logs.

use crate::doc::Link;
use crate::gemini::{self, GeminiUrl};
use crate::gopher::GopherUrl;

#[derive(Clone, PartialEq, Eq)]
pub enum Purpose {
    /// Gemini 0.24.1 §Input expected: the answer becomes the URL's query.
    GeminiInput { url: GeminiUrl },
    /// Gemini §Client certificates: name a new identity for `scope`, then
    /// request `url` again.
    IdentityName { url: GeminiUrl, scope: GeminiUrl },
    /// UMN Gopher+ §2.8: one ASK answer for the current form.
    GopherAnswer {
        item: GopherUrl,
        control: usize,
        line: Option<usize>,
    },
    /// RFC 2229 §3.11: the user name for AUTH at a DICT server.
    DictUser { server: String },
    /// RFC 2229 §3.11: the shared secret, never echoed.
    DictSecret { server: String, user: String },
}

#[derive(Clone, PartialEq, Eq)]
pub struct Prompt {
    pub purpose: Purpose,
    pub label: String,
    /// Editor prefill; empty for every secret.
    pub initial: String,
    pub sensitive: bool,
}

impl std::fmt::Debug for Prompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prompt")
            .field("tag", &self.tag())
            .field("sensitive", &self.sensitive)
            .finish_non_exhaustive()
    }
}

/// What a frontend does with a submitted answer.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Open this destination (it may carry a sensitive Gemini query).
    Navigate(Link),
    /// Ask the next question.
    Next(Prompt),
    /// Store this text in the current Gopher+ form, if it is still `item`.
    Answer {
        item: GopherUrl,
        control: usize,
        line: Option<usize>,
        text: String,
    },
    /// DICT credentials were saved for `server` (`host[:port]`).
    DictCredentials { server: String, status: String },
}

impl Prompt {
    /// The chrome badge naming the prompt.
    pub fn tag(&self) -> &'static str {
        match &self.purpose {
            Purpose::GeminiInput { .. } if self.sensitive => "GEMINI:SECRET",
            Purpose::GeminiInput { .. } => "GEMINI:INPUT",
            Purpose::IdentityName { .. } => "GEMINI:IDENTITY",
            Purpose::GopherAnswer { .. } if self.sensitive => "GOPHER:SECRET",
            Purpose::GopherAnswer { .. } => "GOPHER:FORM",
            Purpose::DictUser { .. } => "DICT:LOGIN",
            Purpose::DictSecret { .. } => "DICT:SECRET",
        }
    }

    pub fn label(&self) -> String {
        self.label.clone()
    }

    /// Gemini status 10/11 (and undefined 1x, read as 10).
    pub fn gemini_input(response: &gemini::Response) -> Option<Self> {
        (10..20).contains(&response.status).then(|| Self {
            purpose: Purpose::GeminiInput {
                url: response.url.public_url(),
            },
            label: response.meta.clone(),
            initial: String::new(),
            sensitive: response.status == 11,
        })
    }

    pub fn identity_name(url: &GeminiUrl, scope: &GeminiUrl) -> Self {
        Self {
            purpose: Purpose::IdentityName {
                url: url.clone(),
                scope: scope.clone(),
            },
            label: format!(
                "Name for a new identity sent to {scope} and below — the capsule sees this name"
            ),
            initial: String::new(),
            sensitive: false,
        }
    }

    pub fn gopher_answer(item: &GopherUrl, edit: crate::gopher::AskEdit) -> Self {
        Self {
            purpose: Purpose::GopherAnswer {
                item: item.clone(),
                control: edit.control,
                line: edit.line,
            },
            label: edit.label,
            initial: edit.initial,
            sensitive: edit.sensitive,
        }
    }

    pub fn dict_user(server: &str, current: Option<&str>) -> Self {
        Self {
            purpose: Purpose::DictUser {
                server: server.into(),
            },
            label: format!("DICT user name for {server} (RFC 2229 AUTH)"),
            initial: current.unwrap_or_default().into(),
            sensitive: false,
        }
    }

    pub fn dict_secret(server: &str, user: &str) -> Self {
        Self {
            purpose: Purpose::DictSecret {
                server: server.into(),
                user: user.into(),
            },
            label: format!("Shared secret for {user} at {server}"),
            initial: String::new(),
            sensitive: true,
        }
    }

    pub fn submit(&self, text: &str) -> Result<Outcome, String> {
        match &self.purpose {
            Purpose::GeminiInput { url } => url
                .with_input(text, self.sensitive)
                .map(|url| Outcome::Navigate(Link::Gemini(url))),
            Purpose::IdentityName { url, scope } => {
                gemini::identity::create(scope, text)?;
                Ok(Outcome::Navigate(Link::Gemini(url.clone())))
            }
            Purpose::GopherAnswer {
                item,
                control,
                line,
            } => Ok(Outcome::Answer {
                item: item.clone(),
                control: *control,
                line: *line,
                text: text.into(),
            }),
            Purpose::DictUser { server } => {
                let user = crate::dict::credentials::validate_user(text)?;
                Ok(Outcome::Next(Self::dict_secret(server, &user)))
            }
            Purpose::DictSecret { server, user } => {
                crate::dict::credentials::save(server, user, text)?;
                Ok(Outcome::DictCredentials {
                    server: server.clone(),
                    status: format!(
                        "Saved DICT credentials for {user} at {server}; lookups there now authenticate."
                    ),
                })
            }
        }
    }
}
