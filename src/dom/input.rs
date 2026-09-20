//! HTML input value/dirty-value state belongs to the arena, not JS wrappers.
use super::{Dom, NodeId};
use crate::input::{NumericInput, NumericType};

#[derive(Clone, Debug)]
pub(super) struct InputValue {
    pub(super) value: String,
    pub(super) editing: Option<String>,
    pub(super) dirty: bool,
    bad_input: bool,
}

impl Dom {
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
            Some("textarea") => self
                .text_content(id)
                .replace("\r\n", "\n")
                .replace('\r', "\n"),
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
            self.text_content(id)
                .replace("\r\n", "\n")
                .replace('\r', "\n")
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
        if self.tag_name(id) != Some("input") || !self.input_values.contains_key(&id) {
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
    fn input_value_mode(t: &str) -> bool {
        !matches!(
            t,
            "hidden" | "submit" | "image" | "reset" | "button" | "checkbox" | "radio" | "file"
        )
    }
    pub(crate) fn numeric_input(&self, id: NodeId) -> Option<NumericInput> {
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
            config.sanitize(value)
        } else {
            value.replace(['\r', '\n'], "")
        }
    }
    pub fn input_value(&self, id: NodeId) -> String {
        let ty = self.input_type(id);
        if ty == "file" {
            return String::new();
        }
        if !Self::input_value_mode(&ty) {
            return self
                .attr(id, "value")
                .unwrap_or(if matches!(ty.as_str(), "checkbox" | "radio") {
                    "on"
                } else {
                    ""
                })
                .into();
        }
        if let Some(state) = self.input_values.get(&id) {
            return state.value.clone();
        }
        // A presentation snapshot transports the current value separately
        // from the content attribute, which remains the numeric step base.
        let value = if !self.render_live() {
            self.attr(id, "data-trust-input-value")
                .or_else(|| self.attr(id, "value"))
        } else {
            self.attr(id, "value")
        };
        self.sanitize_input_value(id, value.unwrap_or(""))
    }
    pub fn set_input_value(&mut self, id: NodeId, value: &str, user: bool) -> bool {
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
        if let Some(state) = self.input_values.get(&id) {
            state.editing.as_deref()
        } else if !self.render_live() {
            self.attr(id, "data-trust-input-edit")
        } else {
            None
        }
    }
    pub fn input_mutable(&self, id: NodeId) -> bool {
        !self.actually_disabled(id, "input") && self.attr(id, "readonly").is_none()
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
        if (self.input_type(id) == "range"
            && ["min", "max", "step"]
                .iter()
                .any(|name| attr.eq_ignore_ascii_case(name)))
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
}
