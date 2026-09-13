//! RFC 9083 §§4–6, 9: preserve enclosing-object semantics, optional members,
//! notice paragraphs, events, roles, and error descriptions. jCard values use
//! RFC 7095 §§3.2–3.3, including structured and multiple property values.
//! Redaction signals use RFC 9537 §4.2. RFC Editor snapshot: 2026-09-06.

use super::*;

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn link(value: &str, base: &Url) -> Option<Link> {
    let url = base.join(value).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.scheme() {
        "http" | "https" => Some(Link::Http(url)),
        "mailto" | "tel" => Some(Link::External(url.into())),
        _ => None,
    }
}

fn links(record: &mut Record, value: &Value, source: &Url, section: Section, scope: &str) {
    for item in array(value, "links").iter().take(64) {
        let href = string(item, "href");
        if href.is_empty() {
            continue;
        }
        let base = Url::parse(string(item, "value")).unwrap_or_else(|_| source.clone());
        let Some(mut target) = link(href, &base) else {
            continue;
        };
        let address = target.to_string();
        if string(item, "type")
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/rdap+json"))
            && let Link::Http(url) = &target
        {
            target = direct_action(url);
        }
        let title = string(item, "title");
        let relation = string(item, "rel");
        let label = if title.is_empty() {
            format!("{scope} {relation}").trim().to_string()
        } else {
            format!("{scope} {title}").trim().to_string()
        };
        record.add(section, &label, &address, source.as_str(), Some(target));
    }
}

fn notices(record: &mut Record, value: &Value, source: &Url, scope: &str) {
    for key in ["notices", "remarks"] {
        for item in array(value, key).iter().take(64) {
            let title = string(item, "title");
            let kind = string(item, "type");
            let lower = title.to_ascii_lowercase();
            let boilerplate = [
                "terms of",
                "terms and",
                "copyright",
                "status codes",
                "inaccuracy",
                "complaint",
            ]
            .iter()
            .any(|needle| lower.contains(needle));
            for text in array(item, "description").iter().filter_map(Value::as_str) {
                let label = format!("{scope} {}", if title.is_empty() { key } else { title })
                    .trim()
                    .to_string();
                record.add(Section::Details, &label, text, source.as_str(), None);
                // Unknown notices remain visible; abbreviating the summary
                // never removes their complete paragraph from Details.
                if !boilerplate || kind.contains("truncated") {
                    let clean = registration::clean(text);
                    let excerpt: String = clean.chars().take(240).collect();
                    record.notice(&format!(
                        "{label}: {excerpt}{}",
                        if excerpt.len() < clean.len() {
                            "… (see Record details)"
                        } else {
                            ""
                        }
                    ));
                }
            }
            links(record, item, source, Section::Details, title);
        }
    }
    for item in array(value, "redacted").iter().take(64) {
        record.notice("Some fields are redacted by the server; see Record details.");
        let name = item.get("name").unwrap_or(&Value::Null);
        let reason = item.get("reason").unwrap_or(&Value::Null);
        let name = [string(name, "type"), string(name, "description")]
            .into_iter()
            .find(|v| !v.is_empty())
            .unwrap_or("Unspecified field");
        let reason = [string(reason, "type"), string(reason, "description")]
            .into_iter()
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        record.add(
            Section::Details,
            &format!("Redacted: {name}"),
            if reason.is_empty() {
                "Reason not supplied"
            } else {
                &reason
            },
            source.as_str(),
            None,
        );
    }
}

fn events(record: &mut Record, value: &Value, source: &Url, scope: &str, primary: bool) {
    for event in array(value, "events").iter().take(64) {
        let action = string(event, "eventAction");
        let (section, label) = match action {
            "registration" if primary => (Section::Summary, "Registered"),
            "expiration" if primary => (Section::Summary, "Expiry"),
            "last changed" if primary => (Section::Summary, "Updated"),
            _ => (Section::Details, action),
        };
        let label = format!("{scope} {label}").trim().to_string();
        record.add(
            section,
            &label,
            string(event, "eventDate"),
            source.as_str(),
            None,
        );
        record.add(
            Section::Details,
            &format!("{label} actor"),
            string(event, "eventActor"),
            source.as_str(),
            None,
        );
        links(record, event, source, Section::Details, &label);
    }
}

fn card_text(value: &Value, depth: usize) -> String {
    if depth > 3 {
        return String::new();
    }
    match value {
        Value::String(text) => registration::clean(text),
        Value::Array(values) => values
            .iter()
            .take(32)
            .map(|value| card_text(value, depth + 1))
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    }
}

