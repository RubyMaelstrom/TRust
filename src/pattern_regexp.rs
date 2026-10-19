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
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Queueing and matching share this budget. An unfinished match is treated as
/// absent for this validation pass and may be retried on a later pass.
const MATCH_BUDGET: Duration = Duration::from_millis(250);
/// Requests beyond this bound fail open instead of making style workers queue
/// unbounded work. Queueing and execution share the same caller deadline.
const PENDING_REQUESTS: usize = 64;
/// Verdicts kept for repeated selector matching across style passes.
const CACHE_ENTRIES: usize = 512;
/// Values longer than this are evaluated but not cached.
const CACHE_VALUE_BYTES: usize = 4096;

struct Request {
    pattern: String,
    value: String,
    deadline: Instant,
    reply: SyncSender<Evaluation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Evaluation {
    Complete(Option<bool>),
    Unavailable,
}

impl Evaluation {
    fn verdict(self) -> Option<bool> {
        match self {
            Self::Complete(verdict) => verdict,
            Self::Unavailable => None,
        }
    }
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

fn service() -> Option<&'static SyncSender<Request>> {
    static SERVICE: OnceLock<Option<SyncSender<Request>>> = OnceLock::new();
    SERVICE
        .get_or_init(|| {
            let (sender, receiver) = sync_channel(PENDING_REQUESTS);
            std::thread::Builder::new()
                .name("trust-pattern-regexp".into())
                .spawn(move || serve(receiver))
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

/// Whether `value` matches the compiled pattern regular expression of a
/// `pattern` attribute whose value is `pattern`. `None` when the element has
/// no compiled pattern (the attribute is not a valid `v`-flag pattern), or
/// when the match could not be completed.
pub(crate) fn pattern_matches(pattern: &str, value: &str) -> Option<bool> {
    pattern_matches_with_cache(pattern, value, cache(), || evaluate(pattern, value))
}

fn pattern_matches_with_cache(
    pattern: &str,
    value: &str,
    cache: &Mutex<Cache>,
    evaluate: impl FnOnce() -> Evaluation,
) -> Option<bool> {
    let cacheable = value.len() <= CACHE_VALUE_BYTES && pattern.len() <= CACHE_VALUE_BYTES;
    let key = (pattern.to_owned(), value.to_owned());
    if cacheable
        && let Ok(cache) = cache.lock()
        && let Some(verdict) = cache.verdicts.get(&key)
    {
        return *verdict;
    }
    let evaluation = evaluate();
    // A SyntaxError establishes an invalid pattern. A host deadline or other
    // runtime failure says nothing about its validity and must remain
    // retryable on the next validation/style pass.
    if cacheable
        && let Evaluation::Complete(verdict) = evaluation
        && let Ok(mut cache) = cache.lock()
    {
        if cache.order.len() >= CACHE_ENTRIES
            && let Some(oldest) = cache.order.pop_front()
        {
            cache.verdicts.remove(&oldest);
        }
        if cache.verdicts.insert(key.clone(), verdict).is_none() {
            cache.order.push_back(key);
        }
    }
    evaluation.verdict()
}

fn evaluate(pattern: &str, value: &str) -> Evaluation {
    let Some(service) = service() else {
        return Evaluation::Unavailable;
    };
    evaluate_until(service, pattern, value, Instant::now() + MATCH_BUDGET)
}

fn evaluate_until(
    service: &SyncSender<Request>,
    pattern: &str,
    value: &str,
    deadline: Instant,
) -> Evaluation {
    let (reply, answer) = sync_channel(1);
    if service
        .try_send(Request {
            pattern: pattern.to_owned(),
            value: value.to_owned(),
            deadline,
            reply,
        })
        .is_err()
    {
        return Evaluation::Unavailable;
    }
    answer
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(Evaluation::Unavailable)
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
            } catch (error) {
                // ECMA-262 ParsePattern/RegExpInitialize reject invalid
                // patterns with SyntaxError. Other failures are retryable.
                if (!(error instanceof SyntaxError)) throw error;
                regexp = null;
            }
            if (compiled.size >= 64) compiled.clear();
            compiled.set(pattern, regexp);
        }
        return regexp === null ? null : regexp.exec(value) !== null;
    };
})()"#;

