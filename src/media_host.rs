//! The page actor's half of HTML media elements (HTML #media-elements): the
//! prelude's HTMLMediaElement runs the element algorithms in script, and
//! these bindings own each element's [`Player`].
//!
//! `__media_load` starts the resource fetch algorithm for a selected URL
//! (#concept-media-load-resource) with the element's potential-CORS request
//! (destination "audio" or "video") and returns a player id. Commands go to
//! the player through `__media_control`. The player's reports arrive as host
//! tasks (`TrustJsHostTask::Media`) and run in the element's Window realm as
//! `trust.mediaEvent(id, kind, a, b, c)`, which queues the element's media
//! element tasks.
//!
//! Media Source Extensions: `__media_source(op, id, …)` runs the operations
//! of the MediaSource object `id` (`crate::media::mse`), and
//! `__media_attach(node, id, destination)` attaches it to a media element
//! (#mediasource-attach), whose player reads its track buffers.
//!
//! Inline playback exists only in graphical frontends; the terminal keeps
//! its external-player representation, and there `canPlayType()` answers ""
//! (it can play nothing inline) and MediaSource does not exist.

use std::collections::HashMap;
use std::sync::Arc;

use super::{
    Ctx, HostState, TrustJsHostTask, TrustJsTaskSender, Value, host_arg_node, host_arg_string,
    host_dom,
};
use crate::dom::media::MediaPresentation;
use crate::media::container::{self, Container};
use crate::media::decode::Registry;
use crate::media::demux::{ByteSource, MemorySource};
use crate::media::mse::{self, AppendMode, MediaSource, Outcome};
use crate::media::player::{AudioMode, ErrorKind, Event, Input, Options, Player, ReadyState};
use crate::media::resource::{FileSource, HttpSource, NetworkEvent};
use crate::media::{Micros, micros_to_seconds, seconds_to_micros};

/// A player's report, carried to the actor as a host task.
pub(super) enum MediaTask {
    Player(Event),
    Network(NetworkEvent),
}

struct Entry {
    player: Player,
    node: usize,
    context: u64,
    /// The attached MediaSource, for a Media Source Extensions player.
    source: Option<u64>,
    taint: Taint,
    /// The user's mute of the traversable the element plays in
    /// (`HostState::apply_audio_mutes`).
    mute: crate::media::audio::Mute,
}

/// Whether a player's resource is CORS-cross-origin (Fetch: an opaque
/// response, which a MediaElementAudioSourceNode must not expose).
#[derive(Clone)]
pub(super) enum Taint {
    /// Same-origin by construction: data:, same-partition blob:, local
    /// files of local documents, MediaSource and Blob providers.
    Clean,
    /// An HTTP(S) resource: tainted when fetched without CORS from an
    /// origin other than the document's.
    Http {
        origins: crate::media::resource::ResponseOrigins,
        cors: bool,
        document: url::Origin,
    },
}

impl Taint {
    /// `None` until the resource has answered.
    fn opaque(&self) -> Option<bool> {
        match self {
            Self::Clean => Some(false),
            Self::Http {
                origins,
                cors,
                document,
            } => {
                let origins = origins.get();
                if origins.is_empty() {
                    return None;
                }
                Some(!cors && origins.iter().any(|origin| origin != document))
            }
        }
    }
}

struct SourceEntry {
    source: MediaSource,
    /// The player of the media element it is attached to.
    player: Option<u64>,
}

#[derive(Default)]
pub(super) struct MediaHost {
    players: HashMap<u64, Entry>,
    sources: HashMap<u64, SourceEntry>,
    /// Clear Key CDM instances, by MediaKeys id (Encrypted Media Extensions).
    cdms: HashMap<u64, crate::media::eme::ClearKey>,
    /// The bytes of the appendBuffer() call in progress (converted from
    /// its BufferSource before the prepare append steps).
    staged: Option<Vec<u8>>,
    next_id: u64,
    /// Play audio silently in real time instead of on the audio device
    /// (headless rendering and tests).
    pub(super) silent: bool,
    /// Media elements rerouted into a Web Audio graph, by node.
    taps: HashMap<usize, crate::media::audio::Tap>,
}

impl MediaHost {
    /// Whether a media element of the page has a player: live playback the
    /// page's actor must keep.
    pub(super) fn has_players(&self) -> bool {
        !self.players.is_empty()
    }

    /// The media element player `id` plays for.
    pub(super) fn player_node(&self, id: u64) -> Option<usize> {
        self.players.get(&id).map(|entry| entry.node)
    }

    /// Stop the players of the elements `gone` names (their Documents were
    /// destroyed, HTML #destroy-a-document), and forget their audio taps.
    pub(super) fn release_elements(&mut self, gone: impl Fn(usize) -> bool) {
        self.players.retain(|_, entry| !gone(entry.node));
        self.taps.retain(|&node, _| !gone(node));
    }

