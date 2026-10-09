//! HTML input value/dirty-value state belongs to the arena, not JS wrappers.
use super::{Dom, NodeId, StyleView};
use crate::input::{NumericInput, NumericType};

const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

/// HTML #rules-for-parsing-non-negative-integers.
pub(super) fn parse_non_negative_integer(value: &str) -> Option<usize> {
    let trimmed = value.trim_start_matches(['\t', '\n', '\x0c', '\r', ' ']);
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let digits = &digits[..end];
    if negative {
        // Only a negative zero is a non-negative integer.
        return digits.bytes().all(|b| b == b'0').then_some(0);
    }
    Some(digits.parse::<usize>().unwrap_or(usize::MAX))
}

#[derive(Clone, Debug)]
pub(super) struct InputValue {
    pub(super) value: String,
    pub(super) editing: Option<String>,
    pub(super) dirty: bool,
    bad_input: bool,
}

/// Element state that is not a content attribute. For form controls: an
/// input's checkedness and dirty checkedness flag (#concept-fe-checked), its
/// indeterminate flag (#dom-input-indeterminate), an option's selectedness
/// and dirtiness (#concept-option-selectedness), and a textarea's raw value
/// with its dirty value flag (#concept-textarea-raw-value). `None` means the
/// state is still derived from the content attribute or child text. For
/// custom elements (HTML #custom-element-state and #states-set): whether the
/// element is defined (`Some(true)`: "custom"; `Some(false)`: "undefined" or
/// "failed"; `None`: derived from its name and `is` attribute), whether it is
/// a form-associated custom element, the entries of its states set, which
/// `:defined`, `:disabled` and `:state()` match, and an is value given at
/// creation (DOM #concept-element-is-value) that serialization writes.
#[derive(Clone, Debug, Default)]
pub(super) struct ElementState {
    pub(super) checked: Option<bool>,
    pub(super) checked_dirty: bool,
    pub(super) indeterminate: bool,
    pub(super) selected: Option<bool>,
    pub(super) selected_dirty: bool,
    pub(super) raw_value: Option<String>,
    pub(super) defined: Option<bool>,
    pub(super) form_associated: bool,
    pub(super) custom_states: Vec<String>,
    pub(super) is_value: Option<String>,
    /// A Document's design mode enabled flag
    /// (HTML #making-entire-documents-editable).
    pub(super) design_mode: bool,
    /// The names of a File Upload input's selected files
    /// (HTML #concept-input-type-file-selected). Their bodies belong to the
    /// page's `File` objects; the arena keeps the names for the control's
    /// rendering and its filename-mode value (#dom-input-value-filename).
    pub(super) selected_files: Vec<String>,
    /// HTML #custom-validity-error-message is not the empty string
    /// (`setCustomValidity()`; for a form-associated custom element, the
    /// customError flag of `face_validity`).
    pub(super) custom_error: bool,
    /// The validity flags `ElementInternals.setValidity()` gave a
    /// form-associated custom element (`validity::ValidityFlags` bits).
    pub(super) face_validity: u16,
    /// HTML #concept-fe-value: the value was last changed by a user edit (as
    /// opposed to a script), which `maxlength`/`minlength` observe.
    pub(super) user_edited: bool,
    /// HTML #user-validity, which `:user-valid`/`:user-invalid` observe.
    pub(super) user_validity: bool,
    /// HTML #selector-open: the select's drop-down box, or the input's
    /// picker, is open.
    pub(super) picker_open: bool,
    /// On a Document: whether its focused area indicates focus
    /// (Selectors 4 #indicate-focus, which `:focus-visible` matches).
    pub(super) focus_indicated: bool,
    /// On the arena's root Document: the user last interacted through a
    /// pointing device rather than a keyboard (the `:focus-visible`
    /// heuristics' modality; keyboard until a pointer is used).
    pub(super) pointer_modality: bool,
    /// HTML #selector-active: the element is being activated, or is a flat
    /// tree ancestor of one, or a label's labeled control.
    pub(super) active: bool,
    /// A media element's muted IDL attribute once script set it
    /// (HTML #dom-media-muted); `None` keeps the `muted` content attribute's.
    pub(super) muted: Option<bool>,
}

/// The selector-dependency names under which custom element state changes
/// invalidate `:defined` and `:state()`. NULL never occurs in an attribute
/// name (DOM #valid-attribute-local-name), so they cannot collide.
pub(super) const DEFINED_DEPENDENCY: &str = "\0defined";
pub(super) const CUSTOM_STATE_DEPENDENCY: &str = "\0state";
/// The dependency name under which constraint validation state that is not
/// an attribute (custom validity, user validity, user edits) invalidates the
/// validity pseudo-classes. A file selection change is a value change
/// (`Dom::set_input_files` touches the input value).
pub(super) const VALIDITY_DEPENDENCY: &str = "\0validity";
/// The dependency name of a select's drop-down box or an input's picker
/// opening or closing (`:open`).
pub(super) const OPEN_DEPENDENCY: &str = "\0open";
/// The dependency names of `:active` and of a media element's muted state.
pub(super) const ACTIVE_DEPENDENCY: &str = "\0active";
pub(super) const MUTED_DEPENDENCY: &str = "\0muted";

impl ElementState {
    fn is_default(&self) -> bool {
        self.checked.is_none()
            && !self.checked_dirty
            && !self.indeterminate
            && self.selected.is_none()
            && !self.selected_dirty
            && self.raw_value.is_none()
            && self.defined.is_none()
            && !self.form_associated
            && self.custom_states.is_empty()
            && self.is_value.is_none()
            && !self.design_mode
            && self.selected_files.is_empty()
            && !self.custom_error
            && self.face_validity == 0
            && !self.user_edited
            && !self.user_validity
            && !self.picker_open
            && !self.focus_indicated
            && !self.pointer_modality
            && !self.active
            && self.muted.is_none()
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.raw_value.as_ref().map_or(0, String::capacity)
            + self.is_value.as_ref().map_or(0, String::capacity)
            + self.custom_states.capacity() * std::mem::size_of::<String>()
            + self
                .custom_states
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + self.selected_files.capacity() * std::mem::size_of::<String>()
            + self
                .selected_files
                .iter()
                .map(String::capacity)
                .sum::<usize>()
    }
    /// The state an element's clone starts with (HTML's cloning steps copy
    /// checkedness and the textarea raw value; a clone is a new element for
    /// custom element purposes and its option selectedness is initialized
    /// from its attribute). The input cloning steps do not copy selected
    /// files.
    pub(super) fn for_clone(&self) -> Option<Self> {
        let copy = Self {
            checked: self.checked,
            checked_dirty: self.checked_dirty,
            indeterminate: self.indeterminate,
            raw_value: self.raw_value.clone(),
            // DOM #concept-node-clone: the copy keeps the is value.
            is_value: self.is_value.clone(),
            ..Self::default()
        };
        (!copy.is_default()).then_some(copy)
    }
}

