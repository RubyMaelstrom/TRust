//! HTML constraint validation as selectors observe it (local whatwg/html
//! e5071a2): #candidate-for-constraint-validation and the validity states of
//! #constraints, which `:valid`, `:invalid`, `:user-valid`, `:user-invalid`,
//! `:in-range` and `:out-of-range` match (#pseudo-classes); `:default`,
//! `:open`, and `:heading()` with the computed heading level
//! (#heading-levels-&-offsets). The same validity states back the
//! constraint validation API (`ValidityState`), so script and selectors
//! agree.
//!
//! State that is not an attribute lives in `input::ElementState` (custom
//! validity, user validity, the "last changed by a user edit" flag, a file
//! input's selection, an open picker); a presentation snapshot carries it in
//! `data-trust-*` attributes.

use super::input::{
    ElementState, MUTED_DEPENDENCY, OPEN_DEPENDENCY, VALIDITY_DEPENDENCY,
    parse_non_negative_integer,
};
use super::invalidation::LiveState;
use super::{Dom, NodeId, StyleView};
use crate::input::NumericType;

const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

/// The input types whose value is text the user types: `pattern`,
/// `maxlength` and `minlength` apply to them.
const TEXT_TYPES: [&str; 6] = ["text", "search", "url", "tel", "email", "password"];

/// The input types to which `readonly` and (with checkbox, radio and file)
/// `required` apply (HTML #input-type-attr-summary).
fn readonly_applies(input_type: &str) -> bool {
    TEXT_TYPES.contains(&input_type)
        || matches!(
            input_type,
            "date" | "month" | "week" | "time" | "datetime-local" | "number"
        )
}

/// The validity states of HTML #validitystate, as bits in that order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ValidityFlags(pub(crate) u16);

impl ValidityFlags {
    pub(crate) const VALUE_MISSING: u16 = 1 << 0;
    pub(crate) const TYPE_MISMATCH: u16 = 1 << 1;
    pub(crate) const PATTERN_MISMATCH: u16 = 1 << 2;
    pub(crate) const TOO_LONG: u16 = 1 << 3;
    pub(crate) const TOO_SHORT: u16 = 1 << 4;
    pub(crate) const RANGE_UNDERFLOW: u16 = 1 << 5;
    pub(crate) const RANGE_OVERFLOW: u16 = 1 << 6;
    pub(crate) const STEP_MISMATCH: u16 = 1 << 7;
    pub(crate) const BAD_INPUT: u16 = 1 << 8;
    pub(crate) const CUSTOM_ERROR: u16 = 1 << 9;
    pub(crate) const ALL: u16 = (1 << 10) - 1;

    fn set(&mut self, flag: u16, on: bool) {
        if on {
            self.0 |= flag;
        }
    }

    /// HTML #concept-fv-valid: the element satisfies its constraints.
    pub(crate) fn valid(self) -> bool {
        self.0 == 0
    }
}

/// HTML #valid-e-mail-address.
fn valid_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    let local_char = |c: char| c.is_ascii_alphanumeric() || ".!#$%&'*+/=?^_`{|}~-".contains(c);
    !local.is_empty()
        && local.chars().all(local_char)
        && domain.split('.').all(|label| {
            let bytes = label.as_bytes();
            (1..=63).contains(&bytes.len())
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
                && bytes[0] != b'-'
                && bytes[bytes.len() - 1] != b'-'
        })
}

/// HTML #rules-for-parsing-non-negative-integers, for the form control
/// `maxlength`/`minlength` attributes and `headingoffset`.
fn non_negative_attribute(value: Option<&str>) -> Option<usize> {
    value.and_then(parse_non_negative_integer)
}

