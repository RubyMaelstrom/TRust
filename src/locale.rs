//! Browser-wide language preferences.
//!
//! TRust deliberately advertises US English regardless of the host's language,
//! region, or geographic location. These are build defaults, not OS-derived
//! preferences. Lumen's native ECMA-402 default is also en-US; the integration
//! test below keeps it aligned with HTTP and NavigatorLanguage.

/// The user's most-preferred language (WHATWG HTML, NavigatorLanguage).
pub(crate) const LANGUAGE: &str = "en-US";

/// The user's preferred languages, in descending order.
pub(crate) const LANGUAGES: [&str; 2] = [LANGUAGE, "en"];

/// RFC 9110 §12.5.4 language priority list sent by default from Fetch.
pub(crate) const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_navigator_preferences_stay_in_lockstep() {
        let http_languages: Vec<&str> = ACCEPT_LANGUAGE
            .split(',')
            .map(|range| range.split_once(';').map_or(range, |(tag, _)| tag))
            .collect();

        assert_eq!(http_languages, LANGUAGES);
        assert_eq!(LANGUAGES.first().copied(), Some(LANGUAGE));
    }

    #[test]
    fn page_language_and_native_intl_default_to_us_english() {
        // HTML NavigatorLanguage and ECMA-402 §6.2.3 DefaultLocale describe user
        // preferences, not the Document's language or the machine's location.
        // Also run this test in a subprocess with German LANG/LC_ALL/LANGUAGE.
        let html = r#"<!doctype html><html lang="de"><body><output id="locale"></output>
            <script>
                document.getElementById("locale").textContent = [
                    navigator.language, navigator.languages.join(","),
                    new Intl.NumberFormat().resolvedOptions().locale,
                    new Intl.DateTimeFormat().resolvedOptions().locale,
                    new Intl.NumberFormat().format(1234.5),
                    (1234.5).toLocaleString(),
                    new Intl.DisplayNames(undefined, {type: "language"}).of("de")
                ].join("|");
            </script></body></html>"#;
        let (rendered, outcome) =
            crate::js::transform(html, &crate::js::PageEnv::bare("https://example.de/"));
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{outcome:?}");
        assert!(
            rendered.contains(">en-US|en-US,en|en-US|en-US|1,234.5|1,234.5|German</output>"),
            "{rendered}"
        );
    }
}
