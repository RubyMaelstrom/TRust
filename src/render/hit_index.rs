//! Retained CSSOM View hit testing, independent of raster scene construction.
//!
//! CSSOM View #dom-document-elementsfrompoint requires reverse paint order,
//! with the current transforms and clips, even for covered boxes. Compile
//! immutable paint into clip/scroll property nodes and local-space BVHs. A
//! scroll samples those properties; it does not copy paint, rebuild a BVH, or
//! project every box again. Static transforms remain exact narrow-phase tests.
//! Independent clip/transform stacks preserve clips that outlive a transform.
//!
//! CSSWG local snapshot 81c27f68690138345b2b3b6af8ccc42dad3dca1d:
//! CSSOM View §5, Transforms 1 #transform-rendering, Overflow 3 #scrolling,
//! Position 3 #stickypos-insets. The caller replaces this index with its paint
//! transaction; only retained scroll offsets and timeline samples may change.

use super::*;

#[derive(Clone, Copy, Debug)]
struct SpaceRef {
    dynamic: usize,
    local: Affine2d,
}

impl SpaceRef {
    fn root(dynamic: usize) -> Self {
        Self {
            dynamic,
            local: Affine2d::IDENTITY,
        }
    }
}

enum Motion {
    Identity,
    Document,
    Scroll(Option<usize>),
    Fixed,
    Sticky {
        constraint: Box<StickyConstraint>,
        container: Option<usize>,
        document: bool,
    },
    Animation(CssAnimationScope),
    Marquee(MarqueeScope),
}

struct Space {
    parent: SpaceRef,
    motion: Motion,
}

struct ClipNode {
    parent: Option<usize>,
    space: SpaceRef,
    shape: PaintShape,
}

struct Entry {
    region: HitRegion,
    space: SpaceRef,
    inverse: Affine2d,
    clip: Option<usize>,
    bounds: Bounds,
}

#[derive(Clone, Copy, Debug)]
struct Bounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Bounds {
    fn of(rect: CssRect) -> Self {
        Self {
            left: rect.x.next_down(),
            top: rect.y.next_down(),
            right: (rect.x + rect.width).next_up(),
            bottom: (rect.y + rect.height).next_up(),
        }
    }
    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
    fn contains(self, point: CssPoint) -> bool {
        point.x >= self.left
            && point.x <= self.right
            && point.y >= self.top
            && point.y <= self.bottom
    }
}

struct Branch {
    bounds: Bounds,
    start: usize,
    end: usize,
    children: Option<(usize, usize)>,
}

#[derive(Default)]
struct Group {
    entries: Vec<usize>,
    branches: Vec<Branch>,
}

impl Group {
    fn build(&mut self, entries: &[Entry]) {
        if self.entries.is_empty() {
            return;
        }
        self.partition(entries, 0, self.entries.len());
    }
    // Median partition bounds depth logarithmically, including identical boxes.
    fn partition(&mut self, entries: &[Entry], start: usize, end: usize) -> usize {
        let bounds = self.entries[start..end]
            .iter()
            .map(|&i| entries[i].bounds)
            .reduce(Bounds::union)
            .unwrap();
        let index = self.branches.len();
        self.branches.push(Branch {
            bounds,
            start,
            end,
            children: None,
        });
        if end - start > 8 {
            let middle = start + (end - start) / 2;
            let horizontal = bounds.right - bounds.left >= bounds.bottom - bounds.top;
            self.entries[start..end].select_nth_unstable_by(middle - start, |&a, &b| {
                let center = |bounds: Bounds| {
                    if horizontal {
                        bounds.left * 0.5 + bounds.right * 0.5
                    } else {
                        bounds.top * 0.5 + bounds.bottom * 0.5
                    }
                };
                center(entries[a].bounds).total_cmp(&center(entries[b].bounds))
            });
            let left = self.partition(entries, start, middle);
            let right = self.partition(entries, middle, end);
            self.branches[index].children = Some((left, right));
        }
        index
    }
}

/// One immutable paint transaction plus reusable, query-local scratch. No
/// point-result cache: every query observes the current scroll properties.
pub(crate) struct PageHitIndex {
    spaces: Vec<Space>,
    clips: Vec<ClipNode>,
    entries: Vec<Entry>,
    groups: Vec<Group>,
    world: Vec<Affine2d>,
    clip_marks: Vec<(u64, bool)>,
    serial: u64,
    stack: Vec<usize>,
    candidates: Vec<usize>,
    #[cfg(test)]
    visited_entries: usize,
}

