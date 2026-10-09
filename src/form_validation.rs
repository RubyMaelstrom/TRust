//! HTML #interactively-validate-the-constraints for a static form, the
//! presentation of a document without page scripts (local whatwg/html
//! e5071a20, 2026-09-04): the form submission algorithm validates the form
//! unless the submitter's no-validate state is true (#concept-fs-novalidate,
//! `novalidate` or a submit button's `formnovalidate`), and an invalid form
//! is not submitted; the problem is reported for the first invalid control
//! in tree order.
//!
//! A static form keeps its controls' values in the presentation's model, not
//! a live DOM. The constraints themselves (#constraints: required, type,
//! pattern, length, range and step, and #barred-from-constraint-validation)
//! are the arena's, the same validity states `ValidityState` and the
//! validity pseudo-classes use: the document is parsed again, its controls
//! take the model's values, and the arena answers.

use crate::doc::{FieldKind, Form};
use crate::dom::Dom;

/// The first control of `forms[form]` that does not satisfy its
/// constraints, by field index, with the message reporting the problem;
/// `None` when the form is valid. `body` is the static document the forms
/// were extracted from.
pub fn first_invalid_control(
    url: &url::Url,
    content_type: &str,
    body: &[u8],
    forms: &[Form],
    form: usize,
) -> Option<(usize, String)> {
    let target = forms.get(form)?;
    let html = crate::http::decode_document_body(url, content_type, body).text;
    let mut dom = Dom::parse_document(&html);
    dom.set_doc_url(Some(url.clone()));
    let (fresh, controls) = crate::http::extract_forms_arena(&dom, url, None);
    // The same document extracts the same forms; a different shape means
    // the model no longer describes this document.
    let shape = fresh.get(form)?;
    if shape.fields.len() != target.fields.len() {
        return None;
    }
    let mut nodes = vec![None; target.fields.len()];
    for (&node, &(f, field)) in &controls {
        if f == form && field < nodes.len() {
            nodes[field] = Some(node);
        }
    }
    for (field, node) in target.fields.iter().zip(&nodes) {
        if let Some(node) = *node {
            apply_field_state(&mut dom, node, field);
        }
    }
    target
        .fields
        .iter()
        .zip(&nodes)
        .enumerate()
        .find_map(|(index, (_, node))| {
            let node = (*node)?;
            if !dom.will_validate(node) {
                return None;
            }
            let flags = dom.validity_flags(node);
            (flags != 0).then(|| (index, validation_message(&dom, node, flags)))
        })
}

/// Give the arena control `node` the static model's state.
fn apply_field_state(dom: &mut Dom, node: usize, field: &crate::doc::Field) {
    match &field.kind {
        FieldKind::Checkbox | FieldKind::Radio => {
            dom.set_checkedness(node, field.checked, Some(true));
        }
        FieldKind::Textarea => {
            dom.set_textarea_raw_value(node, Some(&field.value));
        }
        FieldKind::Select(_) => {
            let options = dom.select_options(node);
            let multiple = field.select.as_ref().filter(|select| select.multiple);
            let mut chosen = false;
            for (index, option) in options.into_iter().enumerate() {
                let selected = match multiple {
                    Some(select) => select.options.get(index).is_some_and(|o| o.selected),
                    // A single select's selection is its value: the first
                    // option with that value.
                    None => {
                        let value = field
                            .select
                            .as_ref()
                            .and_then(|select| select.options.get(index))
                            .map(|o| o.value.as_str());
                        let pick = !chosen && value == Some(field.value.as_str());
                        chosen |= pick;
                        pick
                    }
                };
                dom.set_option_selectedness(option, Some(selected), Some(true));
            }
        }
        FieldKind::File(control) => {
            dom.set_input_files(
                node,
                control.files.iter().map(|file| file.name.clone()).collect(),
            );
        }
        FieldKind::Text | FieldKind::Number | FieldKind::Password => {
            dom.set_input_value(node, field.editing_value(), true);
        }
        FieldKind::Hidden | FieldKind::Submit | FieldKind::Button | FieldKind::Reset => {}
    }
}