    /// Reroute media element `node`'s audio into a Web Audio graph from now
    /// on (createMediaElementSource()), including its current player's.
    pub(super) fn tap(&mut self, node: usize) -> crate::media::audio::Tap {
        if let Some(tap) = self.taps.get(&node) {
            return tap.clone();
        }
        let tap = crate::media::audio::Tap::new();
        for entry in self.players.values().filter(|entry| entry.node == node) {
            entry.player.route(tap.clone());
            if let Some(opaque) = entry.taint.opaque() {
                tap.set_opaque(opaque);
            }
        }
        self.taps.insert(node, tap.clone());
        tap
    }

    /// Update a tapped element's silence once its resource has answered.
    fn update_taint(&self, id: u64) {
        if let Some(entry) = self.players.get(&id)
            && let Some(tap) = self.taps.get(&entry.node)
            && let Some(opaque) = entry.taint.opaque()
        {
            tap.set_opaque(opaque);
        }
    }

    /// Silence each player whose element plays in a traversable the user
    /// muted: `muted(node, context)` for its element and Window. Players
    /// rerouted into a Web Audio graph are heard through its context,
    /// which its own traversable's mute silences.
    pub(super) fn apply_mutes(&self, muted: impl Fn(usize, u64) -> bool) {
        for entry in self.players.values() {
            entry.mute.set(muted(entry.node, entry.context));
        }
    }

    /// Each player's mute, in player order.
    #[cfg(test)]
    pub(super) fn player_mutes(&self) -> Vec<bool> {
        let mut mutes: Vec<(u64, bool)> = self
            .players
            .iter()
            .map(|(&id, entry)| (id, entry.mute.is_muted()))
            .collect();
        mutes.sort_unstable();
        mutes.into_iter().map(|(_, muted)| muted).collect()
    }

    /// The media elements that have players.
    pub(super) fn player_nodes(&self) -> impl Iterator<Item = usize> + '_ {
        self.players.values().map(|entry| entry.node)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.players.capacity() * std::mem::size_of::<(u64, Entry)>()
            + self
                .sources
                .values()
                .map(|entry| entry.source.memory_bytes())
                .sum::<usize>()
            + self.staged.as_ref().map_or(0, Vec::len)
    }
}

use crate::media::logging;
use crate::media_log;

/// `TRUST_MEDIA_DUMP=DIR`: append every SourceBuffer's appended bytes to
/// `DIR/ms<id>-sb<buffer>.bin` (diagnostics: replaying a site's byte stream).
fn dump_appended(source: u64, buffer: u32, bytes: &[u8]) {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let Some(dir) = DIR.get_or_init(|| std::env::var_os("TRUST_MEDIA_DUMP").map(Into::into)) else {
        return;
    };
    use std::io::Write as _;
    let path = dir.join(format!("ms{source}-sb{buffer}.bin"));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(bytes);
    }
}

fn inline_media(ctx: &mut Ctx) -> bool {
    host_dom(ctx).borrow().inline_media()
}

/// HTML #dom-navigator-canplaytype, from the decoders this process has.
pub(super) fn can_play_type(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    if !inline_media(ctx) {
        return Ok(Value::from_string(String::new()));
    }
    let input = host_arg_string(ctx, args, 0);
    let answer = container::can_play_type(&input, Registry::get());
    Ok(Value::from_string(answer.as_str().to_string()))
}

/// MSE #dom-mediasource-istypesupported.
pub(super) fn mse_type_supported(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    if !inline_media(ctx) {
        return Ok(Value::Bool(false));
    }
    let input = host_arg_string(ctx, args, 0);
    Ok(Value::Bool(container::is_type_supported_mse(
        &input,
        Registry::get(),
    )))
}

/// `__media_capabilities(contentType, media, type, width, height,
/// framerate)`: Media Capabilities (local w3c/media-capabilities@dbc6ff7)
/// for one track of `media` ("audio" or "video"): 8 when its MIME type is
/// valid (#check-mime-type-validity), plus 1 supported, 2 smooth and 4 power
/// efficient for decoding type `type` (TRust neither records nor sends
/// media, so encoding and WebRTC types are unsupported).
pub(super) fn media_capabilities(
    ctx: &mut Ctx,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let content = host_arg_string(ctx, args, 0);
    let media = host_arg_string(ctx, args, 1);
    let kind = host_arg_string(ctx, args, 2);
    let number = |index: usize| args.get(index).and_then(Value::as_num_opt).unwrap_or(0.0);
    if !container::valid_capability_type(&content, &media) {
        return Ok(Value::Num(0.0));
    }
    if !inline_media(ctx) || !matches!(kind.as_str(), "media-source" | "file") {
        return Ok(Value::Num(8.0));
    }
    let video = (media == "video").then(|| (number(3) as u32, number(4) as u32, number(5)));
    let info = container::decoding_info(&content, kind == "media-source", video, Registry::get());
    let flags = 8
        | u32::from(info.supported)
        | u32::from(info.smooth) << 1
        | u32::from(info.power_efficient) << 2;
    Ok(Value::Num(f64::from(flags)))
}

fn task_sender(ctx: &mut Ctx) -> Option<TrustJsTaskSender> {
    ctx.host_mut::<HostState>()?.task_events.clone()
}