/// HTML #valid-custom-element-name.
pub(crate) fn is_valid_custom_element_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.contains('-')
        && !name.chars().any(|c| {
            c.is_ascii_uppercase()
                || matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ' | '\0' | '/' | '>')
        })
        && !matches!(
            name,
            "annotation-xml"
                | "color-profile"
                | "font-face"
                | "font-face-src"
                | "font-face-uri"
                | "font-face-format"
                | "font-face-name"
                | "missing-glyph"
        )
}

impl Dom {
    pub(super) fn update_control_state(
        &mut self,
        id: NodeId,
        update: impl FnOnce(&mut ElementState),
    ) {
        let mut state = self.element_states.remove(&id).unwrap_or_default();
        update(&mut state);
        if !state.is_default() {
            self.element_states.insert(id, state);
        }
    }
    /// HTML #concept-fe-checked for a checkbox or radio button input.
    pub fn checkedness(&self, id: NodeId) -> bool {
        self.style_view().checkedness(id)
    }
    pub fn checked_dirty(&self, id: NodeId) -> bool {
        self.element_states
            .get(&id)
            .is_some_and(|s| s.checked_dirty)
    }
    /// Set the checkedness (and, when given, the dirty checkedness flag).
    /// Returns whether the checkedness changed; selectors are invalidated.
    pub(crate) fn set_checkedness(
        &mut self,
        id: NodeId,
        checked: bool,
        dirty: Option<bool>,
    ) -> bool {
        if !self.is_valid(id) {
            return false;
        }
        let before = self.checkedness(id);
        self.update_control_state(id, |state| {
            state.checked = Some(checked);
            if let Some(dirty) = dirty {
                state.checked_dirty = dirty;
            }
        });
        if before != checked {
            self.touch_attr(id, "checked");
        }
        before != checked
    }
    /// HTML #the-input-element reset algorithm for checkedness: clear the
    /// dirty flag and take the checked content attribute's presence.
    pub(crate) fn reset_checkedness(&mut self, id: NodeId) -> bool {
        let before = self.checkedness(id);
        self.update_control_state(id, |state| {
            state.checked = None;
            state.checked_dirty = false;
        });
        let changed = before != self.checkedness(id);
        if changed {
            self.touch_attr(id, "checked");
        }
        changed
    }
    pub fn indeterminate(&self, id: NodeId) -> bool {
        self.style_view().indeterminate(id)
    }
    pub(crate) fn set_indeterminate(&mut self, id: NodeId, value: bool) {
        if !self.is_valid(id) || self.indeterminate(id) == value {
            return;
        }
        self.update_control_state(id, |state| state.indeterminate = value);
        self.touch_attr(id, "checked");
    }
    /// HTML #concept-element-custom-element-state, as `:defined` observes
    /// it: `Some(true)` once the element is custom, `Some(false)` while it is
    /// undefined or failed, `None` to derive it from the element's name.
    pub(crate) fn set_custom_element_defined(&mut self, id: NodeId, defined: Option<bool>) {
        if !self.is_valid(id) {
            return;
        }
        let before = self.style_view().is_defined(id);
        self.update_control_state(id, |state| state.defined = defined);
        if before != self.style_view().is_defined(id) {
            self.touch_attr(id, DEFINED_DEPENDENCY);
        }
    }
    /// HTML #form-associated-custom-element: whether the element is one,
    /// for `:disabled`/`:enabled` (#concept-fe-disabled).
    pub(crate) fn set_form_associated_custom_element(&mut self, id: NodeId, value: bool) {
        if !self.is_valid(id)
            || self
                .element_states
                .get(&id)
                .is_some_and(|s| s.form_associated)
                == value
        {
            return;
        }
        self.update_control_state(id, |state| state.form_associated = value);
        self.touch_attr(id, "disabled");
    }
    /// HTML #designMode: enabling it makes every element of the Document
    /// editable (`:read-write`).
    pub(crate) fn set_design_mode(&mut self, document: NodeId, enabled: bool) {
        if !self.is_valid(document)
            || self
                .element_states
                .get(&document)
                .is_some_and(|s| s.design_mode)
                == enabled
        {
            return;
        }
        self.update_control_state(document, |state| state.design_mode = enabled);
        self.touch();
    }
    /// DOM #concept-element-is-value given when the element was created
    /// (createElement's options); the HTML fragment serialization algorithm
    /// writes it as an `is` attribute when the element has none.
    pub(crate) fn set_is_value(&mut self, id: NodeId, value: Option<&str>) {
        if !self.is_valid(id) {
            return;
        }
        self.update_control_state(id, |state| state.is_value = value.map(str::to_owned));
    }
    pub(super) fn is_value(&self, id: NodeId) -> Option<&str> {
        self.element_states
            .get(&id)
            .and_then(|s| s.is_value.as_deref())
    }
    pub fn is_value_of(&self, id: NodeId) -> Option<&str> {
        self.is_value(id)
    }
    /// Replace the entries of a custom element's states set
    /// (HTML #customstateset), which `:state()` matches.
    pub(crate) fn set_custom_states(&mut self, id: NodeId, states: Vec<String>) {
        if !self.is_valid(id) {
            return;
        }
        let unchanged = self
            .element_states
            .get(&id)
            .map_or(states.is_empty(), |state| state.custom_states == states);
        if unchanged {
            return;
        }
        self.update_control_state(id, |state| state.custom_states = states);
        self.touch_attr(id, CUSTOM_STATE_DEPENDENCY);
    }
    /// The select whose list of options loses options when `removed` leaves
    /// `parent`: the nearest select ancestor of `parent` (HTML
    /// #option-element-nearest-ancestor-select stops at datalist, hr and
    /// option), when the removed subtree holds an HTML option.
    pub(super) fn select_losing_options(&self, parent: NodeId, removed: NodeId) -> Option<NodeId> {
        let mut current = Some(parent);
        let mut select = None;
        while let Some(node) = current {
            match self.tag_name(node) {
                Some("select") if self.is_html_element(node) => {
                    select = Some(node);
                    break;
                }
                Some("datalist" | "hr" | "option") if self.is_html_element(node) => return None,
                _ => {}
            }
            current = self.nodes[node].parent;
        }
        let select = select?;
        std::iter::once(removed)
            .chain(self.descendants(removed))
            .any(|node| self.tag_name(node) == Some("option") && self.is_html_element(node))
            .then_some(select)
    }
    /// Fix a pristine select's derived option selectedness into explicit
    /// state before its list of options changes.
    pub(super) fn materialize_select_states(&mut self, select: NodeId) {
        let options = self.select_options(select);
        if options.iter().any(|option| {
            self.element_states
                .get(option)
                .is_some_and(|s| s.selected.is_some())
        }) {
            return;
        }
        let selected: Vec<bool> = options
            .iter()
            .map(|&option| self.option_selectedness(option))
            .collect();
        for (option, selected) in options.into_iter().zip(selected) {
            self.update_control_state(option, |state| state.selected = Some(selected));
        }
    }
    /// HTML #selectedness-setting-algorithm.
    pub(super) fn run_selectedness_setting_algorithm(&mut self, select: NodeId) {
        if !self.is_valid(select) || self.attr(select, "multiple").is_some() {
            return;
        }
        let options = self.select_options(select);
        let selected: Vec<NodeId> = options
            .iter()
            .copied()
            .filter(|&option| self.option_selectedness(option))
            .collect();
        if selected.is_empty() {
            if self.style_view().select_display_size(select) != 1 {
                return;
            }
            let first = options
                .into_iter()
                .find(|&option| !self.style_view().option_disabled(option));
            if let Some(first) = first {
                self.set_option_selectedness(first, Some(true), None);
            }
            return;
        }
        for &option in &selected[..selected.len() - 1] {
            self.set_option_selectedness(option, Some(false), None);
        }
    }
    /// Whether the subtree holds an HTML option or radio button input, whose
    /// insertion or removal changes select or radio button group state.
    pub fn subtree_has_form_state_controls(&self, root: NodeId) -> bool {
        std::iter::once(root)
            .chain(self.descendants(root))
            .any(|node| match self.tag_name(node) {
                Some("option") => self.is_html_element(node),
                Some("input") => self.is_html_element(node) && self.input_type(node) == "radio",
                _ => false,
            })
    }
    pub fn is_html_element(&self, id: NodeId) -> bool {
        self.style_view().nodes.namespace_uri(id) == Some(HTML_NAMESPACE)
    }
    /// HTML #concept-select-option-list.
    pub fn select_options(&self, select: NodeId) -> Vec<NodeId> {
        self.style_view().select_options(select)
    }
    /// HTML #option-element-nearest-ancestor-select.
    pub fn option_nearest_select(&self, option: NodeId) -> Option<NodeId> {
        self.style_view().option_nearest_select(option)
    }
    /// HTML #concept-option-selectedness.
    pub fn option_selectedness(&self, id: NodeId) -> bool {
        self.style_view().option_selectedness(id)
    }
    /// HTML #concept-option-disabled.
    pub fn option_is_disabled(&self, id: NodeId) -> bool {
        self.style_view().option_disabled(id)
    }
    /// HTML #concept-select-size.
    pub fn select_size(&self, select: NodeId) -> usize {
        self.style_view().select_display_size(select)
    }
    pub fn option_dirtiness(&self, id: NodeId) -> bool {
        self.element_states
            .get(&id)
            .is_some_and(|s| s.selected_dirty)
    }
    /// Set an option's selectedness (`None`: derive it from the content
    /// attribute again) and, when given, its dirtiness.
    pub(crate) fn set_option_selectedness(
        &mut self,
        id: NodeId,
        selected: Option<bool>,
        dirty: Option<bool>,
    ) -> bool {
        if !self.is_valid(id) {
            return false;
        }
        let before = self.option_selectedness(id);
        self.update_control_state(id, |state| {
            state.selected = selected;
            if let Some(dirty) = dirty {
                state.selected_dirty = dirty;
            }
        });
        let changed = before != self.option_selectedness(id);
        if changed {
            self.touch_attr(id, "selected");
        }
        changed
    }
    /// HTML #concept-textarea-api-value: the raw value (the child text
    /// content until the dirty value flag is set) with newlines normalized.
    pub fn textarea_api_value(&self, id: NodeId) -> String {
        self.style_view().textarea_api_value(id)
    }
    pub fn textarea_dirty(&self, id: NodeId) -> bool {
        self.element_states
            .get(&id)
            .is_some_and(|s| s.raw_value.is_some())
    }
    /// Set the textarea's raw value (marking it dirty), or with `None` reset
    /// it to the child text content (HTML #the-textarea-element reset).
    pub(crate) fn set_textarea_raw_value(&mut self, id: NodeId, value: Option<&str>) -> bool {
        if !self.is_valid(id) || self.tag_name(id) != Some("textarea") {
            return false;
        }
        let before = self.textarea_api_value(id);
        self.update_control_state(id, |state| state.raw_value = value.map(str::to_owned));
        let changed = before != self.textarea_api_value(id);
        if changed {
            self.clamp_control_selection(id);
            self.touch_input_value(id);
        }
        changed
    }
    pub(super) fn write_control_presentation(&self, id: NodeId, out: &mut String) {
        // A design-mode Document's flag rides on its document element.
        if let Some(parent) = self.nodes[id].parent
            && self
                .element_states
                .get(&parent)
                .is_some_and(|s| s.design_mode)
        {
            out.push_str(" data-trust-design-mode=\"\"");
        }
        let Some(state) = self.element_states.get(&id) else {
            return;
        };
        if let Some(checked) = state.checked {
            out.push_str(if checked {
                " data-trust-checked=\"true\""
            } else {
                " data-trust-checked=\"false\""
            });
        }
        if state.indeterminate {
            out.push_str(" data-trust-indeterminate=\"\"");
        }
        if let Some(selected) = state.selected {
            out.push_str(if selected {
                " data-trust-selected=\"true\""
            } else {
                " data-trust-selected=\"false\""
            });
        }
        if let Some(value) = &state.raw_value {
            out.push_str(" data-trust-textarea-value=\"");
            out.push_str(&super::escape_attr(value));
            out.push('"');
        }
        if state.form_associated {
            out.push_str(" data-trust-face=\"\"");
        }

        if let Some(defined) = state.defined {
            out.push_str(if defined {
                " data-trust-defined=\"true\""
            } else {
                " data-trust-defined=\"false\""
            });
        }
        if !state.custom_states.is_empty() {
            let states = serde_json::to_string(&state.custom_states).unwrap_or_default();
            out.push_str(" data-trust-states=\"");
            out.push_str(&super::escape_attr(&states));
            out.push('"');
        }
        // Constraint validation state that is not an attribute (see
        // `ElementState`), for the validity and `:open` pseudo-classes.
        if state.custom_error {
            out.push_str(" data-trust-custom-error=\"\"");
        }
        if state.face_validity != 0 {
            out.push_str(&format!(
                " data-trust-face-validity=\"{}\"",
                state.face_validity
            ));
        }
        if state.user_edited {
            out.push_str(" data-trust-user-edited=\"\"");
        }
        if state.user_validity {
            out.push_str(" data-trust-user-validity=\"\"");
        }
        if state.picker_open {
            out.push_str(" data-trust-open=\"\"");
        }
    }
    pub fn control_selection(&self, id: NodeId) -> Option<crate::doc::ControlSelection> {
        let value = match self.tag_name(id) {
            Some("input")
                if matches!(
                    self.input_type(id).as_str(),
                    "text" | "search" | "url" | "tel" | "password"
                ) =>
            {
                self.input_value(id)
            }
            Some("textarea") => self.textarea_api_value(id),
            _ => return None,
        };
        let mut selection = self
            .control_selections
            .get(&id)
            .copied()
            .unwrap_or_default();
        let length = value.encode_utf16().count() as u32;
        selection.end = selection.end.min(length);
        selection.start = selection.start.min(selection.end);
        Some(selection)
    }

