//! Conservative interpretation of common WHOIS records. RFC 3912 defines no
//! field schema: only an object matching the query can supply summary fields.
//! RPSL object boundaries/continuations follow RFC 2622 §2 (2026-09-06 snapshot).

use super::*;
use crate::registration::{Record, Section};

type Fields = Vec<(String, String)>;

fn domain(value: &str) -> Option<String> {
    if value.is_empty()
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '%' | '/' | '\\' | ':' | '@' | '#' | '?'))
    {
        return None;
    }
    match url::Host::parse(value.trim().trim_end_matches('.')).ok()? {
        url::Host::Domain(name) => Some(name.to_ascii_lowercase()),
        _ => None,
    }
}

fn pairs(text: &str, rpsl: bool) -> Fields {
    let mut fields: Fields = Vec::new();
    for line in text.lines() {
        if line.starts_with(['%', '#']) {
            continue;
        }
        if rpsl && line.starts_with([' ', '\t', '+']) {
            if let Some((_, value)) = fields.last_mut()
                && value.len() < text_reply::MAX_COLUMNS
            {
                value.push(' ');
                value.push_str(line[1..].trim());
            }
            continue;
        }
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        if key.is_empty()
            || key.len() > 80
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '/' | '_'))
        {
            continue;
        }
        let value = if rpsl {
            value.split('#').next().unwrap_or("")
        } else {
            value
        };
        fields.push((key.to_ascii_lowercase(), value.trim().to_string()));
        if fields.len() >= MAX_ROWS {
            break;
        }
    }
    fields
}

pub(super) fn discovery(hop: &Hop, text: &str) -> bool {
    let fields = pairs(text, false);
    fields
        .iter()
        .any(|(key, value)| key == "refer" && !value.is_empty())
        && fields.iter().any(|(key, value)| match key.as_str() {
            "domain" => domain(value) != domain(&hop.target.query),
            "inetnum" | "inet6num" => network_bounds(value) != network_bounds(&hop.target.query),
            "as-block" => !value.eq_ignore_ascii_case(&hop.target.query),
            _ => false,
        })
}

fn network_bounds(value: &str) -> Option<(bool, u128, u128)> {
    fn ip(value: &str) -> Option<(bool, u128)> {
        match value.trim().parse::<std::net::IpAddr>().ok()? {
            std::net::IpAddr::V4(ip) => Some((true, u128::from(u32::from(ip)))),
            std::net::IpAddr::V6(ip) => Some((false, u128::from(ip))),
        }
    }
    if let Some((start, end)) = value.split_once('-') {
        let (v4, start) = ip(start)?;
        let (end_v4, end) = ip(end)?;
        return (v4 == end_v4 && start <= end).then_some((v4, start, end));
    }
    if let Some((network, length)) = value.split_once('/') {
        let (v4, network) = ip(network)?;
        let length: u32 = length.trim().parse().ok()?;
        let bits = if v4 { 32 } else { 128 };
        if length > bits {
            return None;
        }
        let host = if bits - length == 128 {
            u128::MAX
        } else {
            (1u128 << (bits - length)) - 1
        };
        return Some((v4, network & !host, network | host));
    }
    let (v4, address) = ip(value)?;
    Some((v4, address, address))
}

fn match_span(fields: &Fields, query: &str) -> Option<u128> {
    fields
        .iter()
        .filter_map(|(key, value)| match key.as_str() {
            "aut-num" | "nic-hdl" | "organisation" if value.eq_ignore_ascii_case(query) => Some(0),
            "inetnum" | "inet6num" | "netrange" | "cidr" => {
                let (v4, start, end) = network_bounds(query)?;
                value
                    .split(',')
                    .filter_map(|value| {
                        let (other_v4, other_start, other_end) = network_bounds(value.trim())?;
                        (v4 == other_v4 && other_start <= start && end <= other_end)
                            .then_some(other_end - other_start)
                    })
                    .min()
            }
            _ => None,
        })
        .min()
}