/// `__media_load(node, url, destination, typeHint, provider, deferred)`:
/// start fetching and playing `url` (or the Blob `provider`'s bytes) for
/// media element `node`. A `deferred` player fetches nothing until the
/// `start` or `play` control (HTML #concept-media-load-resource's wait for
/// the user to request playback). Returns the player id, or -1 when the URL
/// cannot be fetched (the element then runs its failure steps).
pub(super) fn load(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    if !inline_media(ctx) {
        return Ok(Value::Num(-1.0));
    }
    let Some(node) = ({
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .filter(|&node| matches!(dom.tag_name(node), Some("video" | "audio")))
    }) else {
        return Ok(Value::Num(-1.0));
    };
    let target = host_arg_string(ctx, args, 1);
    let destination: &'static str = if host_arg_string(ctx, args, 2) == "audio" {
        "audio"
    } else {
        "video"
    };
    let hint = container::parse_mime_type(&host_arg_string(ctx, args, 3))
        .and_then(|mime| Container::from_essence(&mime.essence()))
        .map(|(container, _)| container);
    let Some(sender) = task_sender(ctx) else {
        return Ok(Value::Num(-1.0));
    };
    let calling = ctx.host_job_context();
    let deferred = matches!(args.get(5), Some(Value::Bool(true)));
    let id = {
        let Some(state) = ctx.host_mut::<HostState>() else {
            return Ok(Value::Num(-1.0));
        };
        state.media.next_id += 1;
        state.media.next_id
    };
    // A Blob media provider object (`srcObject`): its bytes, read by the
    // element, are the media resource (#concept-media-load-resource, mode
    // local).
    let provided = args
        .get(4)
        .filter(|value| !matches!(value, Value::Undefined | Value::Null))
        .cloned();
    if let Some(value) = provided {
        let Some(bytes) = ctx.buffer_source_bytes(&value, true) else {
            return Ok(Value::Num(-1.0));
        };
        let source = Box::new(MemorySource::new(bytes));
        return Ok(Value::Num(start(
            ctx,
            node,
            id,
            calling,
            Input::Resource { source, hint },
            sender,
            Taint::Clean,
            deferred,
        )));
    }
    let Ok(url) = url::Url::parse(&target) else {
        return Ok(Value::Num(-1.0));
    };
    media_log!(
        "player {id}: load{} {}",
        if deferred { " (deferred)" } else { "" },
        url.as_str().chars().take(200).collect::<String>()
    );
    let mut taint = Taint::Clean;
    let source: Box<dyn ByteSource> = match url.scheme() {
        "data" => match crate::img::decode_data_url(url.as_str()) {
            Some(bytes) => Box::new(MemorySource::new(bytes)),
            None => return Ok(Value::Num(-1.0)),
        },
        "blob" => {
            // File API #blob-url-obtain-object: an entry of this environment's
            // partition (URL parsing already resolved it to a Blob).
            let Some(bytes) = blob_bytes(ctx, &url, calling) else {
                return Ok(Value::Num(-1.0));
            };
            Box::new(MemorySource::new(bytes))
        }
        "file" => {
            // Local media of a local document; web content cannot read files.
            let document_is_file = {
                let state = ctx.host_mut::<HostState>();
                state.is_some_and(|state| state.base.scheme() == "file")
            };
            let Some(path) = url.to_file_path().ok().filter(|_| document_is_file) else {
                return Ok(Value::Num(-1.0));
            };
            match FileSource::open(&path) {
                Ok(source) => Box::new(source),
                Err(_) => return Ok(Value::Num(-1.0)),
            }
        }
        "http" | "https" => {
            let Some(request) =
                super::prepare_element_request(ctx, node, url.as_str(), destination, false)
            else {
                return Ok(Value::Num(-1.0));
            };
            let Some(handle) = ctx
                .host_mut::<HostState>()
                .and_then(|state| state.network.as_ref())
                .map(|network| network.handle.clone())
            else {
                return Ok(Value::Num(-1.0));
            };
            let network_sender = sender.clone();
            let events: crate::media::resource::NetworkEvents = Arc::new(move |event| {
                let _ = network_sender.send(TrustJsHostTask::Media {
                    id,
                    task: MediaTask::Network(event),
                });
            });
            let source = HttpSource::new(request, handle, events);
            let cors = {
                let dom = host_dom(ctx);
                let dom = dom.borrow();
                dom.attr(node, "crossorigin").is_some()
            };
            if let Some(state) = ctx.host_mut::<HostState>() {
                taint = Taint::Http {
                    origins: source.response_origins(),
                    cors,
                    document: super::realm_origin(state, calling),
                };
            }
            Box::new(source)
        }
        _ => return Ok(Value::Num(-1.0)),
    };
    Ok(Value::Num(start(
        ctx,
        node,
        id,
        calling,
        Input::Resource { source, hint },
        sender,
        taint,
        deferred,
    )))
}

