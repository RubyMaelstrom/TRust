//! Native timing records, shared by Navigation Timing and Resource Timing.
//!
//! WHATWG Fetch #fetch-timing-info / #connection-timing-info and HR-Time
//! #dfn-unsafe-shared-current-time (local September 6, 2026 snapshots).
//! Internal timestamps use one monotonic epoch anchor. Zero means an operation
//! did not occur; conversion/coarsening happens at the realm exposure boundary.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(crate) fn now_ms() -> f64 {
    static ORIGIN: OnceLock<(Instant, f64)> = OnceLock::new();
    let (instant, epoch) = ORIGIN.get_or_init(|| {
        let instant = Instant::now();
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |duration| duration.as_secs_f64() * 1000.0);
        (instant, epoch)
    });
    epoch + instant.elapsed().as_secs_f64() * 1000.0
}

#[derive(Clone, Debug, Default)]
pub struct FetchTiming {
    pub navigation_type: NavigationType,
    pub connection_reused: bool,
    pub start_time: f64,
    pub redirect_start: f64,
    pub redirect_end: f64,
    pub fetch_start: f64,
    pub domain_lookup_start: f64,
    pub domain_lookup_end: f64,
    pub connect_start: f64,
    pub connect_end: f64,
    pub secure_connection_start: f64,
    pub request_start: f64,
    pub first_interim_response_start: f64,
    pub final_response_start: f64,
    pub response_end: f64,
    pub encoded_body_size: usize,
    pub decoded_body_size: usize,
    pub response_status: u16,
    pub next_hop_protocol: &'static str,
    pub content_type: String,
    pub content_encoding: String,
    pub navigation_redirect_count: u16,
    pub navigation_cross_origin_redirect: bool,
    pub resource_timing_allowed: bool,
    pub resource_body_exposed: bool,
    pub resource_response_status: u16,
    pub render_blocking: bool,
}

/// Send-only report retained from a real fetch until its owning Window can
/// run the networking task. Never carries response bytes or engine values.
#[derive(Clone)]
pub(crate) struct ResourceTiming {
    pub name: String,
    pub initiator: &'static str,
    pub timing: Box<FetchTiming>,
    pub cached: bool,
}

impl ResourceTiming {
    pub fn fetched(
        name: String,
        initiator: &'static str,
        timing: Option<Box<FetchTiming>>,
        started: f64,
    ) -> Option<Self> {
        timing.map(|mut timing| {
            // Include the native API's scheduling delay before parallel I/O.
            timing.start_time = started;
            if timing.redirect_start == 0.0 {
                timing.fetch_start = started;
            } else {
                timing.redirect_start = started;
            }
            Self {
                name,
                initiator,
                timing,
                cached: false,
            }
        })
    }

    /// A new consumer of cached bytes has its own lifetime. Retain the
    /// response's CORS/TAO restrictions, not the old connection timestamps.
    pub fn cached(
        name: String,
        initiator: &'static str,
        previous: Option<&FetchTiming>,
        start: f64,
    ) -> Option<Self> {
        let previous = previous?;
        Some(Self {
            name,
            initiator,
            cached: true,
            timing: Box::new(FetchTiming {
                start_time: start,
                fetch_start: start,
                domain_lookup_start: start,
                domain_lookup_end: start,
                connect_start: start,
                connect_end: start,
                request_start: start,
                final_response_start: start,
                response_end: now_ms(),
                encoded_body_size: previous.encoded_body_size,
                decoded_body_size: previous.decoded_body_size,
                content_type: previous.content_type.clone(),
                content_encoding: previous.content_encoding.clone(),
                resource_timing_allowed: previous.resource_timing_allowed,
                resource_body_exposed: previous.resource_body_exposed,
                resource_response_status: previous.resource_response_status,
                ..Default::default()
            }),
        })
    }