fn add_fields(record: &mut Record, fields: &Fields, hop: &Hop, related: bool) {
    let source = authority(&hop.target);
    let mut contact = String::new();
    let has_public_org = fields.iter().any(|(key, value)| {
        key == "registrant organization"
            && !value.is_empty()
            && !crate::registration::redacted(value)
    });
    let related_name = fields
        .iter()
        .find(|(key, _)| matches!(key.as_str(), "org-name" | "role" | "person" | "nic-hdl"))
        .map(|(_, value)| value.as_str())
        .unwrap_or("Related contact");
    for (key, value) in fields {
        if value.is_empty() {
            continue;
        }
        if key == "contact" {
            contact = value.clone();
            continue;
        }
        let (section, label) = match key.as_str() {
            "organisation" if related || fields.iter().any(|(key, _)| key == "org-name") => {
                (Section::Details, "Organization handle")
            }
            "domain" | "domain name" | "aut-num" => continue,
            "registrant organization" => (Section::Summary, "Registrant"),
            "registrant country" => (Section::Summary, "Country"),
            "registrar" => (Section::Summary, "Registrar"),
            "creation date" | "created" if !related && contact.is_empty() => {
                (Section::Summary, "Registered")
            }
            "registry expiry date" => (Section::Summary, "Registry expiry"),
            "registrar registration expiration date" => (Section::Summary, "Registrar expiry"),
            "expiry date" | "expiration date" | "paid-till" => (Section::Summary, "Expiry"),
            "updated date" | "last-modified" | "changed" if !related && contact.is_empty() => {
                (Section::Summary, "Updated")
            }
            "domain status" | "status" if !related => (Section::Summary, "Status"),
            "name server" | "nserver" => (Section::Summary, "Nameserver"),
            "dnssec" => (Section::Summary, "DNSSEC"),
            "inetnum" | "inet6num" | "netrange" | "cidr" => (Section::Summary, "Range"),
            "netname" => (Section::Summary, "Network"),
            "as-name" => (Section::Summary, "AS name"),
            "orgname" | "org-name" | "organisation" | "organization" if contact.is_empty() => {
                (Section::Summary, "Organization")
            }
            "country" if contact.is_empty() => (Section::Summary, "Country"),
            "refer" | "whois" | "whois server" | "registrar whois server" | "referralserver" => {
                (Section::Details, key.as_str())
            }
            "registrar abuse contact email" | "abuse-mailbox" => (Section::Contacts, "Abuse email"),
            "registrar abuse contact phone" => (Section::Contacts, "Abuse phone"),
            "registrant email" => (Section::Contacts, "Registrant email"),
            "registrant name" if !has_public_org => (Section::Summary, "Registrant"),
            "registrant name" => (Section::Contacts, "Registrant name"),
            "registrar url" => (Section::Contacts, "Registrar website"),
            "url" | "website" => (Section::Contacts, "Website"),
            key if key.starts_with("registrant ")
                || key.starts_with("admin ")
                || key.starts_with("tech ") =>
            {
                (Section::Contacts, key)
            }
            "person" | "role" | "address" | "phone" | "fax-no" | "e-mail" | "email" | "name" => {
                (Section::Contacts, key.as_str())
            }
            _ => (Section::Details, key.as_str()),
        };
        let label = if !contact.is_empty() {
            format!("{contact} {label}")
        } else if related && section != Section::Summary {
            format!("{related_name} {label}")
        } else {
            label.to_string()
        };
        let section = if !contact.is_empty() {
            Section::Contacts
        } else {
            section
        };
        let first = value.split_whitespace().next().unwrap_or(value);
        let shown = if matches!(key.as_str(), "domain status" | "name server" | "nserver") {
            first
        } else {
            value
        };
        let links = line_links(&format!("{key}: {value}"), &hop.target);
        record.add(section, &label, shown, &source, links.first().cloned());
        for link in links.iter().skip(1) {
            record.add(
                Section::Contacts,
                "Related link",
                &link.to_string(),
                &source,
                Some(link.clone()),
            );
        }
        if shown != value && key == "nserver" {
            record.add(
                Section::Details,
                "Nameserver addresses",
                value,
                &source,
                links.get(1).cloned(),
            );
        }
    }
}

