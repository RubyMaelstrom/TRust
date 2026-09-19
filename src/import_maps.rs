//! HTML import-map parsing, merging and module resolution. Local WHATWG HTML
//! snapshot e5071a20 (2026-09-06), "Import map processing model" and
//! "Resolve a module specifier". SRI snapshot 632bf53a, verification algorithms.
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

type Specifiers = BTreeMap<String, Option<Url>>;
pub(crate) type Handle = std::sync::Arc<std::sync::Mutex<ImportMap>>;

#[derive(Default)]
pub(crate) struct ImportMap {
    imports: Specifiers,
    scopes: BTreeMap<String, Specifiers>,
    integrity: BTreeMap<String, String>,
    resolved: BTreeSet<(String, String, bool)>,
}

// Preserve JSON member order before normalizing URL keys: distinct spellings
// can normalize to the same key, and the later member must win.
#[derive(Default)]
struct Input {
    object: Option<Vec<(String, Input)>>,
    string: Option<String>,
}
impl<'de> serde::Deserialize<'de> for Input {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Input;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Input, M::Error> {
                let mut members: Vec<(String, Input)> = Vec::new();
                while let Some(k) = map.next_key::<String>()? {
                    let v = map.next_value()?;
                    if let Some(index) = members.iter().position(|(name, _)| *name == k) {
                        members[index].1 = v;
                    } else {
                        members.push((k, v));
                    }
                }
                Ok(Input {
                    object: Some(members),
                    string: None,
                })
            }
            fn visit_seq<S: serde::de::SeqAccess<'de>>(
                self,
                mut seq: S,
            ) -> Result<Input, S::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Input::default())
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Input, E> {
                Ok(Input {
                    object: None,
                    string: Some(s.into()),
                })
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Input, E> {
                Ok(Input::default())
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Input, E> {
                Ok(Input::default())
            }
        }
        d.deserialize_any(Visitor)
    }
}