impl PageHitIndex {
    #[cfg(test)]
    pub(crate) fn storage_identity(&self) -> usize {
        self.entries.as_ptr() as usize
    }
    pub(crate) fn new(page: &PagePaint) -> Self {
        let mut index = Self {
            spaces: vec![
                Space {
                    parent: SpaceRef::root(0),
                    motion: Motion::Identity,
                },
                Space {
                    parent: SpaceRef::root(0),
                    motion: Motion::Document,
                },
            ],
            clips: Vec::new(),
            entries: Vec::new(),
            groups: vec![Group::default(), Group::default()],
            world: Vec::new(),
            clip_marks: Vec::new(),
            serial: 0,
            stack: Vec::new(),
            candidates: Vec::new(),
            #[cfg(test)]
            visited_entries: 0,
        };
        let scrolls: HashMap<_, _> = page
            .scroll_containers
            .iter()
            .enumerate()
            .map(|(i, c)| (c.node, i))
            .collect();
        // Scroll scopes are emitted around multiple fragments. Intern their
        // dynamic property by parent space and exact static transform, rather
        // than creating one group/transform per repeated display-list marker.
        let mut interned = HashMap::new();
        index.compile(&page.fixed_under_primitives, false, &scrolls, &mut interned);
        index.compile(&page.primitives, true, &scrolls, &mut interned);
        if !page.fixed_interleaved {
            index.compile(&page.fixed_primitives, false, &scrolls, &mut interned);
        }
        for entry in &page.top_layer {
            index.compile(&entry.primitives, !entry.fixed, &scrolls, &mut interned);
        }
        for group in &mut index.groups {
            group.build(&index.entries);
        }
        index.world.resize(index.spaces.len(), Affine2d::IDENTITY);
        index.clip_marks.resize(index.clips.len(), (0, false));
        index
    }