/// `__media_attach(node, source, destination)`: attach MediaSource `source`
/// to media element `node` (MSE #mediasource-attach). Returns the player
/// id, or -1 when the MediaSource is not "closed" (the element runs its
/// failure steps).
pub(super) fn attach(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    if !inline_media(ctx) {
        return Ok(Value::Num(-1.0));
    }
    let Some(node) = ({
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .filter(|&node| matches!(dom.tag_name(node), Some("video" | "audio")))
    }) else {
        return Ok(Value::Num(-1.0));
    };
    let source_id = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0) as u64;
    let Some(sender) = task_sender(ctx) else {
        return Ok(Value::Num(-1.0));
    };
    let calling = ctx.host_job_context();
    let (id, demuxer) = {
        let Some(state) = ctx.host_mut::<HostState>() else {
            return Ok(Value::Num(-1.0));
        };
        let Some(entry) = state.media.sources.get(&source_id) else {
            return Ok(Value::Num(-1.0));
        };
        if !entry.source.attach() {
            return Ok(Value::Num(-1.0));
        }
        let demuxer = entry.source.demuxer();
        state.media.next_id += 1;
        (state.media.next_id, demuxer)
    };
    media_log!("player {id}: attach MediaSource {source_id}");
    let player = start(
        ctx,
        node,
        id,
        calling,
        Input::Demuxer(Box::new(demuxer)),
        sender,
        Taint::Clean,
        false,
    );
    if let Some(state) = ctx.host_mut::<HostState>() {
        if let Some(entry) = state.media.players.get_mut(&id) {
            entry.source = Some(source_id);
        }
        if let Some(entry) = state.media.sources.get_mut(&source_id) {
            entry.player = Some(id);
        }
    }
    Ok(Value::Num(player))
}

fn outcome_json(outcome: &Outcome) -> String {
    let tracks: Vec<String> = outcome
        .tracks
        .iter()
        .map(|track| {
            format!(
                "{{\"kind\":\"{}\",\"id\":{},\"kindName\":\"{}\",\"label\":{},\"language\":{},\"enabled\":{}}}",
                if track.kind == crate::media::track::TrackKind::Audio { "audio" } else { "video" },
                json_string(&track.id),
                track.kind_name,
                json_string(&track.label),
                json_string(&track.language),
                track.enabled
            )
        })
        .collect();
    let init_data: Vec<String> = outcome
        .init_data
        .iter()
        .map(|(kind, data)| format!("[{},{}]", json_string(kind), bytes_json(data)))
        .collect();
    format!(
        "{{\"error\":{},\"tracks\":[{}],\"activated\":{},\"durationChanged\":{},\"metadata\":{},\"initData\":[{}]}}",
        outcome
            .append_error
            .as_deref()
            .map_or_else(|| String::from("null"), json_string),
        tracks.join(","),
        outcome.activated,
        outcome.duration_changed,
        outcome.metadata,
        init_data.join(",")
    )
}

/// Bytes as a JSON array of numbers (small EME messages and init data).
fn bytes_json(bytes: &[u8]) -> String {
    let numbers: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("[{}]", numbers.join(","))
}

/// `__eme(op, cdm, a, b, c)`: the Clear Key CDM of MediaKeys `cdm` (local
/// w3c/encrypted-media@fb9130a #clear-key). Failures come back as
/// "ExceptionName:message".
pub(super) fn eme(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    use crate::media::eme::{ClearKey, Error};
    let op = host_arg_string(ctx, args, 0);
    let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0) as u64;
    let number = |index: usize| args.get(index).and_then(Value::as_num_opt).unwrap_or(-1.0);
    let string = |text: &str| Value::from_string(text.to_string());
    let failure = |error: Error| match error {
        Error::Type(message) => format!("TypeError:{message}"),
        Error::NotSupported(message) => format!("NotSupportedError:{message}"),
        Error::InvalidState => String::from("InvalidStateError:the session is closed"),
    };
    match op.as_str() {
        "create" => {
            if !inline_media(ctx) {
                return Ok(Value::Num(-1.0));
            }
            let Some(state) = ctx.host_mut::<HostState>() else {
                return Ok(Value::Num(-1.0));
            };
            state.media.next_id += 1;
            let id = state.media.next_id;
            state.media.cdms.insert(id, ClearKey::new());
            Ok(Value::Num(id as f64))
        }
        "generate" => {
            let kind = host_arg_string(ctx, args, 2);
            let Some(data) = args
                .get(3)
                .and_then(|value| ctx.buffer_source_bytes(value, false))
            else {
                return Ok(string("TypeError:initData is not a BufferSource"));
            };
            let Some(cdm) = ctx
                .host_mut::<HostState>()
                .and_then(|state| state.media.cdms.get_mut(&id))
            else {
                return Ok(string("InvalidStateError:no CDM"));
            };
            Ok(string(&match cdm.generate_request(&kind, &data) {
                Ok((session, message)) => format!(
                    "{{\"session\":{session},\"message\":{}}}",
                    bytes_json(&message)
                ),
                Err(error) => failure(error),
            }))
        }
        "update" => {
            let session = number(2) as u32;
            let Some(data) = args
                .get(3)
                .and_then(|value| ctx.buffer_source_bytes(value, false))
            else {
                return Ok(string("TypeError:response is not a BufferSource"));
            };
            let Some(cdm) = ctx
                .host_mut::<HostState>()
                .and_then(|state| state.media.cdms.get_mut(&id))
            else {
                return Ok(string("InvalidStateError:no CDM"));
            };
            Ok(string(&match cdm.update(session, &data) {
                Ok(known) => {
                    let known: Vec<String> = known.iter().map(|kid| bytes_json(kid)).collect();
                    format!("{{\"known\":[{}]}}", known.join(","))
                }
                Err(error) => failure(error),
            }))
        }
        "close" | "remove" => {
            let session = number(2) as u32;
            if let Some(cdm) = ctx
                .host_mut::<HostState>()
                .and_then(|state| state.media.cdms.get_mut(&id))
            {
                if op == "close" {
                    cdm.close(session);
                } else {
                    cdm.remove(session);
                }
            }
            Ok(Value::Undefined)
        }
        // Decrypt the media of player `a` with this CDM's keys (`cdm` -1:
        // none).
        "attach" => {
            let player = number(2) as u64;
            let Some(state) = ctx.host_mut::<HostState>() else {
                return Ok(Value::Undefined);
            };
            let keys = state.media.cdms.get(&id).map(ClearKey::ring);
            if let Some(entry) = state.media.players.get(&player) {
                entry.player.set_keys(keys);
            }
            Ok(Value::Undefined)
        }
        _ => Ok(Value::Undefined),
    }
}

