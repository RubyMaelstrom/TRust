//! Style computation, generic over where its inputs and memo storage live.
//!
//! The cascade, computed values (CSS Cascade 5 #computed, #inheriting) and
//! the typed style records run on `ComputeView<B>`. On the page thread `B`
//! is the `Dom` itself, with its caches; a parallel style pass gives each
//! worker its own backend with per-thread memo storage. One implementation,
//! monomorphized per backend: the page thread's code is the same as calling
//! the DOM directly.

use super::color_scheme::ColorScheme;
use super::*;
use crate::layout2::{BoxStyle, InlineStyle};

/// The style algorithms' receiver: a reference to their backend.
pub(super) struct ComputeView<'a, B: ?Sized>(pub(super) &'a B);

impl<B: ?Sized> Clone for ComputeView<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<B: ?Sized> Copy for ComputeView<'_, B> {}

impl<B: ?Sized> std::ops::Deref for ComputeView<'_, B> {
    type Target = B;
    #[inline]
    fn deref(&self) -> &B {
        self.0
    }
}

/// What style computation reads and memoizes besides its algorithms: the
/// document (through `StyleView`), document-level style state, and per-node
/// memo storage. The DOM's own caches are page-thread state; a worker
/// backend brings its own.
pub(super) trait StyleBackend {
    type Index: std::ops::Deref<Target = StyleIndex>;

    fn style_view(&self) -> StyleView<'_>;
    /// The parsed style index, built if needed.
    fn style_index(&self) -> Self::Index;
    /// Apply deferred style invalidations before reading memoized values.
    fn flush_style_invalidations(&self);
    fn matched_rules(&self, id: NodeId) -> std::rc::Rc<Vec<u32>>;

    fn style_value_epoch(&self) -> u64;
    fn style_epoch(&self) -> u64;
    fn epoch(&self) -> u64;
    fn viewport_px(&self) -> (f32, f32);
    fn device_pixel_ratio(&self) -> f32;
    fn doc_url(&self) -> Option<&url::Url>;
    fn page_support(&self) -> &color_scheme::PageSupport;
    fn properties(&self) -> &properties::State;
    fn render_clickables(&self) -> &std::collections::HashSet<NodeId>;
    fn render_live(&self) -> bool;
    /// The node document's base URL for presentational hints.
    fn document_base(&self, id: NodeId) -> html_hints::DocumentBase;
    /// The `::slotted()` rules matching a light-DOM element.
    fn slotted_rules<'i>(&self, index: &'i StyleIndex, id: NodeId) -> Vec<&'i StyleRule>;
    /// CSS Conditional 5 #container-rule: whether `query` holds for `subject`.
    fn container_matches(
        &self,
        query: &container_queries::Query,
        subject: NodeId,
        pseudo: bool,
    ) -> bool;
    /// Record that `subject`'s style read `container`'s size (CSS Conditional 5
    /// #container-lengths), so layout settles again when it resizes.
    fn record_container_read(&self, subject: NodeId, container: NodeId, axes: u8, units: bool);
    fn container_size(&self, container: NodeId) -> Option<[f32; 2]>;
    /// Flat-tree children (CSS Scoping #flat-tree).
    fn flat_children(&self, id: NodeId) -> Vec<NodeId>;

    /// CSS Transitions 1 #application and CSS Animations 1 origins.
    fn transition_value(&self, id: NodeId, name: &str) -> Option<String>;
    fn transition_affects(&self, name: &str) -> bool;
    fn css_transitions_active(&self) -> bool;
    fn animation_value(&self, id: NodeId, name: &str) -> Option<String>;
    fn animation_private_row(&self, id: NodeId) -> bool;

    fn computed_cache(&self) -> &RefCell<ComputedCache>;
    fn cascaded_cache(&self) -> &RefCell<NodeCache<std::rc::Rc<CascadedMaps>>>;
    fn custom_prop_cache(&self) -> &RefCell<CustomPropCache>;
    fn style_sharing(&self) -> &RefCell<style_sharing::State>;
    fn hidden_cache(&self) -> &RefCell<NodeCache<BoxGeneration>>;
    fn font_cache(&self) -> &RefCell<NodeCache<f32>>;
    fn font_units_cache(&self) -> &RefCell<NodeCache<(u64, crate::layout2::Units)>>;
    fn decoration_cache(&self) -> &RefCell<NodeCache<(bool, bool)>>;

    // Document queries, through the view.
    fn is_valid(&self, id: NodeId) -> bool {
        self.style_view().nodes.contains(id)
    }
    fn render_clickable(&self, id: NodeId) -> bool {
        self.render_live() && self.render_clickables().contains(&id)
    }
    fn tag_name(&self, id: NodeId) -> Option<&str> {
        self.style_view().nodes.tag_name(id)
    }
    fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        self.style_view().nodes.attr(id, name)
    }
    fn namespace_uri(&self, id: NodeId) -> Option<&str> {
        self.style_view().nodes.namespace_uri(id)
    }
    fn child_iter(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.style_view().nodes.child_iter(id)
    }
    fn descendants(&self, root: NodeId) -> Descendants<'_> {
        self.style_view().nodes.descendants(root)
    }
    fn tree_scope(&self, id: NodeId) -> NodeId {
        self.style_view().tree_scope(id)
    }
    fn style_parent(&self, id: NodeId) -> Option<NodeId> {
        self.style_view().style_parent(id)
    }
    fn parent_composed(&self, id: NodeId) -> Option<NodeId> {
        self.style_view().parent_composed(id)
    }
    fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        self.style_view().shadow_root(host)
    }
    fn in_quirks_mode(&self, id: NodeId) -> bool {
        self.style_view().in_quirks_mode(id)
    }
    fn input_type(&self, id: NodeId) -> String {
        self.style_view().input_type(id)
    }
    fn is_popover_showing(&self, id: NodeId) -> bool {
        self.style_view().is_popover_showing(id)
    }
}