impl StyleView<'_> {
    fn is_html_element(&self, id: NodeId) -> bool {
        self.nodes.is_element(id) && self.nodes.namespace_uri(id) == Some(HTML_NAMESPACE)
    }

    fn validity_state(
        &self,
        id: NodeId,
        live: impl Fn(&ElementState) -> bool,
        presented: &str,
    ) -> bool {
        self.state.element_states.get(&id).is_some_and(live)
            || self.presentation_marker(id, presented)
    }

    /// HTML #suffering-from-a-custom-error for an element other than a
    /// form-associated custom element.
    fn custom_validity_error(&self, id: NodeId) -> bool {
        self.validity_state(id, |s| s.custom_error, "data-trust-custom-error")
    }

    /// HTML #user-validity.
    pub(super) fn user_validity(&self, id: NodeId) -> bool {
        self.validity_state(id, |s| s.user_validity, "data-trust-user-validity")
    }

    fn user_edited(&self, id: NodeId) -> bool {
        self.validity_state(id, |s| s.user_edited, "data-trust-user-edited")
    }

    /// Whether a File Upload input's list of selected files is non-empty
    /// (the names live in the control state; see `input_file_names`).
    fn files_selected(&self, id: NodeId) -> bool {
        match self.state.element_states.get(&id) {
            Some(state) => !state.selected_files.is_empty(),
            None => self.presentation_marker(id, "data-trust-input-files"),
        }
    }

    pub(super) fn picker_open(&self, id: NodeId) -> bool {
        self.validity_state(id, |s| s.picker_open, "data-trust-open")
    }

    fn face_validity(&self, id: NodeId) -> u16 {
        if let Some(state) = self.state.element_states.get(&id)
            && state.face_validity != 0
        {
            return state.face_validity;
        }
        if self.state.render_live {
            return 0;
        }
        self.attr(id, "data-trust-face-validity")
            .and_then(|bits| bits.parse().ok())
            .unwrap_or(0)
    }

    /// DOM #connected: the shadow-including root is a Document.
    fn is_connected(&self, id: NodeId) -> bool {
        let mut current = id;
        loop {
            if self.nodes.is_document(current) {
                return true;
            }
            match self.parent_composed(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }

    /// HTML #form-owner (#reset-the-form-owner): the form named by the
    /// `form` attribute of a connected element, else the nearest ancestor
    /// form.
    pub(super) fn form_owner(&self, id: NodeId) -> Option<NodeId> {
        let is_form = |node| self.tag_name(node) == Some("form") && self.is_html_element(node);
        if self.tag_name(id) != Some("img")
            && let Some(name) = self.attr(id, "form")
            && self.is_connected(id)
        {
            if name.is_empty() {
                return None;
            }
            let scope = self.tree_scope(id);
            return self
                .descendants(self.tree_root(id))
                .find(|&node| self.attr(node, "id") == Some(name) && self.tree_scope(node) == scope)
                .filter(|&node| is_form(node));
        }
        let mut parent = self.nodes.parent_of(id);
        while let Some(node) = parent {
            if self.nodes.is_document(node) {
                break;
            }
            if is_form(node) {
                return Some(node);
            }
            parent = self.nodes.parent_of(node);
        }
        None
    }

    /// HTML #concept-submit-button.
    pub(super) fn is_submit_button(&self, id: NodeId) -> bool {
        if !self.is_html_element(id) {
            return false;
        }
        match self.tag_name(id) {
            Some("input") => matches!(self.input_type(id).as_str(), "submit" | "image"),
            Some("button") => {
                match self
                    .attr(id, "type")
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("submit") => true,
                    Some("reset" | "button") => false,
                    // The Auto state (missing and invalid value default).
                    _ => {
                        self.attr(id, "command").is_none()
                            && self.attr(id, "commandfor").is_none()
                            && !self.nodes.parent_of(id).is_some_and(|parent| {
                                self.tag_name(parent) == Some("select")
                                    && self.is_html_element(parent)
                            })
                    }
                }
            }
            _ => false,
        }
    }

    fn has_datalist_ancestor(&self, id: NodeId) -> bool {
        let mut parent = self.nodes.parent_of(id);
        while let Some(node) = parent {
            if self.nodes.is_document(node) {
                return false;
            }
            if self.tag_name(node) == Some("datalist") && self.is_html_element(node) {
                return true;
            }
            parent = self.nodes.parent_of(node);
        }
        false
    }

    /// HTML #category-submit.
    fn is_submittable(&self, id: NodeId) -> bool {
        (self.is_html_element(id)
            && matches!(
                self.tag_name(id),
                Some("button" | "input" | "select" | "textarea")
            ))
            || self.is_form_associated_custom_element(id)
    }

    /// HTML #candidate-for-constraint-validation: a submittable element no
    /// condition bars from constraint validation.
    pub(super) fn will_validate(&self, id: NodeId) -> bool {
        if !self.nodes.is_element(id) || !self.is_submittable(id) {
            return false;
        }
        let Some(tag) = self.tag_name(id) else {
            return false;
        };
        // #attr-fe-disabled and the datalist element's barring condition.
        if self.actually_disabled(id, tag) || self.has_datalist_ancestor(id) {
            return false;
        }
        if self.is_form_associated_custom_element(id) {
            // #attr-face-readonly.
            return self.attr(id, "readonly").is_none();
        }
        match tag {
            "input" => {
                let input_type = self.input_type(id);
                !matches!(input_type.as_str(), "hidden" | "reset" | "button")
                    && !(readonly_applies(&input_type) && self.attr(id, "readonly").is_some())
            }
            "button" => self.is_submit_button(id),
            "textarea" => self.attr(id, "readonly").is_none(),
            _ => true,
        }
    }

    /// HTML #radio-button-group: the radio buttons of `id`'s tree with its
    /// form owner and (non-empty) name.
    fn radio_group(&self, id: NodeId) -> Vec<NodeId> {
        let Some(name) = self.attr(id, "name").filter(|name| !name.is_empty()) else {
            return vec![id];
        };
        let owner = self.form_owner(id);
        let root = self.tree_root(id);
        let scope = self.tree_scope(id);
        let mut group: Vec<NodeId> = self
            .descendants(root)
            .filter(|&node| {
                node == id
                    || (self.tag_name(node) == Some("input")
                        && self.is_html_element(node)
                        && self.attr(node, "name") == Some(name)
                        && self.input_type(node) == "radio"
                        && self.tree_scope(node) == scope
                        && self.form_owner(node) == owner)
            })
            .collect();
        if !group.contains(&id) {
            group.push(id);
        }
        group
    }

    /// HTML #placeholder-label-option of a required select.
    fn placeholder_label_option(&self, select: NodeId, options: &[NodeId]) -> Option<NodeId> {
        if self.attr(select, "multiple").is_some() || self.select_display_size(select) != 1 {
            return None;
        }
        let first = *options.first()?;
        (self.nodes.parent_of(first) == Some(select) && self.option_value_is_empty(first))
            .then_some(first)
    }

    /// Whether an option's value (#concept-option-value) is the empty
    /// string: its value attribute, else its text (#dom-option-text) with
    /// ASCII whitespace stripped and collapsed.
    fn option_value_is_empty(&self, option: NodeId) -> bool {
        if let Some(value) = self.attr(option, "value") {
            return value.is_empty();
        }
        let mut stack: Vec<NodeId> = self.child_iter(option).collect();
        stack.reverse();
        while let Some(node) = stack.pop() {
            if let Some(text) = self.nodes.text(node) {
                if !text
                    .chars()
                    .all(|c| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' '))
                {
                    return false;
                }
            } else if self.tag_name(node) != Some("script") {
                let start = stack.len();
                stack.extend(self.child_iter(node));
                stack[start..].reverse();
            }
        }
        true
    }

    /// The range states of a numeric input: (underflow, overflow, step
    /// mismatch), HTML #attr-input-min, #attr-input-max, #attr-input-step.
    fn numeric_validity(&self, id: NodeId) -> (bool, bool, bool) {
        let Some(config) = self.numeric_input(id) else {
            return (false, false, false);
        };
        let Some(number) = config.kind.parse(&self.input_value(id)) else {
            return (false, false, false);
        };
        let mut underflow = config.min.is_some_and(|min| number < min);
        let mut overflow = config.max.is_some_and(|max| number > max);
        // #has-a-reversed-range: a time value outside the reversed range
        // suffers from both at once.
        if config.kind == NumericType::Time
            && config.min.zip(config.max).is_some_and(|(a, b)| a > b)
        {
            underflow = underflow && overflow;
            overflow = underflow;
        }
        (underflow, overflow, config.mismatch(number))
    }

    /// The dirty value flag (#concept-fe-dirty). A presentation snapshot
    /// carries the user edit flag only while the value is dirty.
    fn value_dirty(&self, id: NodeId, tag: &str) -> bool {
        if !self.state.render_live {
            return true;
        }
        match tag {
            "input" => self.state.input_values.get(&id).is_some_and(|v| v.dirty),
            "textarea" => self
                .state
                .element_states
                .get(&id)
                .is_some_and(|s| s.raw_value.is_some()),
            _ => false,
        }
    }

    /// #attr-fe-maxlength / #attr-fe-minlength: a dirty value last changed
    /// by a user edit, measured in UTF-16 code units of the API value.
    fn length_validity(&self, id: NodeId, tag: &str, value: &str, flags: &mut ValidityFlags) {
        if !self.user_edited(id) || !self.value_dirty(id, tag) {
            return;
        }
        let length = value.encode_utf16().count();
        if let Some(max) = non_negative_attribute(self.attr(id, "maxlength")) {
            flags.set(ValidityFlags::TOO_LONG, length > max);
        }
        if let Some(min) = non_negative_attribute(self.attr(id, "minlength")) {
            flags.set(ValidityFlags::TOO_SHORT, length > 0 && length < min);
        }
    }

    fn input_validity(&self, id: NodeId, flags: &mut ValidityFlags) {
        let input_type = self.input_type(id);
        let required = self.attr(id, "required").is_some();
        match input_type.as_str() {
            "checkbox" => flags.set(
                ValidityFlags::VALUE_MISSING,
                required && !self.checkedness(id),
            ),
            "radio" => {
                let group = self.radio_group(id);
                flags.set(
                    ValidityFlags::VALUE_MISSING,
                    group
                        .iter()
                        .any(|&radio| self.attr(radio, "required").is_some())
                        && !group.iter().any(|&radio| self.checkedness(radio)),
                );
            }
            "file" => flags.set(
                ValidityFlags::VALUE_MISSING,
                required && !self.files_selected(id),
            ),
            _ => {}
        }
        if !Dom::input_value_mode(&input_type) {
            return;
        }
        let value = self.input_value(id);
        if readonly_applies(&input_type) {
            let mutable =
                !self.actually_disabled(id, "input") && self.attr(id, "readonly").is_none();
            flags.set(
                ValidityFlags::VALUE_MISSING,
                required && mutable && value.is_empty(),
            );
        }
        // #concept-fe-values: an email input with `multiple` has the values
        // its value sanitization split on commas.
        let multiple = input_type == "email" && self.attr(id, "multiple").is_some();
        let values: Vec<&str> = if !multiple {
            vec![value.as_str()]
        } else if value.is_empty() {
            Vec::new()
        } else {
            value.split(',').collect()
        };
        match input_type.as_str() {
            // #email-state-(type=email): a valid email address (list).
            "email" if multiple => flags.set(
                ValidityFlags::TYPE_MISMATCH,
                values.iter().any(|value| !valid_email(value)),
            ),
            "email" => flags.set(
                ValidityFlags::TYPE_MISMATCH,
                !value.is_empty() && !valid_email(&value),
            ),
            // #url-state-(type=url): a valid absolute URL.
            "url" => flags.set(
                ValidityFlags::TYPE_MISMATCH,
                !value.is_empty() && url::Url::parse(&value).is_err(),
            ),
            _ => {}
        }
        if TEXT_TYPES.contains(&input_type.as_str()) {
            // #attr-input-pattern.
            if !value.is_empty()
                && let Some(pattern) = self.attr(id, "pattern")
            {
                flags.set(
                    ValidityFlags::PATTERN_MISMATCH,
                    values.iter().any(|value| {
                        crate::pattern_regexp::pattern_matches(pattern, value) == Some(false)
                    }),
                );
            }
            self.length_validity(id, "input", &value, flags);
        }
        let (underflow, overflow, step) = self.numeric_validity(id);
        flags.set(ValidityFlags::RANGE_UNDERFLOW, underflow);
        flags.set(ValidityFlags::RANGE_OVERFLOW, overflow);
        flags.set(ValidityFlags::STEP_MISMATCH, step);
        flags.set(
            ValidityFlags::BAD_INPUT,
            self.input_editing_value(id).is_some(),
        );
    }

    /// The validity states of `id` (HTML #the-constraint-validation-api).
    pub(super) fn validity(&self, id: NodeId) -> ValidityFlags {
        if self.is_form_associated_custom_element(id) {
            return ValidityFlags(self.face_validity(id) & ValidityFlags::ALL);
        }
        let mut flags = ValidityFlags::default();
        flags.set(ValidityFlags::CUSTOM_ERROR, self.custom_validity_error(id));
        if !self.is_html_element(id) {
            return flags;
        }
        match self.tag_name(id) {
            Some("input") => self.input_validity(id, &mut flags),
            Some("select") if self.attr(id, "required").is_some() => {
                let options = self.select_options(id);
                let selected: Vec<NodeId> = options
                    .iter()
                    .copied()
                    .filter(|&option| self.option_selectedness(option))
                    .collect();
                let missing = match selected.as_slice() {
                    [] => true,
                    [only] => Some(*only) == self.placeholder_label_option(id, &options),
                    _ => false,
                };
                flags.set(ValidityFlags::VALUE_MISSING, missing);
            }
            Some("textarea") => {
                let value = self.textarea_api_value(id);
                let mutable =
                    !self.actually_disabled(id, "textarea") && self.attr(id, "readonly").is_none();
                flags.set(
                    ValidityFlags::VALUE_MISSING,
                    self.attr(id, "required").is_some() && mutable && value.is_empty(),
                );
                self.length_validity(id, "textarea", &value, &mut flags);
            }
            _ => {}
        }
        flags
    }

    /// A candidate for constraint validation that does not satisfy its
    /// constraints.
    fn invalid_candidate(&self, id: NodeId) -> bool {
        self.will_validate(id) && !self.validity(id).valid()
    }

    /// HTML #selector-valid / #selector-invalid: a candidate by its own
    /// constraints, a form by the candidates it owns, a fieldset by its
    /// descendant candidates. `valid` selects which of the two matches.
    pub(super) fn matches_validity(&self, id: NodeId, tag: &str, valid: bool) -> bool {
        if self.is_html_element(id) && tag == "form" {
            let scope = self.tree_scope(id);
            let invalid = self.descendants(self.tree_root(id)).any(|node| {
                self.tree_scope(node) == scope
                    && self.is_submittable(node)
                    && self.form_owner(node) == Some(id)
                    && self.invalid_candidate(node)
            });
            return invalid != valid;
        }
        if self.is_html_element(id) && tag == "fieldset" {
            let invalid = self
                .descendants(id)
                .any(|node| self.is_submittable(node) && self.invalid_candidate(node));
            return invalid != valid;
        }
        self.will_validate(id) && self.validity(id).valid() == valid
    }

    /// HTML #selector-user-valid / #selector-user-invalid.
    pub(super) fn matches_user_validity(&self, id: NodeId, tag: &str, valid: bool) -> bool {
        self.is_html_element(id)
            && matches!(tag, "input" | "textarea" | "select")
            && self.user_validity(id)
            && self.will_validate(id)
            && self.validity(id).valid() == valid
    }

    /// HTML #selector-in-range / #selector-out-of-range: `Some(out of
    /// range)` for a candidate that has range limitations
    /// (#have-range-limitations), else `None`.
    pub(super) fn range_state(&self, id: NodeId, tag: &str) -> Option<bool> {
        if tag != "input" || !self.is_html_element(id) {
            return None;
        }
        let config = self.numeric_input(id)?;
        if config.min.is_none() && config.max.is_none() || !self.will_validate(id) {
            return None;
        }
        let (underflow, overflow, _) = self.numeric_validity(id);
        Some(underflow || overflow)
    }

    /// HTML #selector-default: default buttons, checkboxes and radio
    /// buttons with a `checked` attribute, options with `selected`.
    pub(super) fn matches_default(&self, id: NodeId, tag: &str) -> bool {
        if !self.is_html_element(id) {
            return false;
        }
        match tag {
            "input" if matches!(self.input_type(id).as_str(), "checkbox" | "radio") => {
                self.attr(id, "checked").is_some()
            }
            "input" | "button" => self.is_default_button(id),
            "option" => self.attr(id, "selected").is_some(),
            _ => false,
        }
    }

    /// HTML #default-button: the first submit button in tree order whose
    /// form owner is its form.
    fn is_default_button(&self, id: NodeId) -> bool {
        if !self.is_submit_button(id) {
            return false;
        }
        let Some(form) = self.form_owner(id) else {
            return false;
        };
        let scope = self.tree_scope(form);
        self.descendants(self.tree_root(form)).find(|&node| {
            self.tree_scope(node) == scope
                && self.is_submit_button(node)
                && self.form_owner(node) == Some(form)
        }) == Some(id)
    }

    /// HTML #selector-open.
    pub(super) fn matches_open(&self, id: NodeId, tag: &str) -> bool {
        if !self.is_html_element(id) {
            return false;
        }
        match tag {
            "details" | "dialog" => self.attr(id, "open").is_some(),
            // A drop-down box (#concept-select-dropdown): no `multiple`
            // attribute and a display size of 1.
            "select" => {
                self.attr(id, "multiple").is_none()
                    && self.select_display_size(id) == 1
                    && self.picker_open(id)
            }
            // #input-support-picker.
            "input" => {
                matches!(
                    self.input_type(id).as_str(),
                    "file" | "date" | "month" | "week" | "time" | "datetime-local" | "color"
                ) && self.picker_open(id)
            }
            _ => false,
        }
    }

    /// HTML #selector-active.
    pub(super) fn matches_active(&self, id: NodeId) -> bool {
        self.state
            .element_states
            .get(&id)
            .is_some_and(|state| state.active)
    }

    /// HTML #selector-playing, #selector-paused, #selector-seeking,
    /// #selector-buffering, #selector-stalled, #selector-muted and
    /// #selector-volume-locked for a media element. TRust plays no media
    /// inline (a media element's `paused` stays true and it never seeks or
    /// buffers) and has no volume lock.
    pub(super) fn matches_media_state(&self, id: NodeId, tag: &str, state: MediaState) -> bool {
        if !matches!(tag, "audio" | "video") || !self.is_html_element(id) {
            return false;
        }
        match state {
            MediaState::Paused => true,
            MediaState::Muted => self
                .state
                .element_states
                .get(&id)
                .and_then(|state| state.muted)
                .unwrap_or_else(|| self.attr(id, "muted").is_some()),
            MediaState::Playing
            | MediaState::Seeking
            | MediaState::Buffering
            | MediaState::Stalled
            | MediaState::VolumeLocked => false,
        }
    }

    /// HTML #selector-heading / #selector-heading-functional.
    pub(super) fn matches_heading(&self, id: NodeId, tag: &str, levels: Option<&[i64]>) -> bool {
        let Some(level) = tag
            .strip_prefix('h')
            .and_then(|level| level.parse::<u32>().ok())
            .filter(|level| (1..=6).contains(level) && tag.len() == 2)
        else {
            return false;
        };
        if !self.is_html_element(id) {
            return false;
        }
        let Some(levels) = levels else {
            return true;
        };
        let level = i64::from(level) + self.heading_offset(id) as i64;
        levels.contains(&level.min(9))
    }

    /// HTML "get an element's computed heading offset": `headingoffset`
    /// values summed over the flat inclusive ancestors (a shadow root's host
    /// continues the walk) up to the first with `headingreset`.
    fn heading_offset(&self, id: NodeId) -> u32 {
        let mut offset: u32 = 0;
        let mut current = Some(id);
        while let Some(node) = current {
            if self.is_html_element(node) {
                if let Some(next) = non_negative_attribute(self.attr(node, "headingoffset")) {
                    offset = offset.saturating_add(u32::try_from(next).unwrap_or(u32::MAX));
                }
                if self.attr(node, "headingreset").is_some() {
                    break;
                }
            }
            current = match self.nodes.parent_of(node) {
                Some(parent) if self.nodes.is_element(parent) => Some(parent),
                Some(parent) => self.state.shadow_hosts.get(&parent).copied(),
                None => self.state.shadow_hosts.get(&node).copied(),
            };
        }
        offset
    }
}