fn entity(
    record: &mut Record,
    value: &Value,
    source: &Url,
    parent: &str,
    depth: usize,
    remaining: &mut usize,
    primary: bool,
) {
    if depth > 8 || *remaining == 0 {
        record.clipped = true;
        return;
    }
    *remaining -= 1;
    let roles = array(value, "roles")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let role = if roles.is_empty() {
        "Entity".to_string()
    } else {
        roles.join(", ")
    };
    let scope = if parent.is_empty() {
        role.clone()
    } else {
        format!("{parent} / {role}")
    };
    let summary_role = if depth == 0 && roles.contains(&"registrar") {
        Some("Registrar")
    } else if depth == 0 && roles.contains(&"registrant") {
        Some("Registrant")
    } else if primary {
        Some("Organization")
    } else {
        None
    };
    let mut identity = false;
    if let Some(card) = value
        .get("vcardArray")
        .and_then(Value::as_array)
        .filter(|card| card.len() == 2 && card[0] == "vcard")
        && let Some(properties) = card[1].as_array()
    {
        // An empty fn is valid in RDAP; it is not a person's name or evidence
        // of redaction by itself (RFC 9083 §3).
        let properties: Vec<_> = properties
            .iter()
            .filter_map(Value::as_array)
            .filter(|p| p.len() >= 4)
            .collect();
        let has_org = properties.iter().any(|p| {
            p[0] == "org"
                && p[2] == "text"
                && !card_text(&p[3], 0).is_empty()
                && !registration::redacted(&card_text(&p[3], 0))
        });
        for property in properties {
            let key = property[0].as_str().unwrap_or("");
            let data_type = property[2].as_str().unwrap_or("");
            if !matches!(data_type, "text" | "uri") {
                continue;
            }
            let label = match key {
                "fn" => "name",
                "org" => "organization",
                "email" => "email",
                "tel" => "phone",
                "adr" => "address",
                "url" => "website",
                _ => continue,
            };
            let mut values: Vec<_> = property[3..]
                .iter()
                .map(|v| card_text(v, 0))
                .filter(|v| !v.is_empty())
                .collect();
            if key == "adr"
                && let Some(label) = property[1]
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            {
                values.push(label.into());
            }
            for text in values {
                let target = match key {
                    "email" => link(&format!("mailto:{text}"), source),
                    "url" => link(&text, source),
                    "tel" if data_type == "uri" => link(&text, source),
                    _ => None,
                };
                record.add(
                    Section::Contacts,
                    &format!("{scope} {label}"),
                    &text,
                    source.as_str(),
                    target,
                );
                if let Some(label) = summary_role
                    && (key == "org" || (key == "fn" && !has_org))
                    && !registration::redacted(&text)
                {
                    record.add(Section::Summary, label, &text, source.as_str(), None);
                    identity = true;
                }
            }
        }
    }
    if !identity && let Some(label) = summary_role {
        let handle = string(value, "handle");
        if !handle.is_empty() {
            record.add(
                Section::Summary,
                label,
                &format!("Handle {handle}"),
                source.as_str(),
                None,
            );
        }
    }
    record.add(
        Section::Details,
        &format!("{scope} handle"),
        string(value, "handle"),
        source.as_str(),
        None,
    );
    links(record, value, source, Section::Contacts, &scope);
    notices(record, value, source, &scope);
    events(record, value, source, &scope, false);
    for id in array(value, "publicIds") {
        record.add(
            Section::Details,
            &format!("{scope} {}", string(id, "type")),
            string(id, "identifier"),
            source.as_str(),
            None,
        );
    }
    for child in array(value, "entities") {
        entity(record, child, source, &scope, depth + 1, remaining, false);
    }
}