    pub(crate) fn set_control_selection(
        &mut self,
        id: NodeId,
        mut selection: crate::doc::ControlSelection,
    ) -> bool {
        let Some(previous) = self.control_selection(id) else {
            return false;
        };
        // HTML #set-the-selection-range clamps to the relevant value and
        // collapses a reversed range at its end. Selection is UI state, not a
        // DOM mutation or a reason to invalidate the layout fragment cache.
        let value = if self.tag_name(id) == Some("input") {
            self.input_value(id)
        } else {
            self.textarea_api_value(id)
        };
        selection.end = selection.end.min(value.encode_utf16().count() as u32);
        selection.start = selection.start.min(selection.end);
        selection.direction = selection.direction.signum();
        self.control_selections.insert(id, selection);
        let changed = previous != selection;
        self.dirty |= changed;
        changed
    }

    pub(super) fn clamp_control_selection(&mut self, id: NodeId) {
        if self.control_selections.contains_key(&id)
            && let Some(selection) = self.control_selection(id)
        {
            self.control_selections.insert(id, selection);
        }
    }

    pub(super) fn write_input_presentation(&self, id: NodeId, out: &mut String) {
        if self.tag_name(id) != Some("input") {
            return;
        }
        if let Some(state) = self.element_states.get(&id)
            && !state.selected_files.is_empty()
        {
            out.push_str(" data-trust-input-files=\"");
            out.push_str(&super::escape_attr(
                &serde_json::to_string(&state.selected_files).unwrap_or_default(),
            ));
            out.push('"');
        }
        if !self.input_values.contains_key(&id) {
            return;
        }
        out.push_str(" data-trust-input-value=\"");
        out.push_str(&super::escape_attr(&self.input_value(id)));
        out.push('"');
        if let Some(editing) = self.input_editing_value(id) {
            out.push_str(" data-trust-input-edit=\"");
            out.push_str(&super::escape_attr(editing));
            out.push('"');
        }
    }
    pub(super) fn input_value_mode(t: &str) -> bool {
        !matches!(
            t,
            "hidden" | "submit" | "image" | "reset" | "button" | "checkbox" | "radio" | "file"
        )
    }
    pub(crate) fn numeric_input(&self, id: NodeId) -> Option<NumericInput> {
        self.style_view().numeric_input(id)
    }
    fn sanitize_input_value(&self, id: NodeId, value: &str) -> String {
        self.style_view().sanitize_input_value(id, value)
    }
    pub fn input_value(&self, id: NodeId) -> String {
        self.style_view().input_value(id)
    }
    pub fn set_input_value(&mut self, id: NodeId, value: &str, user: bool) -> bool {
        if !self.is_valid(id) {
            return false;
        }
        if !Self::input_value_mode(&self.input_type(id)) {
            if self.input_type(id) != "file" {
                self.set_attr(id, "value", value);
            }
            return false;
        }
        if user && !self.input_mutable(id) {
            return false;
        }
        let sanitized = self.sanitize_input_value(id, value);
        let bad_input =
            user && !value.is_empty() && sanitized.is_empty() && self.numeric_input(id).is_some();
        let value_changed = self.input_value(id) != sanitized;
        let changed = value_changed || self.input_bad_input(id) != bad_input;
        let end = sanitized.encode_utf16().count() as u32;
        let editing = bad_input.then(|| value.to_string());
        let changed = changed || self.input_editing_value(id) != editing.as_deref();
        self.input_values.insert(
            id,
            InputValue {
                value: sanitized,
                editing,
                dirty: true,
                bad_input,
            },
        );
        if changed {
            self.touch_input_value(id);
        }
        if value_changed && !user {
            self.set_control_selection(
                id,
                crate::doc::ControlSelection {
                    start: end,
                    end,
                    direction: 0,
                },
            );
        }
        self.clamp_control_selection(id);
        changed
    }
    pub(crate) fn input_bad_input(&self, id: NodeId) -> bool {
        self.input_values.get(&id).is_some_and(|s| s.bad_input)
    }
    pub(crate) fn input_editing_value(&self, id: NodeId) -> Option<&str> {
        self.style_view().input_editing_value(id)
    }
    /// HTML #concept-fe-mutable: a disabled input is not mutable, nor is a
    /// read-only one where #attr-input-readonly applies (text-like, date and
    /// time, and number states). Where it does not apply (e.g. File Upload)
    /// the attribute is ignored.
    pub fn input_mutable(&self, id: NodeId) -> bool {
        !self.actually_disabled(id, "input")
            && (self.attr(id, "readonly").is_none()
                || !matches!(
                    self.input_type(id).as_str(),
                    "text"
                        | "search"
                        | "url"
                        | "tel"
                        | "email"
                        | "password"
                        | "date"
                        | "month"
                        | "week"
                        | "time"
                        | "datetime-local"
                        | "number"
                ))
    }
    pub fn input_spin_buttons(&self, id: NodeId) -> bool {
        self.tag_name(id) == Some("input")
            && self.input_type(id) == "number"
            && !["appearance", "-webkit-appearance"].iter().any(|p| {
                self.computed_value_resolved(id, p)
                    .is_some_and(|v| matches!(v.trim(), "none" | "textfield"))
            })
    }
    pub(crate) fn reset_input_value(&mut self, id: NodeId) {
        self.control_selections.remove(&id);
        if self.input_values.remove(&id).is_some() {
            self.touch_input_value(id);
        }
    }
    /// The names of a File Upload input's selected files, in order.
    pub fn input_file_names(&self, id: NodeId) -> Vec<String> {
        self.style_view().input_file_names(id)
    }
    /// Replace the names of an input's selected files (HTML
    /// #concept-input-type-file-selected); the page realm owns the matching
    /// `File` objects. Returns whether the names changed.
    pub(crate) fn set_input_files(&mut self, id: NodeId, names: Vec<String>) -> bool {
        if !self.is_valid(id) || self.tag_name(id) != Some("input") {
            return false;
        }
        if self
            .element_states
            .get(&id)
            .map_or(names.is_empty(), |state| state.selected_files == names)
        {
            return false;
        }
        self.update_control_state(id, |state| state.selected_files = names);
        self.touch_input_value(id);
        true
    }
    pub(super) fn input_attribute_changed(
        &mut self,
        id: NodeId,
        attr: &str,
        before: Option<(String, String)>,
    ) {
        if self.tag_name(id) != Some("input") {
            return;
        }
        if attr.eq_ignore_ascii_case("type") {
            let new_type = self.input_type(id);
            let (old_type, old_value) = before.unwrap_or_else(|| ("text".into(), String::new()));
            if old_type == new_type {
                return;
            }
            let old_mode = Self::input_value_mode(&old_type);
            let new_mode = Self::input_value_mode(&new_type);
            if old_mode && !new_mode {
                self.input_values.remove(&id);
                if !old_value.is_empty() && new_type != "file" {
                    self.set_attr(id, "value", &old_value);
                }
            } else if !old_mode && new_mode {
                self.input_values.remove(&id);
            } else if old_mode && new_mode {
                let dirty = self.input_values.get(&id).is_some_and(|s| s.dirty);
                let value = self.sanitize_input_value(id, &old_value);
                self.input_values.insert(
                    id,
                    InputValue {
                        value,
                        dirty,
                        bad_input: false,
                        editing: None,
                    },
                );
            }
            if !matches!(
                old_type.as_str(),
                "text" | "search" | "url" | "tel" | "password"
            ) {
                self.control_selections.remove(&id);
            }
            self.clamp_control_selection(id);
            return;
        }
        if attr.eq_ignore_ascii_case("value")
            && self.input_values.get(&id).is_some_and(|s| !s.dirty)
        {
            self.input_values.remove(&id);
        }
        self.clamp_control_selection(id);
        // HTML #color-state-(type=color): a change of the alpha or
        // colorspace attribute runs "update a color well control color",
        // which reads the value content attribute while the dirty value
        // flag is false (and the value derives from it again).
        if self.input_type(id) == "color"
            && ["alpha", "colorspace"]
                .iter()
                .any(|name| attr.eq_ignore_ascii_case(name))
            && !self.input_values.get(&id).is_some_and(|s| s.dirty)
        {
            self.input_values.remove(&id);
            return;
        }
        let color_well = self.input_type(id) == "color"
            && ["alpha", "colorspace"]
                .iter()
                .any(|name| attr.eq_ignore_ascii_case(name));
        if (color_well
            || (self.input_type(id) == "range"
                && ["min", "max", "step"]
                    .iter()
                    .any(|name| attr.eq_ignore_ascii_case(name))))
            && let Some((_, old_value)) = before
        {
            let dirty = self.input_values.get(&id).is_some_and(|s| s.dirty);
            let value = self.sanitize_input_value(id, &old_value);
            self.input_values.insert(
                id,
                InputValue {
                    value,
                    dirty,
                    bad_input: false,
                    editing: None,
                },
            );
        }
    }
    pub(crate) fn step_input(&mut self, id: NodeId, down: bool, count: i32) -> Result<bool, ()> {
        let config = self.numeric_input(id).ok_or(())?;
        let Some(value) = config.stepped(&self.input_value(id), down, count)? else {
            return Ok(false);
        };
        // The common APIs set the value itself; they do not run the `.value`
        // setter's additional dirty-flag steps. Preserve existing dirtiness.
        let dirty = self.input_values.get(&id).is_some_and(|s| s.dirty);
        let changed = self.set_input_value(id, &value, false);
        if let Some(state) = self.input_values.get_mut(&id) {
            state.dirty = dirty;
        }
        Ok(changed)
    }
    pub(crate) fn set_input_number(&mut self, id: NodeId, value: f64) -> bool {
        let Some(config) = self.numeric_input(id) else {
            return false;
        };
        let dirty = self.input_values.get(&id).is_some_and(|s| s.dirty);
        self.set_input_value(id, &config.kind.format(value), false);
        if let Some(state) = self.input_values.get_mut(&id) {
            state.dirty = dirty;
        }
        true
    }
}