impl StyleBackend for Dom {
    type Index = std::rc::Rc<StyleIndex>;

    #[inline]
    #[cfg_attr(feature = "architecture-diagnostics", track_caller)]
    fn tag_name(&self, id: NodeId) -> Option<&str> {
        // Keeps the architecture diagnostics' DOM read accounting.
        Dom::tag_name(self, id)
    }

    #[inline]
    fn style_view(&self) -> StyleView<'_> {
        Dom::style_view(self)
    }
    #[inline]
    fn style_index(&self) -> Self::Index {
        Dom::style_index(self)
    }
    #[inline]
    fn flush_style_invalidations(&self) {
        Dom::flush_style_invalidations(self)
    }
    #[inline]
    fn matched_rules(&self, id: NodeId) -> std::rc::Rc<Vec<u32>> {
        Dom::matched_rules(self, id)
    }
    #[inline]
    fn style_value_epoch(&self) -> u64 {
        self.style_value_epoch
    }
    #[inline]
    fn style_epoch(&self) -> u64 {
        self.style_epoch
    }
    #[inline]
    fn epoch(&self) -> u64 {
        self.epoch
    }
    #[inline]
    fn viewport_px(&self) -> (f32, f32) {
        self.viewport_px
    }
    #[inline]
    fn device_pixel_ratio(&self) -> f32 {
        self.device_pixel_ratio
    }
    #[inline]
    fn doc_url(&self) -> Option<&url::Url> {
        self.doc_url.as_ref()
    }
    #[inline]
    fn page_support(&self) -> &color_scheme::PageSupport {
        &self.page_color_schemes
    }
    #[inline]
    fn properties(&self) -> &properties::State {
        &self.properties
    }
    #[inline]
    fn render_clickables(&self) -> &std::collections::HashSet<NodeId> {
        &self.render_clickables
    }
    #[inline]
    fn render_live(&self) -> bool {
        self.render_live
    }
    #[inline]
    fn document_base(&self, id: NodeId) -> html_hints::DocumentBase {
        Dom::document_base(self, id)
    }
    #[inline]
    fn slotted_rules<'i>(&self, index: &'i StyleIndex, id: NodeId) -> Vec<&'i StyleRule> {
        Dom::slotted_rules(self, index, id)
    }
    #[inline]
    fn container_matches(
        &self,
        query: &container_queries::Query,
        subject: NodeId,
        pseudo: bool,
    ) -> bool {
        query.matches(self, subject, pseudo)
    }
    #[inline]
    fn record_container_read(&self, subject: NodeId, container: NodeId, axes: u8, units: bool) {
        Dom::record_container_read(self, subject, container, axes, units)
    }
    #[inline]
    fn container_size(&self, container: NodeId) -> Option<[f32; 2]> {
        self.container_sizes.borrow().get(&container).copied()
    }
    #[inline]
    fn flat_children(&self, id: NodeId) -> Vec<NodeId> {
        Dom::flat_children(self, id)
    }
    #[inline]
    fn transition_value(&self, id: NodeId, name: &str) -> Option<String> {
        self.transitions.value(id, name)
    }
    #[inline]
    fn transition_affects(&self, name: &str) -> bool {
        self.transitions.affects_computation(name)
    }
    #[inline]
    fn css_transitions_active(&self) -> bool {
        Dom::css_transitions_active(self)
    }
    #[inline]
    fn animation_value(&self, id: NodeId, name: &str) -> Option<String> {
        self.animations.value(id, name)
    }
    #[inline]
    fn animation_private_row(&self, id: NodeId) -> bool {
        self.animations.private_row(id)
    }
    #[inline]
    fn computed_cache(&self) -> &RefCell<ComputedCache> {
        &self.computed_cache
    }
    #[inline]
    fn cascaded_cache(&self) -> &RefCell<NodeCache<std::rc::Rc<CascadedMaps>>> {
        &self.cascaded_cache
    }
    #[inline]
    fn custom_prop_cache(&self) -> &RefCell<CustomPropCache> {
        &self.custom_prop_cache
    }
    #[inline]
    fn style_sharing(&self) -> &RefCell<style_sharing::State> {
        &self.style_sharing
    }
    #[inline]
    fn hidden_cache(&self) -> &RefCell<NodeCache<BoxGeneration>> {
        &self.hidden_cache
    }
    #[inline]
    fn font_cache(&self) -> &RefCell<NodeCache<f32>> {
        &self.font_cache
    }
    #[inline]
    fn font_units_cache(&self) -> &RefCell<NodeCache<(u64, crate::layout2::Units)>> {
        &self.font_units_cache
    }
    #[inline]
    fn decoration_cache(&self) -> &RefCell<NodeCache<(bool, bool)>> {
        &self.decoration_cache
    }
}