    fn compile(
        &mut self,
        commands: &[Primitive],
        document: bool,
        scrolls: &HashMap<usize, usize>,
        interned: &mut HashMap<(usize, [u32; 6], usize), usize>,
    ) {
        let mut space = SpaceRef::root(usize::from(document));
        let mut transforms = Vec::new();
        let mut clip = None;
        for command in commands {
            match command {
                Primitive::PushTransform(matrix) => {
                    transforms.push(space);
                    space.local = space.local.then(*matrix);
                }
                Primitive::PopTransform
                | Primitive::EndScroll
                | Primitive::EndFixed
                | Primitive::EndSticky
                | Primitive::EndCssAnimation
                | Primitive::EndMarquee => {
                    space = transforms
                        .pop()
                        .unwrap_or(SpaceRef::root(usize::from(document)));
                }
                Primitive::PushClip(shape) => {
                    let next = self.clips.len();
                    self.clips.push(ClipNode {
                        parent: clip,
                        space,
                        shape: shape.clone(),
                    });
                    clip = Some(next);
                }
                Primitive::PopClip => {
                    clip = clip.and_then(|id| self.clips[id].parent);
                }
                Primitive::BeginScroll(_)
                | Primitive::BeginFixed
                | Primitive::BeginSticky(_)
                | Primitive::BeginCssAnimation(_)
                | Primitive::BeginMarquee(_) => {
                    transforms.push(space);
                    let motion = match command {
                        Primitive::BeginScroll(node) => Motion::Scroll(scrolls.get(node).copied()),
                        Primitive::BeginFixed if document => Motion::Fixed,
                        Primitive::BeginFixed => continue,
                        Primitive::BeginSticky(constraint) => Motion::Sticky {
                            constraint: Box::new(constraint.clone()),
                            container: constraint.container.and_then(|n| scrolls.get(&n).copied()),
                            document,
                        },
                        Primitive::BeginCssAnimation(scope) => Motion::Animation(scope.clone()),
                        Primitive::BeginMarquee(scope) => Motion::Marquee(scope.clone()),
                        _ => unreachable!(),
                    };
                    let key = match command {
                        Primitive::BeginScroll(node) => {
                            Some((space.dynamic, space.local.0.map(f32::to_bits), *node))
                        }
                        _ => None,
                    };
                    let dynamic = key
                        .and_then(|key| interned.get(&key).copied())
                        .unwrap_or_else(|| {
                            let id = self.spaces.len();
                            self.spaces.push(Space {
                                parent: space,
                                motion,
                            });
                            self.groups.push(Group::default());
                            if let Some(key) = key {
                                interned.insert(key, id);
                            }
                            id
                        });
                    space = SpaceRef::root(dynamic);
                }
                Primitive::HitRegion(region) => {
                    let Some(inverse) = space.local.inverse() else {
                        continue;
                    };
                    if region.rect.width <= 0. || region.rect.height <= 0. {
                        continue;
                    }
                    let rect = transformed_bounds(region.rect, space.local);
                    if ![rect.x, rect.y, rect.width, rect.height]
                        .into_iter()
                        .all(f32::is_finite)
                    {
                        continue;
                    }
                    let id = self.entries.len();
                    self.entries.push(Entry {
                        region: region.clone(),
                        space,
                        inverse,
                        clip,
                        bounds: Bounds::of(rect),
                    });
                    self.groups[space.dynamic].entries.push(id);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn query(
        &mut self,
        page: &PagePaint,
        viewport: CssSize,
        scroll: CssPoint,
        point: CssPoint,
        elapsed_seconds: f32,
    ) -> Vec<PageHit> {
        if !CssRect::new(0., 0., viewport.width, viewport.height).contains(point) {
            return Vec::new();
        }
        self.serial = self.serial.wrapping_add(1);
        if self.serial == 0 {
            self.clip_marks.fill((0, false));
            self.serial = 1;
        }
        self.candidates.clear();
        #[cfg(test)]
        {
            self.visited_entries = 0;
        }
        for (id, space) in self.spaces.iter().enumerate() {
            let offset = match &space.motion {
                Motion::Identity => {
                    self.world[id] = Affine2d::IDENTITY;
                    continue;
                }
                Motion::Document => CssPoint::new(-scroll.x, -scroll.y),
                Motion::Scroll(index) => {
                    let offset = index
                        .and_then(|i| page.scroll_containers.get(i))
                        .map(|c| c.offset)
                        .unwrap_or_default();
                    CssPoint::new(-offset.x, -offset.y)
                }
                Motion::Fixed => scroll,
                Motion::Sticky {
                    constraint,
                    container,
                    document,
                } => {
                    let (offset, viewport) = container
                        .and_then(|i| page.scroll_containers.get(i))
                        .map(|c| (c.offset, c.viewport))
                        .unwrap_or((
                            if *document {
                                scroll
                            } else {
                                CssPoint::default()
                            },
                            CssRect::new(0., 0., viewport.width, viewport.height),
                        ));
                    constraint.offset(offset, viewport)
                }
                Motion::Animation(scope) => {
                    self.world[id] = self.world[space.parent.dynamic]
                        .then(space.parent.local)
                        .then(sample_css_animation_scope(scope, elapsed_seconds));
                    continue;
                }
                Motion::Marquee(scope) => sample_marquee_scope(scope, elapsed_seconds),
            };
            self.world[id] = self.world[space.parent.dynamic]
                .then(space.parent.local)
                .then(Affine2d::translate(offset.x, offset.y));
        }
        for (space, group) in self.groups.iter().enumerate() {
            if group.branches.is_empty() {
                continue;
            }
            let Some(inverse) = self.world[space].inverse() else {
                continue;
            };
            let local = inverse.map_point(point);
            self.stack.clear();
            self.stack.push(0);
            while let Some(index) = self.stack.pop() {
                let branch = &group.branches[index];
                if !branch.bounds.contains(local) {
                    continue;
                }
                if let Some((left, right)) = branch.children {
                    self.stack.extend([left, right]);
                    continue;
                }
                for &entry in &group.entries[branch.start..branch.end] {
                    #[cfg(test)]
                    {
                        self.visited_entries += 1;
                    }
                    let candidate = &self.entries[entry];
                    if candidate.bounds.contains(local)
                        && candidate
                            .region
                            .rect
                            .contains(candidate.inverse.map_point(local))
                    {
                        self.candidates.push(entry);
                    }
                }
            }
        }
        self.candidates.sort_unstable_by(|a, b| b.cmp(a));
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for index in 0..self.candidates.len() {
            let entry = self.candidates[index];
            if seen.contains(&self.entries[entry].region.node)
                || !self.clip_contains(self.entries[entry].clip, point)
            {
                continue;
            }
            let entry = &self.entries[entry];
            let region = &entry.region;
            seen.insert(region.node);
            hits.push(PageHit {
                rect: transformed_bounds(
                    region.rect,
                    self.world[entry.space.dynamic].then(entry.space.local),
                ),
                node: region.node,
                actor: region.actor,
                link: region.link.clone(),
                cursor: region.cursor.clone(),
            });
        }
        hits
    }

    fn clip_contains(&mut self, mut clip: Option<usize>, point: CssPoint) -> bool {
        self.stack.clear();
        let mut contains = true;
        while let Some(id) = clip {
            let (serial, previous) = self.clip_marks[id];
            if serial == self.serial {
                contains = previous;
                break;
            }
            self.stack.push(id);
            let node = &self.clips[id];
            let transform = self.world[node.space.dynamic].then(node.space.local);
            if !transform
                .inverse()
                .is_some_and(|inverse| shape_contains(&node.shape, inverse.map_point(point)))
            {
                contains = false;
                break;
            }
            clip = node.parent;
        }
        for &id in &self.stack {
            self.clip_marks[id] = (self.serial, contains);
        }
        contains
    }

    pub(crate) fn retained_memory(&self) -> (usize, bool) {
        let mut bytes = self.spaces.capacity() * std::mem::size_of::<Space>()
            + self.clips.capacity() * std::mem::size_of::<ClipNode>()
            + self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.groups.capacity() * std::mem::size_of::<Group>()
            + self.world.capacity() * std::mem::size_of::<Affine2d>()
            + self.clip_marks.capacity() * std::mem::size_of::<(u64, bool)>()
            + (self.stack.capacity() + self.candidates.capacity()) * std::mem::size_of::<usize>();
        let mut opaque = false;
        for group in &self.groups {
            bytes += group.entries.capacity() * std::mem::size_of::<usize>()
                + group.branches.capacity() * std::mem::size_of::<Branch>();
        }
        for clip in &self.clips {
            bytes += clip.shape.retained_bytes();
        }
        for space in &self.spaces {
            match &space.motion {
                Motion::Sticky { .. } => bytes += std::mem::size_of::<StickyConstraint>(),
                Motion::Animation(scope) => {
                    bytes += scope.animations.capacity() * std::mem::size_of::<CssPaintAnimation>();
                    for animation in &scope.animations {
                        bytes += animation.name.capacity()
                            + animation.direction.capacity()
                            + animation.fill_mode.capacity()
                            + animation.timing_function.capacity()
                            + animation.position.capacity()
                                * std::mem::size_of::<CssAnimationPoint>()
                            + animation.transform.capacity()
                                * std::mem::size_of::<super::CssTransformFrame>()
                            + animation
                                .transform
                                .iter()
                                .map(|frame| {
                                    frame.steps.capacity()
                                        * std::mem::size_of::<super::TransformStep>()
                                })
                                .sum::<usize>();
                    }
                }
                Motion::Marquee(_)
                | Motion::Identity
                | Motion::Document
                | Motion::Scroll(_)
                | Motion::Fixed => {}
            }
        }
        for entry in &self.entries {
            if let Some(link) = &entry.region.link {
                let (extra, is_opaque) = link.retained_memory();
                bytes += extra;
                opaque |= is_opaque;
            }
            bytes += entry.region.cursor.as_ref().map_or(0, String::capacity);
        }
        (bytes, opaque)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(node: usize, rect: CssRect) -> Primitive {
        Primitive::HitRegion(HitRegion {
            rect,
            node,
            actor: Some(node),
            link: None,
            cursor: None,
        })
    }

    fn compare(index: &mut PageHitIndex, page: &PagePaint, scroll: CssPoint, point: CssPoint) {
        let viewport = CssSize::new(320., 240.);
        let expected = page_element_hits_at(page, viewport, scroll, point);
        let actual = index.query(page, viewport, scroll, point, 0.);
        assert_eq!(
            actual.iter().map(|h| h.node).collect::<Vec<_>>(),
            expected.iter().map(|h| h.node).collect::<Vec<_>>(),
            "scroll={scroll:?} point={point:?}"
        );
        for (actual, expected) in actual.iter().zip(&expected) {
            assert_eq!(
                (actual.actor, &actual.link, &actual.cursor),
                (expected.actor, &expected.link, &expected.cursor)
            );
            for (a, b) in [
                actual.rect.x,
                actual.rect.y,
                actual.rect.width,
                actual.rect.height,
            ]
            .into_iter()
            .zip([
                expected.rect.x,
                expected.rect.y,
                expected.rect.width,
                expected.rect.height,
            ]) {
                assert!((a - b).abs() < 0.001, "{actual:?} != {expected:?}");
            }
        }
    }

    #[test]
    fn retained_hit_index_matches_scrolling_fixed_sticky_transformed_and_clipped_paint() {
        let clip = PaintShape::RoundedRect {
            rect: CssRect::new(5., 5., 210., 170.),
            radii: CornerRadii {
                corners: [(18., 12.); 4],
            },
        };
        let mut page = PagePaint {
            width: 640.,
            height: 900.,
            fixed_interleaved: true,
            scroll_containers: vec![ScrollContainer {
                node: 10,
                viewport: CssRect::new(5., 5., 210., 170.),
                content: CssSize::new(500., 800.),
                ..Default::default()
            }],
            primitives: vec![
                hit(1, CssRect::new(0., 0., 640., 900.)),
                Primitive::PushClip(clip),
                Primitive::BeginScroll(10),
                Primitive::PushTransform(Affine2d([0.96, 0.28, -0.28, 0.96, 20., 20.])),
                hit(2, CssRect::new(10., 10., 120., 70.)),
                Primitive::PushClip(PaintShape::Polygon {
                    points: vec![
                        CssPoint::new(0., 0.),
                        CssPoint::new(220., 0.),
                        CssPoint::new(80., 200.),
                    ],
                    evenodd: true,
                }),
                Primitive::PopTransform,
                hit(3, CssRect::new(20., 30., 170., 160.)),
                Primitive::PopClip,
                Primitive::BeginSticky(StickyConstraint {
                    node: 4,
                    rect: CssRect::new(10., 80., 70., 20.),
                    container: Some(10),
                    insets: [Some(0.), None, None, None],
                    movement: [-80., 0., 250., 0.],
                    reverse: [false; 2],
                }),
                hit(4, CssRect::new(10., 80., 70., 20.)),
                Primitive::EndSticky,
                Primitive::EndScroll,
                Primitive::PopClip,
                Primitive::BeginFixed,
                hit(5, CssRect::new(220., 5., 80., 30.)),
                Primitive::EndFixed,
                Primitive::PushTransform(Affine2d([0., 0., 0., 1., 0., 0.])),
                hit(6, CssRect::new(0., 0., 320., 240.)),
                Primitive::PopTransform,
            ],
            top_layer: vec![
                TopLayerEntry {
                    fixed: true,
                    primitives: vec![hit(7, CssRect::new(70., 80., 30., 40.))],
                },
                TopLayerEntry {
                    fixed: false,
                    primitives: vec![hit(8, CssRect::new(50., 200., 40., 40.))],
                },
            ],
            ..Default::default()
        };
        let mut index = PageHitIndex::new(&page);
        let identity = index.storage_identity();
        let mut seed = 918273u32;
        for offset in [0., 20., 85., -40., 160.] {
            page.scroll_containers[0].offset = CssPoint::new(-offset * 0.1, offset);
            for _ in 0..180 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let x = (seed % 33000) as f32 * 0.01 - 5.;
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let y = (seed % 25000) as f32 * 0.01 - 5.;
                compare(
                    &mut index,
                    &page,
                    CssPoint::new(3., offset * 0.2),
                    CssPoint::new(x, y),
                );
            }
        }
        assert_eq!(identity, index.storage_identity());
        index.serial = u64::MAX;
        compare(
            &mut index,
            &page,
            CssPoint::default(),
            CssPoint::new(30., 25.),
        );
    }

    #[test]
    fn retained_hit_index_uses_bounded_candidate_work_on_large_scrolling_documents() {
        let mut page = PagePaint {
            scroll_containers: vec![ScrollContainer {
                node: 9,
                ..Default::default()
            }],
            ..Default::default()
        };
        for node in 0..32768 {
            page.primitives.extend([
                Primitive::BeginScroll(9),
                hit(node + 100, CssRect::new(0., node as f32 * 4., 100., 4.)),
                Primitive::EndScroll,
            ]);
        }
        let mut index = PageHitIndex::new(&page);
        assert_eq!(index.spaces.len(), 3, "repeated scroll scopes share a BVH");
        let identity = index.storage_identity();
        for offset in [0., 40., 4096., 100000.] {
            page.scroll_containers[0].offset.y = offset;
            let hits = index.query(
                &page,
                CssSize::new(320., 240.),
                CssPoint::default(),
                CssPoint::new(10., 10.),
                0.,
            );
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].node, 100 + ((offset + 10.) / 4.) as usize);
            assert!(
                index.visited_entries <= 16,
                "visited {} of 32768 boxes",
                index.visited_entries
            );
            assert_eq!(identity, index.storage_identity());
        }
        assert!(index.retained_memory().0 > 32768 * std::mem::size_of::<Entry>());
    }

    #[test]
    fn retained_hit_index_keeps_covered_nodes_in_paint_order_and_excludes_browser_ui() {
        let mut page = PagePaint {
            primitives: vec![
                hit(1, CssRect::new(0., 0., 100., 100.)),
                hit(2, CssRect::new(0., 0., 100., 100.)),
                hit(1, CssRect::new(0., 0., 50., 50.)),
            ],
            fixed_primitives: vec![hit(3, CssRect::new(0., 0., 100., 100.))],
            browser_media: vec![hit(999, CssRect::new(0., 0., 100., 100.))],
            ..Default::default()
        };
        let mut index = PageHitIndex::new(&page);
        compare(
            &mut index,
            &page,
            CssPoint::default(),
            CssPoint::new(20., 20.),
        );
        assert_eq!(
            index
                .query(
                    &page,
                    CssSize::new(320., 240.),
                    CssPoint::default(),
                    CssPoint::new(20., 20.),
                    0.
                )
                .iter()
                .map(|h| h.node)
                .collect::<Vec<_>>(),
            [3, 1, 2]
        );
        page.fixed_interleaved = true;
        let mut index = PageHitIndex::new(&page);
        compare(
            &mut index,
            &page,
            CssPoint::default(),
            CssPoint::new(20., 20.),
        );
    }

    #[test]
    fn retained_hit_index_samples_animation_properties_without_rebuilding() {
        let animation = CssAnimationScope {
            animations: vec![CssPaintAnimation {
                name: "move".into(),
                duration_seconds: 2.,
                delay_seconds: 0.,
                iteration_count: None,
                direction: "alternate".into(),
                fill_mode: "both".into(),
                timing_function: "linear".into(),
                running: true,
                opacity: Vec::new(),
                position: Vec::new(),
                transform: vec![
                    super::super::CssTransformFrame {
                        offset: 0.,
                        steps: vec![super::super::TransformStep::Translate(0., 0.)],
                    },
                    super::super::CssTransformFrame {
                        offset: 1.,
                        steps: vec![super::super::TransformStep::Translate(160., 80.)],
                    },
                ],
                transform_origin: CssPoint::default(),
                static_transform: Affine2d::IDENTITY,
            }],
        };
        let marquee = MarqueeScope {
            viewport: CssRect::new(0., 0., 300., 200.),
            content: CssRect::new(0., 0., 80., 30.),
            behavior: MarqueeBehavior::Alternate,
            direction: MarqueeDirection::Right,
            scroll_interval_seconds: 0.1,
            scroll_distance: 10.,
            loop_count: None,
            running: true,
            paused_at_seconds: None,
            paused_total_seconds: 0.,
        };
        let page = PagePaint {
            primitives: vec![
                Primitive::BeginCssAnimation(animation),
                hit(7, CssRect::new(10., 10., 80., 50.)),
                Primitive::EndCssAnimation,
                Primitive::BeginMarquee(marquee),
                hit(8, CssRect::new(0., 0., 80., 30.)),
                Primitive::EndMarquee,
            ],
            ..Default::default()
        };
        let mut index = PageHitIndex::new(&page);
        let identity = index.storage_identity();
        for elapsed in [0., 0.5, 1.5, 2.5, 4.] {
            let mut scene = Scene {
                viewport: ViewportMetrics::from_physical(
                    PhysicalSize::new(320, 240),
                    Default::default(),
                ),
                primitives: Vec::new(),
                controls: Vec::new(),
                content_viewport: CssRect::new(0., 0., 320., 240.),
                image_store: Default::default(),
                canvas_images: Default::default(),
                page_scroll_containers: Vec::new(),
                page_size: CssSize::default(),
            };
            scene.append_page_at(&page, CssPoint::default(), elapsed);
            for y in (0..240).step_by(13) {
                for x in (0..320).step_by(17) {
                    let point = CssPoint::new(x as f32 + 0.25, y as f32 + 0.25);
                    let actual = index.query(
                        &page,
                        CssSize::new(320., 240.),
                        CssPoint::default(),
                        point,
                        elapsed,
                    );
                    let expected = scene.page_element_hits_at(point);
                    assert_eq!(actual, expected, "time={elapsed} point={point:?}");
                }
            }
        }
        assert_eq!(index.storage_identity(), identity);
    }
}