pub(super) fn parse(value: &Value, source: &Url) -> Record {
    let class = string(value, "objectClassName");
    let name = [
        string(value, "unicodeName"),
        string(value, "ldhName"),
        string(value, "name"),
        string(value, "handle"),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .unwrap_or("Registration record");
    let mut record = Record {
        title: name.into(),
        ..Default::default()
    };
    let mut remaining = 256;
    match class {
        "domain" | "nameserver" => {
            record.add(Section::Details, "ASCII name", string(value, "ldhName"), source.as_str(), None);
            record.add(Section::Details, "Unicode name", string(value, "unicodeName"), source.as_str(), None);
            for ns in array(value, "nameservers").iter().take(128) {
                let name = [string(ns, "unicodeName"), string(ns, "ldhName")].into_iter().find(|v| !v.is_empty()).unwrap_or("");
                record.add(Section::Summary, "Nameserver", name, source.as_str(), None);
                addresses(&mut record, ns, source, name, Section::Details);
                notices(&mut record, ns, source, name);
                links(&mut record, ns, source, Section::Details, name);
            }
            if class == "nameserver" { addresses(&mut record, value, source, "Address", Section::Summary); }
            if let Some(dns) = value.get("secureDNS") {
                if let Some(signed) = dns.get("delegationSigned").and_then(Value::as_bool) {
                    record.add(Section::Summary, "DNSSEC", if signed { "Signed delegation" } else { "Unsigned delegation" }, source.as_str(), None);
                }
                if let Some(signed) = dns.get("zoneSigned").and_then(Value::as_bool) {
                    record.add(Section::Summary, "Zone signed", if signed { "Yes" } else { "No" }, source.as_str(), None);
                }
                for key in ["dsData", "keyData"] {
                    for entry in array(dns, key).iter().take(64) { record.add(Section::Details, key, &entry.to_string(), source.as_str(), None); }
                }
            }
        }
        "ip network" => {
            record.add(Section::Summary, "Network", string(value, "name"), source.as_str(), None);
            let start = string(value, "startAddress");
            let end = string(value, "endAddress");
            if !start.is_empty() && !end.is_empty() {
                record.add(Section::Summary, "Range", &format!("{start} – {end}"), source.as_str(), None);
            } else {
                record.add(Section::Summary, "Start address", start, source.as_str(), None);
                record.add(Section::Summary, "End address", end, source.as_str(), None);
            }
        }
        "autnum" => {
            let start = value.get("startAutnum").and_then(Value::as_u64);
            let end = value.get("endAutnum").and_then(Value::as_u64);
            if let Some(start) = start {
                record.title = if end.is_none_or(|end| end == start) { format!("AS{start}") } else { format!("AS{start} – AS{}", end.unwrap()) };
            }
            record.add(Section::Summary, "AS name", string(value, "name"), source.as_str(), None);
        }
        "entity" => entity(&mut record, value, source, "", 0, &mut remaining, true),
        "" if value.get("errorCode").is_some() => {
            record.title = format!("Error {} · {}", value["errorCode"], string(value, "title"));
            for text in array(value, "description").iter().filter_map(Value::as_str) { record.notice(text); }
        }
        _ => record.notice("This response has no supported registration object. Its original JSON is available below."),
    }
    record.add(
        Section::Summary,
        "Country",
        string(value, "country"),
        source.as_str(),
        None,
    );
    for status in array(value, "status").iter().filter_map(Value::as_str) {
        record.add(Section::Summary, "Status", status, source.as_str(), None);
    }
    record.add(
        Section::Details,
        "Handle",
        string(value, "handle"),
        source.as_str(),
        None,
    );
    record.add(
        Section::Details,
        "Type",
        string(value, "type"),
        source.as_str(),
        None,
    );
    record.add(
        Section::Details,
        "Parent handle",
        string(value, "parentHandle"),
        source.as_str(),
        None,
    );
    let port43 = string(value, "port43");
    record.add(
        Section::Details,
        "WHOIS server",
        port43,
        source.as_str(),
        crate::whois::server_target(port43, name).map(Link::OneShot),
    );
    events(&mut record, value, source, "", true);
    notices(&mut record, value, source, "");
    links(&mut record, value, source, Section::Contacts, "Record");
    if class != "entity" {
        for item in array(value, "entities") {
            entity(&mut record, item, source, "", 0, &mut remaining, false);
        }
    }
    // Unknown extension members do not invalidate an RFC 9083 response. Keep
    // their names discoverable without interpreting their private semantics.
    if let Some(object) = value.as_object() {
        for (key, _) in object.iter().filter(|(key, _)| key.contains('_')).take(64) {
            record.add(Section::Details, "Extension", key, source.as_str(), None);
        }
    }
    record
}

fn addresses(record: &mut Record, value: &Value, source: &Url, label: &str, section: Section) {
    if let Some(addresses) = value.get("ipAddresses") {
        for key in ["v4", "v6"] {
            for address in array(addresses, key).iter().filter_map(Value::as_str) {
                record.add(
                    section,
                    label,
                    address,
                    source.as_str(),
                    crate::rdap::action(address),
                );
            }
        }
    }
}
