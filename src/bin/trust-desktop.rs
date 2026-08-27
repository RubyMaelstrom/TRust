// Native TRust desktop frontend: winit events → shared browser controller →
// renderer-neutral scene → selected Vello backend → native surface.

use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use accesskit::{
    Action as AccessAction, ActionData, ActionRequest, Affine, Node as AccessNode,
    NodeId as AccessNodeId, Rect as AccessRect, Role as AccessRole, Tree, TreeId, TreeUpdate,
    Vec2 as AccessVec2,
};
use accesskit_winit::{Adapter as AccessAdapter, Event as AccessEvent};
use softbuffer::{Context, Surface};
use tokio::runtime::Handle;
use trust::accessibility::{Role as SemanticRole, SemanticTree};
use trust::core::{
    BrowserController, ButtonState, CssPoint, CssSize, FetchedDocument, ImeAction, Key, KeyInput,
    KeyState, Modifiers, PhysicalSize, PointerButton, ScaleFactor, UserAction, ViewportMetrics,
};
use trust::doc::{FieldKind, Link};
use trust::render::vello_cpu::VelloCpuRenderer;
use trust::render::vello_hybrid::{PresentOutcome, VelloHybridRenderer};
use trust::render::{
    ChromeModel, ControlId, CssRect, DisplayCommand, EditorVisual, HeartVisual, ImageHandle,
    ImageResource, ImageStore, PageHit, PaintBrush, PaintColor, PaintShape, RasterBackend,
    RendererKind, RendererPreference, Scene, SceneDamage, ScrollContainer, ScrollbarAxis,
    StrokeStyle, TextSelection, desktop_chrome, desktop_heart_image_handle, horizontal_heart_track,
    paint_desktop_overlay, paint_text_editor, raster_damage, scene_damage, scrollbar_fraction,
    scrollbar_position, scrollbar_track_fraction, vertical_heart_track,
};
use trust::text::{TextEditor, TextStyle};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{
    ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy, OwnedDisplayHandle,
};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::window::{CursorIcon, CustomCursor, Window, WindowId};

const PAGE_ACCESS_BASE: u64 = 10_000;
const ACCESS_ROOT: AccessNodeId = AccessNodeId(0);
const ACCESS_FIND: AccessNodeId = AccessNodeId(6);
const ACCESS_COMMAND: AccessNodeId = AccessNodeId(7);
const IMAGE_FETCH_CONCURRENCY: usize = 8;
/// Encoded animation sources are much smaller than decoded frame sequences,
/// but remain attacker-controlled cache data. Keep this document-local pool
/// bounded independently of the 256 MiB current-frame `ImageStore`.
const MAX_ANIMATION_SOURCES: usize = 512;
const MAX_ANIMATION_SOURCE_BYTES: usize = 256 * 1024 * 1024;
const MAX_ACTIVE_ANIMATIONS: usize = 128;
const MAX_ACTIVE_ANIMATION_WORKING_BYTES: usize = 256 * 1024 * 1024;
/// Explicit image-animation presentation ceiling. GIF89a permits zero-delay
/// frames and delegates timing policy to the decoder; a 100 fps ceiling keeps
/// hostile streams bounded while preserving all ordinarily authored motion.
const MAX_IMAGE_ANIMATION_FPS: u32 = 100;
/// GIF permits a zero delay, APNG allows viewers to impose a reasonable lower
/// bound, and WebP explicitly makes zero/very-short duration handling an
/// implementation choice. Ten milliseconds preserves 100 fps content while
/// preventing a malformed zero-delay stream from becoming an idle spin.
const MIN_ANIMATION_FRAME_DELAY: Duration =
    Duration::from_millis(1_000 / MAX_IMAGE_ANIMATION_FPS as u64);
const CSS_ANIMATION_FRAME_DELAY: Duration = Duration::from_nanos(16_666_667);
/// Preserve absolute frame deadlines (PNG 3 §11.3.6.2) but cap one overdue
/// catch-up burst so an expensive/corrupt animation cannot monopolize the one
/// shared worker and starve other visible images.
const MAX_ANIMATION_CATCH_UP_FRAMES: usize = 8;
/// Coalesce a slow image stream into an occasional progressive relayout. A
/// fast page bypasses this timer and relayouts as soon as its whole request set
/// reaches available/broken state. Full CSS layout can be substantially more
/// expensive than fetching another bounded network wave, so this fallback must
/// never interrupt an ordinary gallery burst every few hundred milliseconds.
const IMAGE_PROGRESS_RELAYOUT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
/// Internal `ImageSizes` sentinel. Layout turns it into HTML's 300×150 CSS
/// default object size without applying a responsive candidate's density; no
/// decodable image can reach this pair (the decoder caps each axis at 12,000).
const PENDING_IMAGE_SIZE: (u32, u32) = (u32::MAX, u32::MAX);
const IDLE_HEART_PNG: &[u8] = include_bytes!("../assets/IdleHeart30.png");
const ACTIVE_HEART_PNG: &[u8] = include_bytes!("../assets/ActiveHeart30.png");
static HEART_ASSETS: OnceLock<Result<[(ImageHandle, ImageResource); 2], String>> = OnceLock::new();

fn embedded_heart_assets() -> &'static Result<[(ImageHandle, ImageResource); 2], String> {
    HEART_ASSETS.get_or_init(|| {
        // PNG Third Edition §6.2 defines PNG alpha as non-associated. TRust's
        // graphical decoder preserves that straight RGBA representation until
        // each Vello backend performs its one retained upload conversion.
        // https://www.w3.org/TR/png-3/#6AlphaRepresentation
        let idle = trust::img::decode_graphical(IDLE_HEART_PNG)?;
        let active = trust::img::decode_graphical(ACTIVE_HEART_PNG)?;
        if [(&idle, "idle"), (&active, "active")]
            .iter()
            .any(|(image, _)| image.width != 30 || image.height != 30 || !image.has_alpha)
        {
            return Err(String::from(
                "embedded heart assets must be 30×30 PNGs with alpha",
            ));
        }
        Ok([
            (desktop_heart_image_handle(false), idle),
            (desktop_heart_image_handle(true), active),
        ])
    })
}

fn ensure_embedded_heart_assets(store: &ImageStore) -> Result<(), String> {
    let assets = embedded_heart_assets()
        .as_ref()
        .map_err(std::clone::Clone::clone)?;
    for (handle, image) in assets {
        if !store.contains(*handle) {
            store.insert(*handle, image.clone());
        }
    }
    Ok(())
}

#[derive(Debug)]
enum DesktopEvent {
    BrowserWake,
    ChromeTick,
    ImageLoaded {
        generation: u64,
        image_epoch: u64,
        handle: ImageHandle,
        source: String,
        result: Result<trust::img::DecodedGraphicalImage, String>,
    },
    ImagesReady {
        generation: u64,
        image_epoch: u64,
    },
    AnimationWake,
    Access(AccessEvent),
    Telnet(trust::telnet::Event),
}

#[derive(Default)]
struct AnimationSourceStore {
    entries: HashMap<ImageHandle, trust::img::GraphicalAnimation>,
    order: VecDeque<ImageHandle>,
    bytes: usize,
}

