//! Pointer Events #the-touch-action-css-property / #conforming-touch-behavior.
//! Keep policy in the same transform scopes as graphical paint, independently
//! of pointer-events hit eligibility and visibility of the ancestor's own box.

use super::*;

pub(super) fn record(builder: &mut Builder<'_>, node: NodeId, replaced: bool) {
    if node == NO_NODE || builder.dom.tag_name(node).is_none() {
        return;
    }
    let display = builder
        .dom
        .cssom_resolved_value(node, "display")
        .unwrap_or_else(|| "inline".into());
    let replaced = replaced
        || matches!(
            builder.dom.tag_name(node),
            Some(
                "img"
                    | "video"
                    | "audio"
                    | "canvas"
                    | "iframe"
                    | "frame"
                    | "input"
                    | "textarea"
                    | "select"
                    | "object"
                    | "embed"
                    | "svg"
            )
        );
    let applies = !(matches!(
        display.as_str(),
        "none"
            | "contents"
            | "table-row"
            | "table-row-group"
            | "table-header-group"
            | "table-footer-group"
            | "table-column"
            | "table-column-group"
    ) || display == "inline" && !replaced);
    let action = if applies {
        builder
            .dom
            .computed_value_resolved(node, "touch-action")
            .as_deref()
            .and_then(crate::touch::TouchAction::parse)
            .unwrap_or_default()
    } else {
        crate::touch::TouchAction::Auto
    };
    let scroll_container = builder.scroll_tree.axes(node).into_iter().any(|axis| axis);
    let document_root = builder.dom.is_document_element(node);
    let scroll_space = builder.scroll_tree.has_space(node);
    let scroll_target = builder
        .scroll_tree
        .chain(node, true)
        .find(|link| builder.scroll_indices.contains_key(&link.node))
        .map(|link| link.node);
    if action != crate::touch::TouchAction::Auto || scroll_space || document_root {
        builder
            .commands
            .push(DisplayCommand::TouchScope(crate::render::TouchScope {
                node,
                action,
                scroll_container,
                document_root,
                scroll_space,
                scroll_target,
            }));
    }
}
