//! HTML #the-video-element permits an external playback utility when the UA
//! cannot render video. This is browser UI, not a CSS z-index or a document
//! top-layer entry. Keep only its anchor's spatial scopes: author paint and
//! compositing effects cannot cover the button. Clips still prevent controls
//! leaking from an offscreen/closed player. HTML #focusable-area identifies the
//! media element as the DOM anchor of its native subwidgets.
//!
//! Local HTML snapshot e5071a20c8569d8a3ec02ed27dd01b948773f850.

use super::*;

impl PagePaint {
    /// Compile once per paint transaction, not per frame or pointer event.
    /// `anchor` filters eligible media and supplies its actual replaced box.
    pub(crate) fn collect_browser_media(
        &mut self,
        mut anchor: impl FnMut(usize, CssRect) -> Option<CssRect>,
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
                let Some(Link::Media(_)) = &hit.link else {
                    continue;
                };
                if seen.contains(&hit.node) {
                    continue;
                }
                let Some(rect) = anchor(hit.node, hit.rect) else {
                    continue;
                };
                if rect.width <= 0. || rect.height <= 0. {
                    continue;
                }
                seen.insert(hit.node);
                append_button(&mut result, hit, rect);
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

fn append_button(commands: &mut Vec<Primitive>, hit: &HitRegion, anchor: CssRect) {
    let style = crate::text::TextStyle {
        size: 13.,
        weight: 600.,
        ..Default::default()
    };
    let mut shaped = crate::text::shape("▶ Open in mpv", &style);
    if anchor.width < shaped.advance + 12. {
        shaped = crate::text::shape("▶", &style);
    }
    let inset = if anchor.width > shaped.advance + 20. && anchor.height > shaped.line_height + 16. {
        4.
    } else {
        0.
    };
    let rect = CssRect::new(
        anchor.x + inset,
        anchor.y + inset,
        (shaped.advance + 12.).min(anchor.width - inset),
        (shaped.line_height + 8.).min(anchor.height - inset),
    );
    commands.push(Primitive::PushClip(PaintShape::Rect(rect)));
    commands.push(Primitive::FillRect {
        rect,
        color: PaintColor::Rgba(24, 28, 34, 255),
    });
    commands.push(Primitive::GlyphRun {
        origin: CssPoint::new(
            rect.x + ((rect.width - shaped.advance) * 0.5).max(0.),
            rect.y + ((rect.height - shaped.line_height) * 0.5).max(0.),
        ),
        shaped,
        color: PaintColor::Rgba(255, 255, 255, 255),
        decoration: TextDecorationPaint {
            color: PaintColor::Rgba(255, 255, 255, 255),
            style: DecorationStyle::Solid,
        },
        shadows: Vec::new(),
        clip: None,
        node: hit.node,
        link: None,
    });
    commands.push(Primitive::HitRegion(HitRegion {
        rect,
        node: hit.node,
        actor: None,
        link: hit.link.clone(),
        cursor: Some("pointer".into()),
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
        self.append_sticky_commands(&page.browser_media, page, scroll, elapsed_seconds);
        self.primitives.push(Primitive::PopTransform);
        self.primitives.push(Primitive::PopClip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::Dom;
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

    #[test]
    fn media_controls_override_author_layers_without_changing_cssom_or_layout() {
        let dom = Dom::parse_document(
            r#"<body style="margin:0">
          <div style="position:relative;width:320px;height:180px">
            <video id=v src="clip.mp4" style="position:absolute;inset:0;width:100%;height:100%"></video>
            <div id=poster style="position:absolute;inset:0;background:red">Poster</div>
          </div>
          <a id=cover href=/page style="position:fixed;inset:0;z-index:2147483647;background:blue">Page</a>"#,
        );
        let layout = layout(&dom);
        let v = dom.get_by_id("v").unwrap();
        let cover = dom.get_by_id("cover").unwrap();
        assert!(layout.boxes.contains_key(&dom.get_by_id("poster").unwrap()));
        assert_eq!(layout.paint.browser_media_nodes(), [v].into());
        let scene = scene(&layout.paint, CssPoint::default());
        let target = scene.ordered_focus_hits(&[v]).pop().unwrap();
        assert!(matches!(&target.link, Some(Link::Media(url)) if url.path() == "/clip.mp4"));
        assert_eq!(
            scene.page_hit_at(center(target.rect)).unwrap().link,
            target.link
        );
        assert_eq!(
            page_element_hits_at(
                &layout.paint,
                CssSize::new(640., 480.),
                CssPoint::default(),
                center(target.rect)
            )[0]
            .node,
            cover
        );
        let frame = vello_cpu::VelloCpuRenderer::new()
            .render_rgba(&scene)
            .unwrap();
        let at = ((target.rect.y as usize + 1) * 640 + target.rect.x as usize + 1) * 4;
        assert_eq!(
            &frame.pixels[at..at + 4],
            &[24, 28, 34, 255],
            "native background covers page blue"
        );
    }

    #[test]
    fn media_controls_follow_nested_scroll_transform_and_viewport_scroll() {
        let dom = Dom::parse_document(
            r#"<body style="margin:0">
          <div id=scroller style="margin:40px;width:320px;height:200px;overflow:auto">
            <div style="height:70px"></div>
            <div style="transform:translate(30px,20px)">
              <video id=v src=clip.mp4 style="width:240px;height:100px"></video>
            </div><div style="height:400px"></div>
          </div>"#,
        );
        let mut layout = layout(&dom);
        let v = dom.get_by_id("v").unwrap();
        let scroller = dom.get_by_id("scroller").unwrap();
        let before = scene(&layout.paint, CssPoint::default()).ordered_focus_hits(&[v])[0].rect;
        layout
            .paint
            .scroll_containers
            .iter_mut()
            .find(|c| c.node == scroller)
            .unwrap()
            .offset
            .y = 30.;
        let scene = scene(&layout.paint, CssPoint::new(0., 10.));
        let after = scene.ordered_focus_hits(&[v])[0].rect;
        assert_eq!((after.x, after.y), (before.x, before.y - 40.));
        assert!(matches!(
            scene.page_hit_at(center(after)).unwrap().link,
            Some(Link::Media(_))
        ));
        // A control clipped outside its scrollport must not become a screen-fixed button.
        layout
            .paint
            .scroll_containers
            .iter_mut()
            .find(|c| c.node == scroller)
            .unwrap()
            .offset
            .y = 170.;
        let clipped = self::scene(&layout.paint, CssPoint::default());
        let rect = clipped.ordered_focus_hits(&[v])[0].rect;
        assert!(
            !clipped
                .page_hit_at(center(rect))
                .is_some_and(|hit| matches!(hit.link, Some(Link::Media(_))))
        );
    }

    #[test]
    fn media_controls_respect_suppression_inertness_and_source_changes() {
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0">
          <video id=v src=first.mp4></video>
          <video src=hidden.mp4 style="display:none"></video>
          <video src=transparent.mp4 style="opacity:0"></video>
          <video src=invisible.mp4 style="visibility:hidden"></video>
          <div inert><video src=inert.mp4></video></div>
          <audio></audio>"#,
        );
        let v = dom.get_by_id("v").unwrap();
        assert_eq!(layout(&dom).paint.browser_media_nodes(), [v].into());
        dom.set_attr(v, "src", "second.mp4");
        let paint = layout(&dom).paint;
        let target = scene(&paint, CssPoint::default())
            .ordered_focus_hits(&[v])
            .pop()
            .unwrap();
        assert!(matches!(target.link, Some(Link::Media(url)) if url.path() == "/second.mp4"));
        dom.set_attr(v, "style", "display:none");
        assert!(layout(&dom).paint.browser_media.is_empty());
    }

    #[test]
    fn media_controls_keyboard_order_uses_dom_anchors_not_paint_order() {
        let dom = Dom::parse_document(
            r#"<button id=before>Before</button>
          <video id=normal></video><video id=early tabindex=2></video>
          <video id=skip tabindex=-1></video><div inert><video id=inert></video></div>
          <a id=after href=/next>After</a>"#,
        );
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
        assert!(matches!(targets[0].link, Some(Link::Media(_))));
        assert!(matches!(targets[2].link, Some(Link::Media(_))));
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
                    link: Some(Link::Media(
                        url::Url::parse("https://example.test/v.mp4").unwrap(),
                    )),
                }),
                Primitive::PopClip,
            ],
            ..Default::default()
        };
        page.collect_browser_media(|_, rect| Some(rect));
        let scene = scene(&page, CssPoint::default());
        assert!(matches!(
            scene.page_hit_at(CssPoint::new(110., 10.)).unwrap().link,
            Some(Link::Media(_))
        ));
    }
}