impl AnimationSourceStore {
    fn insert(
        &mut self,
        handle: ImageHandle,
        source: trust::img::GraphicalAnimation,
    ) -> Vec<ImageHandle> {
        if let Some(old) = self.entries.remove(&handle) {
            self.bytes = self.bytes.saturating_sub(old.encoded_len());
            self.order.retain(|candidate| *candidate != handle);
        }
        self.bytes = self.bytes.saturating_add(source.encoded_len());
        self.entries.insert(handle, source);
        self.order.push_back(handle);

        let mut evicted = Vec::new();
        while self.entries.len() > MAX_ANIMATION_SOURCES || self.bytes > MAX_ANIMATION_SOURCE_BYTES
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(old.encoded_len());
                evicted.push(oldest);
            }
        }
        evicted
    }

    fn contains(&self, handle: ImageHandle) -> bool {
        self.entries.contains_key(&handle)
    }

    fn remove(&mut self, handle: ImageHandle) -> Option<trust::img::GraphicalAnimation> {
        let source = self.entries.remove(&handle)?;
        self.bytes = self.bytes.saturating_sub(source.encoded_len());
        self.order.retain(|candidate| *candidate != handle);
        Some(source)
    }

    fn touch_visible(&mut self, handles: &HashSet<ImageHandle>) {
        let mut handles: Vec<_> = handles
            .iter()
            .copied()
            .filter(|handle| self.entries.contains_key(handle))
            .collect();
        handles.sort_unstable_by_key(|handle| handle.0);
        for handle in handles {
            self.order.retain(|candidate| *candidate != handle);
            self.order.push_back(handle);
        }
    }

    fn bounded_active_handles(
        &self,
        visible: &HashSet<ImageHandle>,
        images: &ImageStore,
    ) -> HashSet<ImageHandle> {
        let mut active = HashSet::new();
        let mut working_bytes = 0usize;
        for handle in self.order.iter().rev().copied() {
            let Some(source) = self.entries.get(&handle) else {
                continue;
            };
            let bytes = source.decoder_working_set_bytes();
            if visible.contains(&handle)
                && images.contains(handle)
                && active.len() < MAX_ACTIVE_ANIMATIONS
                && working_bytes.saturating_add(bytes) <= MAX_ACTIVE_ANIMATION_WORKING_BYTES
            {
                working_bytes = working_bytes.saturating_add(bytes);
                active.insert(handle);
            }
        }
        active
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

enum AnimationCommand {
    Reset {
        generation: u64,
        image_epoch: u64,
    },
    Register {
        generation: u64,
        image_epoch: u64,
        handle: ImageHandle,
        source: trust::img::GraphicalAnimation,
    },
    Unregister(ImageHandle),
    SetActive {
        generation: u64,
        image_epoch: u64,
        handles: HashSet<ImageHandle>,
    },
    Shutdown,
}

struct AnimationUpdate {
    generation: u64,
    image_epoch: u64,
    handle: ImageHandle,
    result: Result<ImageResource, String>,
}

#[derive(Default)]
struct AnimationOutput {
    latest: HashMap<ImageHandle, AnimationUpdate>,
    wake_pending: bool,
}

struct AnimationPlayer {
    commands: mpsc::Sender<AnimationCommand>,
    output: Arc<Mutex<AnimationOutput>>,
    thread: Option<JoinHandle<()>>,
}

impl AnimationPlayer {
    fn new(proxy: EventLoopProxy<DesktopEvent>) -> Self {
        let (commands, receiver) = mpsc::channel();
        let output = Arc::new(Mutex::new(AnimationOutput::default()));
        let worker_output = Arc::clone(&output);
        let thread = std::thread::Builder::new()
            .name(String::from("trust-image-animation"))
            .spawn(move || animation_worker(receiver, worker_output, proxy))
            .ok();
        Self {
            commands,
            output,
            thread,
        }
    }

    fn reset(&self, generation: u64, image_epoch: u64) {
        let _ = self.commands.send(AnimationCommand::Reset {
            generation,
            image_epoch,
        });
    }

    fn register(
        &self,
        generation: u64,
        image_epoch: u64,
        handle: ImageHandle,
        source: trust::img::GraphicalAnimation,
    ) {
        let _ = self.commands.send(AnimationCommand::Register {
            generation,
            image_epoch,
            handle,
            source,
        });
    }

    fn unregister(&self, handle: ImageHandle) {
        let _ = self.commands.send(AnimationCommand::Unregister(handle));
    }

    fn set_active(&self, generation: u64, image_epoch: u64, handles: HashSet<ImageHandle>) {
        let _ = self.commands.send(AnimationCommand::SetActive {
            generation,
            image_epoch,
            handles,
        });
    }

    fn drain(&self) -> Vec<AnimationUpdate> {
        let mut output = self.output.lock().expect("animation output poisoned");
        output.wake_pending = false;
        output.latest.drain().map(|(_, update)| update).collect()
    }
}

impl Drop for AnimationPlayer {
    fn drop(&mut self) {
        let _ = self.commands.send(AnimationCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct WorkerAnimation {
    source: trust::img::GraphicalAnimation,
    decoder: Option<trust::img::GraphicalAnimationDecoder>,
    loop_count: Option<trust::img::AnimationLoopCount>,
    plays_completed: u32,
    deadline: Option<Instant>,
    active: bool,
    started_once: bool,
    finished: bool,
    last_delay: Duration,
}

impl WorkerAnimation {
    fn new(source: trust::img::GraphicalAnimation) -> Self {
        Self {
            source,
            decoder: None,
            loop_count: None,
            plays_completed: 0,
            deadline: None,
            active: false,
            started_once: false,
            finished: false,
            last_delay: MIN_ANIMATION_FRAME_DELAY,
        }
    }

    fn can_start_play(&self) -> bool {
        match self.loop_count {
            None | Some(trust::img::AnimationLoopCount::Infinite) => true,
            Some(trust::img::AnimationLoopCount::Finite(count)) => self.plays_completed < count,
        }
    }

    fn start_play(&mut self, now: Instant) -> Result<Option<ImageResource>, String> {
        if !self.can_start_play() {
            self.finished = true;
            self.deadline = None;
            return Ok(None);
        }
        let mut decoder = self.source.decoder()?;
        if self.loop_count.is_none() {
            self.loop_count = Some(decoder.loop_count());
        }
        if !self.can_start_play() {
            self.finished = true;
            return Ok(None);
        }
        let Some(first) = decoder.next_frame()? else {
            self.finished = true;
            return Ok(None);
        };
        self.last_delay = animation_frame_delay(first.delay);
        self.deadline = Some(now + self.last_delay);
        self.decoder = Some(decoder);
        let publish = self.started_once.then_some(first.image);
        self.started_once = true;
        Ok(publish)
    }

    fn activate(&mut self, now: Instant) -> Result<Option<ImageResource>, String> {
        if self.active || self.finished {
            return Ok(None);
        }
        self.active = true;
        self.start_play(now)
    }

    fn deactivate(&mut self) {
        self.active = false;
        self.deadline = None;
        // Sequential decoders retain logical-canvas buffers. Releasing them is
        // what keeps a long, image-heavy document cheap while it is off-screen.
        self.decoder = None;
    }

    fn advance(&mut self, now: Instant) -> Result<Option<ImageResource>, String> {
        let frame_started = self.deadline.unwrap_or(now);
        let next = match self.decoder.as_mut() {
            Some(decoder) => decoder.next_frame()?,
            None => return Ok(None),
        };
        if let Some(frame) = next {
            self.last_delay = animation_frame_delay(frame.delay);
            self.deadline = self.deadline.map(|deadline| deadline + self.last_delay);
            return Ok(Some(frame.image));
        }
        self.decoder = None;
        self.plays_completed = self.plays_completed.saturating_add(1);
        self.start_play(frame_started)
    }
}

fn animation_frame_delay(delay: Duration) -> Duration {
    delay.max(MIN_ANIMATION_FRAME_DELAY)
}

fn publish_animation_update(
    output: &Arc<Mutex<AnimationOutput>>,
    proxy: &EventLoopProxy<DesktopEvent>,
    update: AnimationUpdate,
) -> bool {
    let wake = {
        let mut output = output.lock().expect("animation output poisoned");
        output.latest.insert(update.handle, update);
        if output.wake_pending {
            false
        } else {
            output.wake_pending = true;
            true
        }
    };
    !wake || proxy.send_event(DesktopEvent::AnimationWake).is_ok()
}

fn animation_worker(
    commands: mpsc::Receiver<AnimationCommand>,
    output: Arc<Mutex<AnimationOutput>>,
    proxy: EventLoopProxy<DesktopEvent>,
) {
    let mut generation = 0;
    let mut image_epoch = 0;
    let mut animations: HashMap<ImageHandle, WorkerAnimation> = HashMap::new();

    loop {
        let now = Instant::now();
        let due: Vec<_> = animations
            .iter()
            .filter_map(|(handle, animation)| {
                animation
                    .deadline
                    .filter(|deadline| *deadline <= now)
                    .map(|_| *handle)
            })
            .collect();
        for handle in due {
            let Some(animation) = animations.get_mut(&handle) else {
                continue;
            };
            let mut latest = None;
            let mut error = None;
            for _ in 0..MAX_ANIMATION_CATCH_UP_FRAMES {
                if animation.deadline.is_none_or(|deadline| deadline > now) {
                    break;
                }
                match animation.advance(now) {
                    Ok(frame) => latest = frame.or(latest),
                    Err(reason) => {
                        animation.finished = true;
                        animation.deactivate();
                        error = Some(reason);
                        break;
                    }
                }
            }
            if animation.deadline.is_some_and(|deadline| deadline <= now) {
                // We decoded enough to preserve disposal state but are still
                // behind. Re-anchor rather than spin through an unbounded
                // backlog; subsequent frame spacing remains source-accurate.
                animation.deadline = Some(now + animation.last_delay);
            }
            let result = error.map_or_else(|| latest.map(Ok), |reason| Some(Err(reason)));
            if let Some(result) = result
                && !publish_animation_update(
                    &output,
                    &proxy,
                    AnimationUpdate {
                        generation,
                        image_epoch,
                        handle,
                        result,
                    },
                )
            {
                return;
            }
        }

        let timeout = animations
            .values()
            .filter_map(|animation| animation.deadline)
            .min()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let command = match timeout {
            Some(timeout) => match commands.recv_timeout(timeout) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            },
            None => match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            },
        };
        let Some(command) = command else {
            continue;
        };
        match command {
            AnimationCommand::Reset {
                generation: next_generation,
                image_epoch: next_epoch,
            } => {
                generation = next_generation;
                image_epoch = next_epoch;
                animations.clear();
                let mut pending = output.lock().expect("animation output poisoned");
                pending.latest.clear();
            }
            AnimationCommand::Register {
                generation: candidate_generation,
                image_epoch: candidate_epoch,
                handle,
                source,
            } if candidate_generation == generation && candidate_epoch == image_epoch => {
                animations.insert(handle, WorkerAnimation::new(source));
            }
            AnimationCommand::Register { .. } => {}
            AnimationCommand::Unregister(handle) => {
                animations.remove(&handle);
            }
            AnimationCommand::SetActive {
                generation: candidate_generation,
                image_epoch: candidate_epoch,
                handles,
            } if candidate_generation == generation && candidate_epoch == image_epoch => {
                let now = Instant::now();
                for (handle, animation) in &mut animations {
                    if handles.contains(handle) {
                        let result = match animation.activate(now) {
                            Ok(Some(image)) => Some(Ok(image)),
                            Ok(None) => None,
                            Err(reason) => Some(Err(reason)),
                        };
                        if let Some(result) = result
                            && !publish_animation_update(
                                &output,
                                &proxy,
                                AnimationUpdate {
                                    generation,
                                    image_epoch,
                                    handle: *handle,
                                    result,
                                },
                            )
                        {
                            return;
                        }
                    } else {
                        animation.deactivate();
                    }
                }
            }
            AnimationCommand::SetActive { .. } => {}
            AnimationCommand::Shutdown => return,
        }
    }
}

impl From<AccessEvent> for DesktopEvent {
    fn from(event: AccessEvent) -> Self {
        Self::Access(event)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FocusTarget {
    Page,
    Find,
    #[default]
    Command,
    Form {
        form: usize,
        field: usize,
    },
}

/// A key whose DOM `keydown` is running in the resident page. The native
/// editing/default action waits for the actor's `PageEvt::KeyDefault`, so page
/// script can cancel Enter for rich editors and chat composers.
#[derive(Clone, Debug)]
struct PendingPageKey {
    form: usize,
    field: usize,
    input: KeyInput,
}

struct PageLayoutCache {
    generation: u64,
    revision: u64,
    rendered_revision: u64,
    viewport: CssSize,
    device_pixel_ratio: f32,
    /// The actor and resource work were explicitly retired. Keep the last
    /// complete display list even if navigation subsequently fails.
    frozen: bool,
    document: DesktopPageAdapter,
    layout: std::sync::Arc<trust::layout2::PixelLayout>,
}

#[allow(clippy::too_many_arguments)]
fn may_reuse_page_layout(
    cache_generation: u64,
    document_generation: u64,
    cache_rendered_revision: u64,
    rendered_revision: u64,
    cache_viewport: CssSize,
    viewport: CssSize,
    cache_device_pixel_ratio: f32,
    device_pixel_ratio: f32,
    page_is_live: bool,
    navigation_pending: bool,
    frozen: bool,
) -> bool {
    cache_generation == document_generation
        && (navigation_pending
            || frozen
            || (cache_rendered_revision == rendered_revision
                && (page_is_live
                    || (cache_viewport == viewport
                        && cache_device_pixel_ratio == device_pixel_ratio))))
}

/// Native interaction facts paired with the canonical actor's pixel layout.
/// This deliberately has no DOM, selector engine, cascade, or layout entry
/// point: desktop is an adapter over `RenderedPage`, not a second browser.
struct DesktopPageAdapter {
    base: url::Url,
    forms: Vec<trust::doc::Form>,
    controls: trust::layout2::ControlMap,
    lazy_image_handles: HashSet<ImageHandle>,
    parents: HashMap<usize, usize>,
    fragment_y: HashMap<String, f32>,
    semantics: SemanticTree,
}

impl DesktopPageAdapter {
    fn from_rendered(
        base: url::Url,
        rendered: trust::http::RenderedPage,
    ) -> (Self, std::sync::Arc<trust::layout2::PixelLayout>) {
        let layout = rendered.layout.clone();
        (
            Self {
                base,
                forms: rendered.forms,
                controls: rendered.controls,
                lazy_image_handles: rendered.lazy_image_handles,
                parents: rendered.parents,
                fragment_y: rendered.fragment_y,
                semantics: rendered.semantics,
            },
            layout,
        )
    }

    fn is_inclusive_ancestor(&self, ancestor: usize, mut node: usize) -> bool {
        loop {
            if node == ancestor {
                return true;
            }
            let Some(parent) = self.parents.get(&node).copied() else {
                return false;
            };
            node = parent;
        }
    }
}

fn form_target_for_node(
    mut node: usize,
    controls: &trust::layout2::ControlMap,
    parents: &HashMap<usize, usize>,
) -> Option<(usize, usize)> {
    loop {
        if let Some(target) = controls.get(&node).copied() {
            return Some(target);
        }
        let parent = parents.get(&node).copied()?;
        if parent == node {
            return None;
        }
        node = parent;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ImageScheduleKey {
    generation: u64,
    revision: u64,
    scroll: CssPoint,
    viewport: CssSize,
}

#[derive(Clone, Copy, Debug)]
struct HeartDrag {
    axis: ScrollbarAxis,
    pointer_offset: f32,
}

#[derive(Clone, Copy, Debug)]
struct HeartGlide {
    from: f32,
    to: f32,
    started: Instant,
}

struct ProtocolPageCache {
    generation: u64,
    viewport: CssSize,
    document: trust::doc::Doc,
    layout: trust::render::documents::ProtocolPaint,
    /// Parsed line index highlighted by the Gopherus keyboard model. This is
    /// present only for Gopher/Gemini; one-shot text protocols remain plain.
    selected: Option<usize>,
}

/// Per-document image request state. The HTML image processing model keeps a
/// request available/broken once it reaches that state; this scheduler mirrors
/// that lifecycle while bounding the native frontend's network/decode work.
#[derive(Default)]
struct ImageLoadScheduler {
    generation: Option<u64>,
    page: Option<String>,
    epoch: u64,
    queued: VecDeque<trust::render::ImageRequest>,
    pending: HashSet<ImageHandle>,
    failed: HashSet<ImageHandle>,
    completed: HashSet<ImageHandle>,
    retired: bool,
}

impl ImageLoadScheduler {
    fn reset(&mut self, generation: u64, page: &url::Url) -> bool {
        if self.generation == Some(generation) && self.page.as_deref() == Some(page.as_str()) {
            return false;
        }
        self.generation = Some(generation);
        self.page = Some(page.to_string());
        self.epoch = self.epoch.wrapping_add(1);
        self.queued.clear();
        self.pending.clear();
        self.failed.clear();
        self.completed.clear();
        self.retired = false;
        true
    }

    fn retire(&mut self) -> bool {
        if self.retired {
            return false;
        }
        self.retired = true;
        self.epoch = self.epoch.wrapping_add(1);
        self.queued.clear();
        self.pending.clear();
        true
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn accepts(&self, generation: u64, image_epoch: u64) -> bool {
        !self.retired && self.generation == Some(generation) && self.epoch == image_epoch
    }

    fn enqueue(&mut self, request: trust::render::ImageRequest) {
        if self.retired {
            return;
        }
        if self.pending.contains(&request.handle)
            || self.failed.contains(&request.handle)
            || self.completed.contains(&request.handle)
        {
            return;
        }
        self.pending.insert(request.handle);
        self.queued.push_back(request);
    }

    fn take_ready(&mut self, slots: usize) -> Vec<trust::render::ImageRequest> {
        if self.retired {
            return Vec::new();
        }
        (0..slots).filter_map(|_| self.queued.pop_front()).collect()
    }

    fn finish(&mut self, handle: ImageHandle, success: bool) -> bool {
        if !self.pending.remove(&handle) {
            return false;
        }
        if success {
            self.completed.insert(handle);
        } else {
            self.failed.insert(handle);
        }
        true
    }

    fn mark_cached(&mut self, handle: ImageHandle) -> bool {
        self.failed.remove(&handle);
        self.pending.remove(&handle);
        self.completed.insert(handle)
    }

    fn retry_evicted(&mut self, handle: ImageHandle) {
        self.completed.remove(&handle);
    }
}

fn apply_graphical_scroll_state(
    layout: &mut std::sync::Arc<trust::layout2::PixelLayout>,
    interaction: &trust::core::InteractionState,
) {
    if interaction.nested_scroll.is_empty() {
        return;
    }
    let layout = std::sync::Arc::make_mut(layout);
    for container in &mut layout.paint.scroll_containers {
        if let Some(offset) = container
            .actor
            .and_then(|actor| interaction.nested_scroll.get(&actor))
        {
            container.offset = *offset;
        }
    }
}

/// Actor ids of graphical boundaries currently represented by retained
/// geometry. Scroll containers are Tier `Size`, independent formatting
/// contexts are Tier `WidthStable`, and paint-only hover selector subjects are
/// Tier `Paint`. Publishing only laid, correlated nodes is the graphical
/// counterpart of terminal `Doc.regions`/`Doc.boundaries`.
fn graphical_live_boundaries(layout: &trust::layout2::GraphicalLayout) -> (Vec<usize>, Vec<usize>) {
    let regions: Vec<usize> = layout
        .paint
        .scroll_containers
        .iter()
        .filter_map(|container| container.actor)
        .collect();
    let mut boundaries = layout
        .patch_boundaries
        .iter()
        .map(|boundary| boundary.actor)
        .chain(
            layout
                .paint_boundaries
                .iter()
                .map(|boundary| boundary.actor),
        )
        .collect::<Vec<_>>();
    boundaries.sort_unstable();
    boundaries.dedup();
    (regions, boundaries)
}

fn image_sizes_for_layout(
    decoded: &trust::layout2::ImageSizes,
    failed: &HashSet<ImageHandle>,
    sources: &[String],
) -> trust::layout2::ImageSizes {
    let mut sizes = decoded.clone();
    for source in sources {
        let handle = ImageHandle::for_source(source);
        if !failed.contains(&handle) {
            // HTML Rendering: while the UA expects an image to become
            // available, it remains a replaced element. CSS 2.2 §10.3.2's
            // no-intrinsic default object size is 300×150 CSS px. This avoids
            // laying a gallery's accessibility text out as page content while
            // its image requests are merely pending. A failed request is
            // deliberately omitted so the canonical alt-text path takes over.
            sizes.entry(source.clone()).or_insert(PENDING_IMAGE_SIZE);
        }
    }
    sizes
}

/// HTML lazy-image scheduling with a two-viewport look-ahead. Eager resources
/// keep their existing immediate path; lazy resources enter the queue shortly
/// before they can be painted. The returned visible set is also the only set
/// allowed to retry after decoded-cache eviction, preventing an off-screen
/// gallery from cycling forever through the bounded LRU.
fn scheduled_page_images(
    page: &PageLayoutCache,
    scroll: CssPoint,
    viewport: CssSize,
) -> (Vec<trust::render::ImageRequest>, HashSet<ImageHandle>) {
    let band = CssRect::new(
        scroll.x - viewport.width,
        scroll.y - viewport.height * 2.0,
        viewport.width * 3.0,
        viewport.height * 5.0,
    );
    let mut visible = HashSet::new();
    collect_visible_image_handles(&page.layout.paint.primitives, band, &mut visible);
    // Fixed-position image commands are viewport-relative. They are cheap and
    // necessarily near the user whenever their layer is active.
    for command in page
        .layout
        .paint
        .fixed_under_primitives
        .iter()
        .chain(page.layout.paint.fixed_primitives.iter())
        .chain(
            page.layout
                .paint
                .top_layer
                .iter()
                .flat_map(|entry| entry.primitives.iter()),
        )
    {
        if let trust::render::DisplayCommand::Image { handle, .. } = command {
            visible.insert(*handle);
        }
    }
    let requests = page
        .layout
        .paint
        .image_requests
        .iter()
        .filter(|request| {
            !page.document.lazy_image_handles.contains(&request.handle)
                || visible.contains(&request.handle)
        })
        .cloned()
        .collect();
    (requests, visible)
}

fn collect_visible_image_handles(
    commands: &[trust::render::DisplayCommand],
    band: CssRect,
    visible: &mut HashSet<ImageHandle>,
) {
    let mut transform = trust::render::Affine2d::IDENTITY;
    let mut stack = Vec::new();
    for command in commands {
        match command {
            trust::render::DisplayCommand::PushTransform(next) => {
                stack.push(transform);
                transform = transform.then(*next);
            }
            trust::render::DisplayCommand::PopTransform => {
                transform = stack.pop().unwrap_or(trust::render::Affine2d::IDENTITY);
            }
            trust::render::DisplayCommand::Image { rect, handle, .. } => {
                let corners = [
                    CssPoint::new(rect.x, rect.y),
                    CssPoint::new(rect.x + rect.width, rect.y),
                    CssPoint::new(rect.x, rect.y + rect.height),
                    CssPoint::new(rect.x + rect.width, rect.y + rect.height),
                ]
                .map(|point| transform.map_point(point));
                let left = corners
                    .iter()
                    .map(|point| point.x)
                    .fold(f32::INFINITY, f32::min);
                let right = corners
                    .iter()
                    .map(|point| point.x)
                    .fold(f32::NEG_INFINITY, f32::max);
                let top = corners
                    .iter()
                    .map(|point| point.y)
                    .fold(f32::INFINITY, f32::min);
                let bottom = corners
                    .iter()
                    .map(|point| point.y)
                    .fold(f32::NEG_INFINITY, f32::max);
                if left < band.x + band.width
                    && right > band.x
                    && top < band.y + band.height
                    && bottom > band.y
                {
                    visible.insert(*handle);
                }
            }
            _ => {}
        }
    }
}

struct TerminalSession {
    view: trust::terminal_view::TerminalView,
    handle: trust::telnet::Handle,
    cols: u16,
    rows: u16,
    connected: bool,
    remote_echo: bool,
    linemode_active: bool,
    linemode_edit: bool,
    line_editor: TextEditor,
}

impl TerminalSession {
    fn char_mode(&self) -> bool {
        self.connected && (self.remote_echo || (self.linemode_active && !self.linemode_edit))
    }
}

struct CpuDesktopRenderer {
    raster: Box<VelloCpuRenderer>,
    retained: Vec<u32>,
    size: PhysicalSize,
    valid: bool,
}

impl CpuDesktopRenderer {
    fn new() -> Self {
        Self {
            raster: Box::new(VelloCpuRenderer::new()),
            retained: Vec::new(),
            size: PhysicalSize::new(0, 0),
            valid: false,
        }
    }

    fn render(&mut self, scene: &Scene, damage: Option<CssRect>) -> Result<&[u32], String> {
        let size = scene.viewport.physical;
        if self.size != size {
            self.size = size;
            self.retained
                .resize(size.width as usize * size.height as usize, 0);
            self.valid = false;
        }
        if self.valid
            && let Some(damage) = damage.and_then(|rect| raster_damage(scene, rect))
        {
            let frame = self.raster.render(&damage.scene)?;
            let source_width = frame.size.width as usize;
            let target_width = size.width as usize;
            let start_x = damage.x as usize;
            let start_y = damage.y as usize;
            for row in 0..frame.size.height as usize {
                let source = row * source_width..(row + 1) * source_width;
                let target_start = (start_y + row) * target_width + start_x;
                self.retained[target_start..target_start + source_width]
                    .copy_from_slice(&frame.pixels[source]);
            }
        } else {
            let frame = self.raster.render(scene)?;
            self.retained.copy_from_slice(frame.pixels);
            self.valid = true;
        }
        Ok(&self.retained)
    }
}

enum DesktopRenderer {
    Cpu(CpuDesktopRenderer),
    Hybrid(Box<VelloHybridRenderer>),
}

impl DesktopRenderer {
    fn kind(&self) -> RendererKind {
        match self {
            Self::Cpu(_) => RendererKind::Cpu,
            Self::Hybrid(renderer) => renderer.kind(),
        }
    }
}

struct DesktopApp {
    browser: BrowserController,
    renderer: Option<DesktopRenderer>,
    renderer_preference: RendererPreference,
    surface: Option<Surface<OwnedDisplayHandle, Arc<Window>>>,
    context: Option<Context<OwnedDisplayHandle>>,
    window: Option<Arc<Window>>,
    accessibility: Option<AccessAdapter>,
    metrics: ViewportMetrics,
    scene: Option<Scene>,
    pointer: CssPoint,
    pointer_inside: bool,
    cursor_icon: CursorIcon,
    cursor_custom: Option<(ImageHandle, u16, u16)>,
    cursor_visible: bool,
    custom_cursors: HashMap<(ImageHandle, u16, u16), CustomCursor>,
    cursor_hotspots: HashMap<ImageHandle, (u16, u16)>,
    modifiers: ModifiersState,
    focus: FocusTarget,
    /// The command-line URL starts only after the native window reports its
    /// real CSS viewport and device scale. Responsive scripts must not observe
    /// the constructor's provisional 1x environment during initial parsing.
    initial_navigation: Option<String>,
    find: TextEditor,
    command: TextEditor,
    command_history: trust::command::History,
    form_editor: Option<TextEditor>,
    composing: bool,
    page_layout: Option<PageLayoutCache>,
    protocol_page: Option<ProtocolPageCache>,
    runtime: Handle,
    event_proxy: EventLoopProxy<DesktopEvent>,
    image_store: ImageStore,
    image_sizes: trust::layout2::ImageSizes,
    /// Number of decoded intrinsic sizes successfully queued to the resident
    /// page actor. `try_send` can legitimately find the command queue full;
    /// retain the unsent map and retry from the native draw/event loop.
    image_sizes_sent: usize,
    image_loads: ImageLoadScheduler,
    image_tasks: HashMap<ImageHandle, tokio::task::JoinHandle<()>>,
    image_flush_scheduled: bool,
    decoded_images_pending_layout: bool,
    animation_sources: AnimationSourceStore,
    animations: AnimationPlayer,
    active_animations: HashSet<ImageHandle>,
    image_schedule_key: Option<ImageScheduleKey>,
    window_focused: bool,
    hovered_actor: Option<usize>,
    hover_document_generation: u64,
    css_animation_generation: u64,
    css_animation_started: Instant,
    link_preview: String,
    selection: Option<TextSelection>,
    selecting: bool,
    find_matches: Vec<TextSelection>,
    find_index: usize,
    clipboard: Option<arboard::Clipboard>,
    exit_requested: bool,
    terminal: Option<TerminalSession>,
    pending_page_keys: VecDeque<PendingPageKey>,
    keyboard_target: Option<PageHit>,
    pressed_hit: Option<PageHit>,
    pressed_control: Option<ControlId>,
    heart_hover: Option<ScrollbarAxis>,
    heart_drag: Option<HeartDrag>,
    heart_glide: Option<HeartGlide>,
    last_document_generation: u64,
    last_vertical_heart_fraction: Option<f32>,
    loading_started: Option<Instant>,
    chrome_tick_scheduled: bool,
    /// winit/Softbuffer may deliver platform exposure/redraw notifications
    /// after a present. Only browser/UI invalidation is allowed to consume CPU
    /// and build a new frame; this latch prevents an idle presentation loop.
    redraw_pending: bool,
    /// ImageStore is shared by old/new scenes, so retain its scalar generation
    /// at presentation time to prevent partial raster damage across decoded or
    /// animated pixel changes.
    presented_image_generation: u64,
    /// Native surface recreation/exposure can require a present even when the
    /// renderer-neutral scene is byte-identical to the last one.
    force_full_raster: bool,
}

/// Apply a wheel delta to the deepest scroll container and return the portion
/// that could not be consumed. CSS Overflow 3 §2.3 allows that residual to
/// continue to an ancestor scrollport (the default `overscroll-behavior:auto`);
/// this matters for auto-height `overflow:auto` wrappers whose scroll range is
/// zero even though their computed overflow makes them discoverable.
fn scroll_container_delta(container: &ScrollContainer, delta: CssPoint) -> (CssPoint, CssPoint) {
    let mut next = container.offset;
    let mut residual = delta;
    if container.horizontal {
        let max_x = (container.content.width - container.viewport.width).max(0.0);
        next.x = (container.offset.x + delta.x).clamp(0.0, max_x);
        residual.x = delta.x - (next.x - container.offset.x);
    }
    if container.vertical {
        let max_y = (container.content.height - container.viewport.height).max(0.0);
        next.y = (container.offset.y + delta.y).clamp(0.0, max_y);
        residual.y = delta.y - (next.y - container.offset.y);
    }
    (next, residual)
}

/// Browser-page navigation keys use the same retained CSS-pixel scroll state
/// as wheel and scrollbar input. A page step leaves a small visual overlap so
/// readers retain context; arrow keys use the controller's ordinary 40px line
/// step, and Home/End select the scrolling area's vertical extremes.
fn viewport_scroll_key_target(
    key: &Key,
    current: CssPoint,
    viewport: CssSize,
    page: CssSize,
) -> Option<CssPoint> {
    let max_x = (page.width - viewport.width).max(0.0);
    let max_y = (page.height - viewport.height).max(0.0);
    let x = current.x.clamp(0.0, max_x);
    let y = current.y.clamp(0.0, max_y);
    let line_step = 40.0;
    let page_step = viewport.height.max(1.0) * 0.9;
    let target = match key {
        Key::ArrowUp => CssPoint::new(x, (y - line_step).max(0.0)),
        Key::ArrowDown => CssPoint::new(x, (y + line_step).min(max_y)),
        Key::ArrowLeft => CssPoint::new((x - line_step).max(0.0), y),
        Key::ArrowRight => CssPoint::new((x + line_step).min(max_x), y),
        Key::PageUp => CssPoint::new(x, (y - page_step).max(0.0)),
        Key::PageDown => CssPoint::new(x, (y + page_step).min(max_y)),
        Key::Home => CssPoint::new(x, 0.0),
        Key::End => CssPoint::new(x, max_y),
        _ => return None,
    };
    Some(target)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CssCursorFallback {
    Automatic,
    Hidden,
    Icon(CursorIcon),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CssCursorImage {
    source: String,
    hotspot: Option<(u16, u16)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CssCursorSpec {
    images: Vec<CssCursorImage>,
    fallback: CssCursorFallback,
}

fn parse_css_cursor(value: &str) -> Option<CssCursorSpec> {
    let parts = split_css_cursor_list(value);
    let fallback = css_cursor_fallback(parts.last()?.trim())?;
    let images = parts[..parts.len().saturating_sub(1)]
        .iter()
        .map(|part| parse_css_cursor_image(part))
        .collect::<Option<Vec<_>>>()?;
    Some(CssCursorSpec { images, fallback })
}

fn split_css_cursor_list(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0usize;
    for (index, ch) in value.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ',' if depth == 0 => {
                parts.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn parse_css_cursor_image(value: &str) -> Option<CssCursorImage> {
    let value = value.trim();
    let open = value.find('(')?;
    if !value[..open].trim().eq_ignore_ascii_case("url") {
        return None;
    }
    let close = value.rfind(')')?;
    if close <= open {
        return None;
    }
    let source = value[open + 1..close]
        .trim()
        .trim_matches(['\'', '"'])
        .to_string();
    if source.is_empty() {
        return None;
    }
    let coordinate_tokens = value[close + 1..].split_whitespace().collect::<Vec<_>>();
    let hotspot = match coordinate_tokens.as_slice() {
        [] => None,
        [x, y] => {
            let x = x.parse::<f32>().ok()?.round().clamp(0.0, u16::MAX as f32) as u16;
            let y = y.parse::<f32>().ok()?.round().clamp(0.0, u16::MAX as f32) as u16;
            Some((x, y))
        }
        _ => return None,
    };
    Some(CssCursorImage { source, hotspot })
}

fn css_cursor_fallback(value: &str) -> Option<CssCursorFallback> {
    let icon = match value.trim().to_ascii_lowercase().as_str() {
        "auto" => return Some(CssCursorFallback::Automatic),
        "none" => return Some(CssCursorFallback::Hidden),
        "default" => CursorIcon::Default,
        "context-menu" => CursorIcon::ContextMenu,
        "help" => CursorIcon::Help,
        "pointer" => CursorIcon::Pointer,
        "progress" => CursorIcon::Progress,
        "wait" => CursorIcon::Wait,
        "cell" => CursorIcon::Cell,
        "crosshair" => CursorIcon::Crosshair,
        "text" => CursorIcon::Text,
        "vertical-text" => CursorIcon::VerticalText,
        "alias" => CursorIcon::Alias,
        "copy" => CursorIcon::Copy,
        "move" => CursorIcon::Move,
        "no-drop" => CursorIcon::NoDrop,
        "not-allowed" => CursorIcon::NotAllowed,
        "grab" => CursorIcon::Grab,
        "grabbing" => CursorIcon::Grabbing,
        "e-resize" => CursorIcon::EResize,
        "n-resize" => CursorIcon::NResize,
        "ne-resize" => CursorIcon::NeResize,
        "nw-resize" => CursorIcon::NwResize,
        "s-resize" => CursorIcon::SResize,
        "se-resize" => CursorIcon::SeResize,
        "sw-resize" => CursorIcon::SwResize,
        "w-resize" => CursorIcon::WResize,
        "ew-resize" => CursorIcon::EwResize,
        "ns-resize" => CursorIcon::NsResize,
        "nesw-resize" => CursorIcon::NeswResize,
        "nwse-resize" => CursorIcon::NwseResize,
        "col-resize" => CursorIcon::ColResize,
        "row-resize" => CursorIcon::RowResize,
        "all-scroll" => CursorIcon::AllScroll,
        "zoom-in" => CursorIcon::ZoomIn,
        "zoom-out" => CursorIcon::ZoomOut,
        _ => return None,
    };
    Some(CssCursorFallback::Icon(icon))
}

fn resolve_cursor_source(base: &url::Url, source: &str) -> Option<String> {
    if source.starts_with("data:") || source.starts_with("blob:") {
        Some(source.to_string())
    } else {
        base.join(source).ok().map(|url| url.to_string())
    }
}

fn native_cursor_pixels(
    image: &trust::render::ImageResource,
    hotspot: (u16, u16),
) -> Option<(Vec<u8>, u16, u16, u16, u16)> {
    let (width, height) = (image.width, image.height);
    if width == 0 || height == 0 {
        return None;
    }
    let max = u32::from(winit::window::MAX_CURSOR_SIZE);
    let scale = (max as f64 / width as f64)
        .min(max as f64 / height as f64)
        .min(1.0);
    let out_width = ((width as f64 * scale).round() as u32).max(1);
    let out_height = ((height as f64 * scale).round() as u32).max(1);
    let rgba = if out_width == width && out_height == height {
        image.rgba.to_vec()
    } else {
        let source = image::RgbaImage::from_raw(width, height, image.rgba.to_vec())?;
        image::imageops::resize(
            &source,
            out_width,
            out_height,
            image::imageops::FilterType::Lanczos3,
        )
        .into_raw()
    };
    let hotspot_x = ((u32::from(hotspot.0).min(width - 1) as f64 * scale).round() as u32)
        .min(out_width - 1) as u16;
    let hotspot_y = ((u32::from(hotspot.1).min(height - 1) as f64 * scale).round() as u32)
        .min(out_height - 1) as u16;
    Some((
        rgba,
        out_width as u16,
        out_height as u16,
        hotspot_x,
        hotspot_y,
    ))
}

impl DesktopApp {
    fn new(
        browser: BrowserController,
        runtime: Handle,
        event_proxy: EventLoopProxy<DesktopEvent>,
        renderer_preference: RendererPreference,
        initial_navigation: Option<String>,
    ) -> Self {
        let style = chrome_text_style();
        let image_store = ImageStore::default();
        if let Err(error) = ensure_embedded_heart_assets(&image_store) {
            eprintln!("trust-desktop: heart assets unavailable: {error}");
        }
        let animations = AnimationPlayer::new(event_proxy.clone());
        Self {
            browser,
            renderer: None,
            renderer_preference,
            surface: None,
            context: None,
            window: None,
            accessibility: None,
            metrics: ViewportMetrics::from_physical(
                PhysicalSize::new(960, 640),
                ScaleFactor::default(),
            ),
            scene: None,
            pointer: CssPoint::default(),
            pointer_inside: false,
            cursor_icon: CursorIcon::Default,
            cursor_custom: None,
            cursor_visible: true,
            custom_cursors: HashMap::new(),
            cursor_hotspots: HashMap::new(),
            modifiers: ModifiersState::empty(),
            focus: FocusTarget::default(),
            initial_navigation,
            find: TextEditor::new("", &style, 500.0, false),
            command: TextEditor::new("", &style, 700.0, false),
            command_history: trust::command::History::default(),
            form_editor: None,
            composing: false,
            page_layout: None,
            protocol_page: None,
            runtime,
            event_proxy,
            image_store,
            image_sizes: trust::layout2::ImageSizes::new(),
            image_sizes_sent: 0,
            image_loads: ImageLoadScheduler::default(),
            image_tasks: HashMap::new(),
            image_flush_scheduled: false,
            decoded_images_pending_layout: false,
            animation_sources: AnimationSourceStore::default(),
            animations,
            active_animations: HashSet::new(),
            image_schedule_key: None,
            window_focused: true,
            hovered_actor: None,
            hover_document_generation: 0,
            css_animation_generation: 0,
            css_animation_started: Instant::now(),
            link_preview: String::new(),
            selection: None,
            selecting: false,
            find_matches: Vec::new(),
            find_index: 0,
            clipboard: arboard::Clipboard::new().ok(),
            exit_requested: false,
            terminal: None,
            pending_page_keys: VecDeque::new(),
            keyboard_target: None,
            pressed_hit: None,
            pressed_control: None,
            heart_hover: None,
            heart_drag: None,
            heart_glide: None,
            last_document_generation: 0,
            last_vertical_heart_fraction: None,
            loading_started: None,
            chrome_tick_scheduled: false,
            redraw_pending: false,
            presented_image_generation: 0,
            force_full_raster: true,
        }
    }

    fn request_page_images(
        &mut self,
        generation: u64,
        page: &url::Url,
        requests: &[trust::render::ImageRequest],
        visible_retries: &HashSet<ImageHandle>,
    ) {
        self.sync_image_document(generation, page);
        if self.image_loads.retired {
            return;
        }
        let image_epoch = self.image_loads.epoch();
        for request in requests {
            if self.image_loads.completed.contains(&request.handle)
                && self.image_store.contains(request.handle)
            {
                if visible_retries.contains(&request.handle) {
                    let _ = self.image_store.get(request.handle);
                }
                continue;
            }
            if let Some(image) = self.image_store.get(request.handle) {
                if self.image_loads.mark_cached(request.handle)
                    && self.image_sizes.get(&request.source) != Some(&(image.width, image.height))
                {
                    self.image_sizes
                        .insert(request.source.clone(), (image.width, image.height));
                    self.decoded_images_pending_layout = true;
                }
                continue;
            }
            if self.image_loads.completed.contains(&request.handle) {
                if !visible_retries.contains(&request.handle) {
                    continue;
                }
                self.image_loads.retry_evicted(request.handle);
            }
            self.image_loads.enqueue(request.clone());
        }
        self.pump_image_loads(generation, image_epoch, page);
        if self.decoded_images_pending_layout {
            self.flush_images_when_settled(generation, image_epoch);
        }
    }

    fn sync_image_document(&mut self, generation: u64, page: &url::Url) {
        if !self.image_loads.reset(generation, page) {
            return;
        }
        // Decoded pixels belong to one foreground document. Compressed HTTP
        // cache entries remain reusable, but retaining the previous page's
        // full decoded gallery would pin its high-water after navigation.
        self.image_store.clear();
        self.image_sizes.clear();
        self.custom_cursors.clear();
        self.cursor_hotspots.clear();
        self.image_sizes_sent = 0;
        self.animation_sources.clear();
        self.active_animations.clear();
        self.image_schedule_key = None;
        self.animations.reset(generation, self.image_loads.epoch());
        for (_, task) in self.image_tasks.drain() {
            task.abort();
        }
        self.image_flush_scheduled = false;
        self.decoded_images_pending_layout = false;
    }

    /// Stop all unfinished work owned by the displayed document while keeping
    /// its completed layout and decoded pixels available as a frozen snapshot.
    /// HTML §7.5.11's abort-a-document algorithm cancels document fetches and
    /// discards their queued tasks; the epoch makes already-queued native
    /// completion events equally inert.
    fn retire_page_loading(&mut self) {
        if let Some(cache) = &mut self.page_layout {
            cache.frozen = true;
        }
        if !self.image_loads.retire() {
            return;
        }
        for (_, task) in self.image_tasks.drain() {
            task.abort();
        }
        self.image_flush_scheduled = false;
        self.decoded_images_pending_layout = false;
        self.animation_sources.clear();
        self.active_animations.clear();
        self.image_schedule_key = None;
        self.animations
            .reset(self.browser.document_generation(), self.image_loads.epoch());
    }

    fn pump_image_loads(&mut self, generation: u64, image_epoch: u64, page: &url::Url) {
        let slots = IMAGE_FETCH_CONCURRENCY.saturating_sub(self.image_tasks.len());
        for request in self.image_loads.take_ready(slots) {
            let proxy = self.event_proxy.clone();
            let page = page.clone();
            let source = request.source.clone();
            let handle = request.handle;
            let task = self.runtime.spawn(async move {
                let result = match trust::http::fetch_graphical_image(&page, &source).await {
                    Ok(bytes) => {
                        let decode_source = source.clone();
                        tokio::task::spawn_blocking(move || {
                            trust::img::decode_graphical_image_for_source(&decode_source, bytes)
                        })
                        .await
                        .map_err(|error| format!("image decode task failed: {error}"))
                        .and_then(|result| result)
                    }
                    Err(error) => Err(error),
                };
                let _ = proxy.send_event(DesktopEvent::ImageLoaded {
                    generation,
                    image_epoch,
                    handle,
                    source,
                    result,
                });
            });
            self.image_tasks.insert(handle, task);
        }
    }

    fn schedule_image_flush(&mut self, generation: u64, image_epoch: u64) {
        if self.image_flush_scheduled {
            return;
        }
        self.image_flush_scheduled = true;
        let proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep(IMAGE_PROGRESS_RELAYOUT_DELAY).await;
            let _ = proxy.send_event(DesktopEvent::ImagesReady {
                generation,
                image_epoch,
            });
        });
    }

    fn flush_images_when_settled(&mut self, generation: u64, image_epoch: u64) {
        if self.image_loads.pending.is_empty() {
            // Do not make a fast gallery wait for the progressive-load timer.
            // A previously armed timer is harmless: after this event drains the
            // dirty flag, the delayed event becomes a no-op.
            let _ = self.event_proxy.send_event(DesktopEvent::ImagesReady {
                generation,
                image_epoch,
            });
        } else {
            self.schedule_image_flush(generation, image_epoch);
        }
    }

    fn request_redraw(&mut self) {
        self.redraw_pending = true;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn set_active_animations(&mut self, handles: HashSet<ImageHandle>) {
        if self.active_animations == handles {
            return;
        }
        self.active_animations = handles.clone();
        self.animations.set_active(
            self.browser.document_generation(),
            self.image_loads.epoch(),
            handles,
        );
    }

    fn handle_decoded_image_evictions(&mut self, evicted: Vec<ImageHandle>) {
        if evicted.is_empty() {
            return;
        }
        self.image_schedule_key = None;
        for handle in evicted {
            if self.animation_sources.remove(handle).is_some() {
                self.animations.unregister(handle);
            }
            self.image_loads.retry_evicted(handle);
            self.active_animations.remove(&handle);
        }
    }

    fn chrome_loading(&self, snapshot: &trust::core::BrowserSnapshot) -> bool {
        snapshot.loading
            || !self.image_loads.pending.is_empty()
            || !self.image_tasks.is_empty()
            || self.image_flush_scheduled
    }

    fn schedule_chrome_tick(&mut self, fast: bool) {
        if self.chrome_tick_scheduled {
            return;
        }
        self.chrome_tick_scheduled = true;
        let proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep(if fast {
                CSS_ANIMATION_FRAME_DELAY
            } else {
                Duration::from_millis(120)
            })
            .await;
            let _ = proxy.send_event(DesktopEvent::ChromeTick);
        });
    }

    fn css_animations_active(&self) -> bool {
        self.window_focused
            && self
                .page_layout
                .as_ref()
                .is_some_and(|page| page.layout.paint.has_css_animations())
    }

    fn css_animation_elapsed(&mut self) -> f32 {
        if !self.css_animations_active() {
            return 0.0;
        }
        let generation = self.browser.document_generation();
        if self.css_animation_generation != generation {
            self.css_animation_generation = generation;
            self.css_animation_started = Instant::now();
        }
        self.css_animation_started.elapsed().as_secs_f32()
    }

    fn cancel_heart_glide(&mut self) {
        self.heart_glide = None;
    }

    fn heart_visual(
        &mut self,
        scene: &Scene,
        snapshot: &trust::core::BrowserSnapshot,
    ) -> HeartVisual {
        let now = Instant::now();
        let loading = self.chrome_loading(snapshot);
        match (loading, self.loading_started) {
            (true, None) => self.loading_started = Some(now),
            (false, Some(_)) => self.loading_started = None,
            _ => {}
        }
        let energy = self
            .loading_started
            .map_or(0.0, |started| heartbeat_energy(now.duration_since(started)));
        let actual_vertical = scrollbar_fraction(
            self.browser.interaction().scroll.y,
            scene.page_size.height,
            scene.content_viewport.height,
        )
        .or_else(|| (scene.page_size.height > 0.0 || loading).then_some(0.0));
        let actual_horizontal = scrollbar_fraction(
            self.browser.interaction().scroll.x,
            scene.page_size.width,
            scene.content_viewport.width,
        );
        let generation = self.browser.document_generation();
        if self.last_document_generation == 0 {
            self.last_document_generation = generation;
        } else if generation != self.last_document_generation {
            let from = self
                .last_vertical_heart_fraction
                .or(actual_vertical)
                .unwrap_or(0.0);
            let to = actual_vertical.unwrap_or(0.0);
            self.heart_glide = ((from - to).abs() > 0.001).then_some(HeartGlide {
                from,
                to,
                started: now,
            });
            self.last_document_generation = generation;
        }
        if let Some(glide) = &mut self.heart_glide {
            // Fragment navigation and history restoration can establish their
            // authoritative offset just after document commit. The decoration
            // follows that actual offset; it never delays or rewrites it.
            glide.to = actual_vertical.unwrap_or(0.0);
        }
        let vertical_fraction = match self.heart_glide {
            Some(glide) => {
                let (position, finished) =
                    heart_glide_position(glide.from, glide.to, now.duration_since(glide.started));
                if finished {
                    self.heart_glide = None;
                    actual_vertical
                } else {
                    Some(position)
                }
            }
            None => actual_vertical,
        };
        self.last_vertical_heart_fraction = vertical_fraction;
        HeartVisual {
            vertical_visible: scene.page_size.height > 0.0 || loading,
            vertical_fraction,
            horizontal_fraction: actual_horizontal,
            energy,
            vertical_engaged: self.heart_hover == Some(ScrollbarAxis::Vertical),
            horizontal_engaged: self.heart_hover == Some(ScrollbarAxis::Horizontal),
            dragging: self.heart_drag.map(|drag| drag.axis),
        }
    }

    fn dispatch(&mut self, action: UserAction) {
        let leaves_terminal = matches!(
            &action,
            UserAction::Back
                | UserAction::Forward
                | UserAction::Navigate(_)
                | UserAction::Activate(
                    Link::Gopher(_) | Link::Gemini(_) | Link::Http(_) | Link::OneShot(_)
                )
        );
        let leaves_keyboard_target = matches!(
            &action,
            UserAction::Back
                | UserAction::Forward
                | UserAction::Reload
                | UserAction::Navigate(_)
                | UserAction::Activate(_)
        );
        if leaves_terminal && let Some(session) = self.terminal.take() {
            let _ = session
                .handle
                .commands
                .try_send(trust::telnet::Command::Close);
        }
        if leaves_keyboard_target {
            self.keyboard_target = None;
            self.pending_page_keys.clear();
        }
        let outcome = self.browser.handle_action(action);
        self.launch_external_media_requests();
        if outcome.loading_retired {
            self.retire_page_loading();
        }
        if outcome.invalidated {
            self.request_redraw();
        }
    }

    fn launch_external_media_requests(&mut self) {
        while let Some((url, referrer)) = self.browser.take_external_media() {
            match trust::media::launch_mpv(url.as_str(), referrer.as_ref()) {
                Ok(()) => self.browser.set_status(format!("▶ mpv {url}")),
                Err(error) => self.browser.set_status(error),
            }
        }
    }

    fn stop_current_page(&mut self) {
        let was_loading = self.chrome_loading(&self.browser.snapshot());
        self.dispatch(UserAction::Stop);
        // Image fetch/decode is frontend-owned, while the document fetch and
        // resident script actor are controller-owned. Escape crosses both
        // halves of that single abort boundary.
        self.retire_page_loading();
        if was_loading && !self.browser.snapshot().status.starts_with("Stopped") {
            self.browser.set_status("Stopped loading page resources.");
        }
        self.request_redraw();
    }

    /// Apply the user-agent default for a live-page key after its cancelable
    /// `keydown` has completed in the resident actor. HTML Editing APIs leave
    /// editing-host insertion to the user agent; HTML §4.10.22.2 defines the
    /// Enter implicit-submission path for single-line text controls.
    fn apply_page_key_defaults(&mut self) {
        while let Some(prevented) = self.browser.take_page_key_default() {
            let Some(pending) = self.pending_page_keys.pop_front() else {
                continue;
            };
            if prevented
                || self.focus
                    != (FocusTarget::Form {
                        form: pending.form,
                        field: pending.field,
                    })
            {
                continue;
            }
            let kind = self
                .page_layout
                .as_ref()
                .and_then(|page| page.document.forms.get(pending.form))
                .and_then(|form| form.fields.get(pending.field))
                .map(|field| field.kind.clone());
            match kind {
                Some(FieldKind::Text | FieldKind::Password) => {
                    self.finish_text_edit();
                    self.submit_form_default(pending.form);
                }
                Some(FieldKind::Textarea)
                    if self
                        .form_editor
                        .as_mut()
                        .is_some_and(|editor| editor.handle_key(&pending.input)) =>
                {
                    self.finish_text_edit();
                    self.request_redraw();
                }
                _ => {}
            }
        }
    }

    /// Activate a form's HTML default button for an un-canceled Enter on a
    /// single-line live text control. A live submit button is clicked so its
    /// page handlers and `SubmitEvent.submitter` observe the standard default
    /// action; static forms retain the existing native submission path.
    fn submit_form_default(&mut self, form_index: usize) {
        let Some(form) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form_index))
            .cloned()
        else {
            return;
        };
        let submitter = form
            .fields
            .iter()
            .position(|field| field.kind == FieldKind::Submit);
        if let Some(field) = submitter
            .and_then(|index| form.fields.get(index))
            .and_then(|field| field.live_node)
        {
            self.dispatch(UserAction::Activate(Link::JsClick {
                node: field,
                href: String::new(),
            }));
        } else {
            self.dispatch(UserAction::SubmitForm { form, submitter });
        }
        self.set_focus(FocusTarget::Page);
    }

    /// Drain the shared controller and then release any native key defaults
    /// that the resident actor returned. A navigation retires the old key
    /// queue along with the old page.
    fn process_browser_events(&mut self) -> trust::core::ActionOutcome {
        let outcome = self.browser.process_async_events();
        self.launch_external_media_requests();
        if outcome.loading_retired {
            let had_pending_key = !self.pending_page_keys.is_empty();
            self.pending_page_keys.clear();
            if had_pending_key {
                self.set_focus(FocusTarget::Page);
            }
            self.retire_page_loading();
        }
        self.apply_page_key_defaults();
        outcome
    }

    fn navigate(&mut self, address: String) {
        if let Some((host, port, tls)) = parse_telnet_target(&address) {
            self.start_terminal(host, port, tls, address);
        } else {
            if let Some(session) = self.terminal.take() {
                let _ = session
                    .handle
                    .commands
                    .try_send(trust::telnet::Command::Close);
            }
            self.dispatch(UserAction::Navigate(address));
        }
    }

    fn start_terminal(&mut self, host: String, port: u16, tls: bool, address: String) {
        if let Some(session) = self.terminal.take() {
            let _ = session
                .handle
                .commands
                .try_send(trust::telnet::Command::Close);
        }
        let view = trust::terminal_view::TerminalView::new(24, 80);
        let _runtime = self.runtime.enter();
        let (handle, mut events) = trust::telnet::connect(host, port, (80, 24), tls);
        drop(_runtime);
        let proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            while let Some(event) = events.recv().await {
                if proxy.send_event(DesktopEvent::Telnet(event)).is_err() {
                    break;
                }
            }
        });
        self.browser.open_external_session(address.clone());
        self.retire_page_loading();
        self.image_store.clear();
        self.image_sizes.clear();
        self.image_sizes_sent = 0;
        self.page_layout = None;
        self.protocol_page = None;
        self.terminal = Some(TerminalSession {
            view,
            handle,
            cols: 80,
            rows: 24,
            connected: false,
            remote_echo: false,
            linemode_active: false,
            linemode_edit: false,
            line_editor: TextEditor::new("", &terminal_text_style(), 700.0, false),
        });
        self.set_focus(FocusTarget::Page);
    }

    fn update_metrics(&mut self, scale_factor: Option<f64>) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let scale = ScaleFactor::new(scale_factor.unwrap_or_else(|| window.scale_factor()));
        self.metrics =
            ViewportMetrics::from_physical(PhysicalSize::new(size.width, size.height), scale);
        self.dispatch(UserAction::DevicePixelRatio(
            self.metrics.scale_factor.get() as f32,
        ));
        self.dispatch(UserAction::Resize(self.browser_viewport()));
    }

    fn browser_viewport(&self) -> CssSize {
        let panel = if self.focus == FocusTarget::Command {
            trust::render::COMMAND_PANEL_HEIGHT
        } else if self.focus == FocusTarget::Find {
            trust::render::FIND_PANEL_HEIGHT
        } else {
            0.0
        };
        CssSize::new(
            self.metrics.css.width,
            (self.metrics.css.height - panel).max(0.0),
        )
    }

    fn editor_visual(editor: &mut TextEditor, password: bool) -> EditorVisual {
        let (selection, caret, _) = editor.geometry();
        EditorVisual {
            text: if password {
                "•".repeat(editor.text().chars().count())
            } else {
                editor.raw_text().to_string()
            },
            selection: selection
                .into_iter()
                .map(|rect| CssRect::new(rect.x, rect.y, rect.width, rect.height))
                .collect(),
            caret: caret.map(|rect| CssRect::new(rect.x, rect.y, rect.width, rect.height)),
        }
    }

    fn chrome_model(&mut self, snapshot: &trust::core::BrowserSnapshot) -> ChromeModel {
        self.command
            .set_width((self.metrics.css.width - 95.0).max(1.0));
        self.find
            .set_width((self.metrics.css.width - 160.0).max(1.0));
        let find =
            (self.focus == FocusTarget::Find).then(|| Self::editor_visual(&mut self.find, false));
        let command = (self.focus == FocusTarget::Command)
            .then(|| Self::editor_visual(&mut self.command, false));
        ChromeModel {
            command,
            status: snapshot.status.clone(),
            status_label: self.status_label(snapshot),
            link_preview: self.link_preview.clone(),
            find,
            find_count: (!self.find_matches.is_empty())
                .then_some((self.find_index + 1, self.find_matches.len())),
            heart: HeartVisual::default(),
        }
    }

    fn status_label(&self, snapshot: &trust::core::BrowserSnapshot) -> String {
        if self.terminal.is_some() {
            return if snapshot.loading {
                String::from("LINK:OPENING")
            } else {
                String::from("TELNET")
            };
        }
        match self.browser.current_page().map(|page| &page.document) {
            Some(FetchedDocument::Http(response)) => format!("HTTP:{}", response.status),
            Some(FetchedDocument::Gemini(response)) => format!("GEMINI:{}", response.status),
            Some(FetchedDocument::Gopher(_)) => String::from("GOPHER"),
            Some(FetchedDocument::OneShot(_)) => String::from("QUERY"),
            Some(FetchedDocument::Internal(_)) => String::from("TRUST:LOCAL"),
            None if snapshot.loading => String::from("LINK:OPENING"),
            None => String::from("LINK:DOWN"),
        }
    }

    /// Ensure the retained graphical page matches the actor. `true` means this
    /// draw changed only independently spliceable paint segments and is
    /// therefore eligible for renderer-neutral damage comparison.
    fn ensure_page_layout(&mut self, viewport: CssSize) -> bool {
        let generation = self.browser.document_generation();
        let device_pixel_ratio = self.metrics.scale_factor.get() as f32;
        let navigation_pending = self.browser.snapshot().loading;
        let (revision, rendered_revision) = self
            .browser
            .current_page()
            .map_or((0, 0), |page| (page.revision(), page.rendered_revision()));
        let page_is_live = self.browser.page_is_live();

        // A viewport or image-size command can be in flight while the actor's
        // last typed render is still current. Keep those known-good pixels for
        // that brief interval; never rebuild them from response HTML.
        if let Some(cache) = &mut self.page_layout
            && may_reuse_page_layout(
                cache.generation,
                generation,
                cache.rendered_revision,
                rendered_revision,
                cache.viewport,
                viewport,
                cache.device_pixel_ratio,
                device_pixel_ratio,
                page_is_live,
                navigation_pending,
                cache.frozen,
            )
        {
            // WHATWG HTML §7.4 keeps showing the active document while the
            // replacement is fetched and only changes it when the new history
            // entry becomes active. `begin_fetch` deliberately retires the old
            // actor, so its last complete pixels are the only authoritative
            // presentation during this interval. A viewport change must not
            // rebuild those pixels from the response's pre-script source.
            if navigation_pending || cache.frozen {
                apply_graphical_scroll_state(&mut cache.layout, self.browser.interaction());
                return false;
            }
            cache.revision = revision;
            cache.viewport = viewport;
            cache.device_pixel_ratio = device_pixel_ratio;
            apply_graphical_scroll_state(&mut cache.layout, self.browser.interaction());
            return false;
        }

        let seed = self
            .page_layout
            .as_ref()
            .filter(|cache| cache.generation == generation)
            .map(|cache| cache.document.forms.clone());
        let force_static_reflow = !page_is_live
            && self.page_layout.as_ref().is_some_and(|cache| {
                cache.generation == generation && cache.rendered_revision != rendered_revision
            });
        let typed = self.browser.current_page().and_then(|page| {
            let FetchedDocument::Http(response) = &page.document else {
                return None;
            };
            let mut rendered = page.rendered_page()?.clone();
            if !page_is_live
                && (force_static_reflow
                    || rendered.viewport.width != viewport.width
                    || rendered.viewport.height != viewport.height
                    || rendered.device_pixel_ratio != device_pixel_ratio)
            {
                let image_sizes = image_sizes_for_layout(
                    &self.image_sizes,
                    &self.image_loads.failed,
                    &rendered.image_urls,
                );
                rendered = trust::http::render_html_for_environment(
                    response,
                    trust::layout2::Viewport::new(viewport.width, viewport.height),
                    device_pixel_ratio,
                    seed.as_deref(),
                    &image_sizes,
                )?;
            }
            Some((response.url.clone(), rendered))
        });
        self.page_layout = typed.map(|(base, rendered)| {
            self.sync_image_document(generation, &base);
            let requests = rendered
                .eager_image_urls
                .iter()
                .map(|source| trust::render::ImageRequest {
                    handle: ImageHandle::for_source(source),
                    source: source.clone(),
                })
                .collect::<Vec<_>>();
            self.request_page_images(generation, &base, &requests, &HashSet::new());
            let (document, mut layout) = DesktopPageAdapter::from_rendered(base, rendered);
            apply_graphical_scroll_state(&mut layout, self.browser.interaction());
            PageLayoutCache {
                generation,
                revision,
                rendered_revision,
                viewport,
                device_pixel_ratio,
                frozen: false,
                document,
                layout,
            }
        });
        if let Some(cache) = &self.page_layout {
            let live = graphical_live_boundaries(&cache.layout);
            self.browser.set_live_layout_boundaries(live.0, live.1);
        }

        if self.page_layout.is_none() {
            let fresh = self
                .protocol_page
                .as_ref()
                .is_some_and(|cache| cache.generation == generation && cache.viewport == viewport);
            if !fresh {
                let carried_selection = self
                    .protocol_page
                    .as_ref()
                    .filter(|cache| cache.generation == generation)
                    .and_then(|cache| cache.selected);
                self.protocol_page = self.browser.current_page().and_then(|page| {
                    let document = trust::render::documents::document(page)?;
                    let gopherus = matches!(document.url, Link::Gopher(_) | Link::Gemini(_));
                    let mut selected = if gopherus {
                        carried_selection.filter(|&line| {
                            document
                                .lines
                                .get(line)
                                .is_some_and(|line| line.link.is_some())
                        })
                    } else {
                        None
                    };
                    let mut layout = trust::render::documents::paint_doc_selected(
                        &document,
                        viewport.width,
                        selected,
                    );
                    if gopherus && selected.is_none() {
                        selected = gopherus_visible_links(&layout.lines, 0.0, viewport.height)
                            .first()
                            .copied();
                        if selected.is_some() {
                            layout = trust::render::documents::paint_doc_selected(
                                &document,
                                viewport.width,
                                selected,
                            );
                        }
                    }
                    Some(ProtocolPageCache {
                        generation,
                        viewport,
                        document,
                        layout,
                        selected,
                    })
                });
                self.link_preview = self
                    .protocol_page
                    .as_ref()
                    .and_then(|cache| cache.selected.map(|line| (&cache.document, line)))
                    .and_then(|(document, line)| document.lines.get(line))
                    .and_then(|line| line.link.as_ref())
                    .map(ToString::to_string)
                    .unwrap_or_default();
            }
        } else {
            self.protocol_page = None;
        }
        false
    }

    fn relayout_cached_page(&mut self) {
        // A live layout is actor-owned and arrives asynchronously. Static HTML
        // has no resident engine; invalidate its cache so the next draw runs
        // the shared transient DOM→PixelLayout routine with current form/image
        // inputs. Either way desktop itself never owns a DOM.
        if !self.browser.page_is_live()
            && let Some(cache) = &mut self.page_layout
        {
            cache.rendered_revision = u64::MAX;
        }
    }

    fn initialize_renderer(&mut self) -> Result<(), String> {
        if self.renderer.is_some() {
            return Ok(());
        }
        let window = self
            .window
            .as_ref()
            .cloned()
            .ok_or_else(|| String::from("desktop window is unavailable"))?;
        match self.renderer_preference {
            RendererPreference::Cpu => {
                self.renderer = Some(DesktopRenderer::Cpu(CpuDesktopRenderer::new()));
                self.ensure_software_surface()?;
            }
            RendererPreference::Hybrid | RendererPreference::Auto => {
                match self
                    .runtime
                    .block_on(VelloHybridRenderer::new_window(window))
                {
                    Ok(renderer) => {
                        eprintln!(
                            "trust-desktop: renderer=hybrid adapter={}",
                            renderer.adapter_name()
                        );
                        self.renderer = Some(DesktopRenderer::Hybrid(Box::new(renderer)));
                    }
                    Err(error) if self.renderer_preference == RendererPreference::Auto => {
                        eprintln!("trust-desktop: Hybrid unavailable ({error}); using Vello CPU");
                        self.renderer = Some(DesktopRenderer::Cpu(CpuDesktopRenderer::new()));
                        self.ensure_software_surface()?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn ensure_software_surface(&mut self) -> Result<(), String> {
        if self.surface.is_some() {
            return Ok(());
        }
        let context = self
            .context
            .as_ref()
            .ok_or_else(|| String::from("software display context is unavailable"))?;
        let window = self
            .window
            .as_ref()
            .cloned()
            .ok_or_else(|| String::from("desktop window is unavailable"))?;
        self.surface = Some(Surface::new(context, window).map_err(|error| error.to_string())?);
        Ok(())
    }

    fn present_scene(&mut self, scene: &Scene, damage: Option<CssRect>) -> Result<bool, String> {
        let hybrid_result = match self.renderer.as_mut() {
            Some(DesktopRenderer::Hybrid(renderer)) => Some(renderer.present_damage(scene, damage)),
            Some(DesktopRenderer::Cpu(_)) => None,
            None => return Err(String::from("desktop renderer is not initialized")),
        };
        if let Some(result) = hybrid_result {
            match result {
                Ok(PresentOutcome::Presented) => return Ok(true),
                Ok(PresentOutcome::Skipped) => return Ok(false),
                Err(error) => {
                    // A malformed resource, device loss, or evolving Hybrid
                    // implementation must not tear down the browser. Drop all
                    // GPU-owned state and repaint this same display list using
                    // the independent CPU reference backend.
                    eprintln!("trust-desktop: Hybrid failed ({error}); switching to Vello CPU");
                    self.renderer = Some(DesktopRenderer::Cpu(CpuDesktopRenderer::new()));
                    self.ensure_software_surface()?;
                }
            }
        }

        self.ensure_software_surface()?;
        let (renderer, surface) = (&mut self.renderer, &mut self.surface);
        let Some(DesktopRenderer::Cpu(renderer)) = renderer else {
            return Err(String::from("software fallback renderer is unavailable"));
        };
        let pixels = renderer.render(scene, damage)?;
        let surface = surface
            .as_mut()
            .ok_or_else(|| String::from("desktop software surface is not available"))?;
        let width = NonZeroU32::new(scene.viewport.physical.width)
            .ok_or_else(|| String::from("zero-width desktop surface"))?;
        let height = NonZeroU32::new(scene.viewport.physical.height)
            .ok_or_else(|| String::from("zero-height desktop surface"))?;
        surface
            .resize(width, height)
            .map_err(|error| error.to_string())?;
        let mut buffer = surface.buffer_mut().map_err(|error| error.to_string())?;
        if buffer.len() != pixels.len() {
            return Err(format!(
                "presentation buffer has {} pixels, renderer produced {}",
                buffer.len(),
                pixels.len()
            ));
        }
        buffer.copy_from_slice(pixels);
        buffer
            .present()
            .map(|()| true)
            .map_err(|error| error.to_string())
    }

    fn draw(&mut self) -> Result<(), String> {
        if self.metrics.physical.is_empty() {
            return Ok(());
        }
        let trace = std::env::var_os("TRUST_DESKTOP_TRACE").is_some();
        let frame_started = std::time::Instant::now();
        let snapshot = self.browser.snapshot();
        let initial_chrome = self.chrome_model(&snapshot);
        let initial_scene = desktop_chrome(self.metrics, &snapshot, &initial_chrome);
        let page_viewport = CssSize::new(
            initial_scene.content_viewport.width,
            initial_scene.content_viewport.height,
        );
        let layout_patch = if self.terminal.is_none() {
            let eligible = self.ensure_page_layout(page_viewport);
            // A fragment request can arrive before the matching layout exists.
            // Apply it now that the committed document's boxes are available.
            self.scroll_to_fragment();
            eligible
        } else {
            false
        };
        if trace {
            eprintln!("desktop: layout + chrome {:?}", frame_started.elapsed());
        }
        // Image discovery/layout can update browser-owned state, so rebuild the
        // cheap overlay model immediately before composing it over the page.
        let mut chrome = self.chrome_model(&snapshot);
        let mut scene = desktop_chrome(self.metrics, &snapshot, &chrome);
        // Page galleries share the bounded decoded store and may evict old
        // entries. The two tiny UI images remain decoded once in HEART_ASSETS;
        // restoring an evicted entry is only an Arc clone, never a PNG decode.
        let _ = ensure_embedded_heart_assets(&self.image_store);
        scene.image_store = self.image_store.clone();

        let generation = self.browser.document_generation();
        if let Some(page) = &self.page_layout {
            let key = ImageScheduleKey {
                generation,
                revision: page.revision,
                scroll: self.browser.interaction().scroll,
                viewport: page_viewport,
            };
            if self.image_schedule_key != Some(key) {
                let base = page.document.base.clone();
                let (requests, visible_retries) =
                    scheduled_page_images(page, key.scroll, page_viewport);
                self.request_page_images(generation, &base, &requests, &visible_retries);
                self.animation_sources.touch_visible(&visible_retries);
                let active_animations = if self.window_focused {
                    self.animation_sources
                        .bounded_active_handles(&visible_retries, &self.image_store)
                } else {
                    HashSet::new()
                };
                self.set_active_animations(active_animations);
                self.image_schedule_key = Some(key);
            }
        } else {
            self.set_active_animations(HashSet::new());
            self.image_schedule_key = None;
        }
        // The actor owns the canonical layout, so decoded native pixels become
        // paintable at the actor's next geometry pass. `BrowserController`
        // deliberately uses a nonblocking send; retry a full command queue on
        // the next redraw instead of silently leaving image boxes at their
        // pending intrinsic size.
        if self.browser.page_is_live() && self.image_sizes.len() > self.image_sizes_sent {
            if self.browser.send_image_sizes(&self.image_sizes) {
                self.image_sizes_sent = self.image_sizes.len();
            } else {
                self.request_redraw();
            }
        }
        let css_animation_elapsed = self.css_animation_elapsed();
        if let Some(terminal) = &mut self.terminal {
            let line_mode = !terminal.char_mode();
            let terminal_viewport = CssSize::new(
                page_viewport.width,
                (page_viewport.height - if line_mode { 32.0 } else { 0.0 }).max(1.0),
            );
            let (cols, rows) = terminal.view.size_for_viewport(terminal_viewport);
            if (cols, rows) != (terminal.cols, terminal.rows) {
                terminal.cols = cols;
                terminal.rows = rows;
                terminal.view.resize(cols, rows);
                let _ = terminal
                    .handle
                    .commands
                    .try_send(trust::telnet::Command::Resize { cols, rows });
            }
            scene.append_page(&terminal.view.paint(), CssPoint::default());
            if line_mode {
                let input_rect = CssRect::new(
                    scene.content_viewport.x + 8.0,
                    scene.content_viewport.y + scene.content_viewport.height - 30.0,
                    (scene.content_viewport.width - 16.0).max(1.0),
                    26.0,
                );
                terminal
                    .line_editor
                    .set_width((input_rect.width - 12.0).max(1.0));
                let visual = Self::editor_visual(&mut terminal.line_editor, false);
                scene.primitives.push(DisplayCommand::FillRect {
                    rect: input_rect,
                    color: PaintColor::Rgba(20, 25, 33, 255),
                });
                paint_text_editor(
                    &mut scene.primitives,
                    &visual,
                    input_rect,
                    PaintColor::Rgba(222, 232, 242, 255),
                );
                scene.primitives.push(DisplayCommand::Stroke {
                    shape: PaintShape::Rect(input_rect),
                    brush: PaintBrush::Solid(PaintColor::Accent),
                    style: StrokeStyle::solid(1.0),
                });
            }
        } else if let Some(page) = &self.page_layout {
            scene.append_page_at(
                &page.layout.paint,
                self.browser.interaction().scroll,
                css_animation_elapsed,
            );
        } else if let Some(page) = &self.protocol_page {
            scene.append_page(&page.layout.paint, self.browser.interaction().scroll);
        }

        let query = self.find.text();
        self.find_matches = scene.find_text(&query);
        if !self.find_matches.is_empty() {
            self.find_index = self.find_index.min(self.find_matches.len() - 1);
            for (index, found) in self.find_matches.iter().copied().enumerate() {
                let color = if index == self.find_index {
                    PaintColor::Rgba(255, 166, 32, 125)
                } else {
                    PaintColor::Rgba(255, 226, 64, 80)
                };
                for rect in scene.selection_rects(found) {
                    scene
                        .primitives
                        .push(DisplayCommand::FillRect { rect, color });
                }
            }
        }
        if let Some(selection) = self.selection {
            for rect in scene.selection_rects(selection) {
                scene.primitives.push(DisplayCommand::FillRect {
                    rect,
                    color: PaintColor::Rgba(72, 128, 240, 105),
                });
            }
        }
        self.paint_focused_form(&mut scene);
        self.paint_keyboard_focus(&mut scene);
        chrome.heart = self.heart_visual(&scene, &snapshot);
        paint_desktop_overlay(&mut scene, &snapshot, &chrome);

        if trace {
            eprintln!(
                "desktop: scene {} commands {:?}",
                scene.primitives.len(),
                frame_started.elapsed()
            );
        }

        let image_generation = self.image_store.generation();
        let damage = if !self.force_full_raster
            && image_generation == self.presented_image_generation
            && let Some(previous) = &self.scene
        {
            scene_damage(previous, &scene)
        } else {
            SceneDamage::Full
        };
        let presented = match damage {
            SceneDamage::Unchanged => true,
            SceneDamage::Partial(rect) => self.present_scene(&scene, Some(rect))?,
            SceneDamage::Full => self.present_scene(&scene, None)?,
        };
        self.force_full_raster = !presented;
        self.presented_image_generation = image_generation;
        if trace {
            let renderer = self
                .renderer
                .as_ref()
                .map(|renderer| renderer.kind().name())
                .unwrap_or("uninitialized");
            eprintln!(
                "desktop: raster ({renderer}) {:?} layout_patch={layout_patch} damage={damage:?}",
                frame_started.elapsed()
            );
        }
        if let Some(window) = &self.window {
            let title = if snapshot.address.is_empty() {
                String::from("TRust")
            } else {
                format!("TRust — {}", snapshot.address)
            };
            window.set_title(&title);
        }
        let css_animations_active = self.css_animations_active();
        if self.chrome_loading(&snapshot) || self.heart_glide.is_some() || css_animations_active {
            self.schedule_chrome_tick(self.heart_glide.is_some() || css_animations_active);
        }
        self.scene = Some(scene);
        // UI Events `mouseout`/`mouseover` target the element currently under
        // the pointing device, not the element that occupied that coordinate
        // at the last native motion report. DOM/layout mutations can move or
        // remove a hovered target under a stationary cursor, so re-hit-test
        // every newly committed scene. Also clear actor identity at a document
        // boundary: arena ids are document-local and may be numerically reused.
        if self.pointer_inside {
            let generation = self.browser.document_generation();
            if generation != self.hover_document_generation {
                self.hover_document_generation = generation;
                self.hovered_actor = None;
            }
            self.pointer_moved(None, self.pointer);
        }
        self.update_accessibility(false);
        if trace {
            eprintln!("desktop: frame complete {:?}", frame_started.elapsed());
        }
        Ok(())
    }

    fn paint_focused_form(&mut self, scene: &mut Scene) {
        let FocusTarget::Form { form, field } = self.focus else {
            return;
        };
        let Some(cache) = &self.page_layout else {
            return;
        };
        let Some((&node, _)) = cache
            .document
            .controls
            .iter()
            .find(|(_, indices)| **indices == (form, field))
        else {
            return;
        };
        let Some(bounds) = cache.layout.boxes.get(&node) else {
            return;
        };
        let rect = CssRect::new(
            scene.content_viewport.x + bounds.left as f32 - self.browser.interaction().scroll.x,
            scene.content_viewport.y + bounds.top as f32 - self.browser.interaction().scroll.y,
            bounds.width as f32,
            bounds.height.max(26.0) as f32,
        );
        if let Some(editor) = &mut self.form_editor {
            let password = cache
                .document
                .forms
                .get(form)
                .and_then(|form| form.fields.get(field))
                .is_some_and(|field| field.kind == FieldKind::Password);
            let visual = Self::editor_visual(editor, password);
            scene.primitives.push(DisplayCommand::FillRect {
                rect,
                color: PaintColor::Rgba(250, 252, 255, 255),
            });
            paint_text_editor(
                &mut scene.primitives,
                &visual,
                rect,
                PaintColor::Rgba(25, 31, 39, 255),
            );
        }
        scene.primitives.push(DisplayCommand::Stroke {
            shape: PaintShape::Rect(rect),
            brush: PaintBrush::Solid(PaintColor::Accent),
            style: StrokeStyle::solid(2.0),
        });
    }

    fn paint_keyboard_focus(&self, scene: &mut Scene) {
        let Some(target) = &self.keyboard_target else {
            return;
        };
        let rect = scene
            .interactive_hits()
            .into_iter()
            .find(|candidate| same_page_target(candidate, target))
            .map(|candidate| candidate.rect)
            .unwrap_or(target.rect);
        scene.primitives.push(DisplayCommand::Stroke {
            shape: PaintShape::Rect(rect),
            brush: PaintBrush::Solid(PaintColor::Accent),
            style: StrokeStyle::solid(2.0),
        });
    }

    fn set_focus(&mut self, focus: FocusTarget) {
        let old_viewport = self.browser_viewport();
        self.focus = focus;
        if focus != FocusTarget::Page {
            self.keyboard_target = None;
        }
        let editable = match focus {
            FocusTarget::Find | FocusTarget::Command => true,
            FocusTarget::Form { form, field } => self
                .page_layout
                .as_ref()
                .and_then(|page| page.document.forms.get(form))
                .and_then(|form| form.fields.get(field))
                .is_some_and(|field| {
                    matches!(
                        field.kind,
                        FieldKind::Text | FieldKind::Password | FieldKind::Textarea
                    )
                }),
            FocusTarget::Page => self
                .terminal
                .as_ref()
                .is_some_and(|terminal| !terminal.char_mode()),
        };
        if let Some(window) = &self.window {
            window.set_ime_allowed(editable);
        }
        let new_viewport = self.browser_viewport();
        if new_viewport != old_viewport {
            self.dispatch(UserAction::Resize(new_viewport));
        }
        self.composing = false;
        self.request_redraw();
    }

    fn open_command(&mut self, replace_with_address: bool) {
        if replace_with_address || self.command.text().is_empty() {
            let address = self.browser.snapshot().address;
            self.command.set_text(&address);
        }
        self.command.select_all();
        self.set_focus(FocusTarget::Command);
    }

    fn close_command(&mut self) {
        self.set_focus(FocusTarget::Page);
    }

    fn activate_control(&mut self, control: ControlId) {
        match control {
            ControlId::Find => self.focus_chrome_editor(FocusTarget::Find, control),
            ControlId::Command => self.focus_chrome_editor(FocusTarget::Command, control),
            ControlId::VerticalRail
            | ControlId::HorizontalRail
            | ControlId::VerticalHeart
            | ControlId::HorizontalHeart => {}
        }
    }

    fn focus_chrome_editor(&mut self, focus: FocusTarget, control: ControlId) {
        let rect = self.scene.as_ref().and_then(|scene| {
            scene
                .controls
                .iter()
                .find(|region| region.id == control)
                .map(|region| region.rect)
        });
        self.set_focus(focus);
        if let Some(rect) = rect {
            let point = self.pointer;
            if let Some(editor) = self.active_editor_mut() {
                let editor_x = match focus {
                    FocusTarget::Command => 79.0,
                    FocusTarget::Find => 76.0,
                    _ => rect.x + 6.0,
                };
                let editor_y = match focus {
                    FocusTarget::Command => rect.y + 6.0,
                    FocusTarget::Find => rect.y + 6.0,
                    _ => rect.y + 5.0,
                };
                editor.move_to_point(
                    (point.x - editor_x).max(0.0),
                    (point.y - editor_y).max(0.0),
                    false,
                );
            }
        }
    }

    fn advance_find(&mut self, forward: bool) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_index = if forward {
            (self.find_index + 1) % self.find_matches.len()
        } else {
            (self.find_index + self.find_matches.len() - 1) % self.find_matches.len()
        };
        if let Some(scene) = &self.scene
            && let Some(rect) = scene
                .selection_rects(self.find_matches[self.find_index])
                .first()
        {
            let y =
                (self.browser.interaction().scroll.y + rect.y - scene.content_viewport.y - 24.0)
                    .max(0.0);
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                self.browser.interaction().scroll.x,
                y,
            )));
        }
        self.request_redraw();
    }

    fn active_editor_mut(&mut self) -> Option<&mut TextEditor> {
        match self.focus {
            FocusTarget::Find => Some(&mut self.find),
            FocusTarget::Command => Some(&mut self.command),
            FocusTarget::Form { .. } => self.form_editor.as_mut(),
            FocusTarget::Page => self
                .terminal
                .as_mut()
                .filter(|terminal| !terminal.char_mode())
                .map(|terminal| &mut terminal.line_editor),
        }
    }

    fn finish_text_edit(&mut self) {
        let FocusTarget::Form { form, field } = self.focus else {
            return;
        };
        let Some(value) = self.form_editor.as_ref().map(TextEditor::text) else {
            return;
        };
        let Some(cache) = &mut self.page_layout else {
            return;
        };
        let Some(control) = cache
            .document
            .forms
            .get_mut(form)
            .and_then(|form| form.fields.get_mut(field))
        else {
            return;
        };
        if control.value == value {
            return;
        }
        control.value = value.clone();
        let actor = control.live_node;
        let _ = control;
        self.dispatch(UserAction::SetFormValue {
            actor,
            value,
            checked: None,
        });
        self.relayout_cached_page();
    }

    fn copy(&mut self, cut: bool) {
        let selected = self
            .active_editor_mut()
            .and_then(|editor| editor.selected_text().map(str::to_string))
            .or_else(|| {
                let scene = self.scene.as_ref()?;
                self.selection
                    .map(|selection| scene.selected_text(selection))
            });
        if let Some(text) = selected.filter(|text| !text.is_empty())
            && let Some(clipboard) = &mut self.clipboard
        {
            let _ = clipboard.set_text(text);
            if cut && let Some(editor) = self.active_editor_mut() {
                editor.delete_selection();
                self.finish_text_edit();
            }
        }
    }

    fn paste(&mut self) {
        let text = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get_text().ok());
        if let Some(text) = text
            && let Some(editor) = self.active_editor_mut()
        {
            editor.replace_selection(&text);
            self.finish_text_edit();
            self.request_redraw();
        }
    }

    fn handle_gopherus_key(&mut self, input: &KeyInput) -> bool {
        if input.state != KeyState::Pressed
            || input.modifiers.control
            || input.modifiers.meta
            || input.modifiers.alt
        {
            return false;
        }
        let Some(cache) = &self.protocol_page else {
            return false;
        };
        if !matches!(cache.document.url, Link::Gopher(_) | Link::Gemini(_)) {
            return false;
        }

        match &input.key {
            Key::ArrowLeft => {
                self.dispatch(UserAction::Back);
                true
            }
            Key::ArrowRight | Key::Enter => {
                let link = cache
                    .selected
                    .and_then(|line| cache.document.lines.get(line))
                    .and_then(|line| line.link.clone());
                if let Some(link) = link {
                    self.activate_link(link);
                }
                true
            }
            Key::ArrowUp | Key::ArrowDown => {
                let direction = if input.key == Key::ArrowDown { 1 } else { -1 };
                let position = gopherus_arrow(
                    &cache.layout.lines,
                    cache.selected,
                    self.browser.interaction().scroll.y,
                    cache.viewport.height,
                    cache.layout.paint.height,
                    direction,
                );
                self.apply_gopherus_position(position);
                true
            }
            Key::PageUp | Key::PageDown | Key::Home | Key::End => {
                let current = self.browser.interaction().scroll.y;
                let max_scroll = (cache.layout.paint.height - cache.viewport.height).max(0.0);
                let (target, direction) = match &input.key {
                    Key::PageUp => ((current - cache.viewport.height * 0.9).max(0.0), -1),
                    Key::PageDown => ((current + cache.viewport.height * 0.9).min(max_scroll), 1),
                    Key::Home => (0.0, -1),
                    Key::End => (max_scroll, 1),
                    _ => unreachable!(),
                };
                let selected = gopherus_retarget(
                    &cache.layout.lines,
                    cache.selected,
                    target,
                    cache.viewport.height,
                    direction,
                );
                self.apply_gopherus_position(GopherusPosition {
                    selected,
                    scroll: target,
                });
                true
            }
            _ => false,
        }
    }

    fn handle_http_scroll_key(&mut self, input: &KeyInput) -> bool {
        if input.state != KeyState::Pressed
            || input.modifiers.control
            || input.modifiers.meta
            || input.modifiers.alt
            || self.page_layout.is_none()
        {
            return false;
        }
        let Some(scene) = &self.scene else {
            return false;
        };
        let Some(target) = viewport_scroll_key_target(
            &input.key,
            self.browser.interaction().scroll,
            CssSize::new(scene.content_viewport.width, scene.content_viewport.height),
            scene.page_size,
        ) else {
            return false;
        };
        self.cancel_heart_glide();
        self.keyboard_target = None;
        self.dispatch(UserAction::SetViewportScroll(target));
        true
    }

    /// Send Enter through the resident page before the desktop editor applies
    /// its local default. This is the path that lets contenteditable chat
    /// composers cancel the default newline and activate their authored Send
    /// button, while ordinary uncanceled contenteditable Enter still inserts a
    /// newline when the actor reports that default is allowed.
    fn dispatch_live_form_key(&mut self, input: &KeyInput) -> bool {
        let FocusTarget::Form { form, field } = self.focus else {
            return false;
        };
        let node = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| form.fields.get(field))
            .and_then(|field| field.live_node);
        let Some(node) = node else {
            return false;
        };
        // The text edit must reach the actor before its keydown, so a page
        // handler reading the editor's value sees the latest native input.
        self.finish_text_edit();
        self.pending_page_keys.push_back(PendingPageKey {
            form,
            field,
            input: input.clone(),
        });
        self.dispatch(UserAction::PageKey {
            node,
            input: input.clone(),
        });
        true
    }

    fn apply_gopherus_position(&mut self, position: GopherusPosition) {
        self.cancel_heart_glide();
        let old_scroll = self.browser.interaction().scroll;
        let (selection_changed, preview) = {
            let Some(cache) = &mut self.protocol_page else {
                return;
            };
            let changed = cache.selected != position.selected;
            cache.selected = position.selected;
            if changed {
                cache.layout = trust::render::documents::paint_doc_selected(
                    &cache.document,
                    cache.viewport.width,
                    cache.selected,
                );
            }
            let preview = cache
                .selected
                .and_then(|line| cache.document.lines.get(line))
                .and_then(|line| line.link.as_ref())
                .map(ToString::to_string)
                .unwrap_or_default();
            (changed, preview)
        };
        self.keyboard_target = None;
        self.link_preview = preview;
        if (old_scroll.y - position.scroll).abs() > f32::EPSILON {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                old_scroll.x,
                position.scroll,
            )));
        }
        if selection_changed {
            self.request_redraw();
        }
    }

    fn handle_keyboard(&mut self, event: winit::event::KeyEvent) {
        let pressed = event.state == ElementState::Pressed;
        let command = self.modifiers.control_key() || self.modifiers.super_key();
        if pressed
            && command
            && let WinitKey::Character(character) = &event.logical_key
        {
            if character.eq_ignore_ascii_case("l") {
                self.open_command(true);
                return;
            }
            if character.eq_ignore_ascii_case("f") {
                self.find.select_all();
                self.set_focus(FocusTarget::Find);
                return;
            }
            if character.eq_ignore_ascii_case("r") {
                self.dispatch(UserAction::Reload);
                return;
            }
            if character.eq_ignore_ascii_case("c") {
                self.copy(false);
                return;
            }
            if character.eq_ignore_ascii_case("x") {
                self.copy(true);
                return;
            }
            if character.eq_ignore_ascii_case("v") {
                self.paste();
                return;
            }
        }
        if pressed {
            match (&event.logical_key, self.modifiers.alt_key()) {
                (WinitKey::Named(NamedKey::ArrowLeft), true) => {
                    self.dispatch(UserAction::Back);
                    return;
                }
                (WinitKey::Named(NamedKey::ArrowRight), true) => {
                    self.dispatch(UserAction::Forward);
                    return;
                }
                _ => {}
            }
        }

        let input = KeyInput {
            key: translate_key(&event.logical_key),
            state: if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            modifiers: translate_modifiers(self.modifiers),
            repeat: event.repeat,
            composing: self.composing,
        };
        // UI Events §3.5.6.1 routes keydown to the focused element and makes
        // Tab's default action focus transfer. HTML §6.6.5 explicitly allows
        // that transfer to enter UA controls after the document. TRust uses
        // that UA-control transfer as COMMAND, but an actually focused form or
        // contenteditable control retains the document's sequential-focus
        // behavior. COMMAND itself always yields to Tab.
        if pressed && input.key == Key::Tab && !input.modifiers.control && !input.modifiers.meta {
            match self.focus {
                FocusTarget::Command => self.close_command(),
                FocusTarget::Form { .. } => self.focus_next(input.modifiers.shift),
                _ => self.open_command(false),
            }
            return;
        }
        if pressed
            && input.modifiers.control
            && matches!(&input.key, Key::Character(text) if text == "]")
        {
            if self.focus == FocusTarget::Command {
                self.close_command();
            } else {
                self.open_command(false);
            }
            return;
        }
        // Escape is the browser stop key in every graphical browser mode. It
        // never dismisses COMMAND. `BrowserController::Stop` implements HTML
        // §7.5.11's abort boundary for native fetches and the resident actor.
        if pressed && input.key == Key::Escape && self.terminal.is_none() {
            self.stop_current_page();
            return;
        }
        if self.focus == FocusTarget::Page
            && self.terminal.is_none()
            && self.handle_gopherus_key(&input)
        {
            return;
        }
        if self.focus == FocusTarget::Page
            && self.terminal.is_none()
            && self.handle_http_scroll_key(&input)
        {
            return;
        }
        if pressed && input.key == Key::Enter && self.dispatch_live_form_key(&input) {
            return;
        }
        if pressed {
            let focus = self.focus;
            match (focus, &input.key) {
                (FocusTarget::Find, Key::Enter) => {
                    self.advance_find(!input.modifiers.shift);
                    return;
                }
                (FocusTarget::Command, Key::Enter) => {
                    self.execute_command();
                    return;
                }
                (FocusTarget::Command, Key::ArrowUp | Key::ArrowDown) => {
                    let current = self.command.text();
                    let recalled = if input.key == Key::ArrowUp {
                        self.command_history.up(&current)
                    } else {
                        self.command_history.down()
                    };
                    if let Some(recalled) = recalled {
                        self.command.set_text(&recalled);
                    }
                    self.request_redraw();
                    return;
                }
                (FocusTarget::Form { form, field }, Key::Enter) => {
                    let kind = self
                        .page_layout
                        .as_ref()
                        .and_then(|page| page.document.forms.get(form))
                        .and_then(|form| form.fields.get(field))
                        .map(|field| field.kind.clone());
                    match kind {
                        Some(FieldKind::Textarea) => {}
                        Some(FieldKind::Text | FieldKind::Password) => {
                            self.finish_text_edit();
                            let submitter = self
                                .page_layout
                                .as_ref()
                                .and_then(|page| page.document.forms.get(form))
                                .and_then(|form| {
                                    form.fields
                                        .iter()
                                        .position(|field| field.kind == FieldKind::Submit)
                                });
                            self.submit_form(form, submitter);
                            return;
                        }
                        Some(_) => {
                            self.activate_form_control(form, field);
                            return;
                        }
                        None => {}
                    }
                }
                (FocusTarget::Form { form, field }, Key::Character(text)) if text == " " => {
                    let editable = self
                        .page_layout
                        .as_ref()
                        .and_then(|page| page.document.forms.get(form))
                        .and_then(|form| form.fields.get(field))
                        .is_some_and(|field| {
                            matches!(
                                field.kind,
                                FieldKind::Text | FieldKind::Password | FieldKind::Textarea
                            )
                        });
                    if !editable {
                        self.activate_form_control(form, field);
                        return;
                    }
                }
                (FocusTarget::Page, Key::Enter) if self.terminal.is_none() => {
                    if let Some(target) = self.keyboard_target.clone() {
                        self.activate_page_hit(target);
                        return;
                    }
                }
                (_, Key::Escape)
                    if self
                        .terminal
                        .as_ref()
                        .is_some_and(|terminal| !terminal.char_mode()) =>
                {
                    self.open_command(false);
                    return;
                }
                // ESC is meaningful terminal input; leave it for the VT
                // keyboard encoder below.
                (_, Key::Escape) => {}
                _ => {}
            }
        }
        let consumed = self
            .active_editor_mut()
            .is_some_and(|editor| editor.handle_key(&input));
        if consumed {
            if self.focus == FocusTarget::Command {
                self.command_history.detach();
            }
            self.finish_text_edit();
            self.request_redraw();
            return;
        }
        if self.focus == FocusTarget::Page
            && let Some(terminal) = &mut self.terminal
        {
            if terminal.char_mode() {
                if let Some(bytes) = trust::terminal_view::TerminalView::encode_key(&input) {
                    let _ = terminal
                        .handle
                        .commands
                        .try_send(trust::telnet::Command::Send(bytes));
                    return;
                }
            } else if pressed && input.key == Key::Enter {
                let mut bytes = terminal.line_editor.text().into_bytes();
                bytes.extend_from_slice(b"\r\n");
                terminal.line_editor.set_text("");
                let _ = terminal
                    .handle
                    .commands
                    .try_send(trust::telnet::Command::Send(bytes));
                self.request_redraw();
                return;
            }
        }
        if pressed
            && !self.composing
            && !command
            && !self.modifiers.alt_key()
            && let Some(text) = &event.text
        {
            let text: String = text
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            if !text.is_empty()
                && let Some(editor) = self.active_editor_mut()
            {
                editor.replace_selection(&text);
                if self.focus == FocusTarget::Command {
                    self.command_history.detach();
                }
                self.finish_text_edit();
                self.request_redraw();
                return;
            }
        }
        self.dispatch(UserAction::Key(input));
    }

    fn handle_ime(&mut self, event: Ime) {
        let action = match event {
            Ime::Enabled => {
                self.composing = false;
                ImeAction::Enabled
            }
            Ime::Preedit(text, cursor) => {
                self.composing = !text.is_empty();
                ImeAction::Preedit { text, cursor }
            }
            Ime::Commit(text) => {
                self.composing = false;
                ImeAction::Commit(text)
            }
            Ime::Disabled => {
                self.composing = false;
                ImeAction::Disabled
            }
        };
        if let Some(editor) = self.active_editor_mut() {
            editor.handle_ime(&action);
            self.finish_text_edit();
        } else if let ImeAction::Commit(text) = &action
            && let Some(terminal) = &self.terminal
        {
            let _ = terminal
                .handle
                .commands
                .try_send(trust::telnet::Command::Send(text.as_bytes().to_vec()));
        }
        self.dispatch(UserAction::Ime(action));
        self.update_ime_cursor_area();
        self.request_redraw();
    }

    fn update_ime_cursor_area(&mut self) {
        let focus = self.focus;
        let Some(editor) = self.active_editor_mut() else {
            return;
        };
        let (_, caret, ime) = editor.geometry();
        let area = caret.unwrap_or(ime);
        let origin = match focus {
            FocusTarget::Command => self
                .scene
                .as_ref()
                .map(|scene| {
                    CssPoint::new(
                        79.0,
                        scene.content_viewport.y + scene.content_viewport.height + 30.0,
                    )
                })
                .unwrap_or_default(),
            FocusTarget::Find => self
                .scene
                .as_ref()
                .map(|scene| {
                    CssPoint::new(
                        76.0,
                        scene.content_viewport.y + scene.content_viewport.height + 13.0,
                    )
                })
                .unwrap_or_default(),
            FocusTarget::Form { form, field } => self
                .page_layout
                .as_ref()
                .and_then(|page| {
                    page.document.controls.iter().find_map(|(node, indices)| {
                        (*indices == (form, field))
                            .then(|| page.layout.boxes.get(node))
                            .flatten()
                    })
                })
                .and_then(|bounds| {
                    self.scene.as_ref().map(|scene| {
                        CssPoint::new(
                            scene.content_viewport.x + bounds.left as f32
                                - self.browser.interaction().scroll.x,
                            scene.content_viewport.y + bounds.top as f32
                                - self.browser.interaction().scroll.y,
                        )
                    })
                })
                .unwrap_or(self.pointer),
            FocusTarget::Page => self
                .scene
                .as_ref()
                .map(|scene| {
                    CssPoint::new(
                        scene.content_viewport.x + 8.0,
                        scene.content_viewport.y + scene.content_viewport.height - 30.0,
                    )
                })
                .unwrap_or_default(),
        };
        if let Some(window) = &self.window {
            window.set_ime_cursor_area(
                LogicalPosition::new(f64::from(origin.x + area.x), f64::from(origin.y + area.y)),
                LogicalSize::new(
                    f64::from(area.width.max(1.0)),
                    f64::from(area.height.max(1.0)),
                ),
            );
        }
    }

    fn execute_command(&mut self) {
        let line = self.command.text();
        let command = line.trim();
        self.command_history.push(command);
        let mut parts = command.split_whitespace();
        let verb = parts.next().unwrap_or("").to_ascii_lowercase();
        match verb.as_str() {
            "" => self.close_command(),
            "q" | "quit" | "exit" => self.exit_requested = true,
            "back" => {
                self.close_command();
                self.dispatch(UserAction::Back);
            }
            "forward" => {
                self.close_command();
                self.dispatch(UserAction::Forward);
            }
            "reload" => {
                self.close_command();
                self.dispatch(UserAction::Reload);
            }
            "stop" => self.stop_current_page(),
            "close" | "c" => {
                if let Some(terminal) = self.terminal.take() {
                    let _ = terminal
                        .handle
                        .commands
                        .try_send(trust::telnet::Command::Close);
                    self.browser.set_status("Terminal connection closed.");
                } else {
                    self.browser.set_status("No terminal connection to close.");
                }
            }
            "open" | "o" => {
                let Some(target) = parts.next() else {
                    self.browser.set_status("usage: open <host|url> [port]");
                    return;
                };
                let port = match parts.next() {
                    Some(value) => match trust::command::parse_port(value) {
                        Some(port) => Some(port),
                        None => {
                            self.browser
                                .set_status(format!("bad port or service name: {value}"));
                            return;
                        }
                    },
                    None => None,
                };
                let target = command_target(target, port);
                self.close_command();
                self.navigate(target);
            }
            "post" => {
                let Some(target) = parts.next() else {
                    self.browser.set_status("usage: post <url> [body]");
                    return;
                };
                let Some(url) = trust::http::parse_url(target) else {
                    self.browser.set_status("post needs an http(s):// URL");
                    return;
                };
                let body = parts.collect::<Vec<_>>().join(" ");
                self.close_command();
                let outcome = self.browser.post(url, body);
                if outcome.loading_retired {
                    self.retire_page_loading();
                }
                self.keyboard_target = None;
                self.request_redraw();
            }
            "find" => {
                self.find.set_text(&parts.collect::<Vec<_>>().join(" "));
                self.find.select_all();
                self.set_focus(FocusTarget::Find);
            }
            "set" => match (parts.next(), parts.next()) {
                (Some("encoding"), Some("cp437")) => {
                    if let Some(terminal) = &mut self.terminal {
                        terminal.view.encoding = trust::terminal_view::Encoding::Cp437;
                        self.browser.set_status("Terminal encoding: CP437");
                    } else {
                        self.browser
                            .set_status("CP437 applies to a Telnet session.");
                    }
                }
                (Some("encoding"), Some("utf8" | "utf-8")) => {
                    if let Some(terminal) = &mut self.terminal {
                        terminal.view.encoding = trust::terminal_view::Encoding::Utf8;
                        self.browser.set_status("Terminal encoding: UTF-8");
                    } else {
                        self.browser
                            .set_status("UTF-8 is already the page encoding path.");
                    }
                }
                (Some("cookies"), Some("on")) => {
                    self.browser.set_cookies_enabled(true);
                }
                (Some("cookies"), Some("off")) => {
                    self.browser.set_cookies_enabled(false);
                }
                (Some("borders"), Some("on")) => {
                    trust::layout2::set_borders_enabled(true);
                    self.relayout_cached_page();
                    self.browser.set_status("CSS borders enabled.");
                }
                (Some("borders"), Some("off")) => {
                    trust::layout2::set_borders_enabled(false);
                    self.relayout_cached_page();
                    self.browser.set_status("CSS borders disabled.");
                }
                _ => self.browser.set_status(
                    "usage: set encoding cp437|utf8 · set cookies on|off · set borders on|off",
                ),
            },
            "status" | "st" => {
                let report = self.desktop_status_page();
                self.close_command();
                self.open_internal_page("about:status", report);
            }
            "help" | "?" => {
                self.close_command();
                self.open_internal_page(
                    "about:help",
                    trust::command::HELP_PAGE.as_bytes().to_vec(),
                );
            }
            "finger" | "f" => {
                let Some(target) = parts.next() else {
                    self.browser.set_status("usage: finger [user]@<host>");
                    return;
                };
                let (user, host) = target.rsplit_once('@').unwrap_or(("", target));
                let (host, port) = trust::command::split_host_port(host);
                let address = format!(
                    "finger://{host}{}{path}",
                    port.map_or_else(String::new, |port| format!(":{port}")),
                    path = if user.is_empty() {
                        String::new()
                    } else {
                        format!("/{user}")
                    }
                );
                self.close_command();
                self.navigate(address);
            }
            "whois" => {
                let Some(query) = parts.next() else {
                    self.browser.set_status("usage: whois <domain> [server]");
                    return;
                };
                let server = parts.next().unwrap_or(trust::oneshot::WHOIS_DEFAULT);
                let (host, port) = trust::command::split_host_port(server);
                let address = format!(
                    "whois://{host}{}/{query}",
                    port.map_or_else(String::new, |port| format!(":{port}"))
                );
                self.close_command();
                self.navigate(address);
            }
            "dict" | "define" => {
                let Some(word) = parts.next() else {
                    self.browser.set_status("usage: dict <word> [server]");
                    return;
                };
                let server = parts.next().unwrap_or(trust::oneshot::DICT_DEFAULT);
                let (host, port) = trust::command::split_host_port(server);
                let address = format!(
                    "dict://{host}{}/d:{word}",
                    port.map_or_else(String::new, |port| format!(":{port}"))
                );
                self.close_command();
                self.navigate(address);
            }
            _ if trust::command::looks_like_address(&verb) => {
                let target = match parts.next() {
                    Some(value) => match trust::command::parse_port(value) {
                        Some(port) => command_target(&verb, Some(port)),
                        None => {
                            self.browser
                                .set_status(format!("bad port or service name: {value}"));
                            return;
                        }
                    },
                    None => command.to_string(),
                };
                self.close_command();
                self.navigate(target);
            }
            _ => {
                let target = trust::command::search_url(command);
                self.close_command();
                self.navigate(target);
            }
        }
    }

    fn open_internal_page(&mut self, address: &str, source: Vec<u8>) {
        let outcome = self
            .browser
            .open_internal_gemtext(address.to_string(), source);
        if outcome.loading_retired {
            self.retire_page_loading();
        }
        self.keyboard_target = None;
        self.request_redraw();
    }

    fn desktop_status_page(&self) -> Vec<u8> {
        let snapshot = self.browser.snapshot();
        let renderer = self
            .renderer
            .as_ref()
            .map(|renderer| renderer.kind().name())
            .unwrap_or("uninitialized");
        format!(
            "# TRust status\n\n```\nAddress: {address}\nState: {status}\nRenderer: {renderer}\nViewport: {vw:.0} × {vh:.0} CSS px\nDevice scale: {scale:.2}\nScroll: {sx:.0}, {sy:.0} CSS px\n```\n",
            address = snapshot.address,
            status = snapshot.status,
            vw = self.browser_viewport().width,
            vh = self.browser_viewport().height,
            scale = self.metrics.scale_factor.get(),
            sx = self.browser.interaction().scroll.x,
            sy = self.browser.interaction().scroll.y,
        )
        .into_bytes()
    }

    fn focus_next(&mut self, reverse: bool) {
        let Some(scene) = &self.scene else {
            self.set_focus(FocusTarget::Page);
            return;
        };
        let mut controls = scene.interactive_hits();
        controls.sort_by(|a, b| {
            a.rect
                .y
                .total_cmp(&b.rect.y)
                .then(a.rect.x.total_cmp(&b.rect.x))
        });
        controls.dedup_by(|left, right| same_page_target(left, right));
        if reverse {
            controls.reverse();
        }
        if controls.is_empty() {
            self.finish_text_edit();
            self.set_focus(FocusTarget::Page);
            return;
        }
        let current = self
            .keyboard_target
            .as_ref()
            .and_then(|current| {
                controls
                    .iter()
                    .position(|candidate| same_page_target(candidate, current))
            })
            .or_else(|| {
                let FocusTarget::Form { form, field } = self.focus else {
                    return None;
                };
                controls.iter().position(|candidate| {
                    self.form_target_for_hit(candidate) == Some((form, field))
                })
            })
            .map_or(0, |index| index + 1);
        let target = controls[current % controls.len()].clone();
        if let Some((form, field)) = self.form_target_for_hit(&target) {
            self.keyboard_target = None;
            self.focus_form(form, field);
            return;
        }
        self.finish_text_edit();
        self.set_focus(FocusTarget::Page);
        self.scroll_page_target_into_view(target.node, target.rect);
        self.link_preview = target
            .link
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        self.keyboard_target = Some(target);
        self.request_redraw();
    }

    /// Resolve a graphical hit to the nearest native form control. A real
    /// control may produce both a border-box hit and an inline label/glyph
    /// hit; either must enter the same form interaction path. The terminal
    /// adapter already carries `Link::Form` on its item, while the graphical
    /// border-box hit intentionally carries only the canonical actor node.
    fn form_target_for_hit(&self, hit: &trust::render::PageHit) -> Option<(usize, usize)> {
        let page = self.page_layout.as_ref()?;
        form_target_for_node(hit.node, &page.document.controls, &page.document.parents)
    }

    fn scroll_page_target_into_view(&mut self, node: usize, rect: CssRect) {
        self.cancel_heart_glide();
        let Some(scene) = &self.scene else { return };
        let nested = scene
            .page_scroll_containers
            .iter()
            .rev()
            .find(|container| {
                if !self.page_layout.as_ref().is_some_and(|page| {
                    page.layout.boxes.contains_key(&node)
                        && page.document.is_inclusive_ancestor(container.node, node)
                }) {
                    return false;
                }
                let x = rect.x - container.viewport.x + container.offset.x;
                let y = rect.y - container.viewport.y + container.offset.y;
                x + rect.width > 0.0
                    && y + rect.height > 0.0
                    && x < container.content.width
                    && y < container.content.height
            })
            .cloned();
        if let Some(container) = &nested {
            let mut top = container.offset.y;
            let mut left = container.offset.x;
            if rect.y < container.viewport.y {
                top -= container.viewport.y - rect.y;
            } else if rect.y + rect.height > container.viewport.y + container.viewport.height {
                top += rect.y + rect.height - container.viewport.y - container.viewport.height;
            }
            if rect.x < container.viewport.x {
                left -= container.viewport.x - rect.x;
            } else if rect.x + rect.width > container.viewport.x + container.viewport.width {
                left += rect.x + rect.width - container.viewport.x - container.viewport.width;
            }
            top = top.clamp(
                0.0,
                (container.content.height - container.viewport.height).max(0.0),
            );
            left = left.clamp(
                0.0,
                (container.content.width - container.viewport.width).max(0.0),
            );
            if (top - container.offset.y).abs() > f32::EPSILON
                || (left - container.offset.x).abs() > f32::EPSILON
            {
                self.dispatch(UserAction::SetNestedScroll {
                    actor: container.actor,
                    top,
                    left,
                });
                self.relayout_cached_page();
            }
        }
        let rect = nested.map_or(rect, |container| container.viewport);
        let Some(scene) = &self.scene else { return };
        let viewport = scene.content_viewport;
        let current = self.browser.interaction().scroll;
        let y = if rect.y < viewport.y {
            current.y - (viewport.y - rect.y)
        } else if rect.y + rect.height > viewport.y + viewport.height {
            current.y + (rect.y + rect.height - viewport.y - viewport.height)
        } else {
            current.y
        };
        if (y - current.y).abs() > f32::EPSILON {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                current.x,
                y.max(0.0),
            )));
        }
    }

    fn focus_form(&mut self, form: usize, field: usize) {
        let Some(control) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| form.fields.get(field))
            .cloned()
        else {
            return;
        };
        self.form_editor = match control.kind {
            FieldKind::Text | FieldKind::Password | FieldKind::Textarea => Some(TextEditor::new(
                &control.value,
                &TextStyle::default(),
                360.0,
                control.kind == FieldKind::Textarea,
            )),
            _ => None,
        };
        self.set_focus(FocusTarget::Form { form, field });
        self.scroll_control_into_view(form, field);
    }

    fn activate_form_control(&mut self, form: usize, field: usize) {
        let Some(control) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| form.fields.get(field))
            .cloned()
        else {
            return;
        };
        if !matches!(
            control.kind,
            FieldKind::Text | FieldKind::Password | FieldKind::Textarea | FieldKind::Hidden
        ) {
            self.form_editor = None;
            self.set_focus(FocusTarget::Form { form, field });
        }
        match control.kind {
            FieldKind::Text | FieldKind::Password | FieldKind::Textarea => {
                self.focus_form(form, field);
                self.position_form_caret(form, field);
            }
            FieldKind::Checkbox => self.toggle_check(form, field, !control.checked),
            FieldKind::Radio => self.toggle_radio(form, field),
            FieldKind::Select(options) => {
                if !options.is_empty() {
                    let index = options
                        .iter()
                        .position(|(_, value)| *value == control.value)
                        .map_or(0, |index| (index + 1) % options.len());
                    self.set_field_value(form, field, options[index].1.clone(), None);
                }
            }
            FieldKind::Submit => self.submit_form(form, Some(field)),
            FieldKind::Button => {
                if let Some(actor) = control.live_node {
                    self.dispatch(UserAction::Activate(Link::JsClick {
                        node: actor,
                        href: String::new(),
                    }));
                }
            }
            FieldKind::Reset => self.reset_form(form),
            FieldKind::Hidden => {}
        }
    }

    fn position_form_caret(&mut self, form: usize, field: usize) {
        let origin = self.page_layout.as_ref().and_then(|page| {
            page.document.controls.iter().find_map(|(node, indices)| {
                if *indices != (form, field) {
                    return None;
                }
                let bounds = page.layout.boxes.get(node)?;
                let scene = self.scene.as_ref()?;
                Some(CssPoint::new(
                    scene.content_viewport.x + bounds.left as f32
                        - self.browser.interaction().scroll.x,
                    scene.content_viewport.y + bounds.top as f32
                        - self.browser.interaction().scroll.y,
                ))
            })
        });
        if let Some(origin) = origin {
            let point = self.pointer;
            if let Some(editor) = &mut self.form_editor {
                editor.move_to_point(
                    (point.x - origin.x - 6.0).max(0.0),
                    (point.y - origin.y - 5.0).max(0.0),
                    false,
                );
            }
        }
    }

    fn scroll_control_into_view(&mut self, form: usize, field: usize) {
        self.cancel_heart_glide();
        let Some(page) = &self.page_layout else {
            return;
        };
        let Some(rect) = page.document.controls.iter().find_map(|(node, indices)| {
            (*indices == (form, field))
                .then(|| page.layout.boxes.get(node))
                .flatten()
        }) else {
            return;
        };
        let current = self.browser.interaction().scroll;
        let mut y = current.y;
        if (rect.top as f32) < current.y {
            y = rect.top as f32;
        } else if (rect.top + rect.height) as f32 > current.y + page.viewport.height {
            y = ((rect.top + rect.height) as f32 - page.viewport.height).max(0.0);
        }
        if y != current.y {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(current.x, y)));
        }
    }

    fn reset_form(&mut self, form: usize) {
        if let Some(actor) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| {
                form.fields
                    .iter()
                    .find(|field| field.kind == FieldKind::Reset)
            })
            .and_then(|field| field.live_node)
        {
            self.dispatch(UserAction::Activate(Link::JsClick {
                node: actor,
                href: String::new(),
            }));
            return;
        }
        let Some(page) = &mut self.page_layout else {
            return;
        };
        let Some(current) = page.document.forms.get(form).cloned() else {
            return;
        };
        let mut default = current.clone();
        for field in &mut default.fields {
            field.value.clone_from(&field.default_value);
            field.checked = field.default_checked;
        }
        page.document.forms[form] = default.clone();
        for (before, after) in current.fields.iter().zip(&default.fields) {
            if before.value != after.value || before.checked != after.checked {
                self.dispatch(UserAction::SetFormValue {
                    actor: after.live_node,
                    value: after.value.clone(),
                    checked: matches!(after.kind, FieldKind::Checkbox | FieldKind::Radio)
                        .then_some(after.checked),
                });
            }
        }
        self.relayout_cached_page();
    }

    fn toggle_check(&mut self, form: usize, field: usize, checked: bool) {
        let value = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .and_then(|form| form.fields.get(field))
            .map(|field| field.value.clone())
            .unwrap_or_default();
        self.set_field_value(form, field, value, Some(checked));
    }

    fn toggle_radio(&mut self, form: usize, field: usize) {
        let Some(page) = &mut self.page_layout else {
            return;
        };
        let Some(target) = page
            .document
            .forms
            .get(form)
            .and_then(|form| form.fields.get(field))
            .cloned()
        else {
            return;
        };
        if let Some(fields) = page
            .document
            .forms
            .get_mut(form)
            .map(|form| &mut form.fields)
        {
            for candidate in fields.iter_mut() {
                if candidate.kind == FieldKind::Radio && candidate.name == target.name {
                    candidate.checked = false;
                }
            }
        }
        self.set_field_value(form, field, target.value, Some(true));
    }

    fn set_field_value(&mut self, form: usize, field: usize, value: String, checked: Option<bool>) {
        let Some(page) = &mut self.page_layout else {
            return;
        };
        let Some(control) = page
            .document
            .forms
            .get_mut(form)
            .and_then(|form| form.fields.get_mut(field))
        else {
            return;
        };
        control.value = value.clone();
        if let Some(checked) = checked {
            control.checked = checked;
        }
        let actor = control.live_node;
        self.dispatch(UserAction::SetFormValue {
            actor,
            value,
            checked,
        });
        self.relayout_cached_page();
    }

    fn submit_form(&mut self, form: usize, submitter: Option<usize>) {
        let Some(form) = self
            .page_layout
            .as_ref()
            .and_then(|page| page.document.forms.get(form))
            .cloned()
        else {
            return;
        };
        self.dispatch(UserAction::SubmitForm { form, submitter });
        self.set_focus(FocusTarget::Page);
    }

    fn activate_link(&mut self, link: Link) {
        match link {
            Link::Form { form, field } => self.activate_form_control(form, field),
            Link::Media(url) => {
                let referrer = self
                    .browser
                    .current_page()
                    .and_then(|page| match page.target() {
                        Link::Http(url) => Some(url),
                        _ => None,
                    });
                match trust::media::launch_mpv(url.as_str(), referrer) {
                    Ok(()) => self.browser.set_status(format!("▶ mpv {url}")),
                    Err(error) => self.browser.set_status(error),
                }
            }
            Link::Http(url) if self.same_document_fragment(&url) => {
                self.scroll_static_fragment(url.fragment().unwrap_or(""));
            }
            Link::External(target) if parse_telnet_target(&target).is_some() => {
                self.navigate(target)
            }
            Link::Telnet { host, port, tls } => {
                let address = format!("{}://{host}:{port}", if tls { "telnets" } else { "telnet" });
                self.start_terminal(host, port, tls, address);
            }
            link => self.dispatch(UserAction::Activate(link)),
        }
    }

    fn activate_page_hit(&mut self, target: PageHit) {
        if let Some((form, field)) = self.form_target_for_hit(&target) {
            self.activate_form_control(form, field);
            return;
        }
        if let Some(activation) = page_hit_activation(&target, self.browser.page_is_live()) {
            self.activate_link(activation);
        }
    }

    fn same_document_fragment(&self, target: &url::Url) -> bool {
        let Some(current) = self
            .browser
            .current_page()
            .and_then(|page| match page.target() {
                Link::Http(url) => Some(url),
                _ => None,
            })
        else {
            return false;
        };
        if target.fragment().is_none() {
            return false;
        }
        let mut left = current.clone();
        let mut right = target.clone();
        left.set_fragment(None);
        right.set_fragment(None);
        left == right
    }

    fn scroll_static_fragment(&mut self, fragment: &str) {
        self.cancel_heart_glide();
        let Some(page) = &self.page_layout else {
            return;
        };
        let y = if fragment.is_empty() {
            Some(0.0)
        } else {
            page.document.fragment_y.get(fragment).copied()
        };
        if let Some(y) = y {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                0.0,
                y.max(0.0),
            )));
        }
    }

    fn apply_cursor_icon(&mut self, icon: CursorIcon) {
        let changed =
            !self.cursor_visible || self.cursor_custom.is_some() || self.cursor_icon != icon;
        self.cursor_visible = true;
        self.cursor_custom = None;
        self.cursor_icon = icon;
        if changed && let Some(window) = &self.window {
            window.set_cursor_visible(true);
            window.set_cursor(icon);
        }
    }

    fn apply_hidden_cursor(&mut self) {
        if self.cursor_visible {
            self.cursor_visible = false;
            self.cursor_custom = None;
            if let Some(window) = &self.window {
                window.set_cursor_visible(false);
            }
        }
    }

    fn apply_pointer_cursor(
        &mut self,
        event_loop: Option<&ActiveEventLoop>,
        authored: Option<&str>,
        automatic: CursorIcon,
    ) {
        let Some(spec) = authored.and_then(parse_css_cursor) else {
            self.apply_cursor_icon(automatic);
            return;
        };
        let base = self
            .page_layout
            .as_ref()
            .map(|page| page.document.base.clone());
        if let Some(base) = base {
            for candidate in &spec.images {
                let Some(source) = resolve_cursor_source(&base, &candidate.source) else {
                    continue;
                };
                let handle = ImageHandle::for_source(&source);
                let Some(image) = self.image_store.get(handle) else {
                    continue;
                };
                let hotspot = candidate.hotspot.unwrap_or_else(|| {
                    self.cursor_hotspots.get(&handle).copied().unwrap_or((0, 0))
                });
                let Some((rgba, width, height, hotspot_x, hotspot_y)) =
                    native_cursor_pixels(&image, hotspot)
                else {
                    continue;
                };
                let key = (handle, hotspot_x, hotspot_y);
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.custom_cursors.entry(key)
                {
                    let Some(event_loop) = event_loop else {
                        continue;
                    };
                    let Ok(source) = winit::window::CustomCursor::from_rgba(
                        rgba, width, height, hotspot_x, hotspot_y,
                    ) else {
                        continue;
                    };
                    entry.insert(event_loop.create_custom_cursor(source));
                }
                let changed = !self.cursor_visible || self.cursor_custom != Some(key);
                self.cursor_visible = true;
                self.cursor_custom = Some(key);
                if changed
                    && let (Some(window), Some(cursor)) =
                        (&self.window, self.custom_cursors.get(&key))
                {
                    window.set_cursor_visible(true);
                    window.set_cursor(cursor.clone());
                }
                return;
            }
        }
        match spec.fallback {
            CssCursorFallback::Automatic => self.apply_cursor_icon(automatic),
            CssCursorFallback::Hidden => self.apply_hidden_cursor(),
            CssCursorFallback::Icon(icon) => self.apply_cursor_icon(icon),
        }
    }

    fn pointer_moved(&mut self, event_loop: Option<&ActiveEventLoop>, point: CssPoint) {
        self.pointer = point;
        let chrome_owned = self.heart_drag.is_some()
            || self
                .scene
                .as_ref()
                .and_then(|scene| scene.control_at(point))
                .is_some();
        if !chrome_owned {
            self.dispatch(UserAction::PointerMove(point));
        }
        let mut visual_changed = false;
        if self.heart_drag.is_some() {
            self.seek_heart_drag(point);
            self.apply_cursor_icon(CursorIcon::Grabbing);
            return;
        }
        let heart_hover = self.scene.as_ref().and_then(|scene| {
            let content = scene.content_viewport;
            let has_vertical = scene
                .controls
                .iter()
                .any(|control| control.id == ControlId::VerticalHeart);
            let vertical = has_vertical
                && point.y >= content.y
                && point.y < content.y + content.height
                && point.x >= content.x + content.width - 28.0;
            let has_horizontal = scene
                .controls
                .iter()
                .any(|control| control.id == ControlId::HorizontalHeart);
            let horizontal = has_horizontal
                && point.x >= content.x
                && point.x < content.x + content.width
                && point.y >= content.y + content.height - 28.0;
            if matches!(
                scene.control_at(point),
                Some(ControlId::HorizontalHeart | ControlId::HorizontalRail)
            ) || horizontal
            {
                Some(ScrollbarAxis::Horizontal)
            } else if matches!(
                scene.control_at(point),
                Some(ControlId::VerticalHeart | ControlId::VerticalRail)
            ) || vertical
            {
                Some(ScrollbarAxis::Vertical)
            } else {
                None
            }
        });
        if heart_hover != self.heart_hover {
            self.heart_hover = heart_hover;
            visual_changed = true;
        }
        if self.selecting
            && let Some(position) = self
                .scene
                .as_ref()
                .and_then(|scene| scene.text_position_at(point))
            && let Some(selection) = &mut self.selection
        {
            selection.focus = position;
            visual_changed = true;
        }
        let over_chrome = self
            .scene
            .as_ref()
            .and_then(|scene| scene.control_at(point))
            .is_some();
        let hit = (!over_chrome)
            .then(|| {
                self.scene
                    .as_ref()
                    .and_then(|scene| scene.page_hit_at(point))
            })
            .flatten();
        let actor = hit.as_ref().and_then(|hit| hit.actor).or_else(|| {
            hit.as_ref().and_then(|hit| match &hit.link {
                Some(Link::JsClick { node, .. }) => Some(*node),
                _ => None,
            })
        });
        let actor_changed = actor != self.hovered_actor;
        if actor_changed {
            self.hovered_actor = actor;
            visual_changed = true;
        }
        // A native motion sample remains observable as pointermove/mousemove
        // even when hit testing stays on the same element. Scene-commit
        // re-hit-tests pass no ActiveEventLoop and only synthesize boundary
        // transitions, so DOM movement under a stationary pointer does not
        // manufacture a movement event.
        if actor_changed || event_loop.is_some() {
            let viewport = self.scene.as_ref().map_or(point, |scene| {
                CssPoint::new(
                    point.x - scene.content_viewport.x,
                    point.y - scene.content_viewport.y,
                )
            });
            self.dispatch(UserAction::PageHover {
                actor,
                position: viewport,
            });
        }
        let link_preview = hit
            .as_ref()
            .and_then(|hit| hit.link.as_ref())
            .map(ToString::to_string)
            .unwrap_or_default();
        if link_preview != self.link_preview {
            self.link_preview = link_preview;
            visual_changed = true;
        }
        let cursor = if self.heart_hover.is_some() {
            CursorIcon::Grab
        } else if matches!(
            self.scene
                .as_ref()
                .and_then(|scene| scene.control_at(point)),
            Some(ControlId::Command | ControlId::Find)
        ) {
            CursorIcon::Text
        } else {
            match hit.as_ref().and_then(|hit| hit.link.as_ref()) {
                Some(Link::Form { form, field })
                    if self
                        .page_layout
                        .as_ref()
                        .and_then(|page| page.document.forms.get(*form))
                        .and_then(|form| form.fields.get(*field))
                        .is_some_and(|field| {
                            matches!(
                                field.kind,
                                FieldKind::Text | FieldKind::Password | FieldKind::Textarea
                            )
                        }) =>
                {
                    CursorIcon::Text
                }
                Some(_) => CursorIcon::Pointer,
                None if self
                    .scene
                    .as_ref()
                    .and_then(|scene| scene.text_position_at(point))
                    .is_some() =>
                {
                    CursorIcon::Text
                }
                _ => CursorIcon::Default,
            }
        };
        self.apply_pointer_cursor(
            event_loop,
            hit.as_ref().and_then(|hit| hit.cursor.as_deref()),
            cursor,
        );
        // Plain motion over the same semantic target only updates the core's
        // pointer coordinates and platform cursor. JS/DOM mutations wake us
        // through BrowserWake; there is no reason to rebuild and rasterize an
        // identical full page at the mouse-reporting rate.
        if visual_changed {
            self.request_redraw();
        }
    }

    fn handle_pointer_button(&mut self, state: ElementState, button: MouseButton) {
        let button = translate_button(button);
        let chrome_owned = self.heart_drag.is_some()
            || self
                .scene
                .as_ref()
                .and_then(|scene| scene.control_at(self.pointer))
                .is_some();
        if !chrome_owned {
            self.dispatch(UserAction::PointerButton {
                position: self.pointer,
                button,
                state: if state == ElementState::Pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            });
        }
        if state == ElementState::Released {
            self.selecting = false;
            if self.heart_drag.take().is_some() {
                self.request_redraw();
                return;
            }
            match button {
                PointerButton::Back => self.dispatch(UserAction::Back),
                PointerButton::Forward => self.dispatch(UserAction::Forward),
                PointerButton::Primary => {
                    let control = self
                        .scene
                        .as_ref()
                        .and_then(|scene| scene.control_at(self.pointer));
                    if let Some(control) = control
                        && self.pressed_control.take() == Some(control)
                    {
                        self.activate_control(control);
                        self.pressed_hit = None;
                        return;
                    }
                    let released = self
                        .scene
                        .as_ref()
                        .and_then(|scene| scene.page_hit_at(self.pointer))
                        .filter(|hit| hit.link.is_some() || hit.actor.is_some());
                    if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                        eprintln!(
                            "desktop: primary release pressed={:?} released={released:?}",
                            self.pressed_hit
                        );
                    }
                    let click_target =
                        self.pressed_hit
                            .take()
                            .zip(released)
                            .and_then(|(pressed, released)| {
                                let parents = &self.page_layout.as_ref()?.document.parents;
                                click_target_for_hits(&pressed, &released, parents)
                            });
                    if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                        eprintln!("desktop: primary click target={click_target:?}");
                    }
                    if let Some(target) = click_target {
                        self.activate_page_hit(target);
                    }
                }
                _ => {}
            }
            self.pressed_control = None;
            self.pressed_hit = None;
            return;
        }
        if button != PointerButton::Primary {
            return;
        }
        let scrollbar_press = self.scene.as_ref().and_then(|scene| {
            let control = scene.control_at(self.pointer)?;
            let (axis, heart_control, on_heart) = match control {
                ControlId::VerticalHeart => {
                    (ScrollbarAxis::Vertical, ControlId::VerticalHeart, true)
                }
                ControlId::VerticalRail => {
                    (ScrollbarAxis::Vertical, ControlId::VerticalHeart, false)
                }
                ControlId::HorizontalHeart => {
                    (ScrollbarAxis::Horizontal, ControlId::HorizontalHeart, true)
                }
                ControlId::HorizontalRail => {
                    (ScrollbarAxis::Horizontal, ControlId::HorizontalHeart, false)
                }
                _ => return None,
            };
            let scrollable = match axis {
                ScrollbarAxis::Vertical => {
                    scene.page_size.height > scene.content_viewport.height + f32::EPSILON
                }
                ScrollbarAxis::Horizontal => {
                    scene.page_size.width > scene.content_viewport.width + f32::EPSILON
                }
            };
            if !scrollable {
                return None;
            }
            let center = scene
                .controls
                .iter()
                .find(|region| region.id == heart_control)
                .map(|region| {
                    CssPoint::new(
                        region.rect.x + region.rect.width / 2.0,
                        region.rect.y + region.rect.height / 2.0,
                    )
                })
                .unwrap_or(self.pointer);
            let pointer_offset = if on_heart {
                match axis {
                    ScrollbarAxis::Vertical => self.pointer.y - center.y,
                    ScrollbarAxis::Horizontal => self.pointer.x - center.x,
                }
            } else {
                0.0
            };
            Some((axis, pointer_offset, on_heart))
        });
        if let Some((axis, pointer_offset, on_heart)) = scrollbar_press {
            self.cancel_heart_glide();
            self.heart_drag = Some(HeartDrag {
                axis,
                pointer_offset,
            });
            if !on_heart {
                // A rail press is an instant user scroll to that track
                // fraction. Keeping the drag capture makes press-and-scrub a
                // natural extension of the same CSSOM View scroll operation.
                self.seek_heart_drag(self.pointer);
            }
            self.apply_cursor_icon(CursorIcon::Grabbing);
            self.request_redraw();
            return;
        }
        self.pressed_control = self
            .scene
            .as_ref()
            .and_then(|scene| scene.control_at(self.pointer));
        if self.pressed_control.is_some() {
            return;
        }
        self.pressed_hit = self
            .scene
            .as_ref()
            .and_then(|scene| scene.page_hit_at(self.pointer))
            .filter(|hit| hit.link.is_some() || hit.actor.is_some());
        if self.pressed_hit.is_some() {
            return;
        }
        if let Some(position) = self
            .scene
            .as_ref()
            .and_then(|scene| scene.text_position_at(self.pointer))
        {
            self.selection = Some(TextSelection {
                anchor: position,
                focus: position,
            });
            self.selecting = true;
        } else {
            self.selection = None;
        }
        self.keyboard_target = None;
        self.finish_text_edit();
        self.set_focus(FocusTarget::Page);
    }

    fn handle_scroll(&mut self, delta: MouseScrollDelta) {
        self.cancel_heart_glide();
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (-x * 40.0, -y * 40.0),
            MouseScrollDelta::PixelDelta(position) => {
                let scale = self.metrics.scale_factor.get();
                ((-position.x / scale) as f32, (-position.y / scale) as f32)
            }
        };
        if let Some(terminal) = &mut self.terminal {
            terminal.view.scroll((dy / 19.0).round() as i32);
            self.request_redraw();
            return;
        }
        let mut remaining = CssPoint::new(dx, dy);
        if let Some(container) = self
            .scene
            .as_ref()
            .and_then(|scene| scene.scroll_container_at(self.pointer))
            .cloned()
        {
            let (next, residual) = scroll_container_delta(&container, remaining);
            let changed = next != container.offset;
            if changed {
                if let Some(cache) = &mut self.page_layout
                    && let Some(scroll) = std::sync::Arc::make_mut(&mut cache.layout)
                        .paint
                        .scroll_containers
                        .iter_mut()
                        .find(|scroll| scroll.node == container.node)
                {
                    scroll.offset = next;
                }
                self.dispatch(UserAction::SetNestedScroll {
                    actor: container.actor,
                    top: next.y,
                    left: next.x,
                });
                // Scrolling changes composition transforms and sticky
                // resolution, not CSS box geometry. Updating the retained
                // pixel scroll metadata avoids a full DOM/style/layout pass
                // per touchpad tick.
                self.request_redraw();
            }
            // CSS Overflow 3 §2.3 and CSSOM View's scrolling model allow a
            // user scroll to continue to the ancestor scrollport when the
            // targeted overflow:auto box reaches an edge. This is essential
            // for auto-height wrappers such as Erome's #main: it is reported
            // as a scroll container, but its own range is zero.
            remaining = residual;
            if remaining.x.abs() <= f32::EPSILON && remaining.y.abs() <= f32::EPSILON {
                return;
            }
        }
        let Some(scene) = &self.scene else { return };
        let max_x = (scene.page_size.width - scene.content_viewport.width).max(0.0);
        let max_y = (scene.page_size.height - scene.content_viewport.height).max(0.0);
        let current = self.browser.interaction().scroll;
        self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
            (current.x + remaining.x).clamp(0.0, max_x),
            (current.y + remaining.y).clamp(0.0, max_y),
        )));
    }

    fn seek_heart_drag(&mut self, point: CssPoint) {
        let Some(drag) = self.heart_drag else { return };
        let Some(scene) = &self.scene else { return };
        let content = scene.content_viewport;
        let current = self.browser.interaction().scroll;
        let target = match drag.axis {
            ScrollbarAxis::Vertical => {
                let reserve_corner =
                    scene.page_size.width > scene.content_viewport.width + f32::EPSILON;
                let (start, end) = vertical_heart_track(content, reserve_corner);
                let fraction = scrollbar_track_fraction(point.y - drag.pointer_offset, start, end);
                CssPoint::new(
                    current.x,
                    scrollbar_position(fraction, scene.page_size.height, content.height),
                )
            }
            ScrollbarAxis::Horizontal => {
                let reserve_corner = scene.page_size.height > 0.0;
                let (start, end) = horizontal_heart_track(content, reserve_corner);
                let fraction = scrollbar_track_fraction(point.x - drag.pointer_offset, start, end);
                CssPoint::new(
                    scrollbar_position(fraction, scene.page_size.width, content.width),
                    current.y,
                )
            }
        };
        self.dispatch(UserAction::SetViewportScroll(target));
    }

    fn scroll_to_fragment(&mut self) {
        let Some(page) = &self.page_layout else {
            return;
        };
        if page.generation != self.browser.document_generation() {
            return;
        }
        let Some(fragment) = self.browser.take_fragment_request() else {
            return;
        };
        let target = if fragment.is_empty() {
            Some(0.0)
        } else {
            page.document.fragment_y.get(fragment.as_str()).copied()
        };
        if let Some(y) = target {
            self.dispatch(UserAction::SetViewportScroll(CssPoint::new(
                0.0,
                y.max(0.0),
            )));
        }
    }

    fn update_accessibility(&mut self, initial: bool) {
        let viewport = self
            .scene
            .as_ref()
            .map(|scene| scene.content_viewport)
            .unwrap_or(CssRect::new(0.0, 0.0, self.metrics.css.width, 0.0));
        let scroll = self.browser.interaction().scroll;
        let update = build_accessibility_update(
            AccessibilityFrame {
                metrics: self.metrics,
                page: self.page_layout.as_ref(),
                focus: self.focus,
                content_viewport: viewport,
                scroll,
                keyboard_node: self.keyboard_target.as_ref().map(|target| target.node),
                command_value: (self.focus == FocusTarget::Command)
                    .then(|| self.command.raw_text()),
                find_value: (self.focus == FocusTarget::Find).then(|| self.find.raw_text()),
            },
            initial,
        );
        let Some(adapter) = &mut self.accessibility else {
            return;
        };
        adapter.update_if_active(|| update);
    }

    fn handle_access_action(&mut self, request: ActionRequest) {
        match request.target_node {
            ACCESS_COMMAND => match request.action {
                AccessAction::Focus | AccessAction::Click => self.open_command(false),
                AccessAction::SetValue | AccessAction::ReplaceSelectedText => {
                    if let Some(ActionData::Value(value)) = request.data {
                        self.command.set_text(&value);
                        self.set_focus(FocusTarget::Command);
                    }
                }
                _ => {}
            },
            ACCESS_FIND => match request.action {
                AccessAction::Focus | AccessAction::Click => self.set_focus(FocusTarget::Find),
                AccessAction::SetValue | AccessAction::ReplaceSelectedText => {
                    if let Some(ActionData::Value(value)) = request.data {
                        self.find.set_text(&value);
                        self.set_focus(FocusTarget::Find);
                    }
                }
                _ => {}
            },
            AccessNodeId(id) if id >= PAGE_ACCESS_BASE + SemanticTree::DOM_BASE => {
                let node = (id - PAGE_ACCESS_BASE - SemanticTree::DOM_BASE) as usize;
                let control = self
                    .page_layout
                    .as_ref()
                    .and_then(|page| page.document.controls.get(&node).copied());
                if let Some((form, field)) = control {
                    if let Some(ActionData::Value(value)) = request.data {
                        self.set_field_value(form, field, value.into(), None);
                    } else if request.action == AccessAction::Click {
                        self.activate_form_control(form, field);
                    } else {
                        self.focus_form(form, field);
                    }
                } else {
                    let target = self.scene.as_ref().and_then(|scene| {
                        scene
                            .interactive_hits()
                            .into_iter()
                            .find(|target| target.node == node)
                    });
                    if let Some(target) = target {
                        if request.action == AccessAction::Click {
                            self.activate_page_hit(target);
                        } else if matches!(
                            request.action,
                            AccessAction::Focus | AccessAction::ScrollIntoView
                        ) {
                            self.scroll_page_target_into_view(target.node, target.rect);
                            self.keyboard_target = Some(target);
                            self.set_focus(FocusTarget::Page);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl ApplicationHandler<DesktopEvent> for DesktopApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.force_full_raster = true;
        event_loop.set_control_flow(ControlFlow::Wait);
        self.window_focused = true;
        self.image_schedule_key = None;
        if self.window.is_none() {
            let attributes = Window::default_attributes()
                .with_title("TRust")
                .with_visible(false)
                .with_inner_size(LogicalSize::new(960.0, 640.0))
                .with_min_inner_size(LogicalSize::new(480.0, 320.0));
            match event_loop.create_window(attributes) {
                Ok(window) => {
                    let window = Arc::new(window);
                    self.accessibility = Some(AccessAdapter::with_event_loop_proxy(
                        event_loop,
                        &window,
                        self.event_proxy.clone(),
                    ));
                    window.set_visible(true);
                    self.window = Some(window);
                }
                Err(error) => {
                    eprintln!("trust-desktop: could not create window: {error}");
                    event_loop.exit();
                    return;
                }
            }
        }
        if self.context.is_none() {
            match Context::new(event_loop.owned_display_handle()) {
                Ok(context) => self.context = Some(context),
                Err(error) => {
                    eprintln!("trust-desktop: could not create software context: {error}");
                    event_loop.exit();
                    return;
                }
            }
        }
        if self.renderer.is_none() {
            if let Err(error) = self.initialize_renderer() {
                eprintln!("trust-desktop: could not initialize renderer: {error}");
                event_loop.exit();
                return;
            }
        } else if let Some(DesktopRenderer::Hybrid(renderer)) = &mut self.renderer {
            let Some(window) = &self.window else { return };
            if let Err(error) = renderer.resume(Arc::clone(window)) {
                eprintln!(
                    "trust-desktop: could not resume Hybrid ({error}); switching to Vello CPU"
                );
                self.renderer = Some(DesktopRenderer::Cpu(CpuDesktopRenderer::new()));
                if let Err(error) = self.ensure_software_surface() {
                    eprintln!("trust-desktop: CPU fallback unavailable: {error}");
                    event_loop.exit();
                    return;
                }
            }
        } else if let Err(error) = self.ensure_software_surface() {
            eprintln!("trust-desktop: could not resume software surface: {error}");
            event_loop.exit();
            return;
        }
        self.update_metrics(None);
        // Startup focus is the COMMAND editor. Reapply it after the native
        // window exists so winit enables text/IME input for the visible entry,
        // not merely the browser's internal focus enum.
        self.set_focus(self.focus);
        if let Some(address) = self.initial_navigation.take() {
            self.navigate(address);
        }
        let _outcome = self.process_browser_events();
        self.request_redraw();
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.force_full_raster = true;
        self.window_focused = false;
        self.set_active_animations(HashSet::new());
        if let Some(DesktopRenderer::Hybrid(renderer)) = &mut self.renderer {
            renderer.suspend();
        }
        self.surface = None;
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: DesktopEvent) {
        match event {
            DesktopEvent::BrowserWake => {
                let outcome = self.process_browser_events();
                if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                    eprintln!(
                        "desktop: browser wake changed={} retired={}",
                        outcome.invalidated, outcome.loading_retired
                    );
                }
                if outcome.invalidated {
                    self.scroll_to_fragment();
                    self.request_redraw();
                }
            }
            DesktopEvent::ChromeTick => {
                self.chrome_tick_scheduled = false;
                let snapshot = self.browser.snapshot();
                if self.chrome_loading(&snapshot)
                    || self.heart_glide.is_some()
                    || self.css_animations_active()
                {
                    self.request_redraw();
                }
            }
            DesktopEvent::ImageLoaded {
                generation,
                image_epoch,
                handle,
                source,
                result,
            } => {
                if generation != self.browser.document_generation()
                    || !self.image_loads.accepts(generation, image_epoch)
                {
                    return;
                }
                let success = result.is_ok();
                if !self.image_loads.finish(handle, success) {
                    return;
                }
                self.image_tasks.remove(&handle);
                match result {
                    Ok(decoded) => {
                        let trust::img::DecodedGraphicalImage {
                            image,
                            animation,
                            cursor_hotspot,
                        } = decoded;
                        if let Some(hotspot) = cursor_hotspot {
                            self.cursor_hotspots.insert(handle, hotspot);
                        }
                        self.image_sizes.insert(source, (image.width, image.height));
                        let evicted = self.image_store.insert(handle, image);
                        self.handle_decoded_image_evictions(evicted);
                        let mut animation_registered = false;
                        if let Some(animation) = animation
                            && self.image_store.contains(handle)
                        {
                            let worker_source = animation.clone();
                            let evicted = self.animation_sources.insert(handle, animation);
                            for old in evicted {
                                self.animations.unregister(old);
                                self.image_store.remove(old);
                                self.image_loads.retry_evicted(old);
                                self.active_animations.remove(&old);
                                self.image_schedule_key = None;
                            }
                            if self.animation_sources.contains(handle) {
                                self.animations.register(
                                    generation,
                                    image_epoch,
                                    handle,
                                    worker_source,
                                );
                                animation_registered = true;
                            }
                        }
                        if animation_registered {
                            // The visibility schedule may already have been
                            // cached while this handle was still a pending
                            // static image. Recompute it immediately so a
                            // visible animation becomes active without waiting
                            // for the gallery relayout timer or another scroll.
                            self.image_schedule_key = None;
                            self.request_redraw();
                        }
                        self.decoded_images_pending_layout = true;
                    }
                    Err(error) => {
                        if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                            eprintln!("desktop: image {source} failed: {error}");
                        }
                        // A pending image was represented by the CSS default
                        // object size. Relayout once it becomes broken so the
                        // HTML alt-text rendering path replaces that box.
                        self.decoded_images_pending_layout = true;
                    }
                }
                if let Some(page) = self
                    .page_layout
                    .as_ref()
                    .map(|page| page.document.base.clone())
                {
                    self.pump_image_loads(generation, image_epoch, &page);
                }
                self.flush_images_when_settled(generation, image_epoch);
            }
            DesktopEvent::ImagesReady {
                generation,
                image_epoch,
            } => {
                if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                    eprintln!(
                        "desktop: images-ready dirty={} pending={}",
                        self.decoded_images_pending_layout,
                        self.image_loads.pending.len()
                    );
                }
                if generation != self.browser.document_generation()
                    || !self.image_loads.accepts(generation, image_epoch)
                {
                    return;
                }
                self.image_flush_scheduled = false;
                if !self.decoded_images_pending_layout {
                    return;
                }
                self.decoded_images_pending_layout = false;
                if self.browser.page_is_live()
                    && self.image_sizes.len() > self.image_sizes_sent
                    && !self.browser.send_image_sizes(&self.image_sizes)
                {
                    // Keep the dirty bit set. The next redraw retries the
                    // nonblocking handoff after the actor drains its queue.
                    self.decoded_images_pending_layout = true;
                    self.request_redraw();
                    return;
                }
                self.image_sizes_sent = self.image_sizes.len();
                self.relayout_cached_page();
                self.image_schedule_key = None;
                self.request_redraw();
            }
            DesktopEvent::AnimationWake => {
                let mut redraw = false;
                for update in self.animations.drain() {
                    if update.generation != self.browser.document_generation()
                        || !self
                            .image_loads
                            .accepts(update.generation, update.image_epoch)
                        || !self.animation_sources.contains(update.handle)
                    {
                        continue;
                    }
                    match update.result {
                        Ok(image) if self.image_store.contains(update.handle) => {
                            let evicted = self.image_store.insert(update.handle, image);
                            self.handle_decoded_image_evictions(evicted);
                            redraw = true;
                        }
                        Ok(_) => {
                            // The decoded-frame LRU evicted this image. Stop
                            // producing invisible frames; the ordinary visible
                            // retry path can fetch/decode it again if needed.
                            self.animation_sources.remove(update.handle);
                            self.animations.unregister(update.handle);
                            self.image_loads.retry_evicted(update.handle);
                            self.image_schedule_key = None;
                            redraw = true;
                        }
                        Err(error) => {
                            if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                                eprintln!("desktop: animation frame failed: {error}");
                            }
                            // Graceful degradation: retain the last complete
                            // canvas as a static image and release source data.
                            self.animation_sources.remove(update.handle);
                            self.animations.unregister(update.handle);
                        }
                    }
                }
                if redraw {
                    self.request_redraw();
                }
            }
            DesktopEvent::Access(event) => match event.window_event {
                accesskit_winit::WindowEvent::InitialTreeRequested => {
                    self.update_accessibility(true)
                }
                accesskit_winit::WindowEvent::ActionRequested(request) => {
                    self.handle_access_action(request)
                }
                accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
            },
            DesktopEvent::Telnet(event) => {
                match event {
                    trust::telnet::Event::Connected { peer, tls } => {
                        if let Some(terminal) = &mut self.terminal {
                            terminal.connected = true;
                        }
                        self.browser.set_status(format!(
                            "Connected to {peer}{}",
                            if tls { " over TLS" } else { "" }
                        ));
                    }
                    trust::telnet::Event::Data(data) => {
                        if let Some(terminal) = &mut self.terminal {
                            for reply in terminal.view.process(&data) {
                                let _ = terminal
                                    .handle
                                    .commands
                                    .try_send(trust::telnet::Command::Send(reply));
                            }
                        }
                    }
                    trust::telnet::Event::LineMode { active, edit } => {
                        if let Some(terminal) = &mut self.terminal {
                            terminal.linemode_active = active;
                            terminal.linemode_edit = edit;
                        }
                        self.browser.set_status(format!(
                            "Terminal input: {}",
                            if active && edit {
                                "line mode"
                            } else {
                                "character mode"
                            }
                        ));
                    }
                    trust::telnet::Event::Closed(reason) => {
                        if let Some(terminal) = &mut self.terminal {
                            terminal.connected = false;
                            terminal.remote_echo = false;
                            terminal.linemode_active = false;
                        }
                        self.browser.set_status(reason.map_or_else(
                            || String::from("Connection closed by foreign host."),
                            |error| format!("Terminal connection closed: {error}"),
                        ));
                    }
                    trust::telnet::Event::Negotiation { command, option } => {
                        if option == libmudtelnet::telnet::op_option::ECHO
                            && let Some(terminal) = &mut self.terminal
                        {
                            terminal.remote_echo =
                                command == libmudtelnet::telnet::op_command::WILL;
                        }
                    }
                }
                if self.focus == FocusTarget::Page {
                    self.set_focus(FocusTarget::Page);
                }
                self.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self
            .window
            .as_ref()
            .is_none_or(|window| window.id() != window_id)
        {
            return;
        }
        if let (Some(adapter), Some(window)) = (&mut self.accessibility, &self.window) {
            adapter.process_event(window, &event);
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Destroyed => {
                self.surface = None;
                self.renderer = None;
                self.window = None;
                event_loop.exit();
            }
            WindowEvent::Resized(_) => {
                if std::env::var_os("TRUST_DESKTOP_TRACE").is_some() {
                    eprintln!("desktop: window resized");
                }
                self.update_metrics(None);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.update_metrics(Some(scale_factor));
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                if !consume_pending_redraw(&mut self.redraw_pending) {
                    return;
                }
                if let Err(error) = self.draw() {
                    eprintln!("trust-desktop: draw failed: {error}");
                }
            }
            WindowEvent::Focused(focused) => {
                self.window_focused = focused;
                if !focused {
                    self.composing = false;
                    self.selecting = false;
                    self.set_active_animations(HashSet::new());
                } else {
                    self.image_schedule_key = None;
                    self.request_redraw();
                }
                self.dispatch(UserAction::Focus(focused));
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                self.handle_keyboard(event);
                if self.exit_requested {
                    event_loop.exit();
                }
            }
            WindowEvent::Ime(event) => self.handle_ime(event),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer_inside = true;
                self.pointer_moved(
                    Some(event_loop),
                    self.metrics.physical_to_css(position.x, position.y),
                );
            }
            WindowEvent::CursorEntered { .. } => {
                self.pointer_inside = true;
                self.pointer_moved(Some(event_loop), self.pointer);
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer_inside = false;
                self.hovered_actor = None;
                self.link_preview.clear();
                self.heart_hover = None;
                self.dispatch(UserAction::PageHover {
                    actor: None,
                    position: CssPoint::default(),
                });
                self.request_redraw();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_pointer_button(state, button)
            }
            WindowEvent::MouseWheel { delta, .. } => self.handle_scroll(delta),
            WindowEvent::Touch(touch) => {
                let point = self
                    .metrics
                    .physical_to_css(touch.location.x, touch.location.y);
                self.pointer_moved(Some(event_loop), point);
                match touch.phase {
                    TouchPhase::Started => {
                        self.handle_pointer_button(ElementState::Pressed, MouseButton::Left)
                    }
                    TouchPhase::Ended | TouchPhase::Cancelled => {
                        self.handle_pointer_button(ElementState::Released, MouseButton::Left)
                    }
                    TouchPhase::Moved => {}
                }
            }
            _ => {}
        }
    }
}

struct AccessibilityFrame<'a> {
    metrics: ViewportMetrics,
    page: Option<&'a PageLayoutCache>,
    focus: FocusTarget,
    content_viewport: CssRect,
    scroll: CssPoint,
    keyboard_node: Option<usize>,
    command_value: Option<&'a str>,
    find_value: Option<&'a str>,
}