/// A JSON number, with null for the infinities and NaN.
fn json_number(value: f64) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        String::from("null")
    }
}

fn ranges_json(ranges: &[(f64, f64)]) -> String {
    let ranges: Vec<String> = ranges
        .iter()
        .map(|&(start, end)| format!("[{},{}]", json_number(start), json_number(end)))
        .collect();
    format!("[{}]", ranges.join(","))
}

/// A setter's result: whether the MediaSource reopened, and the exception.
fn setter_json(reopened: bool, result: Result<(), mse::Exception>) -> String {
    format!(
        "{{\"reopened\":{reopened},\"error\":{}}}",
        result
            .err()
            .map_or_else(|| String::from("null"), |error| json_string(error.name()))
    )
}

/// `__media_source(op, id, a, b, c)`: an operation of MediaSource `id` and
/// its SourceBuffer `a` (MSE). Exceptions come back as their names.
pub(super) fn media_source(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let op = host_arg_string(ctx, args, 0);
    let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0) as u64;
    let number = |index: usize| {
        args.get(index)
            .and_then(Value::as_num_opt)
            .unwrap_or(f64::NAN)
    };
    let flag = |index: usize| matches!(args.get(index), Some(Value::Bool(true)));
    let string = |text: &str| Value::from_string(text.to_string());
    match op.as_str() {
        "create" => {
            if !inline_media(ctx) {
                return Ok(Value::Num(-1.0));
            }
            let Some(state) = ctx.host_mut::<HostState>() else {
                return Ok(Value::Num(-1.0));
            };
            state.media.next_id += 1;
            let id = state.media.next_id;
            state.media.sources.insert(
                id,
                SourceEntry {
                    source: MediaSource::new(),
                    player: None,
                },
            );
            return Ok(Value::Num(id as f64));
        }
        "stage" => {
            // Web IDL BufferSource conversion and its copy of the bytes.
            let bytes = args
                .get(2)
                .and_then(|value| ctx.buffer_source_bytes(value, false));
            let length = bytes.as_ref().map_or(-1.0, |bytes| bytes.len() as f64);
            if let Some(state) = ctx.host_mut::<HostState>() {
                state.media.staged = bytes;
            }
            return Ok(Value::Num(length));
        }
        _ => {}
    }
    let (text, mime) = (host_arg_string(ctx, args, 3), host_arg_string(ctx, args, 2));
    let buffer = number(2) as u32;
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Ok(Value::Undefined);
    };
    // The playback position of the attached element, for eviction.
    let position = state
        .media
        .sources
        .get(&id)
        .and_then(|entry| entry.player)
        .and_then(|player| state.media.players.get(&player))
        .map_or(0, |entry| entry.player.current_time());
    let staged = if op == "append" {
        state.media.staged.take()
    } else {
        None
    };
    let Some(entry) = state.media.sources.get_mut(&id) else {
        return Ok(Value::Undefined);
    };
    let source = &entry.source;
    if logging()
        && !matches!(
            op.as_str(),
            "state"
                | "duration"
                | "buffered"
                | "mode"
                | "offset"
                | "window"
                | "parsingMedia"
                | "liveRange"
        )
    {
        media_log!(
            "MediaSource {id}: {op} {} {mime} {text}",
            args.get(2).and_then(Value::as_num_opt).unwrap_or(f64::NAN)
        );
    }
    let value = match op.as_str() {
        "state" => string(source.ready_state().as_str()),
        "duration" => Value::Num(source.duration()),
        "setDuration" => match source.set_duration(number(2)) {
            Ok(outcome) => string(&outcome_json(&outcome)),
            Err(error) => string(error.name()),
        },
        "detach" => {
            source.detach();
            entry.player = None;
            Value::Undefined
        }
        "reopen" => Value::Bool(source.reopen()),
        "add" => match source.add_source_buffer(&mime, flag(3)) {
            Ok(buffer) => Value::Num(f64::from(buffer)),
            Err(error) => string(error.name()),
        },
        "removeBuffer" => string(
            source
                .remove_source_buffer(buffer)
                .err()
                .map_or("", |e| e.name()),
        ),
        "endOfStream" => match source.end_of_stream(flag(2)) {
            Ok(outcome) => string(&outcome_json(&outcome)),
            Err(error) => string(error.name()),
        },
        "live" => {
            let range = (!number(2).is_nan()).then(|| (number(2), number(3)));
            string(
                source
                    .set_live_seekable_range(range)
                    .err()
                    .map_or("", |e| e.name()),
            )
        }
        "liveRange" => string(&ranges_json(
            &source.live_seekable_range().into_iter().collect::<Vec<_>>(),
        )),
        "prepare" => string(
            source
                .prepare_append(buffer, number(3).max(0.0) as usize, position)
                .err()
                .map_or("", |e| e.name()),
        ),
        "append" => {
            let bytes = staged.unwrap_or_default();
            dump_appended(id, buffer, &bytes);
            string(source.append(buffer, &bytes).err().map_or("", |e| e.name()))
        }
        "process" => {
            let outcome = source.process(buffer);
            if let Some(reason) = &outcome.append_error {
                media_log!("MediaSource {id}: SourceBuffer {buffer}: append error: {reason}");
            } else if logging() {
                media_log!(
                    "MediaSource {id}: SourceBuffer {buffer}: buffered {:?}",
                    source.buffered(buffer)
                );
            }
            string(&outcome_json(&outcome))
        }
        "abort" => string(&outcome_json(&source.abort(buffer))),
        "remove" => {
            source.remove(buffer, number(3), number(4));
            Value::Undefined
        }
        "buffered" => {
            let ranges = if number(2) < 0.0 {
                source.element_buffered()
            } else {
                source.buffered(buffer)
            };
            string(&ranges_json(&ranges))
        }
        "mode" => string(source.mode(buffer).as_str()),
        "setMode" => {
            let mode = if text == "sequence" {
                AppendMode::Sequence
            } else {
                AppendMode::Segments
            };
            let (reopened, result) = source.set_mode(buffer, mode);
            string(&setter_json(reopened, result))
        }
        "offset" => Value::Num(source.timestamp_offset(buffer)),
        "setOffset" => {
            let (reopened, result) = source.set_timestamp_offset(buffer, number(3));
            string(&setter_json(reopened, result))
        }
        "window" => {
            let (start, end) = source.append_window(buffer);
            string(&format!("[{},{}]", json_number(start), json_number(end)))
        }
        "setWindow" => {
            source.set_append_window(buffer, number(3), number(4));
            Value::Undefined
        }
        "changeType" => match source.change_type(buffer, &text) {
            Ok((reopened, outcome)) => string(&format!(
                "{{\"reopened\":{reopened},\"outcome\":{}}}",
                outcome_json(&outcome)
            )),
            Err(error) => string(error.name()),
        },
        "parsingMedia" => Value::Bool(source.parsing_media(buffer)),
        "active" => {
            source.set_active(buffer, flag(3));
            Value::Undefined
        }
        _ => Value::Undefined,
    };
    Ok(value)
}

