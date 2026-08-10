//! Renderer-independent accessibility semantics derived from DOM and layout.
//!
//! HTML-AAM is the role/name authority; the desktop bridge maps this compact
//! tree to AccessKit. Bounds stay in document CSS pixels here and are composed
//! with chrome/scroll transforms by the frontend, never reverse-engineered
//! from Vello drawing operations.

use std::collections::HashMap;

use crate::doc::{FieldKind, Form};
use crate::dom::{DOCUMENT, Dom, NodeId};
use crate::layout2::{ControlMap, PxRect};
use crate::render::CssRect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Document,
    Generic,
    Heading,
    Paragraph,
    Link,
    Button,
    TextInput,
    PasswordInput,
    Textarea,
    Checkbox,
    Radio,
    Select,
    Image,
    List,
    ListItem,
    Table,
    Row,
    Cell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Focus,
    Activate,
    SetValue,
    SetSelection,
    ScrollIntoView,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticNode {
    pub id: u64,
    pub dom_node: Option<NodeId>,
    pub role: Role,
    pub name: String,
    pub value: Option<String>,
    pub bounds: CssRect,
    pub children: Vec<u64>,
    pub actions: Vec<Action>,
    pub focused: bool,
    pub checked: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SemanticTree {
    pub root: u64,
    pub focus: u64,
    pub nodes: Vec<SemanticNode>,
}

impl SemanticTree {
    pub const ROOT: u64 = 1;
    pub const DOM_BASE: u64 = 1_000;

    pub fn for_document(
        dom: &Dom,
        boxes: &HashMap<NodeId, PxRect>,
        forms: &[Form],
        controls: &ControlMap,
        focused: Option<NodeId>,
    ) -> Self {
        let mut nodes = Vec::new();
        let root_children = dom
            .children(DOCUMENT)
            .into_iter()
            .filter_map(|node| semantic_id(dom, boxes, node))
            .collect();
        nodes.push(SemanticNode {
            id: Self::ROOT,
            dom_node: Some(DOCUMENT),
            role: Role::Document,
            name: document_title(dom),
            value: None,
            bounds: document_bounds(boxes),
            children: root_children,
            actions: Vec::new(),
            focused: focused == Some(DOCUMENT),
            checked: None,
        });
        for node in dom.descendants(DOCUMENT) {
            let Some(bounds) = boxes.get(&node).copied() else {
                continue;
            };
            if dom.is_hidden(node) || dom.visibility_hidden(node) {
                continue;
            }
            let role = node_role(dom, node, controls, forms);
            let interactive = matches!(
                role,
                Role::Link
                    | Role::Button
                    | Role::TextInput
                    | Role::PasswordInput
                    | Role::Textarea
                    | Role::Checkbox
                    | Role::Radio
                    | Role::Select
            );
            let field = controls
                .get(&node)
                .and_then(|(form, field)| forms.get(*form)?.fields.get(*field));
            let mut actions = Vec::new();
            if interactive {
                actions.push(Action::Focus);
                actions.push(Action::Activate);
                actions.push(Action::ScrollIntoView);
            }
            if matches!(
                role,
                Role::TextInput | Role::PasswordInput | Role::Textarea | Role::Select
            ) {
                actions.push(Action::SetValue);
            }
            if matches!(role, Role::TextInput | Role::PasswordInput | Role::Textarea) {
                actions.push(Action::SetSelection);
            }
            let children = dom
                .children(node)
                .into_iter()
                .filter_map(|child| semantic_id(dom, boxes, child))
                .collect();
            nodes.push(SemanticNode {
                id: Self::DOM_BASE + node as u64,
                dom_node: Some(node),
                role,
                name: accessible_name(dom, node, field.map(|field| field.label.as_str())),
                value: field.and_then(|field| match field.kind {
                    FieldKind::Password => Some(String::new()),
                    FieldKind::Hidden => None,
                    _ => Some(field.value.clone()),
                }),
                bounds: CssRect::new(
                    bounds.left as f32,
                    bounds.top as f32,
                    bounds.width as f32,
                    bounds.height as f32,
                ),
                children,
                actions,
                focused: focused == Some(node),
                checked: field.and_then(|field| {
                    matches!(field.kind, FieldKind::Checkbox | FieldKind::Radio)
                        .then_some(field.checked)
                }),
            });
        }
        let focus = focused
            .map(|node| Self::DOM_BASE + node as u64)
            .filter(|id| nodes.iter().any(|node| node.id == *id))
            .unwrap_or(Self::ROOT);
        Self {
            root: Self::ROOT,
            focus,
            nodes,
        }
    }
}

fn semantic_id(dom: &Dom, boxes: &HashMap<NodeId, PxRect>, node: NodeId) -> Option<u64> {
    (boxes.contains_key(&node) && !dom.is_hidden(node) && !dom.visibility_hidden(node))
        .then_some(SemanticTree::DOM_BASE + node as u64)
}

fn node_role(dom: &Dom, node: NodeId, controls: &ControlMap, forms: &[Form]) -> Role {
    if let Some(role) = dom.attr(node, "role") {
        match role.split_ascii_whitespace().next().unwrap_or("") {
            "link" => return Role::Link,
            "button" => return Role::Button,
            "textbox" => return Role::TextInput,
            "checkbox" => return Role::Checkbox,
            "radio" => return Role::Radio,
            "combobox" | "listbox" => return Role::Select,
            "heading" => return Role::Heading,
            "img" => return Role::Image,
            "list" => return Role::List,
            "listitem" => return Role::ListItem,
            "table" | "grid" => return Role::Table,
            "row" => return Role::Row,
            "cell" | "gridcell" => return Role::Cell,
            _ => {}
        }
    }
    if let Some((form, field)) = controls.get(&node)
        && let Some(field) = forms.get(*form).and_then(|form| form.fields.get(*field))
    {
        return match field.kind {
            FieldKind::Text => Role::TextInput,
            FieldKind::Password => Role::PasswordInput,
            FieldKind::Textarea => Role::Textarea,
            FieldKind::Checkbox => Role::Checkbox,
            FieldKind::Radio => Role::Radio,
            FieldKind::Select(_) => Role::Select,
            FieldKind::Submit | FieldKind::Button | FieldKind::Reset => Role::Button,
            FieldKind::Hidden => Role::Generic,
        };
    }
    match dom.tag_name(node) {
        Some("a") if dom.attr(node, "href").is_some() => Role::Link,
        Some("button") => Role::Button,
        Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6") => Role::Heading,
        Some("p") => Role::Paragraph,
        Some("img") => Role::Image,
        Some("ul" | "ol") => Role::List,
        Some("li") => Role::ListItem,
        Some("table") => Role::Table,
        Some("tr") => Role::Row,
        Some("td" | "th") => Role::Cell,
        _ => Role::Generic,
    }
}

fn accessible_name(dom: &Dom, node: NodeId, field_label: Option<&str>) -> String {
    dom.attr(node, "aria-label")
        .or_else(|| dom.attr(node, "alt"))
        .or_else(|| dom.attr(node, "title"))
        .or(field_label.filter(|label| !label.is_empty()))
        .map(str::to_string)
        .unwrap_or_else(|| {
            dom.text_content(node)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
}

pub fn document_title(dom: &Dom) -> String {
    dom.descendants(DOCUMENT)
        .find(|node| dom.tag_name(*node) == Some("title"))
        .map(|node| dom.text_content(node).trim().to_string())
        .unwrap_or_default()
}

fn document_bounds(boxes: &HashMap<NodeId, PxRect>) -> CssRect {
    let right = boxes
        .values()
        .map(|rect| rect.left + rect.width)
        .fold(0.0, f64::max);
    let bottom = boxes
        .values()
        .map(|rect| rect.top + rect.height)
        .fold(0.0, f64::max);
    CssRect::new(0.0, 0.0, right as f32, bottom as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantics_come_from_dom_and_layout_not_paint() {
        let dom = Dom::parse_document(
            r#"<title>Example</title><a href='/next'>Next</a><input aria-label='Name'>"#,
        );
        let base = url::Url::parse("https://example.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            crate::layout2::Viewport::new(500.0, 300.0),
            &forms,
            &controls,
            &Default::default(),
        );
        let tree = SemanticTree::for_document(&dom, &layout.boxes, &forms, &controls, None);
        assert!(tree.nodes.iter().any(|node| node.role == Role::Link));
        assert!(
            tree.nodes
                .iter()
                .any(|node| { node.role == Role::TextInput && node.name == "Name" })
        );
        assert_eq!(tree.nodes[0].name, "Example");
    }
}