impl crate::layout2::UnitSource for Dom {
    #[inline]
    fn font_px(&self, id: NodeId) -> f32 {
        Dom::font_px(self, id)
    }
    #[inline]
    fn root_font_px(&self) -> f32 {
        Dom::root_font_px(self)
    }
    #[inline]
    fn computed_value_resolved(&self, id: NodeId, name: &str) -> Option<String> {
        Dom::computed_value_resolved(self, id, name)
    }
    #[inline]
    fn viewport_px(&self) -> (f32, f32) {
        Dom::viewport_px(self)
    }
    #[inline]
    fn cached_font_units(
        &self,
        id: NodeId,
        compute: impl FnOnce() -> crate::layout2::Units,
    ) -> crate::layout2::Units {
        Dom::cached_font_units(self, id, compute)
    }
}

impl<B: StyleBackend + ?Sized> crate::layout2::UnitSource for ComputeView<'_, B> {
    #[inline]
    fn font_px(&self, id: NodeId) -> f32 {
        ComputeView::font_px(self, id)
    }
    #[inline]
    fn root_font_px(&self) -> f32 {
        ComputeView::root_font_px(self)
    }
    #[inline]
    fn computed_value_resolved(&self, id: NodeId, name: &str) -> Option<String> {
        ComputeView::computed_value_resolved(self, id, name)
    }
    #[inline]
    fn viewport_px(&self) -> (f32, f32) {
        self.0.viewport_px()
    }
    #[inline]
    fn cached_font_units(
        &self,
        id: NodeId,
        compute: impl FnOnce() -> crate::layout2::Units,
    ) -> crate::layout2::Units {
        ComputeView::cached_font_units(self, id, compute)
    }
}