pub(super) fn summarize(reply: &Reply, encoding: Encoding, query: &str) -> Option<Record> {
    let mut record = Record {
        title: query.to_string(),
        ..Default::default()
    };
    let mut recognized = false;
    let mut other = Vec::new();
    for hop in &reply.hops {
        let (text, clipped, _) = display(hop, encoding);
        let text = if matches!(hop.state, HopState::Connecting | HopState::Receiving)
            && !text.ends_with('\n')
        {
            text.rsplit_once('\n').map_or("", |(prefix, _)| prefix)
        } else {
            &text
        };
        if discovery(hop, text) {
            continue;
        }
        let all = pairs(text, false);
        let identities: Vec<_> = all
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "domain" | "domain name"))
            .collect();
        let is_domain = !identities.is_empty()
            && identities
                .iter()
                .all(|(_, value)| domain(value).is_some() && domain(value) == domain(query));
        if is_domain {
            recognized = true;
            // Registrar replies commonly use blank lines inside a domain
            // record; RPSL blank-line boundaries do not apply to that format.
            add_fields(&mut record, &all, hop, false);
        } else {
            let blocks: Vec<_> = text.split("\n\n").map(|block| pairs(block, true)).collect();
            let closest = blocks
                .iter()
                .filter_map(|fields| match_span(fields, query))
                .min();
            let selected: Vec<_> = blocks
                .iter()
                .filter(|fields| closest.is_some() && match_span(fields, query) == closest)
                .collect();
            if selected.is_empty() {
                if !text.trim().is_empty() {
                    other.push((authority(&hop.target), text.to_string()));
                }
                continue;
            }
            recognized = true;
            let mut references: HashSet<_> = selected
                .iter()
                .flat_map(|fields| fields.iter())
                .filter(|(key, _)| matches!(key.as_str(), "org" | "admin-c" | "tech-c" | "abuse-c"))
                .map(|(_, value)| value.to_ascii_lowercase())
                .collect();
            // An organization's abuse-c commonly identifies a separate role
            // object. Resolve that contact without treating its dates as the
            // queried object's events or unrelated organizations as owners.
            let abuse: Vec<_> = blocks
                .iter()
                .filter(|fields| {
                    fields.iter().any(|(key, value)| {
                        key == "organisation" && references.contains(&value.to_ascii_lowercase())
                    })
                })
                .flat_map(|fields| fields.iter())
                .filter(|(key, _)| key == "abuse-c")
                .map(|(_, value)| value.to_ascii_lowercase())
                .collect();
            references.extend(abuse);
            for fields in selected {
                add_fields(&mut record, fields, hop, false);
            }
            for fields in &blocks {
                if fields.iter().any(|(key, value)| {
                    matches!(key.as_str(), "organisation" | "nic-hdl")
                        && references.contains(&value.to_ascii_lowercase())
                }) {
                    add_fields(&mut record, fields, hop, true);
                }
            }
        }
        record.clipped |= clipped;
        // Preserve substantive prose before the record and known error/lock
        // notices, while keeping legal boilerplate in the full replies.
        let mut before_record = true;
        for line in text.lines() {
            let trimmed = line.trim();
            let lower = trimmed.to_ascii_lowercase();
            if lower.starts_with("terms of use") || lower.starts_with("notice:") {
                break;
            }
            if trimmed.contains(':') && !trimmed.starts_with(['%', '#']) {
                before_record = false;
            }
            if trimmed.is_empty() || trimmed.starts_with(['%', '#', '>']) {
                continue;
            }
            if (before_record && !trimmed.contains(':'))
                || [
                    "no match",
                    "not found",
                    "rate limit",
                    "quota exceeded",
                    "access denied",
                    "error:",
                ]
                .iter()
                .any(|s| lower.starts_with(s))
            {
                record.notice(trimmed);
            }
        }
    }
    if !recognized {
        return None;
    }
    for (source, text) in other {
        let excerpt = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        record.notice(&format!("{source}: {excerpt}"));
    }
    Some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(query: &str, answers: &[(&str, &str)]) -> Reply {
        Reply {
            hops: answers
                .iter()
                .map(|(host, text)| Hop {
                    target: server_target(host, query).unwrap(),
                    body: text.as_bytes().into(),
                    state: HopState::Complete,
                    elapsed_ms: 1,
                    notice: None,
                })
                .collect(),
            finished: true,
            notice: None,
        }
    }
    fn text(doc: &Doc) -> String {
        doc.lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn whois_summary_matches_the_domain_and_deduplicates_redacted_replies() {
        let reply = reply(
            "example.com",
            &[
                (
                    "whois.iana.org",
                    "domain: COM\norganisation: Registry operator\ncreated: 1985-01-01\nnserver: ROOT.TLD\nrefer: registry.test\n",
                ),
                (
                    "registry.test",
                    "Domain Name: EXAMPLE.COM\nRegistrar: Registrar Ltd\nCreation Date: 2001-07-03T20:36:16Z\nRegistry Expiry Date: 2028-07-03T20:36:15Z\nName Server: NS.EXAMPLE.NET\nDomain Status: clientTransferProhibited https://example.test/status\nDNSSEC: unsigned\n",
                ),
                (
                    "registrar.test",
                    "Domain Name: example.com\nRegistrar: REGISTRAR LTD\nCreation Date: 2001-07-03T18:36:16Z\nRegistrar Registration Expiration Date: 2029-07-03T20:36:15Z\nRegistrant Organization: Example Company\nRegistrant Country: HK\nRegistrant Name: REDACTED FOR PRIVACY\nRegistrant Email: relay@example.test\nName Server: ns.example.net\nName Server: \nAdmin Name: REDACTED FOR PRIVACY\n",
                ),
            ],
        );
        let target = reply.hops[0].target.clone();
        let doc = render(&target, Page::new(reply.clone()), 100);
        let shown = text(&doc);
        assert!(shown.contains("Example Company") && shown.contains("HK"));
        for hidden in [
            "Registry operator",
            "1985-01-01",
            "ROOT.TLD",
            "REDACTED FOR PRIVACY",
            "relay@example.test",
        ] {
            assert!(!shown.contains(hidden), "{hidden}: {shown}");
        }
        assert_eq!(
            shown.to_ascii_lowercase().matches("ns.example.net").count(),
            1
        );
        assert!(shown.contains("Sources differ for Registered"));
        assert!(shown.contains("Registry expiry") && shown.contains("2028-07-03"));
        assert!(shown.contains("Registrar expiry") && shown.contains("2029-07-03"));
        let mut page = doc.whois.unwrap();
        page.section = Section::Details;
        let details = text(&render(&target, page.clone(), 120));
        for expected in [
            "2001-07-03T20:36:16Z",
            "2001-07-03T18:36:16Z",
            "registry.test",
            "registrar.test",
            "clientTransferProhibited",
        ] {
            assert!(details.contains(expected), "{expected}");
        }
        page.section = Section::Contacts;
        assert!(text(&render(&target, page.clone(), 100)).contains("relay@example.test"));
        page.section = Section::Raw;
        assert!(text(&render(&target, page, 100)).contains("Registry operator"));
        assert_eq!(doc.raw, reply.transcript());
    }

    #[test]
    fn whois_explicit_tld_queries_keep_the_tld_record() {
        let reply = reply(
            "COM",
            &[(
                "iana.test",
                "domain: COM\norganisation: Registry operator\nnserver: NS.TLD\nrefer: registry.test\n",
            )],
        );
        let record = summarize(&reply, Encoding::Auto, "COM").unwrap();
        assert!(
            record
                .fields
                .iter()
                .any(|field| field.value == "Registry operator")
        );
    }

    #[test]
    fn whois_summary_ignores_database_banner_changes_and_preserves_failures() {
        let old = reply(
            "example.com",
            &[(
                "registry.test",
                "Domain Name: example.com\nUpdated Date: 2024-01-01\n>>> Last update of whois database: yesterday <<<\n",
            )],
        );
        let mut new = old.clone();
        new.hops[0].body = b"Domain Name: example.com\nUpdated Date: 2024-01-01\n>>> Last update of whois database: today <<<\n".as_slice().into();
        let target = old.hops[0].target.clone();
        let mut page = Page::refreshed(new.clone(), &Page::new(old));
        page.view.changes = true;
        assert!(text(&render(&target, page, 100)).contains("No registration field changes"));
        new.hops.extend(
            reply(
                "example.com",
                &[("registrar.test", "Query rate limit exceeded. Try later.\n")],
            )
            .hops,
        );
        let shown = text(&render(&target, Page::new(new), 100));
        assert!(shown.contains("2024-01-01") && shown.contains("rate limit exceeded"));
    }

    #[test]
    fn whois_rpsl_summary_uses_the_requested_object_and_referenced_organization() {
        let reply = reply(
            "AS3333",
            &[(
                "ripe.test",
                "as-block: AS3209 - AS3353\ncreated: 1985-01-01\n\naut-num: AS3333\nas-name: EXAMPLE-AS\norg: ORG-EX-RIPE\nimport: from AS1\n  accept ANY\ncreated: 2000-01-01\n\norganisation: ORG-EX-RIPE\norg-name: Example Network\ncreated: 1990-01-01\ncountry: NL\n",
            )],
        );
        let target = reply.hops[0].target.clone();
        let shown = text(&render(&target, Page::new(reply.clone()), 100));
        assert!(
            shown.contains("EXAMPLE-AS")
                && shown.contains("Example Network")
                && shown.contains("2000-01-01")
        );
        assert!(
            !shown.contains("1985-01-01")
                && !shown.contains("1990-01-01")
                && !shown.contains("accept ANY")
        );
        let record = summarize(&reply, Encoding::Auto, "AS3333").unwrap();
        assert!(
            record
                .fields
                .iter()
                .any(|field| field.value == "from AS1 accept ANY")
        );
    }

    #[test]
    fn whois_large_routing_policies_leave_room_for_identity_and_abuse_contacts() {
        let mut text = "aut-num: AS64496\nas-name: EXAMPLE-AS\norg: ORG-EX-RIPE\n".to_string();
        for n in 0..1200 {
            text.push_str(&format!("import: from AS{} accept ANY\n", n + 1));
        }
        text.push_str("created: 2001-01-01\n\norganisation: ORG-EX-RIPE\norg-name: Example Network\ncountry: NL\nabuse-c: ABUSE-RIPE\n\nrole: Abuse department\nnic-hdl: ABUSE-RIPE\nabuse-mailbox: abuse@example.test\n");
        let reply = reply("AS64496", &[("rir.test", &text)]);
        let record = summarize(&reply, Encoding::Auto, "AS64496").unwrap();
        for expected in ["2001-01-01", "Example Network", "NL", "abuse@example.test"] {
            assert!(
                record.fields.iter().any(|field| field.value == expected),
                "{expected}"
            );
        }
        assert!(record.clipped);
        assert!(record.fields.len() <= 768);
    }

    #[test]
    fn whois_network_summary_selects_the_most_specific_containing_object() {
        let reply = reply(
            "192.0.2.23",
            &[(
                "rir.test",
                "inetnum: 192.0.0.0 - 192.255.255.255\nnetname: PARENT\n\ninetnum: 192.0.2.0 - 192.0.2.255\nnetname: CHILD\n\ninetnum: 203.0.113.0 - 203.0.113.255\nnetname: UNRELATED\n",
            )],
        );
        let record = summarize(&reply, Encoding::Auto, "192.0.2.23").unwrap();
        assert!(record.fields.iter().any(|field| field.value == "CHILD"));
        assert!(
            !record
                .fields
                .iter()
                .any(|field| matches!(field.value.as_str(), "PARENT" | "UNRELATED"))
        );
        assert!(
            summarize(&reply, Encoding::Auto, "192.0.2.0/24")
                .unwrap()
                .fields
                .iter()
                .any(|field| field.value == "CHILD")
        );
        assert!(network_bounds("2001:db8::/32").is_some());
        assert!(network_bounds("::/0").is_some());
        assert!(network_bounds("::/129").is_none());
    }

    #[test]
    fn whois_partial_identity_and_fields_do_not_become_false_records() {
        let mut reply = reply(
            "example.com",
            &[(
                "registry.test",
                "Domain Name: example.com\nRegistrar: unfinished",
            )],
        );
        reply.finished = false;
        reply.hops[0].state = HopState::Receiving;
        let record = summarize(&reply, Encoding::Auto, "example.com").unwrap();
        assert!(
            !record
                .fields
                .iter()
                .any(|field| field.value == "unfinished")
        );
        reply.hops[0].state = HopState::Complete;
        assert!(
            summarize(&reply, Encoding::Auto, "example.com")
                .unwrap()
                .fields
                .iter()
                .any(|field| field.value == "unfinished")
        );
        assert!(summarize(&reply, Encoding::Auto, "other.com").is_none());
    }
}