/// The media element pseudo-classes of HTML #pseudo-classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MediaState {
    Playing,
    Paused,
    Seeking,
    Buffering,
    Stalled,
    Muted,
    VolumeLocked,
}

impl MediaState {
    pub(super) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "playing" => Self::Playing,
            "paused" => Self::Paused,
            "seeking" => Self::Seeking,
            "buffering" => Self::Buffering,
            "stalled" => Self::Stalled,
            "muted" => Self::Muted,
            "volume-locked" => Self::VolumeLocked,
            _ => return None,
        })
    }
}

impl Dom {
    /// HTML #selector-active: `target` is being activated (actively pointed
    /// at, or in a formal activation state), or nothing is. Its flat tree
    /// inclusive ancestors match `:active`, as does the labeled control of
    /// a matching label (which is not itself being activated).
    pub(crate) fn set_active_element(&mut self, target: Option<NodeId>) {
        let mut chain: Vec<NodeId> = Vec::new();
        let mut current =
            target.filter(|&node| self.is_valid(node) && self.tag_name(node).is_some());
        while let Some(node) = current {
            if self.tag_name(node).is_some() {
                chain.push(node);
            }
            current = self.parent_flat(node);
        }
        let labeled: Vec<NodeId> = chain
            .iter()
            .filter(|&&node| self.tag_name(node) == Some("label"))
            .filter_map(|&label| self.labeled_control(label))
            .collect();
        chain.extend(labeled);
        let previous: Vec<NodeId> = self
            .element_states
            .iter()
            .filter(|(_, state)| state.active)
            .map(|(&node, _)| node)
            .collect();
        let mut changed = Vec::new();
        for &node in previous.iter().filter(|node| !chain.contains(node)) {
            self.update_control_state(node, |state| state.active = false);
            changed.push((node, LiveState::Active));
        }
        for node in chain {
            if !previous.contains(&node) {
                self.update_control_state(node, |state| state.active = true);
                changed.push((node, LiveState::Active));
            }
        }
        if changed.is_empty() {
            return;
        }
        // Like the focus update steps: the state is no attribute and no
        // presentational hint, so only the selector subjects its routes
        // reach (and their flat-tree descendants) restyle, never every
        // element under the activated chain's root.
        changed.sort_unstable_by_key(|&(node, _)| node);
        let index = self.style_index();
        if !self.invalidate_live_state(&index, &changed) {
            self.touch();
        }
    }

