//! HTML #compiled-pattern-regular-expression (local whatwg/html e5071a2):
//! an input's `pattern` attribute is a JavaScript `RegExp` created with the
//! `v` flag. Constraint validation and the `:valid`/`:invalid` selectors run
//! in Rust (selector matching, style worker threads, the presentation arena),
//! where no page realm is reachable, so the ECMAScript semantics come from a
//! private TRust JS realm on a service thread. It owns no page state: it compiles
//! patterns, tests strings and answers over a channel, blocked while idle.
//!
//! ECMA-262 §22.2 (RegExp) defines the matcher; HTML's steps are:
//! RegExpCreate(pattern, "v"), which must succeed on its own; then
//! RegExpCreate("^(?:" + pattern + ")$", "v"); the value matches when
//! RegExpBuiltinExec does not return null.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A match that takes longer than this is abandoned; the pattern is then
/// treated as absent for that value (the control does not suffer from a
/// pattern mismatch), which never blocks submission.
const MATCH_BUDGET: Duration = Duration::from_millis(250);
/// Verdicts kept for repeated selector matching across style passes.
const CACHE_ENTRIES: usize = 512;
/// Values longer than this are evaluated but not cached.
const CACHE_VALUE_BYTES: usize = 4096;

struct Request {
    pattern: String,
    value: String,
    reply: SyncSender<Option<bool>>,
}

#[derive(Default)]
struct Cache {
    order: VecDeque<(String, String)>,
    verdicts: rustc_hash::FxHashMap<(String, String), Option<bool>>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn service() -> Option<&'static Mutex<Sender<Request>>> {
    static SERVICE: OnceLock<Option<Mutex<Sender<Request>>>> = OnceLock::new();
    SERVICE
        .get_or_init(|| {
            let (sender, receiver) = channel();
            std::thread::Builder::new()
                .name("trust-pattern-regexp".into())
                .spawn(move || serve(receiver))
                .ok()
                .map(|_| Mutex::new(sender))
        })
        .as_ref()
}

/// Whether `value` matches the compiled pattern regular expression of a
/// `pattern` attribute whose value is `pattern`. `None` when the element has
/// no compiled pattern (the attribute is not a valid `v`-flag pattern), or
/// when the match could not be completed.
pub(crate) fn pattern_matches(pattern: &str, value: &str) -> Option<bool> {
    let cacheable = value.len() <= CACHE_VALUE_BYTES && pattern.len() <= CACHE_VALUE_BYTES;
    let key = (pattern.to_owned(), value.to_owned());
    if cacheable
        && let Ok(cache) = cache().lock()
        && let Some(verdict) = cache.verdicts.get(&key)
    {
        return *verdict;
    }
    let verdict = evaluate(pattern, value);
    if cacheable && let Ok(mut cache) = cache().lock() {
        if cache.order.len() >= CACHE_ENTRIES
            && let Some(oldest) = cache.order.pop_front()
        {
            cache.verdicts.remove(&oldest);
        }
        if cache.verdicts.insert(key.clone(), verdict).is_none() {
            cache.order.push_back(key);
        }
    }
    verdict
}

fn evaluate(pattern: &str, value: &str) -> Option<bool> {
    let (reply, answer) = sync_channel(1);
    service()?
        .lock()
        .ok()?
        .send(Request {
            pattern: pattern.to_owned(),
            value: value.to_owned(),
            reply,
        })
        .ok()?;
    answer.recv().ok().flatten()
}

/// The realm keeps its own bounded cache of compiled RegExp objects. `null`
/// stands for "no compiled pattern regular expression".
const MATCHER: &str = r#"(() => {
    const compiled = new Map();
    return (pattern, value) => {
        let regexp = compiled.get(pattern);
        if (regexp === undefined) {
            try {
                new RegExp(pattern, "v");
                regexp = new RegExp("^(?:" + pattern + ")$", "v");
            } catch (_) {
                regexp = null;
            }
            if (compiled.size >= 64) compiled.clear();
            compiled.set(pattern, regexp);
        }
        return regexp === null ? null : regexp.exec(value) !== null;
    };
})()"#;

fn serve(requests: Receiver<Request>) {
    let interrupt = std::sync::Arc::new(trust_js::RuntimeInterrupt::default());
    let mut engine = trust_js::Engine::new_with_interrupt(interrupt.clone());
    let Ok(Ok(matcher)) = engine.eval_value(MATCHER) else {
        // Without a matcher every request answers "no compiled pattern".
        for request in requests {
            let _ = request.reply.send(None);
        }
        return;
    };
    for request in requests {
        interrupt.set_deadline(Some(Instant::now() + MATCH_BUDGET));
        let result = engine.call_function_interruptible(
            &matcher,
            trust_js::embed::Value::Undefined,
            &[
                trust_js::embed::Value::from_engine_text(request.pattern),
                trust_js::embed::Value::from_engine_text(request.value),
            ],
        );
        interrupt.set_deadline(None);
        let verdict = match result {
            Ok(trust_js::embed::Value::Bool(matched)) => Some(matched),
            _ => None,
        };
        let _ = request.reply.send(verdict);
    }
}

#[cfg(test)]
mod tests {
    use super::pattern_matches;

    #[test]
    fn patterns_are_anchored_v_flag_regexps() {
        assert_eq!(pattern_matches("[0-9]+", "123"), Some(true));
        assert_eq!(pattern_matches("[0-9]+", "12a"), Some(false));
        // Anchored: a partial match does not satisfy the pattern.
        assert_eq!(pattern_matches("a|b", "ab"), Some(false));
        assert_eq!(pattern_matches("a|b", "b"), Some(true));
        // Lookahead, as password rules use it.
        assert_eq!(
            pattern_matches("(?=.*\\d)(?=.*[a-z]).{4,}", "abc1"),
            Some(true)
        );
        assert_eq!(
            pattern_matches("(?=.*\\d)(?=.*[a-z]).{4,}", "abcd"),
            Some(false)
        );
        // `v`-flag class set operations and string properties.
        assert_eq!(pattern_matches("[\\p{L}--[a-z]]+", "ÀB"), Some(true));
        assert_eq!(pattern_matches("[\\p{L}--[a-z]]+", "aB"), Some(false));
        assert_eq!(pattern_matches("\\p{RGI_Emoji}", "👍🏽"), Some(true));
    }

    #[test]
    fn invalid_patterns_have_no_compiled_regexp() {
        // Invalid under the `v` flag: a lone `{` and an unescaped `(` in a
        // class, and a pattern valid only once wrapped in the anchors.
        assert_eq!(pattern_matches("a{", "a{"), None);
        assert_eq!(pattern_matches("[(]", "("), None);
        assert_eq!(pattern_matches("a)|(b", "a"), None);
    }
}