impl crate::layout2::StyleSource for Dom {
    #[inline]
    fn tag_name(&self, id: NodeId) -> Option<&str> {
        Dom::tag_name(self, id)
    }
    #[inline]
    fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        Dom::attr(self, id, name)
    }
    #[inline]
    fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).parent
    }
    #[inline]
    fn children(&self, id: NodeId) -> Vec<NodeId> {
        Dom::children(self, id)
    }
    #[inline]
    fn text(&self, id: NodeId) -> Option<&str> {
        style_view::NodesRef::new(&self.nodes).text(id)
    }
    #[inline]
    fn is_element(&self, id: NodeId) -> bool {
        style_view::NodesRef::new(&self.nodes).is_element(id)
    }
    #[inline]
    fn in_quirks_mode(&self, id: NodeId) -> bool {
        Dom::in_quirks_mode(self, id)
    }
    #[inline]
    fn device_pixel_ratio(&self) -> f32 {
        Dom::device_pixel_ratio(self)
    }
    #[inline]
    fn render_clickable(&self, id: NodeId) -> bool {
        Dom::render_clickable(self, id)
    }
    #[inline]
    fn inherited_lang(&self, id: NodeId) -> Option<&str> {
        Dom::inherited_lang(self, id)
    }
    #[inline]
    fn is_document_element(&self, id: NodeId) -> bool {
        Dom::is_document_element(self, id)
    }
    #[inline]
    fn computed_display(&self, id: NodeId) -> Option<String> {
        Dom::computed_display(self, id)
    }
    #[inline]
    fn css_animation_definitions(&self, id: NodeId) -> Vec<CssAnimationDefinition> {
        Dom::css_animation_definitions(self, id)
    }
    #[inline]
    fn legacy_line_clamp(&self, id: NodeId) -> Option<usize> {
        Dom::legacy_line_clamp(self, id)
    }
    #[inline]
    fn size_container_kind(&self, id: NodeId) -> u8 {
        Dom::size_container_kind(self, id)
    }
    #[inline]
    fn effective_opacity(&self, id: NodeId) -> f32 {
        Dom::effective_opacity(self, id)
    }
    #[inline]
    fn text_decoration(&self, id: NodeId) -> (bool, bool) {
        Dom::text_decoration(self, id)
    }
    #[inline]
    fn author_declares(&self, id: NodeId, prop: &str) -> bool {
        Dom::author_declares(self, id, prop)
    }
    #[inline]
    fn font_size_zero(&self, id: NodeId) -> Option<bool> {
        Dom::font_size_zero(self, id)
    }
    #[inline]
    fn paint_suppressed(&self, id: NodeId) -> bool {
        Dom::paint_suppressed(self, id)
    }
    #[inline]
    fn visibility_hidden(&self, id: NodeId) -> bool {
        Dom::visibility_hidden(self, id)
    }
    #[inline]
    fn document_font_set(&self, id: NodeId) -> Option<std::sync::Arc<crate::text::FontSet>> {
        Dom::document_font_set(self, id)
    }
    #[inline]
    fn retained_box_style(
        &self,
        id: NodeId,
        context: BoxContext,
        compute: impl FnOnce() -> BoxStyle,
    ) -> BoxStyle {
        Dom::retained_box_style(self, id, context, compute)
    }
    #[inline]
    fn retained_inline_style(
        &self,
        id: NodeId,
        parent: &InlineStyle,
        base: &url::Url,
        compute: impl FnOnce() -> InlineStyle,
    ) -> InlineStyle {
        Dom::retained_inline_style(self, id, parent, base, compute)
    }
}