fn build_accessibility_update(frame: AccessibilityFrame<'_>, initial: bool) -> TreeUpdate {
    let AccessibilityFrame {
        metrics,
        page,
        focus,
        content_viewport,
        scroll,
        keyboard_node,
        command_value,
        find_value,
    } = frame;
    let mut nodes = Vec::new();
    let mut root = AccessNode::new(AccessRole::Window);
    root.set_label("TRust Desktop");
    root.set_bounds(AccessRect::new(
        0.0,
        0.0,
        f64::from(metrics.css.width),
        f64::from(metrics.css.height),
    ));
    root.set_transform(
        Affine::translate(AccessVec2::ZERO) * Affine::scale(metrics.scale_factor.get()),
    );
    let mut children = Vec::new();
    if let Some(value) = command_value {
        let mut command = AccessNode::new(AccessRole::TextInput);
        command.set_label("TRust COMMAND");
        command.set_value(value);
        command.set_bounds(AccessRect::new(
            14.0,
            f64::from(content_viewport.y + content_viewport.height + 24.0),
            f64::from(metrics.css.width - 14.0),
            f64::from(content_viewport.y + content_viewport.height + 55.0),
        ));
        command.add_action(AccessAction::Focus);
        command.add_action(AccessAction::SetValue);
        command.add_action(AccessAction::ReplaceSelectedText);
        children.push(ACCESS_COMMAND);
        nodes.push((ACCESS_COMMAND, command));
    } else if let Some(value) = find_value {
        let mut find = AccessNode::new(AccessRole::TextInput);
        find.set_label("Find in page");
        find.set_value(value);
        find.set_bounds(AccessRect::new(
            14.0,
            f64::from(content_viewport.y + content_viewport.height + 7.0),
            f64::from(metrics.css.width - 14.0),
            f64::from(content_viewport.y + content_viewport.height + 38.0),
        ));
        find.add_action(AccessAction::Focus);
        find.add_action(AccessAction::SetValue);
        find.add_action(AccessAction::ReplaceSelectedText);
        children.push(ACCESS_FIND);
        nodes.push((ACCESS_FIND, find));
    }

    if let Some(page) = page {
        let mut semantic = page.document.semantics.clone();
        for node in &mut semantic.nodes {
            let Some(field) = node
                .dom_node
                .and_then(|dom_node| page.document.controls.get(&dom_node))
                .and_then(|(form, field)| page.document.forms.get(*form)?.fields.get(*field))
            else {
                continue;
            };
            node.value = match field.kind {
                FieldKind::Password => Some(String::new()),
                FieldKind::Hidden => None,
                _ => Some(field.value.clone()),
            };
            node.checked = matches!(field.kind, FieldKind::Checkbox | FieldKind::Radio)
                .then_some(field.checked);
        }
        children.push(AccessNodeId(PAGE_ACCESS_BASE + semantic.root));
        for semantic_node in semantic.nodes {
            let id = AccessNodeId(PAGE_ACCESS_BASE + semantic_node.id);
            let mut node = AccessNode::new(access_role(semantic_node.role));
            if !semantic_node.name.is_empty() {
                node.set_label(semantic_node.name.as_str());
            }
            if let Some(value) = semantic_node.value {
                node.set_value(value);
            }
            node.set_bounds(AccessRect::new(
                f64::from(content_viewport.x + semantic_node.bounds.x - scroll.x),
                f64::from(content_viewport.y + semantic_node.bounds.y - scroll.y),
                f64::from(
                    content_viewport.x + semantic_node.bounds.x + semantic_node.bounds.width
                        - scroll.x,
                ),
                f64::from(
                    content_viewport.y + semantic_node.bounds.y + semantic_node.bounds.height
                        - scroll.y,
                ),
            ));
            node.set_children(
                semantic_node
                    .children
                    .into_iter()
                    .map(|child| AccessNodeId(PAGE_ACCESS_BASE + child))
                    .collect::<Vec<_>>(),
            );
            for action in semantic_node.actions {
                node.add_action(match action {
                    trust::accessibility::Action::Focus => AccessAction::Focus,
                    trust::accessibility::Action::Activate => AccessAction::Click,
                    trust::accessibility::Action::SetValue => AccessAction::SetValue,
                    trust::accessibility::Action::SetSelection => AccessAction::SetTextSelection,
                    trust::accessibility::Action::ScrollIntoView => AccessAction::ScrollIntoView,
                });
            }
            if let Some(checked) = semantic_node.checked {
                node.set_toggled(if checked {
                    accesskit::Toggled::True
                } else {
                    accesskit::Toggled::False
                });
            }
            nodes.push((id, node));
        }
    }
    root.set_children(children);
    nodes.push((ACCESS_ROOT, root));
    let focus = match focus {
        FocusTarget::Find => ACCESS_FIND,
        FocusTarget::Command => ACCESS_COMMAND,
        FocusTarget::Form { form, field } => page
            .and_then(|page| {
                page.document
                    .controls
                    .iter()
                    .find_map(|(node, indices)| (*indices == (form, field)).then_some(*node))
            })
            .map(|node| AccessNodeId(PAGE_ACCESS_BASE + SemanticTree::DOM_BASE + node as u64))
            .unwrap_or(ACCESS_ROOT),
        FocusTarget::Page => keyboard_node
            .filter(|node| page.is_some_and(|page| page.layout.boxes.contains_key(node)))
            .map(|node| AccessNodeId(PAGE_ACCESS_BASE + SemanticTree::DOM_BASE + node as u64))
            .unwrap_or(ACCESS_ROOT),
    };
    TreeUpdate {
        nodes,
        tree: initial.then(|| Tree::new(ACCESS_ROOT)),
        tree_id: TreeId::ROOT,
        focus,
    }
}