    pub fn data(&self) -> serde_json::Value {
        let mut data = self.timing.resource_data(&self.name, self.initiator);
        if self.cached && self.timing.resource_timing_allowed {
            data["transferSize"] = serde_json::json!(0);
            data["deliveryType"] = serde_json::json!("cache");
        }
        data
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NavigationType {
    #[default]
    Navigate,
    Reload,
    BackForward,
}

impl NavigationType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::Reload => "reload",
            Self::BackForward => "back_forward",
        }
    }
}

impl FetchTiming {
    /// Resource Timing #marking-resource-timing / Fetch #fetch-finale. Send
    /// only a privacy-filtered native scalar record across the host boundary.
    /// The owning Window then converts these coarse shared timestamps against
    /// its private time origin; no author-writable Performance getter is used.
    pub(crate) fn resource_data(&self, name: &str, initiator: &str) -> serde_json::Value {
        let coarse = |value: f64| (value * 10.0).floor() / 10.0;
        let detail = |value| {
            if self.resource_timing_allowed {
                coarse(value)
            } else {
                0.0
            }
        };
        let connection = |value| {
            detail(if self.connection_reused {
                self.fetch_start
            } else {
                value
            })
        };
        let body_size = |value| if self.resource_body_exposed { value } else { 0 };
        serde_json::json!({
            "name": name,
            "initiatorType": initiator,
            "startTime": coarse(self.start_time),
            "fetchStart": coarse(if self.resource_timing_allowed { self.fetch_start } else { self.start_time }),
            "redirectStart": detail(self.redirect_start),
            "redirectEnd": detail(self.redirect_end),
            "domainLookupStart": connection(self.domain_lookup_start),
            "domainLookupEnd": connection(self.domain_lookup_end),
            "connectStart": connection(self.connect_start),
            "connectEnd": connection(self.connect_end),
            "secureConnectionStart": if self.secure_connection_start == 0.0 { 0.0 } else { connection(self.secure_connection_start) },
            "requestStart": detail(self.request_start),
            "firstInterimResponseStart": detail(self.first_interim_response_start),
            "finalResponseHeadersStart": detail(self.final_response_start),
            "responseStart": detail(if self.first_interim_response_start == 0.0 { self.final_response_start } else { self.first_interim_response_start }),
            "responseEnd": coarse(self.response_end),
            "encodedBodySize": body_size(self.encoded_body_size),
            "decodedBodySize": body_size(self.decoded_body_size),
            // Resource Timing #sec-timing-allow-origin permits retaining the
            // cross-origin size restriction even when TAO grants timestamps.
            "transferSize": if self.resource_timing_allowed && self.resource_body_exposed { self.encoded_body_size.saturating_add(300) } else { 0 },
            "responseStatus": self.resource_response_status,
            "nextHopProtocol": if self.resource_timing_allowed { self.next_hop_protocol } else { "" },
            "contentType": if self.resource_body_exposed { crate::download::minimized_mime_type(&self.content_type) } else { String::new() },
            "contentEncoding": if self.resource_body_exposed { self.content_encoding.as_str() } else { "" },
            "deliveryType": "",
            "renderBlockingStatus": if self.render_blocking { "blocking" } else { "non-blocking" },
        })
    }

    pub fn new() -> Self {
        let start = now_ms();
        Self {
            start_time: start,
            fetch_start: start,
            ..Self::default()
        }
    }

    pub fn reused_connection(&mut self, secure: bool) {
        // Fetch #clamp-and-coarsen-connection-timing-info: never reveal the
        // previous request's DNS/connection timestamps through pool reuse.
        self.connection_reused = true;
        self.domain_lookup_start = self.fetch_start;
        self.domain_lookup_end = self.fetch_start;
        self.connect_start = self.fetch_start;
        self.connect_end = self.fetch_start;
        self.secure_connection_start = if secure { self.fetch_start } else { 0.0 };
        self.next_hop_protocol = "http/1.1";
    }