    /// Whether a key press can hold `id` in a formal activation state: it
    /// has activation behavior a keyboard triggers (buttons, checkable and
    /// button inputs, hyperlinks, summaries).
    pub(crate) fn has_key_activation(&self, id: NodeId) -> bool {
        match self.tag_name(id) {
            Some("button" | "summary") => true,
            Some("a" | "area") => self.attr(id, "href").is_some(),
            Some("input") => matches!(
                self.input_type(id).as_str(),
                "button" | "submit" | "reset" | "image" | "checkbox" | "radio"
            ),
            _ => false,
        }
    }

    /// HTML #dom-media-muted: script set a media element's muted state.
    pub(crate) fn set_media_muted(&mut self, id: NodeId, muted: bool) {
        if !self.is_valid(id) || self.element_states.get(&id).and_then(|s| s.muted) == Some(muted) {
            return;
        }
        self.update_control_state(id, |state| state.muted = Some(muted));
        self.touch_attr(id, MUTED_DEPENDENCY);
    }

    /// HTML #dom-cva-willvalidate.
    pub(crate) fn will_validate(&self, id: NodeId) -> bool {
        self.is_valid(id) && self.style_view().will_validate(id)
    }

    /// The validity states of `id`, as `ValidityFlags` bits.
    pub(crate) fn validity_flags(&self, id: NodeId) -> u16 {
        if !self.is_valid(id) {
            return 0;
        }
        self.style_view().validity(id).0
    }