fn same_page_target(left: &PageHit, right: &PageHit) -> bool {
    match (left.actor, right.actor) {
        (Some(left), Some(right)) => left == right,
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => {
            left.node == right.node
                || (left.link == right.link
                    && (left.rect.x - right.rect.x).abs() < 1.0
                    && (left.rect.y - right.rect.y).abs() < 1.0)
        }
    }
}

/// Choose the activation sent across the renderer/controller boundary.
/// HTML links in a live document must first dispatch `click` in that document;
/// only its uncancelled default action may choose a named navigable. The URL is
/// retained solely as the dead-actor fallback used by the controller's
/// navigation-priority lane.
fn page_hit_activation(hit: &PageHit, page_live: bool) -> Option<Link> {
    if page_live && let Some(node) = hit.actor {
        let href = match hit.link.as_ref() {
            Some(Link::JsClick { href, .. }) => href.clone(),
            Some(Link::Form { .. }) | None => String::new(),
            Some(link) => link.to_string(),
        };
        return Some(Link::JsClick { node, href });
    }
    hit.link.clone().or_else(|| {
        hit.actor.map(|node| Link::JsClick {
            node,
            href: String::new(),
        })
    })
}

/// Pointer Events "click, auxclick, and contextmenu events" dispatch requires
/// a click whose down/up targets differ to target their nearest common
/// inclusive ancestor. SVG controls routinely paint the
/// down and up coordinates as different graphical descendants (`line`,
/// `polyline`, or the containing `svg`); exact-leaf equality incorrectly
/// cancelled those clicks before DOM dispatch.
fn click_target_for_hits(
    pressed: &PageHit,
    released: &PageHit,
    parents: &HashMap<usize, usize>,
) -> Option<PageHit> {
    match (pressed.actor, released.actor) {
        (Some(pressed_actor), Some(released_actor)) => {
            let node = nearest_common_inclusive_ancestor(pressed_actor, released_actor, parents)?;
            Some(PageHit {
                rect: released.rect,
                node,
                actor: Some(node),
                link: (pressed.link == released.link)
                    .then(|| released.link.clone())
                    .flatten(),
                cursor: released.cursor.clone(),
            })
        }
        (None, None) if pressed.link == released.link => {
            let node = nearest_common_inclusive_ancestor(pressed.node, released.node, parents)?;
            Some(PageHit {
                rect: released.rect,
                node,
                actor: None,
                link: released.link.clone(),
                cursor: released.cursor.clone(),
            })
        }
        _ => None,
    }
}