/// Create the player for element `node` reading `source`, and its
/// presentation; returns the player id.
#[allow(clippy::too_many_arguments)]
fn start(
    ctx: &mut Ctx,
    node: usize,
    id: u64,
    calling: u64,
    input: Input,
    sender: TrustJsTaskSender,
    taint: Taint,
    deferred: bool,
) -> f64 {
    let player_sender = std::sync::Mutex::new(sender);
    let context = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        ctx.host_mut::<HostState>()
            .and_then(|state| super::node_window_context(state, &dom, node))
            .unwrap_or(calling)
    };
    let (silent, tap, mute) =
        ctx.host_mut::<HostState>()
            .map_or((false, None, Default::default()), |state| {
                (
                    state.media.silent,
                    state.media.taps.get(&node).cloned(),
                    crate::media::audio::Mute::new(state.audio_muted_at(node, context)),
                )
            });
    if let (Some(tap), Some(opaque)) = (&tap, taint.opaque()) {
        tap.set_opaque(opaque);
    }
    let player = if deferred {
        Player::deferred
    } else {
        Player::new
    };
    let player = player(
        input,
        Options {
            audio: match tap {
                Some(tap) => AudioMode::Tap(tap),
                None if silent => AudioMode::Silent,
                None => AudioMode::Device(mute.clone()),
            },
        },
        move |event| {
            if let Ok(sender) = player_sender.lock() {
                let _ = sender.send(TrustJsHostTask::Media {
                    id,
                    task: MediaTask::Player(event),
                });
            }
        },
    );
    let surface = player.surface().id();
    {
        let dom = host_dom(ctx);
        dom.borrow_mut().set_media_presentation(
            node,
            Some(MediaPresentation {
                surface,
                natural: None,
                show_poster: true,
                controls: None,
            }),
        );
    }
    if let Some(state) = ctx.host_mut::<HostState>() {
        // A previous player of this element is replaced (the media element
        // load algorithm aborts the old resource).
        state.media.players.retain(|_, entry| entry.node != node);
        state.media.players.insert(
            id,
            Entry {
                player,
                node,
                context,
                source: None,
                taint,
                mute,
            },
        );
    }
    id as f64
}