    fn set_state_flag(
        &mut self,
        id: NodeId,
        dependency: &str,
        current: impl Fn(&ElementState) -> bool,
        update: impl FnOnce(&mut ElementState),
        value: bool,
    ) {
        if !self.is_valid(id) || self.element_states.get(&id).is_some_and(&current) == value {
            return;
        }
        self.update_control_state(id, update);
        self.touch_attr(id, dependency);
    }

    /// HTML #custom-validity-error-message became empty or non-empty.
    pub(crate) fn set_custom_validity_error(&mut self, id: NodeId, error: bool) {
        self.set_state_flag(
            id,
            VALIDITY_DEPENDENCY,
            |s| s.custom_error,
            |s| s.custom_error = error,
            error,
        );
    }

    /// The flags `ElementInternals.setValidity()` set on a form-associated
    /// custom element (`ValidityFlags` bits).
    pub(crate) fn set_face_validity(&mut self, id: NodeId, bits: u16) {
        let bits = bits & ValidityFlags::ALL;
        if !self.is_valid(id) || self.element_states.get(&id).map_or(0, |s| s.face_validity) == bits
        {
            return;
        }
        self.update_control_state(id, |s| s.face_validity = bits);
        self.touch_attr(id, VALIDITY_DEPENDENCY);
    }

    /// HTML #concept-fe-value: whether the value was last changed by a user
    /// edit.
    pub(crate) fn set_user_edited(&mut self, id: NodeId, edited: bool) {
        self.set_state_flag(
            id,
            VALIDITY_DEPENDENCY,
            |s| s.user_edited,
            |s| s.user_edited = edited,
            edited,
        );
    }

    /// HTML #user-validity.
    pub(crate) fn set_user_validity(&mut self, id: NodeId, value: bool) {
        self.set_state_flag(
            id,
            VALIDITY_DEPENDENCY,
            |s| s.user_validity,
            |s| s.user_validity = value,
            value,
        );
    }

    /// HTML #concept-form-submit: "For each element field in the list of
    /// submittable elements whose form owner is form, set field's user
    /// validity to true."
    pub(crate) fn set_form_user_validity(&mut self, form: NodeId) {
        if !self.is_valid(form) {
            return;
        }
        let view = self.style_view();
        let root = view.tree_root(form);
        let fields: Vec<NodeId> = view
            .descendants(root)
            .filter(|&node| {
                view.is_submittable(node)
                    && matches!(view.tag_name(node), Some("input" | "select" | "textarea"))
                    && view.form_owner(node) == Some(form)
            })
            .collect();
        for field in fields {
            self.set_user_validity(field, true);
        }
    }

    /// A select's drop-down box or an input's picker opened or closed.
    pub(crate) fn set_picker_open(&mut self, id: NodeId, open: bool) {
        self.set_state_flag(
            id,
            OPEN_DEPENDENCY,
            |s| s.picker_open,
            |s| s.picker_open = open,
            open,
        );
    }

    /// The elements whose validity pseudo-classes can change when `node`'s
    /// own validity inputs change (its attributes, value, checkedness,
    /// selectedness or validity state): the affected candidates (a radio
    /// button's whole group, an option's select, a fieldset's descendants),
    /// and for each its fieldset ancestors and form owner.
    pub(super) fn validity_subjects(&self, node: NodeId) -> Vec<NodeId> {
        let view = self.style_view();
        if !self.is_valid(node) || !view.nodes.is_element(node) {
            return Vec::new();
        }
        let mut controls = Vec::new();
        match view.tag_name(node) {
            Some("option" | "optgroup") => {
                let mut parent = view.nodes.parent_of(node);
                while let Some(ancestor) = parent {
                    if view.tag_name(ancestor) == Some("select") {
                        controls.push(ancestor);
                        break;
                    }
                    parent = view.nodes.parent_of(ancestor);
                }
                controls.push(node);
            }
            Some("fieldset") => {
                controls.push(node);
                controls.extend(view.descendants(node).filter(|&n| view.is_submittable(n)));
            }
            Some("input") if view.input_type(node) == "radio" => {
                controls.extend(view.radio_group(node));
            }
            _ => controls.push(node),
        }
        let mut subjects = Vec::new();
        for control in controls {
            subjects.push(control);
            let mut parent = view.nodes.parent_of(control);
            while let Some(ancestor) = parent {
                if view.nodes.is_document(ancestor) {
                    break;
                }
                if view.tag_name(ancestor) == Some("fieldset") {
                    subjects.push(ancestor);
                }
                parent = view.nodes.parent_of(ancestor);
            }
            subjects.extend(view.form_owner(control));
        }
        subjects.sort_unstable();
        subjects.dedup();
        subjects
    }