impl<B: StyleBackend + ?Sized> crate::layout2::StyleSource for ComputeView<'_, B> {
    #[inline]
    fn tag_name(&self, id: NodeId) -> Option<&str> {
        self.0.tag_name(id)
    }
    #[inline]
    fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        self.0.attr(id, name)
    }
    #[inline]
    fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.0.style_view().nodes.parent(id)
    }
    #[inline]
    fn children(&self, id: NodeId) -> Vec<NodeId> {
        self.0.child_iter(id).collect()
    }
    #[inline]
    fn text(&self, id: NodeId) -> Option<&str> {
        self.0.style_view().nodes.text(id)
    }
    #[inline]
    fn is_element(&self, id: NodeId) -> bool {
        self.0.style_view().nodes.is_element(id)
    }
    #[inline]
    fn in_quirks_mode(&self, id: NodeId) -> bool {
        self.0.in_quirks_mode(id)
    }
    #[inline]
    fn device_pixel_ratio(&self) -> f32 {
        self.0.device_pixel_ratio()
    }
    #[inline]
    fn render_clickable(&self, id: NodeId) -> bool {
        self.0.render_clickable(id)
    }
    #[inline]
    fn inherited_lang(&self, id: NodeId) -> Option<&str> {
        self.0.style_view().inherited_lang(id)
    }
    #[inline]
    fn is_document_element(&self, id: NodeId) -> bool {
        ComputeView::is_document_element(self, id)
    }
    #[inline]
    fn computed_display(&self, id: NodeId) -> Option<String> {
        ComputeView::computed_display(self, id)
    }
    #[inline]
    fn css_animation_definitions(&self, id: NodeId) -> Vec<CssAnimationDefinition> {
        ComputeView::css_animation_definitions(self, id)
    }
    #[inline]
    fn legacy_line_clamp(&self, id: NodeId) -> Option<usize> {
        ComputeView::legacy_line_clamp(self, id)
    }
    #[inline]
    fn size_container_kind(&self, id: NodeId) -> u8 {
        ComputeView::size_container_kind(self, id)
    }
    #[inline]
    fn effective_opacity(&self, id: NodeId) -> f32 {
        ComputeView::effective_opacity(self, id)
    }
    #[inline]
    fn text_decoration(&self, id: NodeId) -> (bool, bool) {
        ComputeView::text_decoration(self, id)
    }
    #[inline]
    fn author_declares(&self, id: NodeId, prop: &str) -> bool {
        ComputeView::author_declares(self, id, prop)
    }
    #[inline]
    fn font_size_zero(&self, id: NodeId) -> Option<bool> {
        ComputeView::font_size_zero(self, id)
    }
    #[inline]
    fn paint_suppressed(&self, id: NodeId) -> bool {
        ComputeView::paint_suppressed(self, id)
    }
    #[inline]
    fn visibility_hidden(&self, id: NodeId) -> bool {
        ComputeView::visibility_hidden(self, id)
    }
    #[inline]
    fn document_font_set(&self, id: NodeId) -> Option<std::sync::Arc<crate::text::FontSet>> {
        ComputeView::document_font_set(self, id)
    }
    #[inline]
    fn retained_box_style(
        &self,
        id: NodeId,
        context: BoxContext,
        compute: impl FnOnce() -> BoxStyle,
    ) -> BoxStyle {
        ComputeView::retained_box_style(self, id, context, compute)
    }
    #[inline]
    fn retained_inline_style(
        &self,
        id: NodeId,
        parent: &InlineStyle,
        base: &url::Url,
        compute: impl FnOnce() -> InlineStyle,
    ) -> InlineStyle {
        ComputeView::retained_inline_style(self, id, parent, base, compute)
    }
}