fn blob_bytes(ctx: &mut Ctx, url: &url::Url, context: u64) -> Option<Vec<u8>> {
    let state = ctx.host_mut::<HostState>()?;
    let origin = super::realm_origin(state, context);
    let key = super::blob_storage_key(&origin);
    let entry = state.blobs.obtain(url.as_str(), Some(&key))?;
    entry.read().ok()
}

/// `__media_control(node, id, op, a, b)`: a command for the player of
/// element `node`.
pub(super) fn control(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let node = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
    };
    let id = args.get(1).and_then(Value::as_num_opt).unwrap_or(-1.0) as u64;
    let op = host_arg_string(ctx, args, 2);
    let a = args.get(3).cloned().unwrap_or(Value::Undefined);
    let b = args.get(4).cloned().unwrap_or(Value::Undefined);
    let number = |value: &Value| value.as_num_opt().unwrap_or(0.0);
    let flag = |value: &Value| matches!(value, Value::Bool(true));
    if op == "close" {
        let removed = ctx.host_mut::<HostState>().and_then(|state| {
            let entry = state.media.players.get(&id)?;
            (Some(entry.node) == node)
                .then(|| state.media.players.remove(&id))
                .flatten()
        });
        if let Some(entry) = removed {
            // A Media Source Extensions player's MediaSource detaches with it.
            if let Some(source) = entry.source
                && let Some(state) = ctx.host_mut::<HostState>()
                && let Some(attached) = state.media.sources.get_mut(&source)
                && attached.player == Some(id)
            {
                attached.source.detach();
                attached.player = None;
            }
            drop(entry.player);
            host_dom(ctx)
                .borrow_mut()
                .set_media_presentation(entry.node, None);
        }
        return Ok(Value::Undefined);
    }
    if op == "ui" {
        // The user agent's controls: `a` holds flags (1 exposed, 2 paused,
        // 4 muted, 8 visible) and `b` "time,duration" in seconds.
        let flags = number(&a) as u32;
        let times = host_arg_string(ctx, args, 4);
        let mut parts = times
            .split(',')
            .map(|part| part.trim().parse::<f32>().unwrap_or(f32::NAN));
        let (time, duration) = (
            parts.next().unwrap_or(0.0),
            parts.next().unwrap_or(f32::NAN),
        );
        let target = ctx.host_mut::<HostState>().and_then(|state| {
            state
                .media
                .players
                .get(&id)
                .filter(|e| Some(e.node) == node)
                .map(|e| e.node)
        });
        if let Some(node) = target {
            let dom = host_dom(ctx);
            let mut dom = dom.borrow_mut();
            if let Some(mut presentation) = dom.media_presentation(node) {
                presentation.controls =
                    (flags & 1 != 0).then_some(crate::dom::media::MediaControlsState {
                        paused: flags & 2 != 0,
                        muted: flags & 4 != 0,
                        time: if time.is_finite() { time } else { 0.0 },
                        duration,
                        visible: flags & 8 != 0,
                    });
                dom.set_media_presentation(node, Some(presentation));
            }
        }
        return Ok(Value::Undefined);
    }
    if op == "poster" {
        // HTML's show poster flag changed.
        let surface = ctx.host_mut::<HostState>().and_then(|state| {
            state
                .media
                .players
                .get(&id)
                .filter(|e| Some(e.node) == node)
                .map(|e| e.node)
        });
        if let Some(node) = surface {
            let dom = host_dom(ctx);
            let mut dom = dom.borrow_mut();
            if let Some(mut presentation) = dom.media_presentation(node) {
                presentation.show_poster = flag(&a);
                dom.set_media_presentation(node, Some(presentation));
            }
        }
        return Ok(Value::Undefined);
    }
    let Some(state) = ctx.host_mut::<HostState>() else {
        return Ok(Value::Undefined);
    };
    let Some(entry) = state
        .media
        .players
        .get(&id)
        .filter(|entry| Some(entry.node) == node)
    else {
        return Ok(Value::Undefined);
    };
    let player = &entry.player;
    match op.as_str() {
        "start" => player.start(),
        "play" => player.play(),
        "pause" => player.pause(),
        "seek" => player.seek(seconds_to_micros(number(&a)), flag(&b)),
        "volume" => player.set_volume(number(&a) as f32, flag(&b)),
        "rate" => player.set_rate(number(&a), flag(&b)),
        "time" => return Ok(Value::Num(micros_to_seconds(player.current_time()))),
        "presented" => return Ok(Value::Num(player.quality().presented as f64)),
        "dropped" => return Ok(Value::Num(player.quality().dropped as f64)),
        _ => {}
    }
    Ok(Value::Undefined)
}

fn seconds_or_nan(value: Option<Micros>) -> f64 {
    value.map_or(f64::NAN, micros_to_seconds)
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
}