/// The message reporting an invalid control's problem (the platform's
/// `validationMessage` wording).
fn validation_message(dom: &Dom, node: usize, flags: u16) -> String {
    use crate::dom::ValidityFlags as V;
    let tag = dom.tag_name(node).unwrap_or("");
    let ty = if tag == "input" {
        dom.input_type(node)
    } else {
        String::new()
    };
    let attr = |name| dom.attr(node, name).unwrap_or("").to_string();
    if flags & V::VALUE_MISSING != 0 {
        return String::from(match (tag, ty.as_str()) {
            ("select", _) => "Please select an item in the list.",
            (_, "checkbox") => "Please check this box if you want to proceed.",
            (_, "radio") => "Please select one of these options.",
            (_, "file") => "Please select a file.",
            _ => "Please fill out this field.",
        });
    }
    if flags & V::TYPE_MISMATCH != 0 {
        return String::from(if ty == "email" {
            "Please enter an email address."
        } else {
            "Please enter a URL."
        });
    }
    if flags & V::PATTERN_MISMATCH != 0 {
        let title = attr("title");
        return if title.is_empty() {
            String::from("Please match the requested format.")
        } else {
            format!("Please match the requested format. {title}")
        };
    }
    if flags & V::TOO_LONG != 0 {
        return format!(
            "Please shorten this text to {} characters or less.",
            attr("maxlength").trim()
        );
    }
    if flags & V::TOO_SHORT != 0 {
        return format!(
            "Please lengthen this text to {} characters or more.",
            attr("minlength").trim()
        );
    }
    if flags & V::RANGE_UNDERFLOW != 0 {
        return format!("Value must be greater than or equal to {}.", attr("min"));
    }
    if flags & V::RANGE_OVERFLOW != 0 {
        return format!("Value must be less than or equal to {}.", attr("max"));
    }
    if flags & (V::BAD_INPUT | V::STEP_MISMATCH) != 0 {
        return String::from("Please enter a valid value.");
    }
    String::from("The value is invalid.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_forms_report_their_first_invalid_control() {
        // HTML #interactively-validate-the-constraints and #constraints over the static
        // model's values; hidden and disabled controls are barred.
        let url = url::Url::parse("https://example.com/").unwrap();
        let html = br#"<form><input type=hidden name=h required>
            <input name=q required><input name=e type=email value="x@example.com">
            <input name=n type=number min=2 value=3><input name=d required disabled>
            <select name=s required><option value="">Pick</option><option>a</option></select>
            <input type=submit></form>"#;
        let doc = crate::http::parse(&url, "text/html", html, 80, 0, &Default::default());
        let mut forms = doc.forms.clone();
        let field = |forms: &[Form], name: &str| {
            forms[0].fields.iter().position(|f| f.name == name).unwrap()
        };
        let check = |forms: &[Form]| first_invalid_control(&url, "text/html", html, forms, 0);
        assert_eq!(
            check(&forms),
            Some((
                field(&forms, "q"),
                String::from("Please fill out this field.")
            ))
        );
        let q = field(&forms, "q");
        forms[0].fields[q].value = String::from("query");
        let e = field(&forms, "e");
        forms[0].fields[e].value = String::from("not an address");
        assert_eq!(
            check(&forms),
            Some((e, String::from("Please enter an email address.")))
        );
        forms[0].fields[e].value = String::from("me@example.com");
        let n = field(&forms, "n");
        forms[0].fields[n].value = String::from("1");
        assert_eq!(
            check(&forms),
            Some((n, String::from("Value must be greater than or equal to 2.")))
        );
        forms[0].fields[n].value = String::from("2");
        let s = field(&forms, "s");
        assert_eq!(
            check(&forms),
            Some((s, String::from("Please select an item in the list.")))
        );
        forms[0].fields[s].value = String::from("a");
        assert_eq!(check(&forms), None);
    }
}