// The DOM's entry points into style computation (`ComputeView`).
impl Dom {
    #[inline]
    pub(super) fn subtree_omitted_from_box_tree(&self, id: NodeId) -> bool {
        ComputeView(self).subtree_omitted_from_box_tree(id)
    }
    #[inline]
    pub fn is_hidden(&self, id: NodeId) -> bool {
        ComputeView(self).is_hidden(id)
    }
    #[inline]
    pub(super) fn box_generation(&self, id: NodeId) -> BoxGeneration {
        ComputeView(self).box_generation(id)
    }
    #[inline]
    pub fn paint_suppressed(&self, id: NodeId) -> bool {
        ComputeView(self).paint_suppressed(id)
    }
    #[inline]
    pub fn visibility_hidden(&self, id: NodeId) -> bool {
        ComputeView(self).visibility_hidden(id)
    }
    #[inline]
    pub fn effective_opacity(&self, id: NodeId) -> f32 {
        ComputeView(self).effective_opacity(id)
    }
    #[inline]
    pub(crate) fn css_animation_definitions(&self, id: NodeId) -> Vec<CssAnimationDefinition> {
        ComputeView(self).css_animation_definitions(id)
    }
    #[inline]
    pub fn computed_display(&self, id: NodeId) -> Option<String> {
        ComputeView(self).computed_display(id)
    }
    #[inline]
    pub fn effective_display(&self, id: NodeId) -> Option<String> {
        ComputeView(self).effective_display(id)
    }
    #[inline]
    pub(crate) fn legacy_line_clamp(&self, id: NodeId) -> Option<usize> {
        ComputeView(self).legacy_line_clamp(id)
    }
    #[inline]
    pub fn establishes_anonymous_table(&self, id: NodeId) -> bool {
        ComputeView(self).establishes_anonymous_table(id)
    }
    #[inline]
    pub(super) fn logical_property(
        &self,
        id: NodeId,
        pseudo: Option<PseudoEl>,
        name: &str,
    ) -> Option<String> {
        ComputeView(self).logical_property(id, pseudo, name)
    }
    #[inline]
    pub fn computed_value(&self, id: NodeId, name: &str) -> Option<String> {
        ComputeView(self).computed_value(id, name)
    }
    #[inline]
    pub fn computed_value_resolved(&self, id: NodeId, name: &str) -> Option<String> {
        ComputeView(self).computed_value_resolved(id, name)
    }
    #[inline]
    pub(crate) fn cached_font_units(
        &self,
        id: NodeId,
        compute: impl FnOnce() -> crate::layout2::Units,
    ) -> crate::layout2::Units {
        ComputeView(self).cached_font_units(id, compute)
    }
    #[inline]
    pub fn font_size_zero(&self, id: NodeId) -> Option<bool> {
        ComputeView(self).font_size_zero(id)
    }
    #[inline]
    pub(crate) fn document_element(&self) -> Option<NodeId> {
        ComputeView(self).document_element()
    }
    #[inline]
    pub(crate) fn is_document_element(&self, id: NodeId) -> bool {
        ComputeView(self).is_document_element(id)
    }
    #[inline]
    pub(crate) fn root_font_px(&self) -> f32 {
        ComputeView(self).root_font_px()
    }
    #[inline]
    pub(super) fn style_scope_root_element(&self, id: NodeId) -> Option<NodeId> {
        ComputeView(self).style_scope_root_element(id)
    }
    #[inline]
    pub(crate) fn font_px(&self, id: NodeId) -> f32 {
        ComputeView(self).font_px(id)
    }
    #[inline]
    pub(crate) fn is_details_summary(&self, id: NodeId) -> bool {
        ComputeView(self).is_details_summary(id)
    }
    #[inline]
    pub fn text_decoration(&self, id: NodeId) -> (bool, bool) {
        ComputeView(self).text_decoration(id)
    }
    #[inline]
    pub(super) fn cascaded(&self, id: NodeId, prop: &str) -> Option<String> {
        ComputeView(self).cascaded(id, prop)
    }
    #[inline]
    pub(crate) fn author_declares(&self, id: NodeId, prop: &str) -> bool {
        ComputeView(self).author_declares(id, prop)
    }
    #[inline]
    pub(crate) fn author_cascades(&self, id: NodeId, prop: &str) -> bool {
        ComputeView(self).author_cascades(id, prop)
    }
    #[inline]
    pub(super) fn cascaded_maps(&self, id: NodeId) -> std::rc::Rc<CascadedMaps> {
        ComputeView(self).cascaded_maps(id)
    }
    #[inline]
    pub(super) fn custom_prop(&self, id: NodeId, name: &str) -> Option<String> {
        ComputeView(self).custom_prop(id, name)
    }
    #[inline]
    pub(super) fn resolve_vars(&self, id: NodeId, value: &str) -> String {
        ComputeView(self).resolve_vars(id, value)
    }
    #[inline]
    pub(super) fn resolve_vars_owned(&self, id: NodeId, value: String) -> String {
        ComputeView(self).resolve_vars_owned(id, value)
    }
    #[inline]
    pub(super) fn resolve_pending_shorthand(
        &self,
        id: NodeId,
        name: &str,
        value: &str,
    ) -> Option<String> {
        ComputeView(self).resolve_pending_shorthand(id, name, value)
    }
    #[inline]
    pub(super) fn resolve_pseudo_pending_shorthand(
        &self,
        id: NodeId,
        which: PseudoEl,
        name: &str,
        value: &str,
    ) -> Option<String> {
        ComputeView(self).resolve_pseudo_pending_shorthand(id, which, name, value)
    }
    #[inline]
    pub(super) fn substitute_vars(
        &self,
        id: NodeId,
        value: &str,
        active: &mut Vec<String>,
    ) -> Option<String> {
        ComputeView(self).substitute_vars(id, value, active)
    }
    #[inline]
    pub(super) fn resolve_pseudo_vars(&self, id: NodeId, which: PseudoEl, value: &str) -> String {
        ComputeView(self).resolve_pseudo_vars(id, which, value)
    }
    #[inline]
    pub fn pseudo_style(&self, id: NodeId, which: PseudoEl, prop: &str) -> Option<String> {
        ComputeView(self).pseudo_style(id, which, prop)
    }
    #[inline]
    pub(crate) fn has_marker_style(&self, id: NodeId) -> bool {
        ComputeView(self).has_marker_style(id)
    }
    #[inline]
    pub(crate) fn pseudo_layout_value(
        &self,
        id: NodeId,
        which: PseudoEl,
        prop: &str,
    ) -> Option<String> {
        ComputeView(self).pseudo_layout_value(id, which, prop)
    }
    #[inline]
    pub(crate) fn baked_pseudo_value(
        &self,
        id: NodeId,
        which: PseudoEl,
        prop: &str,
    ) -> Option<String> {
        ComputeView(self).baked_pseudo_value(id, which, prop)
    }
    #[inline]
    pub(crate) fn document_font_set(
        &self,
        id: NodeId,
    ) -> Option<std::sync::Arc<crate::text::FontSet>> {
        ComputeView(self).document_font_set(id)
    }
    #[inline]
    pub(crate) fn scope_font_set(
        &self,
        id: NodeId,
    ) -> Option<std::sync::Arc<crate::text::FontSet>> {
        ComputeView(self).scope_font_set(id)
    }
    #[inline]
    pub(crate) fn retained_box_style(
        &self,
        id: NodeId,
        context: BoxContext,
        compute: impl FnOnce() -> BoxStyle,
    ) -> BoxStyle {
        ComputeView(self).retained_box_style(id, context, compute)
    }
    #[inline]
    pub(crate) fn retained_inline_style(
        &self,
        id: NodeId,
        parent: &InlineStyle,
        base: &url::Url,
        compute: impl FnOnce() -> InlineStyle,
    ) -> InlineStyle {
        ComputeView(self).retained_inline_style(id, parent, base, compute)
    }
    #[inline]
    pub(crate) fn color_scheme(&self, id: NodeId) -> ColorScheme {
        ComputeView(self).color_scheme(id)
    }
    #[inline]
    pub(super) fn is_color_scheme_meta(&self, id: NodeId) -> bool {
        ComputeView(self).is_color_scheme_meta(id)
    }
    #[inline]
    pub(crate) fn size_container_kind(&self, node: NodeId) -> u8 {
        ComputeView(self).size_container_kind(node)
    }
    #[inline]
    pub(in crate::dom) fn registration_document(&self, id: NodeId) -> NodeId {
        ComputeView(self).registration_document(id)
    }
    #[inline]
    pub(in crate::dom) fn property_base(&self, id: NodeId) -> Option<&url::Url> {
        ComputeView(self).property_base(id)
    }
}