    /// Whether a child-list change below `parent` can change the validity
    /// pseudo-classes or `:default` outside the inserted or removed
    /// subtrees: option lists and textarea default values, and the
    /// candidates a form or fieldset owns.
    pub(super) fn structure_may_change_validity(&self, parent: NodeId, changed: &[NodeId]) -> bool {
        if changed.is_empty() {
            return true;
        }
        let view = self.style_view();
        let mut ancestor = Some(parent);
        while let Some(node) = ancestor {
            if view.nodes.is_document(node) {
                break;
            }
            if matches!(
                view.tag_name(node),
                Some("select" | "textarea" | "datalist")
            ) {
                return true;
            }
            ancestor = view.nodes.parent_of(node);
        }
        changed.iter().any(|&root| {
            let mut stack = vec![root];
            while let Some(node) = stack.pop() {
                if !self.is_valid(node) {
                    continue;
                }
                if matches!(
                    view.tag_name(node),
                    Some("button" | "input" | "select" | "textarea" | "fieldset" | "form")
                ) || view.is_form_associated_custom_element(node)
                {
                    return true;
                }
                stack.extend(view.child_iter(node));
            }
            false
        })
    }

    /// Whether character data below `parent` can change a control's value:
    /// a textarea's default value or an option's text.
    pub(super) fn text_may_change_validity(&self, parent: NodeId) -> bool {
        let view = self.style_view();
        let mut ancestor = Some(parent);
        while let Some(node) = ancestor {
            if view.nodes.is_document(node) {
                return false;
            }
            if matches!(view.tag_name(node), Some("textarea" | "option")) {
                return true;
            }
            ancestor = view.nodes.parent_of(node);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::dom::{Dom, SelectorList};

    fn matches(dom: &Dom, id: &str, selector: &str) -> bool {
        let node = dom.get_by_id(id).unwrap_or_else(|| panic!("#{id}"));
        dom.matches(node, &SelectorList::parse(selector).expect(selector))
    }

    /// (`:valid`, `:invalid`) for each id.
    fn validity(dom: &Dom, ids: &[&str]) -> Vec<(bool, bool)> {
        ids.iter()
            .map(|id| (matches(dom, id, ":valid"), matches(dom, id, ":invalid")))
            .collect()
    }

    const CANDIDATE: (bool, bool) = (true, false);
    const INVALID: (bool, bool) = (false, true);
    const BARRED: (bool, bool) = (false, false);

    #[test]
    fn valid_and_invalid_follow_candidates_forms_and_fieldsets() {
        let mut dom = Dom::parse_document(
            "<form id=f><fieldset id=fs><input id=req required><input id=ok value=x></fieldset>\
             <input id=num type=number min=1 max=5 value=7>\
             <input id=hidden type=hidden required><input id=dis required disabled>\
             <input id=ro required readonly><input id=range type=range required>\
             <button id=submit>go</button><button id=reset type=reset>r</button>\
             <datalist><input id=listed required></datalist>\
             <fieldset id=off disabled><input id=offinput required></fieldset></form>\
             <form id=g><input id=email type=email value='a@b.c'></form>\
             <input id=owned form=g pattern='[0-9]+' value='12a'>\
             <select id=sel required><option value=''>Pick</option><option>A</option></select>\
             <select id=multi required multiple><option>A</option></select>\
             <textarea id=ta required></textarea><output id=out></output>\
             <div id=plain></div>",
        );
        assert_eq!(
            validity(
                &dom,
                &[
                    "req", "ok", "fs", "f", "num", "hidden", "dis", "ro", "range", "submit",
                    "reset", "listed", "offinput", "off", "g", "email", "owned", "sel", "multi",
                    "ta", "out", "plain"
                ]
            ),
            [
                INVALID, CANDIDATE, INVALID, INVALID, INVALID, BARRED, BARRED, BARRED, CANDIDATE,
                CANDIDATE, BARRED, BARRED, BARRED, CANDIDATE, INVALID, CANDIDATE, INVALID, INVALID,
                INVALID, INVALID, BARRED, BARRED
            ]
        );
        let req = dom.get_by_id("req").unwrap();
        dom.set_input_value(req, "x", true);
        assert_eq!(
            validity(&dom, &["req", "fs", "f"]),
            [CANDIDATE, CANDIDATE, INVALID]
        );
        let num = dom.get_by_id("num").unwrap();
        dom.set_input_value(num, "4", true);
        assert_eq!(validity(&dom, &["num", "f"]), [CANDIDATE, CANDIDATE]);
        // The control the form attribute associates makes its owner invalid.
        let owned = dom.get_by_id("owned").unwrap();
        dom.set_input_value(owned, "12", false);
        assert_eq!(validity(&dom, &["owned", "g"]), [CANDIDATE, CANDIDATE]);
        // A selected option other than the placeholder label option.
        let options = dom.select_options(dom.get_by_id("sel").unwrap());
        dom.set_option_selectedness(options[1], Some(true), Some(true));
        dom.set_option_selectedness(options[0], Some(false), Some(true));
        assert_eq!(validity(&dom, &["sel"]), [CANDIDATE]);
    }

    #[test]
    fn validity_states_follow_the_constraint_definitions() {
        let mut dom = Dom::parse_document(
            "<input id=email type=email value='not an address'>\
             <input id=emails type=email multiple value='a@b.c,x@y'>\
             <input id=bademails type=email multiple value='a@b.c, '>\
             <input id=spaced type=email value=' a@b.c '>\
             <input id=spacedurl type=url value=' https://example.test/ '>\
             <input id=url type=url value='relative/path'>\
             <input id=goodurl type=url value='https://example.test/'>\
             <input id=pattern pattern='[a-z]+' value='ABC'>\
             <input id=badpattern pattern='a{' value='zzz'>\
             <input id=step type=number min=0 step=2 value=3>\
             <input id=box type=checkbox required>\
             <input id=r1 type=radio name=g><input id=r2 type=radio name=g required>\
             <input id=file type=file required>",
        );
        for (id, flag) in [
            ("email", ValidityFlags::TYPE_MISMATCH),
            ("emails", 0),
            ("bademails", ValidityFlags::TYPE_MISMATCH),
            // The value sanitization algorithms strip ASCII whitespace.
            ("spaced", 0),
            ("spacedurl", 0),
            ("url", ValidityFlags::TYPE_MISMATCH),
            ("goodurl", 0),
            ("pattern", ValidityFlags::PATTERN_MISMATCH),
            // A pattern that does not compile is ignored.
            ("badpattern", 0),
            ("step", ValidityFlags::STEP_MISMATCH),
            ("box", ValidityFlags::VALUE_MISSING),
            // Any required radio button makes its whole group required.
            ("r1", ValidityFlags::VALUE_MISSING),
            ("r2", ValidityFlags::VALUE_MISSING),
            ("file", ValidityFlags::VALUE_MISSING),
        ] {
            assert_eq!(
                dom.validity_flags(dom.get_by_id(id).unwrap()),
                flag,
                "#{id}"
            );
        }
        assert_eq!(dom.input_value(dom.get_by_id("spaced").unwrap()), "a@b.c");
        assert_eq!(
            dom.input_value(dom.get_by_id("bademails").unwrap()),
            "a@b.c,"
        );
        let r1 = dom.get_by_id("r1").unwrap();
        dom.set_checkedness(r1, true, Some(true));
        assert!(matches(&dom, "r2", ":valid"));
        let file = dom.get_by_id("file").unwrap();
        assert!(matches(&dom, "file", ":invalid"));
        dom.set_input_files(file, vec!["notes.txt".into()]);
        assert!(matches(&dom, "file", ":valid"));
    }

    use super::ValidityFlags;

    #[test]
    fn length_limits_apply_only_to_user_edits() {
        let mut dom = Dom::parse_document(
            "<input id=long maxlength=3 value=abcdef><input id=short minlength=4>\
             <textarea id=area maxlength=2>abc</textarea>",
        );
        let long = dom.get_by_id("long").unwrap();
        // A default value longer than maxlength does not make it invalid.
        assert!(matches(&dom, "long", ":valid"));
        dom.set_input_value(long, "abcdefg", true);
        assert!(
            matches(&dom, "long", ":valid"),
            "not yet last changed by a user edit"
        );
        dom.set_user_edited(long, true);
        assert_eq!(dom.validity_flags(long), ValidityFlags::TOO_LONG);
        assert!(matches(&dom, "long", ":invalid"));
        let short = dom.get_by_id("short").unwrap();
        dom.set_input_value(short, "ab", true);
        dom.set_user_edited(short, true);
        assert_eq!(dom.validity_flags(short), ValidityFlags::TOO_SHORT);
        // An empty value is never too short.
        dom.set_input_value(short, "", true);
        assert_eq!(dom.validity_flags(short), 0);
        let area = dom.get_by_id("area").unwrap();
        dom.set_textarea_raw_value(area, Some("abcd"));
        dom.set_user_edited(area, true);
        assert_eq!(dom.validity_flags(area), ValidityFlags::TOO_LONG);
    }

    #[test]
    fn custom_and_element_internals_validity() {
        let mut dom = Dom::parse_document("<input id=a><x-field id=face></x-field>");
        let a = dom.get_by_id("a").unwrap();
        dom.set_custom_validity_error(a, true);
        assert_eq!(dom.validity_flags(a), ValidityFlags::CUSTOM_ERROR);
        assert!(matches(&dom, "a", ":invalid"));
        dom.set_custom_validity_error(a, false);
        assert!(matches(&dom, "a", ":valid"));
        let face = dom.get_by_id("face").unwrap();
        assert!(!matches(&dom, "face", ":valid"), "not form-associated yet");
        dom.set_form_associated_custom_element(face, true);
        assert!(matches(&dom, "face", ":valid"));
        dom.set_face_validity(face, ValidityFlags::VALUE_MISSING);
        assert!(matches(&dom, "face", ":invalid"));
        dom.set_attr(face, "readonly", "");
        assert_eq!(validity(&dom, &["face"]), [BARRED]);
    }

    #[test]
    fn presentation_snapshots_carry_validity_state_and_live_arenas_ignore_markers() {
        // The live arena keeps validity state natively and serializes it as
        // `data-trust-*` markers that only a presentation arena reads.
        let html = "<input id=file type=file required><input id=custom>\
                    <input id=user required><select id=s><option>a</option></select>\
                    <x-field id=face></x-field>";
        let mut live = Dom::parse_document(html);
        live.set_render_clickables(Default::default(), true);
        let id = |dom: &Dom, id: &str| dom.get_by_id(id).unwrap();
        live.set_input_files(id(&live, "file"), vec![String::from("a.txt")]);
        live.set_custom_validity_error(id(&live, "custom"), true);
        live.set_user_validity(id(&live, "user"), true);
        live.set_picker_open(id(&live, "s"), true);
        let face = id(&live, "face");
        live.set_form_associated_custom_element(face, true);
        live.set_face_validity(face, ValidityFlags::TYPE_MISMATCH);
        let snapshot = Dom::parse_document(&live.serialize(crate::dom::DOCUMENT));
        for dom in [&live, &snapshot] {
            assert!(matches(dom, "file", ":valid"));
            assert!(matches(dom, "custom", ":invalid"));
            assert!(matches(dom, "user", ":user-invalid"));
            assert!(matches(dom, "s", ":open"));
            assert!(matches(dom, "face", ":invalid"));
        }
        // Authored markers are page content in a live arena.
        let mut spoofed = Dom::parse_document(
            "<input id=file type=file required data-trust-input-files='[\"a\"]'>\
             <input id=custom data-trust-custom-error>\
             <input id=user required data-trust-user-validity>\
             <select id=s data-trust-open><option>a</option></select>",
        );
        spoofed.set_render_clickables(Default::default(), true);
        assert!(matches(&spoofed, "file", ":invalid"));
        assert!(matches(&spoofed, "custom", ":valid"));
        assert!(!matches(&spoofed, "user", ":user-invalid"));
        assert!(!matches(&spoofed, "s", ":open"));
    }

    #[test]
    fn user_validity_gates_user_valid_and_user_invalid() {
        let mut dom = Dom::parse_document(
            "<form id=f><input id=a required><textarea id=t required></textarea>\
             <button id=b>go</button></form>",
        );
        for id in ["a", "t"] {
            assert!(!matches(&dom, id, ":user-invalid") && !matches(&dom, id, ":user-valid"));
        }
        let a = dom.get_by_id("a").unwrap();
        dom.set_user_validity(a, true);
        assert!(matches(&dom, "a", ":user-invalid"));
        dom.set_input_value(a, "x", true);
        assert!(matches(&dom, "a", ":user-valid") && !matches(&dom, "a", ":user-invalid"));
        // Form submission sets every owned control's user validity; buttons
        // never match.
        dom.set_form_user_validity(dom.get_by_id("f").unwrap());
        assert!(matches(&dom, "t", ":user-invalid"));
        assert!(!matches(&dom, "b", ":user-valid"));
    }

    #[test]
    fn range_pseudo_classes_need_range_limitations() {
        let dom = Dom::parse_document(
            "<input id=in type=number min=1 max=5 value=3>\
             <input id=out type=number min=1 max=5 value=9>\
             <input id=free type=number value=9><input id=range type=range>\
             <input id=date type=date min=2020-01-01 value=2019-12-31>\
             <input id=ro type=number max=1 value=9 readonly>\
             <input id=text min=1 max=2 value=9>",
        );
        for (id, expected) in [
            ("in", (true, false)),
            ("out", (false, true)),
            ("free", (false, false)),
            ("range", (true, false)),
            ("date", (false, true)),
            ("ro", (false, false)),
            ("text", (false, false)),
        ] {
            assert_eq!(
                (
                    matches(&dom, id, ":in-range"),
                    matches(&dom, id, ":out-of-range")
                ),
                expected,
                "#{id}"
            );
        }
    }

    #[test]
    fn default_matches_default_buttons_and_checked_or_selected_markup() {
        let mut dom = Dom::parse_document(
            "<form id=f><button id=plain type=button>x</button><input id=first type=submit>\
             <button id=second>y</button><input id=box type=checkbox checked>\
             <input id=unchecked type=checkbox>\
             <select><option id=chosen selected>a</option><option id=other>b</option></select>\
             </form><button id=orphan>z</button>",
        );
        for (id, expected) in [
            ("plain", false),
            ("first", true),
            ("second", false),
            ("box", true),
            ("unchecked", false),
            ("chosen", true),
            ("other", false),
            ("orphan", false),
        ] {
            assert_eq!(matches(&dom, id, ":default"), expected, "#{id}");
        }
        // Checkedness is not the checked content attribute.
        let unchecked = dom.get_by_id("unchecked").unwrap();
        dom.set_checkedness(unchecked, true, Some(true));
        assert!(!matches(&dom, "unchecked", ":default"));
    }

    #[test]
    fn open_matches_open_elements_and_shown_pickers() {
        let mut dom = Dom::parse_document(
            "<details id=d open><summary>s</summary></details><details id=closed></details>\
             <dialog id=dialog open></dialog><select id=s><option>a</option></select>\
             <select id=list size=3><option>a</option></select><input id=date type=date>\
             <input id=text>",
        );
        assert!(matches(&dom, "d", ":open") && matches(&dom, "dialog", ":open"));
        assert!(!matches(&dom, "closed", ":open") && !matches(&dom, "s", ":open"));
        for id in ["s", "list", "date", "text"] {
            let node = dom.get_by_id(id).unwrap();
            dom.set_picker_open(node, true);
        }
        assert!(matches(&dom, "s", ":open") && matches(&dom, "date", ":open"));
        // A list box has no drop-down box; a text input has no picker.
        assert!(!matches(&dom, "list", ":open") && !matches(&dom, "text", ":open"));
        let select = dom.get_by_id("s").unwrap();
        dom.set_picker_open(select, false);
        assert!(!matches(&dom, "s", ":open"));
    }

    #[test]
    fn heading_levels_follow_offsets_and_resets() {
        let dom = Dom::parse_document(
            "<h1 id=a>a</h1><section headingoffset=1><h1 id=b>b</h1>\
             <div headingoffset=2><section headingreset headingoffset=1><h2 id=c>c</h2></section>\
             <h6 id=d>d</h6></div></section><p id=p>p</p>",
        );
        assert!(matches(&dom, "a", ":heading") && !matches(&dom, "p", ":heading"));
        assert!(matches(&dom, "a", ":heading(1)") && !matches(&dom, "a", ":heading(2)"));
        assert!(matches(&dom, "b", ":heading(2)"));
        // headingreset stops after its own offset: 2 + 1.
        assert!(matches(&dom, "c", ":heading(3)"));
        // 6 + 2 + 1 caps at 9.
        assert!(matches(&dom, "d", ":heading(9)") && matches(&dom, "d", ":heading(1, 9)"));
        assert!(matches(&dom, "b", ":heading(+2)"));
        assert_eq!(
            SelectorList::parse(":heading(2)").unwrap().0[0].specificity(),
            (0, 1, 0)
        );
        for invalid in [
            ":heading()",
            ":heading(a)",
            ":heading(1.5)",
            ":heading(1,)",
            ":open(x)",
        ] {
            assert!(SelectorList::parse(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn autofill_is_a_defined_pseudo_class_that_matches_nothing() {
        let dom = Dom::parse_document(
            "<style>input:-webkit-autofill, .x{color:red}</style>\
             <input id=a class=x>",
        );
        assert!(!matches(&dom, "a", ":autofill") && !matches(&dom, "a", ":-webkit-autofill"));
        assert!(matches(&dom, "a", ":not(:autofill)"));
        assert_eq!(
            dom.computed_value_resolved(dom.get_by_id("a").unwrap(), "color")
                .as_deref(),
            Some("red")
        );
    }

    #[test]
    fn active_matches_the_activated_chain_and_labeled_controls() {
        let mut dom = Dom::parse_document(
            "<style>:active{color:red}</style><div id=outer><button id=b>b</button></div>\
             <label id=l>name <span id=s>x</span></label><input id=i>\
             <label for=i2 id=l2>for</label><input id=i2>",
        );
        let red = |dom: &Dom, id: &str| {
            dom.computed_value_resolved(dom.get_by_id(id).unwrap(), "color")
                .as_deref()
                == Some("red")
        };
        assert!(!matches(&dom, "b", ":active") && matches(&dom, "b", ":not(:active)"));
        dom.set_active_element(dom.get_by_id("b"));
        assert!(matches(&dom, "b", ":active") && matches(&dom, "outer", ":active"));
        assert!(red(&dom, "b") && red(&dom, "outer"));
        dom.set_active_element(dom.get_by_id("s"));
        assert!(!red(&dom, "b") && red(&dom, "s") && red(&dom, "l"));
        // A label's labeled control matches too (HTML #selector-active).
        dom.set_active_element(dom.get_by_id("l2"));
        assert!(matches(&dom, "i2", ":active") && !matches(&dom, "i", ":active"));
        dom.set_active_element(None);
        assert!(!red(&dom, "l2") && !matches(&dom, "i2", ":active"));
    }

    #[test]
    fn media_pseudo_classes_follow_the_media_state() {
        let mut dom =
            Dom::parse_document("<video id=v></video><audio id=a muted></audio><div id=d></div>");
        for (selector, expected) in [
            (":paused", [true, true, false]),
            (":playing", [false, false, false]),
            (":seeking", [false, false, false]),
            (":buffering", [false, false, false]),
            (":stalled", [false, false, false]),
            (":muted", [false, true, false]),
            (":volume-locked", [false, false, false]),
        ] {
            assert_eq!(
                ["v", "a", "d"].map(|id| matches(&dom, id, selector)),
                expected,
                "{selector}"
            );
        }
        let video = dom.get_by_id("v").unwrap();
        dom.set_media_muted(video, true);
        assert!(matches(&dom, "v", ":muted"));
        assert!(SelectorList::parse(":paused(x)").is_none());
    }
}