struct Matcher {
    engine: trust_js::Engine,
    interrupt: std::sync::Arc<trust_js::RuntimeInterrupt>,
    function: trust_js::embed::Value,
}

impl Matcher {
    fn new() -> Option<Self> {
        let interrupt = std::sync::Arc::new(trust_js::RuntimeInterrupt::default());
        let mut engine = trust_js::Engine::new_with_interrupt(interrupt.clone());
        let Ok(Ok(function)) = engine.eval_value(MATCHER) else {
            return None;
        };
        Some(Self {
            engine,
            interrupt,
            function,
        })
    }

    fn evaluate(
        &mut self,
        pattern: String,
        value: String,
        deadline: Option<Instant>,
    ) -> Evaluation {
        self.interrupt.set_deadline(deadline);
        let result = self.engine.call_function_interruptible(
            &self.function,
            trust_js::embed::Value::Undefined,
            &[
                trust_js::embed::Value::from_engine_text(pattern),
                trust_js::embed::Value::from_engine_text(value),
            ],
        );
        self.interrupt.set_deadline(None);
        match result {
            Ok(trust_js::embed::Value::Bool(matched)) => Evaluation::Complete(Some(matched)),
            Ok(trust_js::embed::Value::Null) => Evaluation::Complete(None),
            _ => Evaluation::Unavailable,
        }
    }
}