pub(crate) fn url_like(value: &str, base: &Url) -> Option<Url> {
    if value.starts_with('/') || value.starts_with("./") || value.starts_with("../") {
        base.join(value).ok()
    } else {
        Url::parse(value).ok()
    }
}
fn scope_matches(scope: &str, base: &str) -> bool {
    scope == base || scope.ends_with('/') && base.starts_with(scope)
}
fn key_matches(key: &str, specifier: &str, special: bool) -> bool {
    key == specifier || special && key.ends_with('/') && specifier.starts_with(key)
}
fn special(url: Option<&Url>) -> bool {
    url.is_none_or(|u| matches!(u.scheme(), "http" | "https" | "file" | "ftp" | "ws" | "wss"))
}
fn normalize(input: Input, base: &Url) -> Result<Specifiers, String> {
    let mut out = Specifiers::new();
    for (key, value) in input.object.ok_or("Specifier maps must be JSON objects")? {
        if key.is_empty() {
            continue;
        }
        let address = value
            .string
            .and_then(|s| url_like(&s, base))
            .filter(|url| !key.ends_with('/') || url.as_str().ends_with('/'));
        out.insert(url_like(&key, base).map_or(key, |u| u.to_string()), address);
    }
    Ok(out)
}
impl ImportMap {
    pub(crate) fn parse(source: &str, base: &Url) -> Result<Self, String> {
        let input: Input =
            serde_json::from_str(source).map_err(|e| format!("Invalid import-map JSON: {e}"))?;
        let mut out = Self::default();
        for (name, value) in input.object.ok_or("Import maps must be JSON objects")? {
            match name.as_str() {
                "imports" => out.imports = normalize(value, base)?,
                "scopes" => {
                    for (prefix, map) in value
                        .object
                        .ok_or("Import-map scopes must be a JSON object")?
                    {
                        if map.object.is_none() {
                            return Err("Scope specifiers must be a JSON object".into());
                        }
                        if let Ok(url) = base.join(&prefix) {
                            out.scopes.insert(url.into(), normalize(map, base)?);
                        }
                    }
                }
                "integrity" => {
                    for (key, value) in value
                        .object
                        .ok_or("Import-map integrity must be a JSON object")?
                    {
                        if let (Some(url), Some(metadata)) = (url_like(&key, base), value.string) {
                            out.integrity.insert(url.into(), metadata);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }
    pub(crate) fn merge(&mut self, mut new: Self) {
        for (scope, map) in &mut new.scopes {
            map.retain(|key, _| {
                !self.resolved.iter().any(|(base, specifier, special)| {
                    scope_matches(scope, base) && key_matches(key, specifier, *special)
                })
            });
        }
        for (scope, map) in new.scopes {
            let old = self.scopes.entry(scope).or_default();
            for (k, v) in map {
                old.entry(k).or_insert(v);
            }
        }
        for (k, v) in new.integrity {
            self.integrity.entry(k).or_insert(v);
        }
        // This prefix direction is the literal HTML merge algorithm, distinct
        // from the matching rule used for scoped entries above.
        new.imports.retain(|key, _| {
            !self
                .resolved
                .iter()
                .any(|(_, specifier, _)| key.starts_with(specifier))
        });
        for (k, v) in new.imports {
            self.imports.entry(k).or_insert(v);
        }
    }
    pub(crate) fn resolve(&mut self, specifier: &str, base: &Url) -> Option<Url> {
        let as_url = url_like(specifier, base);
        let normalized = as_url.as_ref().map_or(specifier, Url::as_str);
        let special = special(as_url.as_ref());
        let resolve = || -> Result<Option<Url>, ()> {
            for (scope, map) in self.scopes.iter().rev() {
                if scope_matches(scope, base.as_str())
                    && let Some(url) = imports_match(map, normalized, special)?
                {
                    return Ok(Some(url));
                }
            }
            Ok(imports_match(&self.imports, normalized, special)?.or(as_url.clone()))
        };
        let result = resolve().ok().flatten()?;
        self.resolved
            .insert((base.to_string(), normalized.into(), special));
        Some(result)
    }
    pub(crate) fn integrity(&self, url: &Url) -> &str {
        self.integrity.get(url.as_str()).map_or("", String::as_str)
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        fn map_size(map: &Specifiers) -> usize {
            map.iter()
                .map(|(k, v)| k.len() + v.as_ref().map_or(0, |u| u.as_str().len()) + 64)
                .sum()
        }
        map_size(&self.imports)
            + self
                .scopes
                .iter()
                .map(|(k, v)| k.len() + map_size(v) + 64)
                .sum::<usize>()
            + self
                .integrity
                .iter()
                .map(|(k, v)| k.len() + v.len() + 64)
                .sum::<usize>()
            + self
                .resolved
                .iter()
                .map(|(b, s, _)| b.len() + s.len() + 64)
                .sum::<usize>()
    }
}
fn imports_match(map: &Specifiers, specifier: &str, special: bool) -> Result<Option<Url>, ()> {
    for (key, address) in map.iter().rev() {
        if key == specifier {
            return address.clone().map(Some).ok_or(());
        }
        if special && key.ends_with('/') && specifier.starts_with(key) {
            let address = address.as_ref().ok_or(())?;
            let url = address.join(&specifier[key.len()..]).map_err(|_| ())?;
            if !url.as_str().starts_with(address.as_str()) {
                return Err(());
            }
            return Ok(Some(url));
        }
    }
    Ok(None)
}

pub(crate) fn integrity_matches(metadata: &str, bytes: &[u8]) -> bool {
    use sha2::{Digest, Sha256, Sha384, Sha512};
    let parsed: Vec<_> = metadata
        .split_ascii_whitespace()
        .filter_map(|item| {
            let mut parts = item.split('?').next()?.split('-');
            let rank = match parts.next()? {
                "sha256" => 1,
                "sha384" => 2,
                "sha512" => 3,
                _ => return None,
            };
            Some((rank, parts.next().unwrap_or("")))
        })
        .collect();
    let Some(rank) = parsed.iter().map(|(rank, _)| *rank).max() else {
        return true;
    };
    let digest = match rank {
        1 => Sha256::digest(bytes).to_vec(),
        2 => Sha384::digest(bytes).to_vec(),
        _ => Sha512::digest(bytes).to_vec(),
    };
    let actual = crate::img::base64_encode(&digest);
    parsed
        .iter()
        .any(|(r, expected)| *r == rank && *expected == actual)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Url {
        Url::parse("https://example.test/app/index.html").unwrap()
    }
    #[test]
    fn import_maps_normalize_scope_block_and_prefix_resolution() {
        let mut map=ImportMap::parse(r#"{"imports":{"three":"./three.js","pkg/":"./package/","bad/":"./bad","no":null,"data:text/plain,x/":"./data/"},"scopes":{"./deep/":{"three":"./special.js"},"./deep/inner/":{"other":"./other.js"}}}"#,&base()).unwrap();
        assert_eq!(
            map.resolve("three", &base()).unwrap().as_str(),
            "https://example.test/app/three.js"
        );
        let deep = base().join("deep/inner/module.js").unwrap();
        assert!(
            map.resolve("three", &deep)
                .unwrap()
                .as_str()
                .ends_with("special.js")
        );
        assert!(
            map.resolve("pkg/file.js", &deep)
                .unwrap()
                .as_str()
                .ends_with("package/file.js")
        );
        assert!(map.resolve("pkg/../escape.js", &deep).is_none());
        assert!(map.resolve("bad/file.js", &deep).is_none());
        assert!(map.resolve("no", &deep).is_none());
        assert_eq!(
            map.resolve("data:text/plain,x/file", &deep)
                .unwrap()
                .as_str(),
            "data:text/plain,x/file"
        );
    }
    #[test]
    fn import_maps_preserve_normalization_order_and_prior_resolutions() {
        let mut map = ImportMap::parse(
            r#"{"imports":{"/z/../a":"./first.js","/a":"./last.js","x":"./x.js"}}"#,
            &base(),
        )
        .unwrap();
        assert!(
            map.resolve("/a", &base())
                .unwrap()
                .as_str()
                .ends_with("last.js")
        );
        let source = base().join("/old.js").unwrap();
        map.resolve("./old.js", &source);
        map.merge(ImportMap::parse(r#"{"imports":{"x":"./replacement.js","/old.js":"./new.js","fresh":"./fresh.js"},"scopes":{"/":{"/old.js":"./new.js"}}}"#,&base()).unwrap());
        assert!(
            map.resolve("x", &base())
                .unwrap()
                .as_str()
                .ends_with("x.js")
        );
        assert_eq!(map.resolve("./old.js", &source).unwrap(), source);
        assert!(map.resolve("fresh", &base()).is_some());
        for invalid in [
            "[]",
            "null",
            r#"{"scopes":{"/":null}}"#,
            r#"{"imports":[]}"#,
            r#"{"integrity":[]}"#,
        ] {
            assert!(ImportMap::parse(invalid, &base()).is_err());
        }
    }
    #[test]
    fn import_map_integrity_uses_strongest_supported_digest() {
        let valid = "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
        assert!(integrity_matches(valid, b"abc"));
        assert!(!integrity_matches(&format!("{valid} sha512-bad"), b"abc"));
        assert!(integrity_matches("unrecognized-bad", b"abc"));
        assert!(!integrity_matches("sha384-", b"abc"));
    }
}
