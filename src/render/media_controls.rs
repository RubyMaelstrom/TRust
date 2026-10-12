//! The user agent's playback controls of inline media elements (HTML
//! #expose-a-user-interface-to-the-user). They are browser UI, not a CSS
//! z-index or a document top-layer entry. Keep only their anchor's spatial
//! scopes: author paint and compositing effects cannot cover the controls.
//! Clips still prevent controls leaking from an offscreen/closed player. HTML
//! #focusable-area identifies the media element as the DOM anchor of its
//! native subwidgets. The external player (mpv) is offered by the media
//! element's context menu (`PagePaint::media_targets`), never over the page.
//!
//! Local HTML snapshot e5071a20c8569d8a3ec02ed27dd01b948773f850.

use super::*;
use crate::dom::media::MediaControlsState;

/// An inline media element's playback controls, in its border box.
pub(crate) struct MediaControlsAnchor {
    pub rect: CssRect,
    pub state: MediaControlsState,
    pub video: bool,
}

impl PagePaint {
    /// Compile once per paint transaction, not per frame or pointer event.
    /// `anchor` filters eligible media and supplies its actual replaced box.
    pub(crate) fn collect_browser_media(
        &mut self,
        mut anchor: impl FnMut(&HitRegion) -> Option<MediaControlsAnchor>,
    ) {
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut collect = |commands: &[Primitive], fixed: bool| {
            let start = result.len();
            let seen_before = seen.len();
            if fixed {
                result.push(Primitive::BeginFixed);
            }
            // Preserve the spatial program, not just the active stack. Clip
            // and transform stacks are independent: a clip can outlive the
            // transform in which it was defined. Replaying their ordering is
            // essential. No author glyphs, images, layers or hits are copied.
            for command in commands {
                if matches!(
                    command,
                    Primitive::PushClip(_)
                        | Primitive::PopClip
                        | Primitive::PushTransform(_)
                        | Primitive::PopTransform
                        | Primitive::BeginScroll(_)
                        | Primitive::EndScroll
                        | Primitive::BeginSticky(_)
                        | Primitive::EndSticky
                        | Primitive::BeginFixed
                        | Primitive::EndFixed
                        | Primitive::BeginCssAnimation(_)
                        | Primitive::EndCssAnimation
                        | Primitive::BeginMarquee(_)
                        | Primitive::EndMarquee
                ) {
                    result.push(command.clone());
                    continue;
                }
                let Primitive::HitRegion(hit) = command else {
                    continue;
                };
                if seen.contains(&hit.node) {
                    continue;
                }
                let Some(media) = anchor(hit) else {
                    continue;
                };
                if media.rect.width <= 0. || media.rect.height <= 0. {
                    continue;
                }
                seen.insert(hit.node);
                append_media_controls(&mut result, hit, media.rect, &media.state, media.video);
            }
            if fixed {
                result.push(Primitive::EndFixed);
            }
            if seen.len() == seen_before {
                result.truncate(start);
            }
        };
        collect(&self.fixed_under_primitives, true);
        collect(&self.primitives, false);
        if !self.fixed_interleaved {
            collect(&self.fixed_primitives, true);
        }
        for entry in &self.top_layer {
            collect(&entry.primitives, entry.fixed);
        }
        self.browser_media = result;
    }

    pub(crate) fn browser_media_nodes(&self) -> std::collections::HashSet<usize> {
        self.browser_media
            .iter()
            .filter_map(|command| match command {
                Primitive::HitRegion(hit) => Some(hit.node),
                _ => None,
            })
            .collect()
    }
}

/// The parts of a media element's control bar, in CSS px. The page actor's
/// hit test of the same controls (`mediaControlsLayout` in the platform
/// prelude) uses this geometry; keep them identical.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MediaControlsLayout {
    pub bar: CssRect,
    pub play: CssRect,
    pub mute: Option<CssRect>,
    pub time: Option<CssRect>,
    pub scrubber: Option<CssRect>,
    pub fullscreen: Option<CssRect>,
}

const CONTROL_BAR_HEIGHT: f32 = 36.;
const CONTROL_TIME_WIDTH: f32 = 92.;