fn nearest_common_inclusive_ancestor(
    left: usize,
    right: usize,
    parents: &HashMap<usize, usize>,
) -> Option<usize> {
    let mut right_ancestors = HashSet::new();
    let mut node = right;
    loop {
        if !right_ancestors.insert(node) {
            break;
        }
        let Some(parent) = parents.get(&node).copied() else {
            break;
        };
        node = parent;
    }

    let mut seen = HashSet::new();
    node = left;
    loop {
        if right_ancestors.contains(&node) {
            return Some(node);
        }
        if !seen.insert(node) {
            return None;
        }
        node = parents.get(&node).copied()?;
    }
}

fn access_role(role: SemanticRole) -> AccessRole {
    match role {
        SemanticRole::Document => AccessRole::RootWebArea,
        SemanticRole::Generic => AccessRole::GenericContainer,
        SemanticRole::Heading => AccessRole::Heading,
        SemanticRole::Paragraph => AccessRole::Paragraph,
        SemanticRole::Link => AccessRole::Link,
        SemanticRole::Button => AccessRole::Button,
        SemanticRole::TextInput => AccessRole::TextInput,
        SemanticRole::PasswordInput => AccessRole::PasswordInput,
        SemanticRole::Textarea => AccessRole::MultilineTextInput,
        SemanticRole::Checkbox => AccessRole::CheckBox,
        SemanticRole::Radio => AccessRole::RadioButton,
        SemanticRole::Select => AccessRole::ComboBox,
        SemanticRole::Image => AccessRole::Image,
        SemanticRole::List => AccessRole::List,
        SemanticRole::ListItem => AccessRole::ListItem,
        SemanticRole::Table => AccessRole::Table,
        SemanticRole::Row => AccessRole::Row,
        SemanticRole::Cell => AccessRole::Cell,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GopherusPosition {
    selected: Option<usize>,
    scroll: f32,
}

fn gopherus_arrow(
    lines: &[trust::render::documents::ProtocolLine],
    selected: Option<usize>,
    scroll: f32,
    viewport_height: f32,
    page_height: f32,
    direction: i32,
) -> GopherusPosition {
    debug_assert!(direction == -1 || direction == 1);
    let max_scroll = (page_height - viewport_height).max(0.0);

    // Gopherus first hands the highlight directly to an adjacent link.
    if let Some(current) = selected {
        let next = current as i64 + i64::from(direction);
        if next >= 0
            && lines
                .get(next as usize)
                .is_some_and(|line| line.link.is_some())
        {
            let next = next as usize;
            let line = &lines[next];
            let center = scroll + viewport_height / 2.0;
            let line_center = line.rect.y + line.rect.height / 2.0;
            let scroll = if direction > 0 && line_center > center {
                (scroll + line.rect.height.max(1.0)).min(max_scroll)
            } else if direction < 0 && line_center < center {
                (scroll - line.rect.height.max(1.0)).max(0.0)
            } else {
                scroll
            };
            return GopherusPosition {
                selected: Some(next),
                scroll,
            };
        }
    }

    let step = lines
        .iter()
        .find(|line| {
            line.rect.y + line.rect.height > scroll
                && line.rect.y < scroll + viewport_height.max(1.0)
        })
        .map_or(trust::theme::TERMINAL_FONT_SIZE_CSS_PX * 1.2, |line| {
            line.rect.height.max(1.0)
        });
    let target = (scroll + direction as f32 * step).clamp(0.0, max_scroll);
    let selected = if (target - scroll).abs() > f32::EPSILON {
        gopherus_retarget(lines, selected, target, viewport_height, direction)
    } else {
        gopherus_walk(lines, selected, scroll, viewport_height, direction)
    };
    GopherusPosition {
        selected,
        scroll: target,
    }
}

fn gopherus_retarget(
    lines: &[trust::render::documents::ProtocolLine],
    selected: Option<usize>,
    scroll: f32,
    viewport_height: f32,
    direction: i32,
) -> Option<usize> {
    let links = gopherus_visible_links(lines, scroll, viewport_height);
    let (&first, rest) = links.split_first()?;
    let last = *rest.last().unwrap_or(&first);
    let center = scroll + viewport_height / 2.0;
    let distance = |line: usize| {
        let rect = lines[line].rect;
        (rect.y + rect.height / 2.0 - center).abs()
    };
    match selected {
        Some(current)
            if lines.get(current).is_some_and(|line| {
                line.rect.y + line.rect.height > scroll && line.rect.y < scroll + viewport_height
            }) =>
        {
            let next = if direction > 0 {
                links.iter().copied().find(|&line| line > current)
            } else {
                links.iter().rev().copied().find(|&line| line < current)
            };
            next.filter(|&line| distance(line) < distance(current))
                .or(Some(current))
        }
        _ => Some(if direction > 0 { first } else { last }),
    }
}

fn gopherus_walk(
    lines: &[trust::render::documents::ProtocolLine],
    selected: Option<usize>,
    scroll: f32,
    viewport_height: f32,
    direction: i32,
) -> Option<usize> {
    let links = gopherus_visible_links(lines, scroll, viewport_height);
    match (selected, links.as_slice()) {
        (_, []) => None,
        (None, links) => Some(if direction > 0 {
            links[0]
        } else {
            *links.last().unwrap()
        }),
        (Some(current), links) if direction > 0 => links
            .iter()
            .copied()
            .find(|&line| line > current)
            .or(Some(current)),
        (Some(current), links) => links
            .iter()
            .rev()
            .copied()
            .find(|&line| line < current)
            .or(Some(current)),
    }
}

fn gopherus_visible_links(
    lines: &[trust::render::documents::ProtocolLine],
    scroll: f32,
    viewport_height: f32,
) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.link.is_some()
                && line.rect.y + line.rect.height > scroll
                && line.rect.y < scroll + viewport_height
        })
        .map(|(index, _)| index)
        .collect()
}