impl<'a> StyleView<'a> {
    /// A presentation snapshot carries control state in `data-trust-*`
    /// attributes; a live arena keeps it in `element_states`.
    fn presented_bool(&self, id: NodeId, name: &str) -> Option<bool> {
        if self.state.render_live {
            return None;
        }
        self.attr(id, name).map(|value| value == "true")
    }
    pub(super) fn checkedness(&self, id: NodeId) -> bool {
        if let Some(checked) = self.state.element_states.get(&id).and_then(|s| s.checked) {
            return checked;
        }
        if let Some(checked) = self.presented_bool(id, "data-trust-checked") {
            return checked;
        }
        if self.attr(id, "checked").is_none() {
            return false;
        }
        // HTML #radio-button-group: inserting a checked radio button unchecks
        // the others of its group, so of several parser-inserted checked
        // buttons the last one in tree order stays checked.
        !(self.input_type(id) == "radio" && self.later_checked_radio(id))
    }
    fn later_checked_radio(&self, id: NodeId) -> bool {
        let Some(name) = self.attr(id, "name").filter(|name| !name.is_empty()) else {
            return false;
        };
        let form = self.radio_form(id);
        let mut root = id;
        while let Some(parent) = self.nodes.parent_of(root) {
            root = parent;
        }
        let mut seen = false;
        for node in self.descendants(root) {
            if node == id {
                seen = true;
                continue;
            }
            if seen
                && self.tag_name(node) == Some("input")
                && self.input_type(node) == "radio"
                && self.attr(node, "name") == Some(name)
                && self.attr(node, "checked").is_some()
                && !self
                    .state
                    .element_states
                    .get(&node)
                    .is_some_and(|s| s.checked.is_some())
                && self.radio_form(node) == form
            {
                return true;
            }
        }
        false
    }
    fn radio_form(&self, id: NodeId) -> Option<NodeId> {
        let mut current = self.nodes.parent_of(id);
        while let Some(ancestor) = current {
            if self.tag_name(ancestor) == Some("form") {
                return Some(ancestor);
            }
            current = self.nodes.parent_of(ancestor);
        }
        None
    }
    /// Selectors 4 / HTML #selector-defined: an element whose custom element
    /// state is "uncustomized" or "custom".
    pub(super) fn is_defined(&self, id: NodeId) -> bool {
        if let Some(defined) = self.state.element_states.get(&id).and_then(|s| s.defined) {
            return defined;
        }
        if let Some(defined) = self.presented_bool(id, "data-trust-defined") {
            return defined;
        }
        // An element created without a definition is "undefined" when it is
        // an HTML element with a valid custom element name or an `is` value,
        // and "uncustomized" otherwise (DOM #concept-create-element).
        if self.nodes.namespace_uri(id) != Some(HTML_NAMESPACE) {
            return true;
        }
        let undefined = self.tag_name(id).is_some_and(is_valid_custom_element_name)
            || self.attr(id, "is").is_some()
            || self
                .state
                .element_states
                .get(&id)
                .is_some_and(|s| s.is_value.is_some());
        !undefined
    }
    /// Whether `id` is in a Document whose design mode is enabled: the
    /// root of its tree is that Document (in a presentation snapshot, its
    /// document element carries the marker).
    pub(super) fn in_design_mode_document(&self, id: NodeId) -> bool {
        let mut root = id;
        let mut element = None;
        while let Some(parent) = self.nodes.parent_of(root) {
            element = Some(root);
            root = parent;
        }
        if self
            .state
            .element_states
            .get(&root)
            .is_some_and(|s| s.design_mode)
        {
            return true;
        }
        !self.state.render_live
            && element.is_some_and(|el| self.attr(el, "data-trust-design-mode").is_some())
    }
    pub(super) fn is_form_associated_custom_element(&self, id: NodeId) -> bool {
        self.state
            .element_states
            .get(&id)
            .is_some_and(|s| s.form_associated)
            || (!self.state.render_live && self.attr(id, "data-trust-face").is_some())
    }
    /// HTML #selector-custom: the element's states set contains `name`.
    pub(super) fn has_custom_state(&self, id: NodeId, name: &str) -> bool {
        if let Some(state) = self.state.element_states.get(&id)
            && !state.custom_states.is_empty()
        {
            return state.custom_states.iter().any(|entry| entry == name);
        }
        if self.state.render_live {
            return false;
        }
        self.attr(id, "data-trust-states")
            .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
            .is_some_and(|states| states.iter().any(|entry| entry == name))
    }
    pub(super) fn indeterminate(&self, id: NodeId) -> bool {
        self.state
            .element_states
            .get(&id)
            .is_some_and(|s| s.indeterminate)
            || (!self.state.render_live && self.attr(id, "data-trust-indeterminate").is_some())
    }
    pub(super) fn option_selectedness(&self, id: NodeId) -> bool {
        if let Some(selected) = self.state.element_states.get(&id).and_then(|s| s.selected) {
            return selected;
        }
        if let Some(selected) = self.presented_bool(id, "data-trust-selected") {
            return selected;
        }
        self.default_option_selectedness(id)
    }
    /// HTML #option-element-nearest-ancestor-select.
    pub(super) fn option_nearest_select(&self, id: NodeId) -> Option<NodeId> {
        let mut optgroup = false;
        let mut current = self.nodes.parent_of(id);
        while let Some(ancestor) = current {
            if !self.nodes.is_element(ancestor)
                || self.nodes.namespace_uri(ancestor) != Some(HTML_NAMESPACE)
            {
                current = self.nodes.parent_of(ancestor);
                continue;
            }
            match self.tag_name(ancestor) {
                Some("datalist" | "hr" | "option") => return None,
                Some("optgroup") => {
                    if optgroup {
                        return None;
                    }
                    optgroup = true;
                }
                Some("select") => return Some(ancestor),
                _ => {}
            }
            current = self.nodes.parent_of(ancestor);
        }
        None
    }
    /// HTML #concept-select-option-list.
    pub(super) fn select_options(&self, select: NodeId) -> Vec<NodeId> {
        let mut options = Vec::new();
        let mut node = self.nodes.first_child_of(select);
        while let Some(current) = node {
            let html = self.nodes.is_element(current)
                && self.nodes.namespace_uri(current) == Some(HTML_NAMESPACE);
            let tag = if html { self.tag_name(current) } else { None };
            if tag == Some("option") {
                options.push(current);
            }
            let nested_optgroup = tag == Some("optgroup") && {
                let mut nested = false;
                let mut up = self.nodes.parent_of(current);
                while let Some(ancestor) = up {
                    if ancestor == select {
                        break;
                    }
                    if self.tag_name(ancestor) == Some("optgroup") {
                        nested = true;
                        break;
                    }
                    up = self.nodes.parent_of(ancestor);
                }
                nested
            };
            let skip =
                matches!(tag, Some("select" | "hr" | "option" | "datalist")) || nested_optgroup;
            node = if !skip && let Some(child) = self.nodes.first_child_of(current) {
                Some(child)
            } else {
                // The next node in tree order within select, skipping
                // `current`'s descendants.
                let mut up = Some(current);
                let mut next = None;
                while let Some(at) = up {
                    if at == select {
                        break;
                    }
                    if let Some(sibling) = self.nodes.next_sibling(at) {
                        next = Some(sibling);
                        break;
                    }
                    up = self.nodes.parent_of(at);
                }
                next
            };
        }
        options
    }
    /// HTML #concept-option-disabled.
    pub(super) fn option_disabled(&self, id: NodeId) -> bool {
        if self.attr(id, "disabled").is_some() {
            return true;
        }
        let mut current = self.nodes.parent_of(id);
        while let Some(ancestor) = current {
            match self.tag_name(ancestor) {
                Some("select" | "hr" | "datalist" | "option") => return false,
                Some("optgroup") => return self.attr(ancestor, "disabled").is_some(),
                _ => {}
            }
            current = self.nodes.parent_of(ancestor);
        }
        false
    }
    /// HTML #concept-select-size.
    pub(super) fn select_display_size(&self, select: NodeId) -> usize {
        match self
            .attr(select, "size")
            .and_then(parse_non_negative_integer)
        {
            Some(size) if size > 0 => size,
            _ if self.attr(select, "multiple").is_some() => 4,
            _ => 1,
        }
    }
    /// The selectedness an untouched option has: the selected attribute's
    /// presence after its select's selectedness setting algorithm
    /// (HTML #selectedness-setting-algorithm).
    fn default_option_selectedness(&self, id: NodeId) -> bool {
        let attribute = self.attr(id, "selected").is_some();
        let Some(select) = self.option_nearest_select(id) else {
            return attribute;
        };
        if self.attr(select, "multiple").is_some() {
            return attribute;
        }
        let options = self.select_options(select);
        // Once script has set any option's selectedness, the platform keeps
        // the select's states and runs the algorithm itself.
        if options.iter().any(|option| {
            self.state
                .element_states
                .get(option)
                .is_some_and(|s| s.selected.is_some())
        }) {
            return attribute;
        }
        let selected: Vec<NodeId> = options
            .iter()
            .copied()
            .filter(|&option| self.attr(option, "selected").is_some())
            .collect();
        if selected.is_empty() {
            if self.select_display_size(select) != 1 {
                return false;
            }
            return options
                .into_iter()
                .find(|&option| !self.option_disabled(option))
                == Some(id);
        }
        selected.last() == Some(&id)
    }
    pub(super) fn textarea_api_value(&self, id: NodeId) -> String {
        let raw = if let Some(raw) = self
            .state
            .element_states
            .get(&id)
            .and_then(|s| s.raw_value.as_deref())
        {
            raw.to_owned()
        } else if let Some(raw) = (!self.state.render_live)
            .then(|| self.attr(id, "data-trust-textarea-value"))
            .flatten()
        {
            raw.to_owned()
        } else {
            let mut text = String::new();
            let mut child = self.nodes.first_child_of(id);
            while let Some(current) = child {
                if let Some(data) = self.nodes.text(current) {
                    text.push_str(data);
                }
                child = self.nodes.next_sibling(current);
            }
            text
        };
        raw.replace("\r\n", "\n").replace('\r', "\n")
    }
    pub(super) fn numeric_input(&self, id: NodeId) -> Option<NumericInput> {
        let kind = NumericType::from_type(&self.input_type(id))?;
        Some(NumericInput::new(
            kind,
            self.attr(id, "min"),
            self.attr(id, "max"),
            self.attr(id, "step"),
            self.attr(id, "value"),
        ))
    }
    fn sanitize_input_value(&self, id: NodeId, value: &str) -> String {
        if let Some(config) = self.numeric_input(id) {
            return config.sanitize(value);
        }
        let value = value.replace(['\r', '\n'], "");
        let strip = |value: &str| {
            value
                .trim_matches(['\t', '\n', '\x0c', '\r', ' '])
                .to_owned()
        };
        match self.input_type(id).as_str() {
            // HTML #url-state-(type=url) and #email-state-(type=email): strip
            // newlines, then leading and trailing ASCII whitespace.
            "url" => strip(&value),
            // With `multiple`: split on commas, strip each token, and join
            // the values with commas.
            "email" if self.attr(id, "multiple").is_some() => {
                if value.is_empty() {
                    return value;
                }
                let mut tokens: Vec<String> = value.split(',').map(strip).collect();
                // Infra's "split on commas" adds no token after a final comma.
                if value.ends_with(',') {
                    tokens.pop();
                }
                tokens.join(",")
            }
            "email" => strip(&value),
            // HTML #color-state-(type=color): "update a color well control
            // color".
            "color" => super::color_well::sanitize(
                &value,
                self.attr(id, "alpha").is_some(),
                super::color_well::ColorSpace::from_attribute(self.attr(id, "colorspace")),
            ),
            _ => value,
        }
    }
    pub(super) fn input_value(&self, id: NodeId) -> String {
        let ty = self.input_type(id);
        if ty == "file" {
            // HTML #dom-input-value-filename: "C:\fakepath\" and the first
            // selected file's name, or the empty string.
            return self
                .input_file_names(id)
                .first()
                .map_or_else(String::new, |name| format!("C:\\fakepath\\{name}"));
        }
        if !Dom::input_value_mode(&ty) {
            return self
                .attr(id, "value")
                .unwrap_or(if matches!(ty.as_str(), "checkbox" | "radio") {
                    "on"
                } else {
                    ""
                })
                .into();
        }
        if let Some(state) = self.state.input_values.get(&id) {
            return state.value.clone();
        }
        // A presentation snapshot transports the current value separately
        // from the content attribute, which remains the numeric step base.
        let value = if !self.state.render_live {
            self.attr(id, "data-trust-input-value")
                .or_else(|| self.attr(id, "value"))
        } else {
            self.attr(id, "value")
        };
        self.sanitize_input_value(id, value.unwrap_or(""))
    }
    /// The names of a File Upload input's selected files. A presentation
    /// snapshot transports them in `data-trust-input-files` as a JSON array.
    pub(super) fn input_file_names(&self, id: NodeId) -> Vec<String> {
        if let Some(state) = self.state.element_states.get(&id) {
            return state.selected_files.clone();
        }
        if !self.state.render_live
            && let Some(names) = self.attr(id, "data-trust-input-files")
        {
            return serde_json::from_str(names).unwrap_or_default();
        }
        Vec::new()
    }
    pub(super) fn input_editing_value(&self, id: NodeId) -> Option<&'a str> {
        if let Some(state) = self.state.input_values.get(&id) {
            state.editing.as_deref()
        } else if !self.state.render_live {
            self.attr(id, "data-trust-input-edit")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::DOCUMENT;
    #[test]
    fn numeric_input_state_survives_presentation_and_reset() {
        let mut dom = Dom::parse_document("<input type=number id=n value=0.2 step=0.5>");
        let id = dom
            .descendants(DOCUMENT)
            .find(|&n| dom.attr(n, "id") == Some("n"))
            .unwrap();
        dom.set_input_value(id, "1.2", false);
        assert_eq!(dom.attr(id, "value"), Some("0.2"));
        assert!(
            !dom.serialize_js(DOCUMENT)
                .contains("data-trust-input-value")
        );
        for html in [
            dom.serialize(DOCUMENT),
            dom.serialize_live(DOCUMENT, &Default::default()),
        ] {
            let snapshot = Dom::parse_document(&html);
            let node = snapshot
                .descendants(DOCUMENT)
                .find(|&n| snapshot.attr(n, "id") == Some("n"))
                .unwrap();
            assert_eq!(snapshot.input_value(node), "1.2");
            assert_eq!(snapshot.attr(node, "value"), Some("0.2"));
        }
        dom.reset_input_value(id);
        assert_eq!(dom.input_value(id), "0.2");
        dom.step_input(id, false, 1).unwrap();
        assert_eq!(dom.input_value(id), "0.7");
        dom.set_attr(id, "value", "0.2");
        assert_eq!(dom.input_value(id), "0.2");
        dom.set_input_value(id, "-", true);
        assert_eq!(dom.input_value(id), "");
        assert_eq!(dom.input_editing_value(id), Some("-"));
        let snapshot = Dom::parse_document(&dom.serialize_live(DOCUMENT, &Default::default()));
        assert_eq!(
            snapshot.input_editing_value(snapshot.get_by_id("n").unwrap()),
            Some("-")
        );
        dom.set_input_value(id, "", false);
        assert_eq!(dom.input_editing_value(id), None);
        dom.reset_input_value(id);
        dom.set_attr(id, "value", "invalid");
        dom.set_attr(id, "type", "text");
        assert_eq!(
            dom.input_value(id),
            "",
            "type changes preserve a sanitized clean value"
        );
        dom.set_attr(id, "value", "50");
        dom.set_attr(id, "type", "range");
        dom.set_attr(id, "step", "20");
        dom.set_attr(id, "min", "71");
        assert_eq!(dom.input_value(id), "71");
        dom.remove_attr(id, "min");
        assert_eq!(dom.input_value(id), "70");
    }

    #[test]
    fn color_inputs_sanitize_through_the_color_well_algorithm() {
        // HTML #color-state-(type=color) (local whatwg/html e5071a20): the
        // value sanitization algorithm runs "update a color well control
        // color", also when `alpha` or `colorspace` changes; while the dirty
        // value flag is false it reads the value content attribute.
        let mut dom = Dom::parse_document(
            "<input type=color id=a><input type=color id=b value=' RED '>\
             <input type=color id=c value='rgb(0 255 0 / 0.5)'><input id=d value='#ABCDEF'>",
        );
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        let c = dom.get_by_id("c").unwrap();
        let d = dom.get_by_id("d").unwrap();
        assert_eq!(dom.input_value(a), "#000000");
        assert_eq!(dom.input_value(b), "#ff0000");
        assert_eq!(dom.input_value(c), "#00ff00");
        dom.set_attr(c, "alpha", "");
        assert_eq!(dom.input_value(c), "color(srgb 0 1 0 / 0.5)");
        dom.set_attr(c, "colorspace", "display-p3");
        assert!(
            dom.input_value(c).starts_with("color(display-p3 "),
            "{}",
            dom.input_value(c)
        );
        dom.remove_attr(c, "colorspace");
        dom.remove_attr(c, "alpha");
        assert_eq!(dom.input_value(c), "#00ff00");
        // The value setter sanitizes and sets the dirty value flag.
        dom.set_input_value(a, "Blue", false);
        assert_eq!(dom.input_value(a), "#0000ff");
        dom.set_input_value(a, "not a color", false);
        assert_eq!(dom.input_value(a), "#000000");
        dom.set_input_value(a, "rgb(255 0 0 / 0.25)", false);
        assert_eq!(dom.input_value(a), "#ff0000");
        // A dirty value is updated from the value itself, not the attribute.
        dom.set_attr(a, "value", "blue");
        dom.set_attr(a, "alpha", "");
        assert_eq!(dom.input_value(a), "color(srgb 1 0 0)");
        // Type changes run the new state's value sanitization algorithm.
        dom.set_attr(d, "type", "color");
        assert_eq!(dom.input_value(d), "#abcdef");
        dom.set_input_value(d, "lime", false);
        dom.set_attr(d, "type", "text");
        assert_eq!(dom.input_value(d), "#00ff00");
    }
}
