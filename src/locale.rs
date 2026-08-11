//! Browser-wide language preferences.
//!
//! Keep the HTTP preference list, HTML `NavigatorLanguage` surface, and
//! ECMA-402 default locale sourced from this one definition so network and
//! script-visible language negotiation cannot drift apart.

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
}