fn chrome_text_style() -> TextStyle {
    terminal_text_style()
}

fn terminal_text_style() -> TextStyle {
    TextStyle {
        family: String::from(trust::theme::TERMINAL_FONT_FAMILY),
        size: trust::theme::TERMINAL_FONT_SIZE_CSS_PX,
        weight: trust::theme::TERMINAL_FONT_WEIGHT,
        ..TextStyle::default()
    }
}

fn parse_telnet_target(address: &str) -> Option<(String, u16, bool)> {
    let address = address.trim();
    let tls = address.starts_with("telnets://");
    if !tls && !address.starts_with("telnet://") {
        return None;
    }
    let parsed = url::Url::parse(address).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port().unwrap_or(if tls { 992 } else { 23 });
    Some((host, port, tls))
}

/// Translate the established `open host [service]` form into the graphical
/// controller's URL-shaped navigation target.
fn command_target(target: &str, port: Option<u16>) -> String {
    if target.contains("://") {
        return target.to_string();
    }
    match port {
        None => target.to_string(),
        Some(443) => format!("https://{target}"),
        Some(80) => format!("http://{target}"),
        Some(70) => format!("gopher://{target}"),
        Some(79) => format!("finger://{target}"),
        Some(1965) => format!("gemini://{target}"),
        Some(992) => format!("telnets://{target}:992"),
        Some(port) => format!("telnet://{target}:{port}"),
    }
}