/// Deliver a player's report to its element.
pub(super) fn dispatch(
    engine: &mut trust_js::Engine,
    id: u64,
    task: MediaTask,
) -> Result<(), String> {
    let Some((node, context)) = engine
        .ctx()
        .host_mut::<HostState>()
        .and_then(|state| state.media.players.get(&id))
        .map(|entry| (entry.node, entry.context))
    else {
        // The element moved on to another resource.
        return Ok(());
    };
    if matches!(task, MediaTask::Player(Event::Metadata { .. }))
        && let Some(state) = engine.ctx().host_mut::<HostState>()
    {
        // The resource has answered: whether a Web Audio graph may hear it.
        state.media.update_taint(id);
    }
    let string = |text: &str| Value::from_string(text.to_string());
    if logging() {
        match &task {
            MediaTask::Player(Event::Time(_)) | MediaTask::Network(NetworkEvent::Progress) => {}
            MediaTask::Player(event) => media_log!("player {id}: {event:?}"),
            MediaTask::Network(event) => media_log!("player {id}: network {event:?}"),
        }
    }
    let mut args = vec![
        Value::Num(id as f64),
        Value::Undefined,
        Value::Undefined,
        Value::Undefined,
        Value::Undefined,
    ];
    match task {
        MediaTask::Network(event) => {
            args[1] = string("network");
            args[2] = string(match event {
                NetworkEvent::Progress => "progress",
                NetworkEvent::Suspend => "suspend",
                NetworkEvent::Stalled => "stalled",
                NetworkEvent::Complete => "complete",
            });
        }
        MediaTask::Player(event) => match event {
            Event::Metadata {
                duration,
                seekable,
                tracks,
                video_size,
            } => {
                if let Some((width, height)) = video_size {
                    set_natural_size(engine, node, width, height);
                }
                let tracks: Vec<String> = tracks
                    .iter()
                    .map(|track| {
                        format!(
                            "{{\"id\":\"{}\",\"kind\":\"{}\",\"codec\":{},\"language\":{},\"label\":{},\"active\":{}}}",
                            track.id,
                            match track.kind {
                                crate::media::track::TrackKind::Audio => "audio",
                                crate::media::track::TrackKind::Video => "video",
                                crate::media::track::TrackKind::Text => "text",
                            },
                            json_string(&track.codec),
                            json_string(&track.language),
                            json_string(&track.label),
                            track.active
                        )
                    })
                    .collect();
                let (width, height) = video_size.unwrap_or((0, 0));
                args[1] = string("metadata");
                args[2] = Value::Num(duration.map_or(f64::NAN, micros_to_seconds));
                args[3] = Value::Bool(seekable);
                args[4] = string(&format!(
                    "{{\"width\":{width},\"height\":{height},\"tracks\":[{}]}}",
                    tracks.join(",")
                ));
            }
            Event::ReadyState(state) => {
                args[1] = string("ready");
                args[2] = Value::Num(match state {
                    ReadyState::Nothing => 0.0,
                    ReadyState::Metadata => 1.0,
                    ReadyState::CurrentData => 2.0,
                    ReadyState::FutureData => 3.0,
                    ReadyState::EnoughData => 4.0,
                });
            }
            Event::Duration(duration) => {
                args[1] = string("duration");
                args[2] = Value::Num(seconds_or_nan(duration));
            }
            Event::Resize(width, height) => {
                set_natural_size(engine, node, width, height);
                args[1] = string("resize");
                args[2] = Value::Num(f64::from(width));
                args[3] = Value::Num(f64::from(height));
            }
            Event::Waiting => args[1] = string("waiting"),
            Event::Playing => args[1] = string("playing"),
            Event::SeekDone(time) => {
                args[1] = string("seeked");
                args[2] = Value::Num(micros_to_seconds(time));
            }
            Event::Time(time) => {
                args[1] = string("time");
                args[2] = Value::Num(micros_to_seconds(time));
            }
            Event::Ended => args[1] = string("ended"),
            Event::WaitingForKey => args[1] = string("waitingforkey"),
            Event::Error(kind, message) => {
                args[1] = string("error");
                args[2] = Value::Num(match kind {
                    ErrorKind::Network => 2.0,
                    ErrorKind::Decode => 3.0,
                    ErrorKind::SrcNotSupported => 4.0,
                });
                args[3] = string(&message);
            }
        },
    }
    super::run_in_context_realm(engine, context, "media task", move |engine| {
        match super::host_call_trust_interruptible(engine.ctx(), "mediaEvent", &args) {
            Ok(_) => Ok(()),
            Err(error) => super::host_task_failure(engine.ctx(), error, "media task"),
        }
    })
}

fn set_natural_size(engine: &mut trust_js::Engine, node: usize, width: u32, height: u32) {
    let dom = host_dom(engine.ctx());
    let mut dom = dom.borrow_mut();
    if let Some(mut presentation) = dom.media_presentation(node) {
        presentation.natural = Some((width, height)).filter(|&(w, h)| w > 0 && h > 0);
        dom.set_media_presentation(node, Some(presentation));
    }
}
