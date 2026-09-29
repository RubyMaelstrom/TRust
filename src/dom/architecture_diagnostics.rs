//! Bounded, opt-in counts of repeated DOM/style work. No DOM owners or values are retained.
//! The feature is absent from ordinary builds; enabled runs are diagnostic, never timing.
//! CSSOM #dom-window-getcomputedstyle and DOM mutation algorithms remain authoritative.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::panic::Location;
use std::time::{Duration, Instant};

const CALLER_LIMIT: usize = 256;

#[derive(Default)]
struct State {
    last: Option<Instant>,
    tag_calls: u64,
    tag_callers: BTreeMap<(&'static str, u32), u64>,
    unrecorded_callers: u64,
    cache_reads: u64,
    cache_hits: u64,
    cloned_bytes: u64,
    resolved_value_reuses: u64,
    resolved_value_reused_bytes: u64,
    properties: BTreeMap<usize, (u64, u64, u64)>,
    full_invalidations: u64,
    invalidation_calls: u64,
    invalidated_nodes: u64,
    max_invalidated_nodes: usize,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

#[track_caller]
pub(super) fn tag_name() {
    let caller = Location::caller();
    STATE.with_borrow_mut(|state| {
        state.tag_calls += 1;
        let key = (caller.file(), caller.line());
        if state.tag_callers.len() < CALLER_LIMIT || state.tag_callers.contains_key(&key) {
            *state.tag_callers.entry(key).or_default() += 1;
        } else {
            state.unrecorded_callers += 1;
        }
    });
}

pub(super) fn cache_read(property: usize, hit: bool, bytes: usize) {
    STATE.with_borrow_mut(|state| {
        state.cache_reads += 1;
        state.cache_hits += u64::from(hit);
        state.cloned_bytes += bytes as u64;
        let entry = state.properties.entry(property).or_default();
        entry.0 += 1;
        entry.1 += u64::from(hit);
        entry.2 += bytes as u64;
    });
}

pub(super) fn invalidate(nodes: Option<usize>) {
    STATE.with_borrow_mut(|state| match nodes {
        None => state.full_invalidations += 1,
        Some(nodes) => {
            state.invalidation_calls += 1;
            state.invalidated_nodes += nodes as u64;
            state.max_invalidated_nodes = state.max_invalidated_nodes.max(nodes);
        }
    });
}

pub(super) fn resolved_value_reuse(bytes: usize) {
    STATE.with_borrow_mut(|state| {
        state.resolved_value_reuses += 1;
        state.resolved_value_reused_bytes += bytes as u64;
    });
}

pub(crate) fn report() {
    STATE.with_borrow_mut(|state| {
        if state.last.is_some_and(|last| last.elapsed() < Duration::from_secs(2)) { return; }
        state.last = Some(Instant::now());
        let callers: Vec<_> = state.tag_callers.iter().map(|((file,line),count)| {
            serde_json::json!({"file":file,"line":line,"count":count})
        }).collect();
        let properties: Vec<_> = state.properties.iter().map(|(index,(reads,hits,bytes))| {
            serde_json::json!({"index":index,"name":super::PROPS[*index].name,"reads":reads,"hits":hits,"cloned_bytes":bytes})
        }).collect();
        eprintln!("[dom-diagnostic] {}", serde_json::json!({
            "scope":"cumulative thread", "tag_calls":state.tag_calls,"tag_callers":callers,
            "unrecorded_tag_callers":state.unrecorded_callers,
            "cache_reads":state.cache_reads,"cache_hits":state.cache_hits,"cloned_bytes":state.cloned_bytes,
            "resolved_value_reuses":state.resolved_value_reuses,
            "resolved_value_reused_bytes":state.resolved_value_reused_bytes,
            "properties":properties,"full_invalidations":state.full_invalidations,
            "subtree_invalidations":state.invalidation_calls,"invalidated_nodes":state.invalidated_nodes,
            "max_invalidated_nodes":state.max_invalidated_nodes,
        }));
    });
}