/// Two restrained jewel-like impulses with a long quiet tail. Sampling stops
/// entirely when loading does, so idle chrome has no animation clock.
fn heartbeat_energy(elapsed: Duration) -> f32 {
    fn pulse(phase: f32, center: f32, half_width: f32) -> f32 {
        (1.0 - (phase - center).abs() / half_width).clamp(0.0, 1.0)
    }
    let phase = elapsed.as_secs_f32() % 1.35;
    pulse(phase, 0.10, 0.09).max(pulse(phase, 0.32, 0.075))
}

fn heart_glide_position(from: f32, to: f32, elapsed: Duration) -> (f32, bool) {
    let progress = (elapsed.as_secs_f32() / 0.5).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - progress).powi(3);
    (from + (to - from) * eased, progress >= 1.0)
}

fn translate_modifiers(modifiers: ModifiersState) -> Modifiers {
    Modifiers {
        shift: modifiers.shift_key(),
        control: modifiers.control_key(),
        alt: modifiers.alt_key(),
        meta: modifiers.super_key(),
    }
}

fn translate_button(button: MouseButton) -> PointerButton {
    match button {
        MouseButton::Left => PointerButton::Primary,
        MouseButton::Middle => PointerButton::Auxiliary,
        MouseButton::Right => PointerButton::Secondary,
        MouseButton::Back => PointerButton::Back,
        MouseButton::Forward => PointerButton::Forward,
        MouseButton::Other(button) => PointerButton::Other(button),
    }
}

fn translate_key(key: &WinitKey) -> Key {
    match key {
        WinitKey::Character(character) => Key::Character(character.to_string()),
        WinitKey::Named(NamedKey::Enter) => Key::Enter,
        WinitKey::Named(NamedKey::Escape) => Key::Escape,
        WinitKey::Named(NamedKey::Backspace) => Key::Backspace,
        WinitKey::Named(NamedKey::Delete) => Key::Delete,
        WinitKey::Named(NamedKey::Tab) => Key::Tab,
        WinitKey::Named(NamedKey::ArrowLeft) => Key::ArrowLeft,
        WinitKey::Named(NamedKey::ArrowRight) => Key::ArrowRight,
        WinitKey::Named(NamedKey::ArrowUp) => Key::ArrowUp,
        WinitKey::Named(NamedKey::ArrowDown) => Key::ArrowDown,
        WinitKey::Named(NamedKey::Home) => Key::Home,
        WinitKey::Named(NamedKey::End) => Key::End,
        WinitKey::Named(NamedKey::PageUp) => Key::PageUp,
        WinitKey::Named(NamedKey::PageDown) => Key::PageDown,
        other => Key::Other(format!("{other:?}")),
    }
}