    pub(crate) fn navigation_data(&self, url: &str) -> serde_json::Value {
        // Non-isolated environments use HR-Time's 100 microsecond precision.
        // Coarsen against the shared clock before converting to relative time.
        let coarsen = |value: f64| (value * 10.0).floor() / 10.0;
        let origin = coarsen(self.start_time);
        let relative = |value| {
            if value == 0.0 {
                0.0
            } else {
                coarsen(value) - origin
            }
        };
        let connection = |value| {
            relative(if self.connection_reused {
                self.fetch_start
            } else {
                value
            })
        };
        let mut data = serde_json::json!({
            "timeOrigin": origin,
            "name": url,
            "type": self.navigation_type.as_str(),
            "fetchStart": relative(self.fetch_start),
            "redirectStart": if self.navigation_redirect_count == 0 { 0.0 } else { relative(self.redirect_start) },
            "redirectEnd": if self.navigation_redirect_count == 0 { 0.0 } else { relative(self.redirect_end) },
            "redirectCount": self.navigation_redirect_count,
            "domainLookupStart": connection(self.domain_lookup_start),
            "domainLookupEnd": connection(self.domain_lookup_end),
            "connectStart": connection(self.connect_start),
            "connectEnd": connection(self.connect_end),
            "secureConnectionStart": if self.secure_connection_start == 0.0 { 0.0 } else { connection(self.secure_connection_start) },
            "requestStart": relative(self.request_start),
            "firstInterimResponseStart": relative(self.first_interim_response_start),
            "finalResponseHeadersStart": relative(self.final_response_start),
            "responseStart": relative(if self.first_interim_response_start == 0.0 {
                self.final_response_start
            } else { self.first_interim_response_start }),
            "responseEnd": relative(self.response_end),
            "encodedBodySize": self.encoded_body_size,
            "decodedBodySize": self.decoded_body_size,
            "transferSize": self.encoded_body_size.saturating_add(300),
            "responseStatus": self.response_status,
            "nextHopProtocol": self.next_hop_protocol,
            "contentType": crate::download::minimized_mime_type(&self.content_type),
            "contentEncoding": self.content_encoding,
        });
        // The obsolete Timing interface uses epoch milliseconds. Preserve
        // occurrence information before relative coarsening: a real DNS
        // event at relative 0 is not an absent DNS event. Its older redirect
        // privacy rule also does not grant the new cross-origin TAO exception.
        let legacy_redirect =
            self.navigation_redirect_count > 0 && !self.navigation_cross_origin_redirect;
        for (name, value) in [
            ("navigationStart", self.start_time),
            ("fetchStart", self.fetch_start),
            (
                "domainLookupStart",
                if self.connection_reused {
                    self.fetch_start
                } else {
                    self.domain_lookup_start
                },
            ),
            (
                "domainLookupEnd",
                if self.connection_reused {
                    self.fetch_start
                } else {
                    self.domain_lookup_end
                },
            ),
            (
                "connectStart",
                if self.connection_reused {
                    self.fetch_start
                } else {
                    self.connect_start
                },
            ),
            (
                "connectEnd",
                if self.connection_reused {
                    self.fetch_start
                } else {
                    self.connect_end
                },
            ),
            (
                "secureConnectionStart",
                if self.secure_connection_start == 0.0 {
                    0.0
                } else if self.connection_reused {
                    self.fetch_start
                } else {
                    self.secure_connection_start
                },
            ),
            ("requestStart", self.request_start),
            (
                "responseStart",
                if self.first_interim_response_start == 0.0 {
                    self.final_response_start
                } else {
                    self.first_interim_response_start
                },
            ),
            ("responseEnd", self.response_end),
            (
                "redirectStart",
                if legacy_redirect {
                    self.redirect_start
                } else {
                    0.0
                },
            ),
            (
                "redirectEnd",
                if legacy_redirect {
                    self.redirect_end
                } else {
                    0.0
                },
            ),
        ] {
            data[format!("legacy:{name}")] = serde_json::json!(coarsen(value).floor());
        }
        data["legacyRedirectCount"] = serde_json::json!(if legacy_redirect {
            self.navigation_redirect_count
        } else {
            0
        });
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_timing_cache_retains_privacy_not_previous_network_timestamps() {
        for allowed in [false, true] {
            for exposed in [false, true] {
                let previous = FetchTiming {
                    start_time: 1.0,
                    fetch_start: 2.0,
                    request_start: 3.0,
                    final_response_start: 4.0,
                    response_end: 5.0,
                    redirect_start: 1.0,
                    redirect_end: 2.0,
                    encoded_body_size: 42,
                    decoded_body_size: 84,
                    content_type: "text/plain".into(),
                    resource_timing_allowed: allowed,
                    resource_body_exposed: exposed,
                    resource_response_status: if exposed { 200 } else { 0 },
                    render_blocking: true,
                    ..Default::default()
                };
                let start = now_ms();
                let report = ResourceTiming::cached(
                    "https://original.test/image".into(),
                    "img",
                    Some(&previous),
                    start,
                )
                .unwrap();
                let data = report.data();
                assert!(data["startTime"].as_f64().unwrap() >= start - 0.11);
                assert!(
                    data["responseEnd"].as_f64().unwrap() >= data["startTime"].as_f64().unwrap()
                );
                assert_eq!(data["redirectStart"], 0.0);
                assert_eq!(data["transferSize"], 0);
                assert_eq!(data["deliveryType"], if allowed { "cache" } else { "" });
                assert_eq!(
                    data["requestStart"],
                    if allowed {
                        data["startTime"].clone()
                    } else {
                        serde_json::json!(0.0)
                    }
                );
                assert_eq!(data["encodedBodySize"], if exposed { 42 } else { 0 });
                assert_eq!(data["decodedBodySize"], if exposed { 84 } else { 0 });
                assert_eq!(data["renderBlockingStatus"], "non-blocking");
            }
        }
    }

    #[test]
    fn resource_timing_parser_flags_follow_media_and_head_eligibility() {
        let resources = crate::js::external_resources_at(
            r#"<!doctype html><head>
            <link rel='stylesheet' href='/head'><link rel='stylesheet' href='/wide' media='(min-width:900px)'>
            <link rel='stylesheet' href='/density' media='(min-resolution:2dppx)'>
            <script src='/plain'></script><script blocking='RENDER other' src='/explicit'></script>
            <link rel='modulepreload' href='/module'>
            </head><body><link rel='stylesheet' href='/body'><script blocking='render' src='/late'></script>"#,
            640.0,
            480.0,
            1.0,
        );
        let get = |name| resources.iter().find(|r| r.source == name).unwrap();
        assert!(get("/head").render_blocking);
        assert!(get("/explicit").render_blocking);
        for name in ["/wide", "/density", "/plain", "/module", "/body", "/late"] {
            assert!(!get(name).render_blocking, "{name}");
        }
        assert_eq!(get("/head").initiator, "css");
        assert_eq!(get("/module").initiator, "script");
    }

    #[test]
    fn navigation_timing_reused_connections_do_not_reveal_an_earlier_fetch() {
        let mut timing = FetchTiming {
            start_time: 10_000.123,
            fetch_start: 10_030.456,
            navigation_type: NavigationType::BackForward,
            domain_lookup_start: 5_000.0,
            domain_lookup_end: 5_010.0,
            connect_start: 5_010.0,
            connect_end: 5_040.0,
            secure_connection_start: 5_020.0,
            ..FetchTiming::default()
        };
        timing.reused_connection(true);
        // Redirect processing finalizes fetchStart after the exchange. Reuse
        // still exposes that final fetchStart, not an old connection or hop.
        timing.fetch_start = 10_025.0;
        let data = timing.navigation_data("https://example.test/");
        for name in [
            "domainLookupStart",
            "domainLookupEnd",
            "connectStart",
            "connectEnd",
            "secureConnectionStart",
        ] {
            assert_eq!(data[name], data["fetchStart"], "{name}");
        }
        assert_eq!(data["timeOrigin"], 10_000.1);
        assert_eq!(data["type"], "back_forward");
        assert_eq!(data["redirectStart"], 0.0);
        assert_eq!(data["requestStart"], 0.0);
        timing.reused_connection(false);
        assert_eq!(
            timing.navigation_data("http://example.test/")["secureConnectionStart"],
            0.0
        );
    }
}
