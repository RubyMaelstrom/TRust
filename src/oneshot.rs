//! Compatibility entry points for Finger (RFC 1288) and WHOIS (RFC 3912).
//! DICT has a structured query and reply model in `crate::dict`.
use crate::doc::Doc;
use std::fmt;
pub const WHOIS_DEFAULT: &str = "whois.iana.org";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Finger,
    Whois,
}
impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Self::Finger => 79,
            Self::Whois => 43,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Finger => "finger",
            Self::Whois => "whois",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneShotUrl {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub query: String,
}
impl OneShotUrl {
    pub fn parse(input: &str) -> Option<Self> {
        let (scheme, _) = input.split_once(':')?;
        if scheme.eq_ignore_ascii_case("finger") {
            crate::finger::parse_url(input)
        } else if scheme.eq_ignore_ascii_case("whois") {
            crate::whois::parse_url(input)
        } else {
            None
        }
    }
}
impl fmt::Display for OneShotUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "{}://[{}]", self.scheme.name(), self.host)?;
        } else {
            write!(f, "{}://{}", self.scheme.name(), self.host)?;
        }
        if self.port != self.scheme.default_port() {
            write!(f, ":{}", self.port)?;
        }
        if !self.query.is_empty() {
            f.write_str("/")?;
            crate::finger::encode_query(&self.query, f)?;
        }
        Ok(())
    }
}
pub async fn fetch(url: &OneShotUrl) -> Result<Vec<u8>, String> {
    match url.scheme {
        Scheme::Finger => crate::finger::fetch(url).await.map(|reply| reply.body),
        Scheme::Whois => crate::whois::fetch(url)
            .await
            .map(|reply| reply.transcript()),
    }
}
pub fn parse(url: &OneShotUrl, raw: Vec<u8>, width: usize) -> Doc {
    match url.scheme {
        Scheme::Whois => crate::whois::render(
            url,
            crate::whois::Page::new(crate::whois::Reply::from_bytes(url.clone(), raw)),
            width,
        ),
        Scheme::Finger => crate::finger::render(
            url,
            crate::finger::Reply {
                body: raw,
                finished: true,
                notice: None,
            },
            Default::default(),
            width,
        ),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let url = OneShotUrl::parse("finger://sdf.org/ruby").unwrap();
        assert_eq!(
            (url.scheme, url.host.as_str(), url.port, url.query.as_str()),
            (Scheme::Finger, "sdf.org", 79, "ruby")
        );
        // Empty finger query lists logged-in users.
        let url = OneShotUrl::parse("finger://sdf.org").unwrap();
        assert_eq!(url.query, "");
        assert_eq!(url.to_string(), "finger://sdf.org");

        let url = OneShotUrl::parse("whois://whois.iana.org/example.com").unwrap();
        assert_eq!((url.port, url.query.as_str()), (43, "example.com"));

        let url = OneShotUrl::parse("finger://bbs.example:7979/sysop").unwrap();
        assert_eq!(url.port, 7979);
        assert_eq!(url.to_string(), "finger://bbs.example:7979/sysop");

        assert!(OneShotUrl::parse("gopher://x").is_none());
        assert!(OneShotUrl::parse("finger://").is_none());
    }

    #[tokio::test]
    async fn fetches_finger_and_follows_whois_referrals() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        // A finger daemon: echoes the query it was sent.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ruby\r\n");
            sock.write_all(b"Login: ruby\nPlan: ship TRust\n")
                .await
                .unwrap();
        });
        let url = OneShotUrl {
            scheme: Scheme::Finger,
            host: String::from("127.0.0.1"),
            port,
            query: String::from("ruby"),
        };
        let body = fetch(&url).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("ship TRust"));

        // A WHOIS root that refers to a second server (host:port form
        // so the test can pick its own port).
        let registry = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry_port = registry.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = registry.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"domain: EXAMPLE.COM\nstatus: ACTIVE\n")
                .await
                .unwrap();
        });
        let root = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root_port = root.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = root.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"example.com\r\n");
            sock.write_all(format!("refer: 127.0.0.1:{registry_port}\n").as_bytes())
                .await
                .unwrap();
        });
        let url = OneShotUrl {
            scheme: Scheme::Whois,
            host: String::from("127.0.0.1"),
            port: root_port,
            query: String::from("example.com"),
        };
        let body = String::from_utf8_lossy(&fetch(&url).await.unwrap()).into_owned();
        assert!(body.contains("% WHOIS whois://127.0.0.1"), "got: {body}");
        assert!(
            body.contains("refer: 127.0.0.1"),
            "original answer retained"
        );
        assert!(body.contains("status: ACTIVE"), "followed the referral");
    }
}