fn consume_pending_redraw(pending: &mut bool) -> bool {
    std::mem::take(pending)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DesktopOptions {
    renderer: RendererPreference,
    address: Option<String>,
    help: bool,
}

fn parse_desktop_args(args: impl IntoIterator<Item = String>) -> Result<DesktopOptions, String> {
    let mut options = DesktopOptions::default();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        if argument == "-h" || argument == "--help" {
            options.help = true;
        } else if let Some(renderer) = argument.strip_prefix("--renderer=") {
            options.renderer = renderer.parse()?;
        } else if argument == "--renderer" {
            let renderer = args
                .next()
                .ok_or_else(|| String::from("--renderer requires cpu, hybrid, or auto"))?;
            options.renderer = renderer.parse()?;
        } else if argument.starts_with('-') {
            return Err(format!("unknown option {argument:?}"));
        } else if options.address.replace(argument).is_some() {
            return Err(String::from(
                "trust-desktop accepts at most one initial URL",
            ));
        }
    }
    Ok(options)
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_desktop_args(std::env::args().skip(1))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if options.help {
        println!(
            "Usage: {} [--renderer=auto|cpu|hybrid] [URL]",
            env!("CARGO_BIN_NAME")
        );
        return Ok(());
    }
    // External URL handlers should not create a browser window for a concrete
    // YouTube player. Navigation/search/channel pages still open normally.
    if let Some(url) = options
        .address
        .as_deref()
        .and_then(trust::media::youtube_video_url)
    {
        trust::media::launch_mpv(url.as_str(), None).map_err(std::io::Error::other)?;
        return Ok(());
    }
    let event_loop = EventLoop::<DesktopEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("trust-desktop-net")
        .build()?;
    let browser = BrowserController::new(
        runtime.handle().clone(),
        move || {
            let _ = proxy.send_event(DesktopEvent::BrowserWake);
        },
        CssSize::new(960.0, 558.0),
    );
    let mut app = DesktopApp::new(
        browser,
        runtime.handle().clone(),
        event_loop.create_proxy(),
        options.renderer,
        options.address,
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphical_form_hit_walks_from_border_or_label_to_control() {
        let controls = HashMap::from([(7usize, (2usize, 3usize))]);
        let parents = HashMap::from([(42usize, 7usize), (7usize, 1usize)]);
        assert_eq!(form_target_for_node(42, &controls, &parents), Some((2, 3)));
        assert_eq!(form_target_for_node(99, &controls, &parents), None);
    }

    #[test]
    fn css_cursor_parser_keeps_order_hotspots_and_mandatory_fallback() {
        let parsed =
            parse_css_cursor("url('first.cur') 2 3, url(data:image/png;base64,AAAA), pointer")
                .unwrap();
        assert_eq!(
            parsed.images,
            vec![
                CssCursorImage {
                    source: "first.cur".into(),
                    hotspot: Some((2, 3)),
                },
                CssCursorImage {
                    source: "data:image/png;base64,AAAA".into(),
                    hotspot: None,
                },
            ]
        );
        assert_eq!(
            parsed.fallback,
            CssCursorFallback::Icon(CursorIcon::Pointer)
        );
        assert!(parse_css_cursor("url(first.cur)").is_none());
    }

    #[test]
    fn native_cursor_clamps_hotspot_to_the_decoded_bitmap() {
        let image = ImageResource {
            width: 2,
            height: 2,
            rgba: Arc::from(vec![255; 16]),
            has_alpha: false,
        };
        let (rgba, width, height, x, y) = native_cursor_pixels(&image, (99, 80)).unwrap();
        assert_eq!((width, height), (2, 2));
        assert_eq!((x, y), (1, 1));
        assert_eq!(rgba.len(), 16);
    }

    #[test]
    fn image_animation_frame_rate_has_an_explicit_ceiling() {
        assert_eq!(MAX_IMAGE_ANIMATION_FPS, 100);
        assert_eq!(
            animation_frame_delay(Duration::from_nanos(1)),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn click_uses_common_ancestor_of_svg_graphics() {
        let parents = HashMap::from([
            (101usize, 10usize),
            (102usize, 10usize),
            (10usize, 7usize),
            (7usize, 1usize),
        ]);
        let hit = |node| PageHit {
            rect: CssRect::new(1.0, 2.0, 3.0, 4.0),
            node,
            actor: Some(node),
            link: None,
            cursor: None,
        };
        let target = click_target_for_hits(&hit(101), &hit(102), &parents).unwrap();
        assert_eq!(target.node, 10);
        assert_eq!(target.actor, Some(10));

        let exact = click_target_for_hits(&hit(101), &hit(101), &parents).unwrap();
        assert_eq!(exact.actor, Some(101));
    }

    #[test]
    fn live_link_hit_dispatches_dom_activation_before_static_url_fallback() {
        let target = PageHit {
            rect: CssRect::new(0.0, 0.0, 20.0, 20.0),
            node: 9,
            actor: Some(42),
            link: Some(Link::Http(
                url::Url::parse("https://example.test/inside").unwrap(),
            )),
            cursor: None,
        };
        assert_eq!(
            page_hit_activation(&target, true),
            Some(Link::JsClick {
                node: 42,
                href: "https://example.test/inside".into(),
            })
        );
        assert_eq!(page_hit_activation(&target, false), target.link);
    }

    #[test]
    fn cpu_damage_raster_matches_a_fresh_full_frame() {
        let viewport =
            ViewportMetrics::from_physical(PhysicalSize::new(160, 100), ScaleFactor::default());
        let old = Scene {
            viewport,
            primitives: vec![
                DisplayCommand::FillRect {
                    rect: CssRect::new(0.0, 0.0, 160.0, 100.0),
                    color: PaintColor::Window,
                },
                DisplayCommand::FillRect {
                    rect: CssRect::new(20.0, 20.0, 40.0, 30.0),
                    color: PaintColor::Rgba(200, 10, 10, 255),
                },
            ],
            controls: Vec::new(),
            content_viewport: CssRect::new(0.0, 0.0, 160.0, 100.0),
            image_store: ImageStore::default(),
            page_scroll_containers: Vec::new(),
            page_size: CssSize::new(160.0, 100.0),
        };
        let mut new = old.clone();
        let DisplayCommand::FillRect { color, .. } = &mut new.primitives[1] else {
            unreachable!()
        };
        *color = PaintColor::Rgba(10, 10, 200, 255);
        let SceneDamage::Partial(damage) = scene_damage(&old, &new) else {
            panic!("leaf fill must be damage eligible")
        };

        let mut retained = CpuDesktopRenderer::new();
        retained.render(&old, None).unwrap();
        let partial = retained.render(&new, Some(damage)).unwrap().to_vec();
        let mut reference = CpuDesktopRenderer::new();
        let full = reference.render(&new, None).unwrap().to_vec();
        assert_eq!(partial, full);
    }

    fn protocol_line(y: f32, linked: bool) -> trust::render::documents::ProtocolLine {
        trust::render::documents::ProtocolLine {
            rect: CssRect::new(0.0, y, 100.0, 20.0),
            link: linked.then(|| Link::External(format!("test:{y}"))),
        }
    }

    #[test]
    fn desktop_cli_selects_renderer_without_stealing_initial_url() {
        assert_eq!(
            parse_desktop_args([
                String::from("--renderer=hybrid"),
                String::from("https://example.test/")
            ])
            .unwrap(),
            DesktopOptions {
                renderer: RendererPreference::Hybrid,
                address: Some(String::from("https://example.test/")),
                help: false,
            }
        );
        assert_eq!(
            parse_desktop_args([String::from("--renderer"), String::from("cpu")])
                .unwrap()
                .renderer,
            RendererPreference::Cpu
        );
        assert!(parse_desktop_args([String::from("--renderer=nope")]).is_err());
    }

    #[test]
    fn platform_redraw_without_trust_invalidation_is_ignored() {
        let mut pending = false;
        assert!(!consume_pending_redraw(&mut pending));
        pending = true;
        assert!(consume_pending_redraw(&mut pending));
        assert!(!consume_pending_redraw(&mut pending));
    }

    #[test]
    fn desktop_starts_with_the_command_editor_focused() {
        assert_eq!(FocusTarget::default(), FocusTarget::Command);
    }

    #[test]
    fn http_page_navigation_keys_target_the_retained_viewport_scroll() {
        let viewport = CssSize::new(800.0, 100.0);
        let page = CssSize::new(1_000.0, 500.0);
        let current = CssPoint::new(25.0, 200.0);
        assert_eq!(
            viewport_scroll_key_target(&Key::PageUp, current, viewport, page),
            Some(CssPoint::new(25.0, 110.0))
        );
        assert_eq!(
            viewport_scroll_key_target(&Key::PageDown, current, viewport, page),
            Some(CssPoint::new(25.0, 290.0))
        );
        assert_eq!(
            viewport_scroll_key_target(&Key::Home, current, viewport, page),
            Some(CssPoint::new(25.0, 0.0))
        );
        assert_eq!(
            viewport_scroll_key_target(&Key::End, current, viewport, page),
            Some(CssPoint::new(25.0, 400.0))
        );
        assert_eq!(
            viewport_scroll_key_target(&Key::ArrowUp, current, viewport, page),
            Some(CssPoint::new(25.0, 160.0))
        );
        assert_eq!(
            viewport_scroll_key_target(&Key::ArrowDown, current, viewport, page),
            Some(CssPoint::new(25.0, 240.0))
        );
        assert_eq!(
            viewport_scroll_key_target(&Key::ArrowLeft, current, viewport, page),
            Some(CssPoint::new(0.0, 200.0))
        );
        assert_eq!(
            viewport_scroll_key_target(&Key::ArrowRight, current, viewport, page),
            Some(CssPoint::new(65.0, 200.0))
        );
        assert_eq!(
            viewport_scroll_key_target(&Key::Enter, current, viewport, page),
            None
        );

        // Every direction clamps at its corresponding scroll boundary.
        assert_eq!(
            viewport_scroll_key_target(&Key::ArrowLeft, CssPoint::new(0.0, 0.0), viewport, page,),
            Some(CssPoint::new(0.0, 0.0))
        );
        assert_eq!(
            viewport_scroll_key_target(
                &Key::ArrowRight,
                CssPoint::new(200.0, 400.0),
                viewport,
                page,
            ),
            Some(CssPoint::new(200.0, 400.0))
        );
    }

    #[test]
    fn zero_range_auto_scroll_container_passes_wheel_to_viewport() {
        let container = ScrollContainer {
            node: 1,
            actor: None,
            viewport: CssRect::new(0.0, 0.0, 800.0, 600.0),
            content: CssSize::new(800.0, 600.0),
            offset: CssPoint::default(),
            horizontal: true,
            vertical: true,
        };
        let (next, residual) = scroll_container_delta(&container, CssPoint::new(0.0, 120.0));
        assert_eq!(next, CssPoint::default());
        assert_eq!(residual, CssPoint::new(0.0, 120.0));
    }

    #[test]
    fn wheel_chaining_preserves_unconsumed_axis_at_inner_edge() {
        let container = ScrollContainer {
            node: 1,
            actor: None,
            viewport: CssRect::new(0.0, 0.0, 800.0, 600.0),
            content: CssSize::new(1_200.0, 900.0),
            offset: CssPoint::new(200.0, 300.0),
            horizontal: true,
            vertical: true,
        };
        let (next, residual) = scroll_container_delta(&container, CssPoint::new(80.0, 700.0));
        assert_eq!(next, CssPoint::new(280.0, 300.0));
        assert_eq!(residual, CssPoint::new(0.0, 700.0));
    }

    #[test]
    fn embedded_heart_states_decode_once_into_retained_rgba_images() {
        let store = ImageStore::default();
        ensure_embedded_heart_assets(&store).unwrap();
        assert_ne!(
            desktop_heart_image_handle(false),
            desktop_heart_image_handle(true)
        );
        for active in [false, true] {
            let image = store
                .get(desktop_heart_image_handle(active))
                .expect("embedded heart is installed");
            assert_eq!((image.width, image.height), (30, 30));
            assert!(image.has_alpha);
            assert_eq!(image.rgba.len(), 30 * 30 * 4);
        }
        ensure_embedded_heart_assets(&store).unwrap();
        assert_eq!(store.len(), 2, "restoring assets does not duplicate them");
    }

    #[test]
    fn command_targets_preserve_urls_and_map_service_ports() {
        assert_eq!(command_target("example.com", None), "example.com");
        assert_eq!(
            command_target("example.com", Some(1965)),
            "gemini://example.com"
        );
        assert_eq!(
            command_target("https://example.com/path", Some(23)),
            "https://example.com/path"
        );
    }

    #[test]
    fn heartbeat_is_bounded_and_has_a_quiet_tail() {
        for millis in 0..2_000 {
            let energy = heartbeat_energy(Duration::from_millis(millis));
            assert!((0.0..=1.0).contains(&energy));
        }
        assert_eq!(heartbeat_energy(Duration::from_millis(800)), 0.0);
    }

    #[test]
    fn navigation_heart_glide_finishes_in_half_a_second() {
        assert_eq!(heart_glide_position(0.8, 0.0, Duration::ZERO), (0.8, false));
        let (middle, finished) = heart_glide_position(0.8, 0.0, Duration::from_millis(250));
        assert!(!finished);
        assert!(middle > 0.0 && middle < 0.4, "ease-out moves early");
        assert_eq!(
            heart_glide_position(0.8, 0.0, Duration::from_millis(500)),
            (0.0, true)
        );
    }

    #[test]
    fn gopherus_arrows_transition_adjacent_links_before_scrolling() {
        let lines = vec![
            protocol_line(0.0, true),
            protocol_line(20.0, true),
            protocol_line(40.0, false),
            protocol_line(60.0, true),
        ];
        assert_eq!(
            gopherus_arrow(&lines, Some(0), 0.0, 60.0, 100.0, 1),
            GopherusPosition {
                selected: Some(1),
                scroll: 0.0,
            }
        );
    }

    #[test]
    fn gopherus_scroll_retargets_to_the_nearest_link_without_skipping() {
        let lines = vec![
            protocol_line(0.0, true),
            protocol_line(20.0, false),
            protocol_line(40.0, true),
            protocol_line(60.0, true),
            protocol_line(80.0, false),
        ];
        assert_eq!(
            gopherus_arrow(&lines, Some(0), 0.0, 60.0, 100.0, 1),
            GopherusPosition {
                selected: Some(2),
                scroll: 20.0,
            }
        );
        // At a pinned document edge, arrows walk the visible links instead.
        assert_eq!(
            gopherus_arrow(&lines[..3], Some(0), 0.0, 60.0, 60.0, 1).selected,
            Some(2)
        );
    }

    fn image_request(index: usize) -> trust::render::ImageRequest {
        let source = format!("https://images.example/{index}.png");
        trust::render::ImageRequest {
            handle: ImageHandle::for_source(&source),
            source,
        }
    }

    fn animated_gif_source(repeat: image::codecs::gif::Repeat) -> trust::img::GraphicalAnimation {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(&mut bytes, 30);
            encoder.set_repeat(repeat).unwrap();
            for (color, delay) in [([255, 0, 0, 255], 20), ([0, 0, 255, 255], 30)] {
                encoder
                    .encode_frame(image::Frame::from_parts(
                        image::RgbaImage::from_pixel(1, 1, image::Rgba(color)),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(delay, 1),
                    ))
                    .unwrap();
            }
        }
        trust::img::decode_graphical_image_for_source("https://e.test/a.gif", bytes)
            .unwrap()
            .animation
            .unwrap()
    }

    #[test]
    fn animation_player_obeys_frame_deadlines_and_stops_on_the_final_finite_frame() {
        let source = animated_gif_source(image::codecs::gif::Repeat::Finite(2));
        let mut animation = WorkerAnimation::new(source);
        let started = Instant::now();
        assert!(animation.activate(started).unwrap().is_none());
        assert_eq!(
            animation.deadline,
            Some(started + Duration::from_millis(20))
        );

        let second = animation
            .advance(started + Duration::from_millis(20))
            .unwrap()
            .unwrap();
        assert_eq!(&second.rgba[..4], &[0, 0, 255, 255]);
        assert_eq!(
            animation.deadline,
            Some(started + Duration::from_millis(50))
        );

        let restarted = animation
            .advance(started + Duration::from_millis(50))
            .unwrap()
            .unwrap();
        assert_eq!(&restarted.rgba[..4], &[255, 0, 0, 255]);
        animation
            .advance(started + Duration::from_millis(70))
            .unwrap()
            .unwrap();
        assert!(
            animation
                .advance(started + Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
        assert!(animation.finished);
        assert!(animation.deadline.is_none());
    }

    #[test]
    fn inactive_animations_release_decoder_canvases_and_zero_delays_are_bounded() {
        let source = animated_gif_source(image::codecs::gif::Repeat::Finite(1));
        let mut animation = WorkerAnimation::new(source);
        animation.activate(Instant::now()).unwrap();
        assert!(animation.decoder.is_some());
        animation.deactivate();
        assert!(animation.decoder.is_none());
        assert!(animation.deadline.is_none());
        let restarted = animation.activate(Instant::now()).unwrap().unwrap();
        assert_eq!(&restarted.rgba[..4], &[255, 0, 0, 255]);
        assert!(animation.advance(Instant::now()).unwrap().is_some());
        assert!(animation.advance(Instant::now()).unwrap().is_none());
        assert!(
            animation.finished,
            "an aborted play must not consume the loop"
        );
        assert_eq!(
            animation_frame_delay(Duration::ZERO),
            MIN_ANIMATION_FRAME_DELAY
        );
        assert_eq!(
            animation_frame_delay(Duration::from_millis(125)),
            Duration::from_millis(125)
        );
    }

    #[test]
    fn compressed_animation_sources_have_a_bounded_lru() {
        let source = animated_gif_source(image::codecs::gif::Repeat::Infinite);
        let mut store = AnimationSourceStore::default();
        let mut evicted = Vec::new();
        for index in 0..=MAX_ANIMATION_SOURCES {
            evicted.extend(store.insert(ImageHandle(index as u64), source.clone()));
        }
        assert_eq!(store.entries.len(), MAX_ANIMATION_SOURCES);
        assert_eq!(evicted, vec![ImageHandle(0)]);
        assert!(!store.contains(ImageHandle(0)));
        assert!(store.contains(ImageHandle(MAX_ANIMATION_SOURCES as u64)));

        let images = ImageStore::default();
        let visible: HashSet<_> = (1..=MAX_ACTIVE_ANIMATIONS + 1)
            .map(|index| ImageHandle(index as u64))
            .collect();
        for handle in &visible {
            images.insert(
                *handle,
                ImageResource {
                    width: 1,
                    height: 1,
                    rgba: Arc::from([0, 0, 0, 255]),
                    has_alpha: false,
                },
            );
        }
        store.touch_visible(&visible);
        assert_eq!(
            store.bounded_active_handles(&visible, &images).len(),
            MAX_ACTIVE_ANIMATIONS
        );
    }

    #[test]
    fn desktop_image_scheduler_bounds_deduplicates_and_resets_requests() {
        let page = url::Url::parse("https://example.com/gallery").unwrap();
        let mut scheduler = ImageLoadScheduler::default();
        assert!(scheduler.reset(7, &page));
        let first_epoch = scheduler.epoch();
        for index in 0..20 {
            scheduler.enqueue(image_request(index));
            scheduler.enqueue(image_request(index));
        }

        let first = scheduler.take_ready(IMAGE_FETCH_CONCURRENCY);
        assert_eq!(first.len(), IMAGE_FETCH_CONCURRENCY);
        assert_eq!(scheduler.pending.len(), 20, "queued and active are pending");
        assert!(scheduler.finish(first[0].handle, true));
        assert!(!scheduler.finish(first[0].handle, true));
        assert_eq!(scheduler.take_ready(1).len(), 1);
        scheduler.enqueue(first[0].clone());
        assert_eq!(
            scheduler.pending.len(),
            19,
            "available images are not restarted"
        );

        let next = url::Url::parse("https://example.com/next").unwrap();
        assert!(scheduler.reset(7, &next));
        assert_ne!(scheduler.epoch(), first_epoch);
        assert!(scheduler.pending.is_empty());
        assert!(scheduler.failed.is_empty());
        assert!(scheduler.completed.is_empty());
    }

    #[test]
    fn retiring_a_document_stops_requests_without_erasing_available_images() {
        let page = url::Url::parse("https://example.com/gallery").unwrap();
        let mut scheduler = ImageLoadScheduler::default();
        assert!(scheduler.reset(7, &page));
        let available = image_request(1);
        let pending = image_request(2);
        scheduler.mark_cached(available.handle);
        scheduler.enqueue(pending.clone());
        let active_epoch = scheduler.epoch();

        assert!(scheduler.retire());
        assert_ne!(scheduler.epoch(), active_epoch);
        assert!(scheduler.pending.is_empty());
        assert!(scheduler.completed.contains(&available.handle));
        scheduler.enqueue(pending);
        assert!(scheduler.take_ready(1).is_empty());
        assert!(
            !scheduler.reset(7, &page),
            "painting the frozen document must not reactivate its scheduler"
        );

        let next = url::Url::parse("https://example.com/next").unwrap();
        assert!(scheduler.reset(8, &next));
        scheduler.enqueue(image_request(3));
        assert_eq!(scheduler.take_ready(1).len(), 1);
    }

    #[test]
    fn pending_navigation_keeps_the_last_complete_page_pixels() {
        let old = CssSize::new(800.0, 600.0);
        let resized = CssSize::new(1200.0, 700.0);
        assert!(may_reuse_page_layout(
            9, 9, 4, 5, old, resized, 1.0, 2.0, false, true, false,
        ));
        assert!(
            !may_reuse_page_layout(9, 10, 4, 5, old, resized, 1.0, 2.0, false, true, false),
            "a committed replacement has a new document identity"
        );
        assert!(
            may_reuse_page_layout(9, 9, 4, 5, old, resized, 1.0, 2.0, false, false, true),
            "a failed or stopped navigation keeps the retired page frozen"
        );
        assert!(
            !may_reuse_page_layout(9, 9, 4, 5, old, resized, 1.0, 2.0, false, false, false),
            "outside navigation, a stale static layout must reflow"
        );
    }

    #[test]
    fn cached_image_enters_available_state_only_once() {
        let page = url::Url::parse("https://example.com/").unwrap();
        let mut scheduler = ImageLoadScheduler::default();
        scheduler.reset(1, &page);
        let handle = image_request(1).handle;
        assert!(scheduler.mark_cached(handle));
        assert!(!scheduler.mark_cached(handle));
        scheduler.enqueue(image_request(1));
        assert!(scheduler.take_ready(1).is_empty());
    }

    #[test]
    fn pending_images_keep_replaced_geometry_until_available_or_broken() {
        let pending = image_request(1);
        let available = image_request(2);
        let broken = image_request(3);
        let mut decoded = trust::layout2::ImageSizes::new();
        decoded.insert(available.source.clone(), (640, 480));
        let failed = HashSet::from([broken.handle]);
        let sources = vec![
            pending.source.clone(),
            available.source.clone(),
            broken.source.clone(),
        ];

        let sizes = image_sizes_for_layout(&decoded, &failed, &sources);

        assert_eq!(sizes.get(&pending.source), Some(&PENDING_IMAGE_SIZE));
        assert_eq!(sizes.get(&available.source), Some(&(640, 480)));
        assert_eq!(sizes.get(&broken.source), None);
    }

    #[test]
    fn lazy_visibility_applies_nested_transforms_and_skips_far_images() {
        let near = image_request(1).handle;
        let far = image_request(2).handle;
        let commands = vec![
            trust::render::DisplayCommand::PushTransform(trust::render::Affine2d::translate(
                0.0, -800.0,
            )),
            trust::render::DisplayCommand::Image {
                rect: CssRect::new(0.0, 900.0, 100.0, 100.0),
                handle: near,
                source_rect: None,
                fit: trust::render::ImageFit::Contain,
                sampling: trust::render::ImageSampling::Smooth,
                clip: None,
                node: 1,
                link: None,
            },
            trust::render::DisplayCommand::PopTransform,
            trust::render::DisplayCommand::Image {
                rect: CssRect::new(0.0, 5_000.0, 100.0, 100.0),
                handle: far,
                source_rect: None,
                fit: trust::render::ImageFit::Contain,
                sampling: trust::render::ImageSampling::Smooth,
                clip: None,
                node: 2,
                link: None,
            },
        ];
        let mut visible = HashSet::new();
        collect_visible_image_handles(
            &commands,
            CssRect::new(0.0, 0.0, 800.0, 1_800.0),
            &mut visible,
        );
        assert!(visible.contains(&near));
        assert!(!visible.contains(&far));
    }
}