pub(crate) fn media_controls_layout(rect: CssRect, video: bool) -> MediaControlsLayout {
    let height = if video {
        CONTROL_BAR_HEIGHT.min(rect.height)
    } else {
        rect.height
    };
    let bar = CssRect::new(rect.x, rect.y + rect.height - height, rect.width, height);
    let square = |x: f32| CssRect::new(x, bar.y, height, height);
    let play = square(bar.x);
    let mut end = bar.x + bar.width;
    let fullscreen = (video && bar.width >= 4. * height).then(|| {
        end -= height;
        square(end)
    });
    let mute = (bar.width >= 3. * height).then(|| square(play.x + height));
    let time = mute
        .filter(|_| bar.width >= 3. * height + CONTROL_TIME_WIDTH + 40.)
        .map(|mute| CssRect::new(mute.x + height, bar.y, CONTROL_TIME_WIDTH, height));
    let start = time
        .or(mute)
        .map_or(play.x + height, |part| part.x + part.width)
        + 8.;
    let scrubber =
        (end - 10. - start >= 30.).then(|| CssRect::new(start, bar.y, end - 10. - start, height));
    MediaControlsLayout {
        bar,
        play,
        mute,
        time,
        scrubber,
        fullscreen,
    }
}

/// `m:ss` (or `h:mm:ss`) for the control bar's time display.
fn clock_text(seconds: f32) -> String {
    if !seconds.is_finite() || seconds < 0. {
        return String::from("--:--");
    }
    let total = seconds.floor() as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Draw a media element's controls (when shown) and claim their area for
/// the element: presses there reach the page actor as presses on the media
/// element, which operates the controls instead of dispatching page events
/// (HTML #expose-a-user-interface-to-the-user).
fn append_media_controls(
    commands: &mut Vec<Primitive>,
    hit: &HitRegion,
    anchor: CssRect,
    state: &MediaControlsState,
    video: bool,
) {
    if video && !state.visible {
        return;
    }
    let layout = media_controls_layout(anchor, video);
    let white = PaintColor::Rgba(255, 255, 255, 255);
    let dim = PaintColor::Rgba(255, 255, 255, 110);
    commands.push(Primitive::PushClip(PaintShape::Rect(layout.bar)));
    commands.push(Primitive::FillRect {
        rect: layout.bar,
        color: if video {
            PaintColor::Rgba(0, 0, 0, 150)
        } else {
            PaintColor::Rgba(32, 33, 36, 255)
        },
    });
    // Play (a triangle) or pause (two bars).
    let icon = |part: CssRect, size: f32| {
        CssRect::new(
            part.x + (part.width - size) / 2.,
            part.y + (part.height - size) / 2.,
            size,
            size,
        )
    };
    let glyph = icon(layout.play, (layout.play.height * 0.42).min(16.));
    if state.paused {
        commands.push(Primitive::FillPolygon {
            points: vec![
                CssPoint::new(glyph.x + glyph.width * 0.15, glyph.y),
                CssPoint::new(glyph.x + glyph.width, glyph.y + glyph.height / 2.),
                CssPoint::new(glyph.x + glyph.width * 0.15, glyph.y + glyph.height),
            ],
            color: white,
        });
    } else {
        for x in [glyph.x + glyph.width * 0.1, glyph.x + glyph.width * 0.6] {
            commands.push(Primitive::FillRect {
                rect: CssRect::new(x, glyph.y, glyph.width * 0.3, glyph.height),
                color: white,
            });
        }
    }
    // A speaker, crossed out when muted.
    if let Some(mute) = layout.mute {
        let glyph = icon(mute, (mute.height * 0.42).min(16.));
        commands.push(Primitive::FillRect {
            rect: CssRect::new(
                glyph.x,
                glyph.y + glyph.height * 0.33,
                glyph.width * 0.25,
                glyph.height * 0.34,
            ),
            color: white,
        });
        commands.push(Primitive::FillPolygon {
            points: vec![
                CssPoint::new(glyph.x + glyph.width * 0.2, glyph.y + glyph.height * 0.33),
                CssPoint::new(glyph.x + glyph.width * 0.55, glyph.y),
                CssPoint::new(glyph.x + glyph.width * 0.55, glyph.y + glyph.height),
                CssPoint::new(glyph.x + glyph.width * 0.2, glyph.y + glyph.height * 0.67),
            ],
            color: white,
        });
        if state.muted {
            commands.push(Primitive::FillRect {
                rect: CssRect::new(
                    glyph.x + glyph.width * 0.7,
                    glyph.y + glyph.height * 0.3,
                    glyph.width * 0.3,
                    glyph.height * 0.4,
                ),
                color: PaintColor::Rgba(234, 67, 53, 255),
            });
        } else {
            for (offset, height) in [(0.7, 0.4), (0.88, 0.7)] {
                commands.push(Primitive::FillRect {
                    rect: CssRect::new(
                        glyph.x + glyph.width * offset,
                        glyph.y + glyph.height * (1. - height) / 2.,
                        glyph.width * 0.1,
                        glyph.height * height,
                    ),
                    color: white,
                });
            }
        }
    }
    if let Some(time) = layout.time {
        let style = crate::text::TextStyle {
            size: 12.,
            ..Default::default()
        };
        let text = format!(
            "{} / {}",
            clock_text(state.time),
            clock_text(state.duration)
        );
        let shaped = crate::text::shape(&text, &style);
        commands.push(Primitive::GlyphRun {
            origin: CssPoint::new(
                time.x,
                time.y + ((time.height - shaped.line_height) * 0.5).max(0.),
            ),
            shaped,
            color: white,
            decoration: TextDecorationPaint {
                color: white,
                style: DecorationStyle::Solid,
                thickness: None,
                underline_offset: None,
            },
            shadows: Vec::new(),
            clip: None,
            node: hit.node,
            link: None,
            selection: Default::default(),
        });
    }
    if let Some(scrubber) = layout.scrubber {
        let track = CssRect::new(
            scrubber.x,
            scrubber.y + scrubber.height / 2. - 2.,
            scrubber.width,
            4.,
        );
        commands.push(Primitive::FillRect {
            rect: track,
            color: dim,
        });
        let fraction = if state.duration.is_finite() && state.duration > 0. {
            (state.time / state.duration).clamp(0., 1.)
        } else {
            0.
        };
        let filled = CssRect::new(track.x, track.y, track.width * fraction, track.height);
        commands.push(Primitive::FillRect {
            rect: filled,
            color: white,
        });
        commands.push(Primitive::FillRect {
            rect: CssRect::new(filled.x + filled.width - 5., track.y - 3., 10., 10.),
            color: white,
        });
    }
    if let Some(fullscreen) = layout.fullscreen {
        // Four corners.
        let glyph = icon(fullscreen, (fullscreen.height * 0.4).min(14.));
        let (thin, long) = (2., glyph.width * 0.35);
        for (x, y, right, bottom) in [
            (glyph.x, glyph.y, false, false),
            (glyph.x + glyph.width, glyph.y, true, false),
            (glyph.x, glyph.y + glyph.height, false, true),
            (glyph.x + glyph.width, glyph.y + glyph.height, true, true),
        ] {
            let horizontal_x = if right { x - long } else { x };
            let vertical_y = if bottom { y - long } else { y };
            commands.push(Primitive::FillRect {
                rect: CssRect::new(horizontal_x, if bottom { y - thin } else { y }, long, thin),
                color: white,
            });
            commands.push(Primitive::FillRect {
                rect: CssRect::new(if right { x - thin } else { x }, vertical_y, thin, long),
                color: white,
            });
        }
    }
    commands.push(Primitive::HitRegion(HitRegion {
        rect: layout.bar,
        node: hit.node,
        actor: hit.actor,
        link: None,
        cursor: Some("default".into()),
    }));
    commands.push(Primitive::PopClip);
}

impl Scene {
    /// Compose UA playback controls after page/selection paint and before
    /// browser dialogs/chrome. CSSOM's scene deliberately never calls this.
    pub fn append_browser_media(
        &mut self,
        page: &PagePaint,
        scroll: CssPoint,
        elapsed_seconds: f32,
    ) {
        self.append_browser_media_with_scroll_offsets(
            page,
            scroll,
            elapsed_seconds,
            &HashMap::new(),
        );
    }

    pub fn append_browser_media_with_scroll_offsets(
        &mut self,
        page: &PagePaint,
        scroll: CssPoint,
        elapsed_seconds: f32,
        offsets: &HashMap<usize, CssPoint>,
    ) {
        if page.browser_media.is_empty() {
            return;
        }
        self.primitives
            .push(Primitive::PushClip(PaintShape::Rect(self.content_viewport)));
        self.primitives
            .push(Primitive::PushTransform(Affine2d::translate(
                self.content_viewport.x - scroll.x,
                self.content_viewport.y - scroll.y,
            )));
        self.append_sticky_commands(
            &page.browser_media,
            &scroll_container_index(page, offsets),
            scroll,
            elapsed_seconds,
            &mut None,
            &page.replaced_images,
        );
        self.primitives.push(Primitive::PopTransform);
        self.primitives.push(Primitive::PopClip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::Dom;
    use crate::dom::media::MediaPresentation;
    use crate::layout2::{GraphicalLayout, Viewport};

    fn layout(dom: &Dom) -> GraphicalLayout {
        crate::layout2::lay_out_graphical(
            dom,
            &url::Url::parse("https://example.test/watch").unwrap(),
            Viewport::new(640., 480.),
            &[],
            &Default::default(),
            &Default::default(),
        )
    }

    /// A document whose media elements are inline players, as a graphical
    /// page actor's (and a graphical static render's) are.
    fn inline_dom(html: &str) -> Dom {
        let mut dom = Dom::parse_document(html);
        dom.set_inline_media(true);
        dom
    }

    /// Give media element `node` a player presentation showing its controls.
    fn show_controls(dom: &mut Dom, node: usize) {
        dom.set_media_presentation(
            node,
            Some(MediaPresentation {
                surface: 0,
                natural: None,
                show_poster: true,
                controls: Some(MediaControlsState {
                    paused: true,
                    muted: false,
                    time: 0.,
                    duration: 10.,
                    visible: true,
                }),
            }),
        );
    }

    fn scene(page: &PagePaint, scroll: CssPoint) -> Scene {
        let mut scene = Scene {
            viewport: ViewportMetrics::from_physical(
                PhysicalSize::new(640, 480),
                Default::default(),
            ),
            primitives: Vec::new(),
            controls: Vec::new(),
            content_viewport: CssRect::new(0., 0., 640., 480.),
            image_store: Default::default(),
            canvas_images: Default::default(),
            page_scroll_containers: Vec::new(),
            page_size: Default::default(),
        };
        scene.append_page(page, scroll);
        scene.append_browser_media(page, scroll, 0.);
        scene
    }

    fn center(rect: CssRect) -> CssPoint {
        CssPoint::new(rect.x + rect.width / 2., rect.y + rect.height / 2.)
    }

    fn media_links(page: &PagePaint) -> usize {
        [&page.primitives, &page.browser_media]
            .into_iter()
            .flatten()
            .filter(|command| {
                matches!(command, Primitive::HitRegion(hit) if matches!(hit.link, Some(Link::Media(_))))
            })
            .count()
    }

    #[test]
    fn a_failed_players_target_stays_with_the_terminal_never_over_the_graphical_page() {
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><section id=player style="width:320px;height:180px;position:relative">
                <div id=inner><video id=v src="/clip.mp4"></video></div></section>"#,
        );
        let base = url::Url::parse("https://example.test/").unwrap();
        let video = dom.get_by_id("v").unwrap();
        let player = dom.get_by_id("player").unwrap();
        dom.set_doc_url(Some(base.clone()));
        dom.remember_media(video, &base);
        assert!(
            dom.retained_media_controls().is_empty(),
            "existing media owns its control"
        );
        dom.set_text(player, "Unable to play inline");
        assert_eq!(dom.retained_media_controls().len(), 1);
        // HTML #attr-media-controls: features beyond the controls "should
        // not interfere with the page's normal rendering". The graphical
        // page paints no button over the page's own error UI.
        let layout = layout(&dom);
        assert!(layout.paint.browser_media.is_empty());
        assert_eq!(media_links(&layout.paint), 0);
        assert!(layout.paint.media_targets.is_empty());
        assert_eq!(
            layout.boxes[&player].height, 180.,
            "native UI does not change CSS geometry"
        );
        assert!(
            !dom.serialize_js(crate::dom::DOCUMENT).contains("mpv"),
            "no synthetic author DOM"
        );
        // The terminal, which plays nothing inline, still offers it.
        let terminal = crate::layout2::lay_out_document(
            &dom,
            &base,
            crate::layout2::TerminalViewport::new(80, 30, 8., 16.),
            &[],
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );
        assert!(
            terminal
                .rows
                .iter()
                .flat_map(|row| &row.items)
                .any(|item| matches!(item.link, Some(Link::Media(_))))
        );
        dom.detach(player);
        assert!(
            dom.retained_media_controls().is_empty(),
            "do not move stale controls to the page body"
        );
    }

    #[test]
    fn media_controls_retained_failure_respects_visibility_reload_and_document_lifetime() {
        let mut dom = Dom::parse_document(
            r#"<body><div id=player><video id=v src="/clip.mp4"></video></div>"#,
        );
        let base = url::Url::parse("https://example.test/").unwrap();
        let video = dom.get_by_id("v").unwrap();
        let player = dom.get_by_id("player").unwrap();
        dom.set_doc_url(Some(base.clone()));
        dom.remember_media(video, &base);
        dom.detach(video);
        assert_eq!(dom.retained_media_controls().len(), 1);
        dom.set_attr(player, "style", "display:none");
        assert!(dom.retained_media_controls().is_empty());
        dom.set_attr(player, "style", "");
        dom.forget_media(video);
        assert!(
            dom.retained_media_controls().is_empty(),
            "reload retires the previous failure"
        );
        dom.append(player, video);
        dom.remember_media(video, &base);
        dom.detach(video);
        dom.set_doc_url(Some(
            url::Url::parse("https://example.test/#chapter").unwrap(),
        ));
        assert_eq!(
            dom.retained_media_controls().len(),
            1,
            "fragment navigation preserves playback"
        );
        dom.set_doc_url(Some(url::Url::parse("https://example.test/next").unwrap()));
        assert!(
            dom.retained_media_controls().is_empty(),
            "same-document route changes retire old resources"
        );
    }

    #[test]
    fn a_frames_player_is_offered_from_its_own_document() {
        let mut dom = inline_dom(r#"<body><iframe id=frame width=320 height=180></iframe>"#);
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            "<body style=margin:0><div><video id=video width=320 height=180></video></div>",
            "https://player.example.test/embed/123",
        )
        .unwrap();
        let video = dom
            .descendants(frame)
            .find(|&id| dom.attr(id, "id") == Some("video"))
            .unwrap();
        let body = dom
            .descendants(frame)
            .find(|&id| dom.tag_name(id) == Some("body"))
            .unwrap();
        let painted = layout(&dom);
        assert_eq!(
            painted
                .paint
                .media_targets
                .get(&video)
                .map(url::Url::as_str),
            Some("https://player.example.test/embed/123"),
            "a sourceless player plays its frame's page"
        );
        assert!(painted.paint.browser_media.is_empty());
        assert_eq!(media_links(&painted.paint), 0);
        let scene = scene(&painted.paint, CssPoint::default());
        let rect = scene.ordered_focus_hits(&[video]).pop().unwrap().rect;
        assert_eq!(
            scene
                .media_target_at(&painted.paint, center(rect))
                .map(|url| url.to_string()),
            Some(String::from("https://player.example.test/embed/123"))
        );
        // Player libraries can replace a failed player with their own UI.
        dom.set_text(body, "Network error");
        let painted = layout(&dom);
        assert!(painted.paint.media_targets.is_empty());
        assert!(painted.paint.browser_media.is_empty());
    }

    #[test]
    fn media_controls_blob_failure_uses_the_document_url_despite_an_asset_base() {
        let mut dom = Dom::parse_document(
            r#"<head><base href="https://cdn.example.test/assets/"></head>
                <body><section id=player><video id=v src="blob:https://example.test/123"></video></section>"#,
        );
        let base = url::Url::parse("https://example.test/watch/123").unwrap();
        let video = dom.get_by_id("v").unwrap();
        let player = dom.get_by_id("player").unwrap();
        dom.remember_media(video, &base);
        dom.set_text(player, "Unable to play inline");
        assert_eq!(dom.retained_media_controls().get(&player), Some(&base));
    }

    #[test]
    fn media_controls_in_frames_use_the_player_document_and_its_resource_base() {
        let mut dom = Dom::parse_document(
            r#"<body><a href="/outer-link"><iframe id=frame width=320 height=180></iframe></a>"#,
        );
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(frame,
            r#"<head><base href="https://cdn.example.test/assets/"></head><body><video id=video></video>"#,
            "https://player.example.test/embed/123").unwrap();
        let video = dom
            .descendants(frame)
            .find(|&id| dom.attr(id, "id") == Some("video"))
            .unwrap();
        let base = url::Url::parse("https://outer.example.test/page").unwrap();
        assert_eq!(
            crate::layout2::media_target(&dom, &base, video)
                .unwrap()
                .as_str(),
            "https://player.example.test/embed/123",
            "external playback uses the final frame page, not its embedding link or asset base"
        );
        dom.set_attr(video, "src", "clip.mp4");
        assert_eq!(
            crate::layout2::media_target(&dom, &base, video)
                .unwrap()
                .as_str(),
            "https://cdn.example.test/assets/clip.mp4",
            "dynamic sources resolve in their own Document"
        );
    }

    #[test]
    fn media_under_a_players_layers_is_found_beneath_them_and_nothing_paints_over_the_page() {
        // A custom player: the video under its poster copy and a page-wide
        // link (X- and erome-style players position both over the video).
        let dom = inline_dom(
            r#"<body style="margin:0">
          <div style="position:relative;width:320px;height:180px">
            <video id=v src="clip.mp4" poster="poster.jpg" style="position:absolute;inset:0;width:100%;height:100%"></video>
            <img id=poster src="poster.jpg" style="position:absolute;inset:0;width:100%;height:100%">
          </div>
          <a id=cover href=/page style="position:fixed;inset:0;z-index:2147483647;background:blue">Page</a>"#,
        );
        let layout = layout(&dom);
        let v = dom.get_by_id("v").unwrap();
        let cover = dom.get_by_id("cover").unwrap();
        assert_eq!(layout.boxes[&v].width, 320.);
        assert!(layout.paint.browser_media.is_empty());
        assert_eq!(
            media_links(&layout.paint),
            0,
            "neither the video nor its poster copy becomes a link to mpv"
        );
        assert_eq!(
            layout.paint.media_targets.get(&v).map(|url| url.path()),
            Some("/clip.mp4")
        );
        let scene = scene(&layout.paint, CssPoint::default());
        let point = CssPoint::new(160., 90.);
        assert_eq!(
            page_element_hits_at(
                &layout.paint,
                CssSize::new(640., 480.),
                CssPoint::default(),
                point
            )[0]
            .node,
            cover,
            "the page's own layers stay on top for its events"
        );
        assert!(
            scene
                .page_hit_at(point)
                .is_none_or(|hit| !matches!(hit.link, Some(Link::Media(_))))
        );
        assert_eq!(
            scene
                .media_target_at(&layout.paint, point)
                .map(|url| url.path().to_string()),
            Some(String::from("/clip.mp4")),
            "the context menu finds the video beneath the layers"
        );
        assert_eq!(
            scene.media_target_at(&layout.paint, CssPoint::new(500., 400.)),
            None
        );
        let frame = vello_cpu::VelloCpuRenderer::new()
            .render_rgba(&scene)
            .unwrap();
        let at = (400 * 640 + 600) * 4;
        assert_eq!(
            &frame.pixels[at..at + 4],
            &[0, 0, 255, 255],
            "the page's own top layer stays on top"
        );
    }

    #[test]
    fn media_controls_follow_nested_scroll_transform_and_viewport_scroll() {
        let mut dom = inline_dom(
            r#"<body style="margin:0">
          <div id=scroller style="margin:40px;width:320px;height:200px;overflow:auto">
            <div style="height:70px"></div>
            <div style="transform:translate(30px,20px)">
              <video id=v src=clip.mp4 style="width:240px;height:100px"></video>
            </div><div style="height:400px"></div>
          </div>"#,
        );
        let v = dom.get_by_id("v").unwrap();
        show_controls(&mut dom, v);
        let mut layout = layout(&dom);
        let scroller = dom.get_by_id("scroller").unwrap();
        let bar = |scene: &Scene| {
            let rect = scene.ordered_focus_hits(&[v])[0].rect;
            media_controls_layout(rect, true).bar
        };
        let before = bar(&scene(&layout.paint, CssPoint::default()));
        assert_eq!(
            scene(&layout.paint, CssPoint::default())
                .page_hit_at(center(before))
                .map(|hit| hit.node),
            Some(v)
        );
        layout
            .paint
            .scroll_containers
            .iter_mut()
            .find(|c| c.node == scroller)
            .unwrap()
            .offset
            .y = 30.;
        let scene = scene(&layout.paint, CssPoint::new(0., 10.));
        let after = bar(&scene);
        assert_eq!((after.x, after.y), (before.x, before.y - 40.));
        assert_eq!(
            scene.page_hit_at(center(after)).map(|hit| hit.node),
            Some(v)
        );
        // Controls clipped outside their scrollport must not become a
        // screen-fixed bar.
        layout
            .paint
            .scroll_containers
            .iter_mut()
            .find(|c| c.node == scroller)
            .unwrap()
            .offset
            .y = 250.;
        let clipped = self::scene(&layout.paint, CssPoint::default());
        let rect = bar(&clipped);
        assert!(
            clipped
                .page_hit_at(center(rect))
                .is_none_or(|hit| hit.node != v)
        );
    }

    #[test]
    fn media_targets_follow_hit_testing_and_sources() {
        let mut dom = inline_dom(
            r#"<body style="margin:0">
          <video id=v src=first.mp4></video>
          <video id=hidden src=hidden.mp4 style="display:none"></video>
          <video id=transparent src=transparent.mp4 style="opacity:0"></video>
          <video id=invisible src=invisible.mp4 style="visibility:hidden"></video>
          <div inert><video id=inert src=inert.mp4></video></div>
          <a href=/watch/1><video id=stream></video></a>
          <audio id=silent src=a.mp3></audio>
          <audio id=player controls src=b.mp3></audio>
          <audio id=sourceless controls></audio>"#,
        );
        let id = |name: &str| dom.get_by_id(name).unwrap();
        let targets = |dom: &Dom| {
            let mut targets: Vec<(usize, String)> = layout(dom)
                .paint
                .media_targets
                .into_iter()
                .map(|(node, url)| (node, url.path().to_string()))
                .collect();
            targets.sort();
            targets
        };
        let mut expected = vec![
            (id("v"), String::from("/first.mp4")),
            // Transparent content is still hit, as CSS hit testing has it.
            (id("transparent"), String::from("/transparent.mp4")),
            // A sourceless (MSE) player plays the page its link names.
            (id("stream"), String::from("/watch/1")),
            (id("player"), String::from("/b.mp3")),
        ];
        expected.sort();
        assert_eq!(targets(&dom), expected);
        let v = id("v");
        dom.set_attr(v, "src", "second.mp4");
        assert!(targets(&dom).contains(&(v, String::from("/second.mp4"))));
        dom.set_attr(v, "style", "display:none");
        assert!(!targets(&dom).iter().any(|(node, _)| *node == v));
    }

    #[test]
    fn media_controls_keyboard_order_uses_dom_anchors_not_paint_order() {
        let mut dom = inline_dom(
            r#"<button id=before>Before</button>
          <video id=normal></video><video id=early tabindex=2></video>
          <video id=skip tabindex=-1></video><div inert><video id=inert></video></div>
          <a id=after href=/next>After</a>"#,
        );
        for name in ["normal", "early", "skip", "inert"] {
            let node = dom.get_by_id(name).unwrap();
            show_controls(&mut dom, node);
        }
        let layout = layout(&dom);
        let order = dom.sequential_focus_order_with_subwidgets(
            &layout.boxes,
            &layout.paint.browser_media_nodes(),
        );
        assert_eq!(
            order
                .iter()
                .map(|&n| dom.attr(n, "id").unwrap())
                .collect::<Vec<_>>(),
            ["early", "before", "normal", "after"]
        );
        let scene = scene(&layout.paint, CssPoint::default());
        let targets = scene.ordered_focus_hits(&order);
        assert_eq!(
            targets.iter().map(|hit| hit.node).collect::<Vec<_>>(),
            order
        );
    }

    #[test]
    fn media_controls_preserve_clips_that_outlive_their_transform() {
        let mut page = PagePaint {
            primitives: vec![
                Primitive::PushTransform(Affine2d::translate(100., 0.)),
                Primitive::PushClip(PaintShape::Rect(CssRect::new(0., 0., 100., 100.))),
                Primitive::PopTransform,
                Primitive::HitRegion(HitRegion {
                    rect: CssRect::new(100., 0., 100., 100.),
                    node: 7,
                    actor: None,
                    cursor: None,
                    link: None,
                }),
                Primitive::PopClip,
            ],
            ..Default::default()
        };
        page.collect_browser_media(|hit| {
            Some(MediaControlsAnchor {
                rect: hit.rect,
                state: MediaControlsState {
                    paused: true,
                    muted: false,
                    time: 0.,
                    duration: 1.,
                    visible: true,
                },
                video: true,
            })
        });
        let scene = scene(&page, CssPoint::default());
        // The bar spans the bottom of the video, inside the clip only as
        // the transform placed it.
        assert_eq!(
            scene
                .page_hit_at(CssPoint::new(110., 90.))
                .map(|hit| hit.node),
            Some(7)
        );
    }
}