fn serve(requests: Receiver<Request>) {
    let mut matcher = Matcher::new();
    for request in requests {
        let verdict = if request.deadline <= Instant::now() {
            Evaluation::Unavailable
        } else {
            if matcher.is_none() {
                matcher = Matcher::new();
            }
            matcher.as_mut().map_or(Evaluation::Unavailable, |matcher| {
                matcher.evaluate(request.pattern, request.value, Some(request.deadline))
            })
        };
        let _ = request.reply.send(verdict);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn match_pattern(matcher: &mut Matcher, pattern: &str, value: &str) -> Option<bool> {
        match matcher.evaluate(pattern.to_owned(), value.to_owned(), None) {
            Evaluation::Complete(verdict) => verdict,
            Evaluation::Unavailable => panic!("unbudgeted matcher failed"),
        }
    }

    #[test]
    fn patterns_are_anchored_v_flag_regexps() {
        // Test ECMAScript semantics independently of the host wall-clock
        // budget: Unicode property compilation can be interrupted under load.
        let mut matcher = Matcher::new().expect("private pattern matcher");
        assert_eq!(match_pattern(&mut matcher, "[0-9]+", "123"), Some(true));
        assert_eq!(match_pattern(&mut matcher, "[0-9]+", "12a"), Some(false));
        // Anchored: a partial match does not satisfy the pattern.
        assert_eq!(match_pattern(&mut matcher, "a|b", "ab"), Some(false));
        assert_eq!(match_pattern(&mut matcher, "a|b", "b"), Some(true));
        // Lookahead, as password rules use it.
        assert_eq!(
            match_pattern(&mut matcher, "(?=.*\\d)(?=.*[a-z]).{4,}", "abc1"),
            Some(true)
        );
        assert_eq!(
            match_pattern(&mut matcher, "(?=.*\\d)(?=.*[a-z]).{4,}", "abcd"),
            Some(false)
        );
        // `v`-flag class set operations and string properties.
        assert_eq!(
            match_pattern(&mut matcher, "[\\p{L}--[a-z]]+", "ÀB"),
            Some(true)
        );
        assert_eq!(
            match_pattern(&mut matcher, "[\\p{L}--[a-z]]+", "aB"),
            Some(false)
        );
        assert_eq!(
            match_pattern(&mut matcher, "\\p{RGI_Emoji}", "👍🏽"),
            Some(true)
        );
    }

    #[test]
    fn invalid_patterns_have_no_compiled_regexp() {
        let mut matcher = Matcher::new().expect("private pattern matcher");
        // Invalid under the `v` flag: a lone `{` and an unescaped `(` in a
        // class, and a pattern valid only once wrapped in the anchors.
        assert_eq!(match_pattern(&mut matcher, "a{", "a{"), None);
        assert_eq!(match_pattern(&mut matcher, "[(]", "("), None);
        assert_eq!(match_pattern(&mut matcher, "a)|(b", "a"), None);
    }

    #[test]
    fn interrupted_pattern_evaluation_can_retry_and_cache_a_completed_verdict() {
        let cache = Mutex::<Cache>::default();
        let mut matcher = Matcher::new().expect("private pattern matcher");
        assert_eq!(
            pattern_matches_with_cache("a+", "aaa", &cache, || {
                let result = matcher.evaluate(
                    "a+".into(),
                    "aaa".into(),
                    Some(Instant::now() - Duration::from_secs(1)),
                );
                assert_eq!(result, Evaluation::Unavailable);
                result
            }),
            None
        );
        assert!(cache.lock().unwrap().verdicts.is_empty());
        assert_eq!(
            pattern_matches_with_cache("a+", "aaa", &cache, || {
                matcher.evaluate("a+".into(), "aaa".into(), None)
            }),
            Some(true)
        );
        assert_eq!(
            pattern_matches_with_cache("a+", "aaa", &cache, || {
                panic!("a completed verdict must be cached")
            }),
            Some(true)
        );
        // A completed invalid pattern is cacheable, unlike an interruption.
        assert_eq!(
            pattern_matches_with_cache("a{", "a{", &cache, || {
                matcher.evaluate("a{".into(), "a{".into(), None)
            }),
            None
        );
        assert_eq!(
            pattern_matches_with_cache("a{", "a{", &cache, || {
                panic!("a completed SyntaxError must be cached")
            }),
            None
        );
    }

    #[test]
    fn non_syntax_compile_failure_does_not_poison_the_compiled_pattern_cache() {
        let mut matcher = Matcher::new().expect("private pattern matcher");
        // A private-realm fault injection avoids timing-dependent exhaustion.
        // The second constructor call must retry the same valid pattern.
        let setup = matcher.engine.eval_value(
            r#"(() => {
                const NativeRegExp = RegExp;
                let fail = true;
                globalThis.RegExp = function(pattern, flags) {
                    if (fail) { fail = false; throw new RangeError("temporary failure"); }
                    return new NativeRegExp(pattern, flags);
                };
            })()"#,
        );
        assert!(matches!(setup, Ok(Ok(_))));
        assert_eq!(
            matcher.evaluate("a+".into(), "aaa".into(), None),
            Evaluation::Unavailable
        );
        assert_eq!(
            matcher.evaluate("a+".into(), "aaa".into(), None),
            Evaluation::Complete(Some(true))
        );
    }

    #[test]
    fn pattern_request_queue_and_reply_share_a_bounded_deadline() {
        let (service, requests) = sync_channel(1);
        // No receiver is serving requests: an expired deadline cannot wait.
        let deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            evaluate_until(&service, "a", "a", deadline),
            Evaluation::Unavailable
        );
        // The expired request still occupies the one-entry queue. A full
        // queue must also return immediately rather than waiting for capacity.
        assert_eq!(
            evaluate_until(&service, "b", "b", Instant::now() + MATCH_BUDGET),
            Evaluation::Unavailable
        );
        assert_eq!(requests.try_recv().unwrap().deadline, deadline);
        assert!(requests.try_recv().is_err());
    }
}
