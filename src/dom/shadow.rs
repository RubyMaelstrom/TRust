//! HTML #parsing-main-inhead and DOM #concept-attach-a-shadow-root.
//! Consulted local WHATWG snapshots: HTML e5071a20, DOM a2331a45 (2026-09-06).

use super::{Dom, NodeData, NodeId, ns};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ShadowRootData {
    pub closed: bool,
    pub declarative: bool,
    pub delegates_focus: bool,
    pub serializable: bool,
    pub clonable: bool,
    pub manual_slot_assignment: bool,
}

impl Dom {
    fn valid_shadow_host(&self, host: NodeId) -> bool {
        let NodeData::Element { name, .. } = &self.nodes[host].data else {
            return false;
        };
        if name.ns != ns!(html) {
            return false;
        }
        let name = name.local.as_ref();
        matches!(
            name,
            "article"
                | "aside"
                | "blockquote"
                | "body"
                | "div"
                | "footer"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "header"
                | "main"
                | "nav"
                | "p"
                | "section"
                | "span"
        ) || (name.starts_with(|c: char| c.is_ascii_lowercase())
            && name.contains('-')
            && !name.chars().any(|c| {
                c.is_ascii_uppercase()
                    || matches!(c, '\t' | '\n' | '\u{000c}' | '\r' | ' ' | '\0' | '/' | '>')
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
            ))
    }

    pub(crate) fn shadow_info(&self, root: NodeId) -> Option<(NodeId, ShadowRootData)> {
        Some((
            *self.shadow_hosts.get(&root)?,
            *self.shadow_data.get(&root)?,
        ))
    }

    pub(crate) fn in_shadow_tree(&self, node: NodeId) -> bool {
        self.shadow_hosts.contains_key(&self.tree_scope(node))
    }

    /// DOM attachment can reclaim a declarative root exactly once, preserving
    /// its identity and options while removing its children in tree order.
    pub(crate) fn attach_shadow_with_options(
        &mut self,
        host: NodeId,
        data: ShadowRootData,
    ) -> Option<NodeId> {
        if !self.valid_shadow_host(host) {
            return None;
        }
        if let Some(root) = self.shadow_root(host) {
            let current = self.shadow_data.get(&root)?;
            if !current.declarative || current.closed != data.closed {
                return None;
            }
            for child in self.children(root) {
                self.detach(child);
            }
            self.shadow_data.get_mut(&root)?.declarative = false;
            return Some(root);
        }
        let root = self.attach_shadow(host);
        self.shadow_data.insert(root, data);
        Some(root)
    }

    pub(super) fn attach_declarative_shadow(&mut self, host: NodeId, template: NodeId) -> bool {
        // HTML rejects a second declaration; it must remain an ordinary inert
        // template, rather than reclaiming the existing declarative root.
        if !self.valid_shadow_host(host) || self.shadow_root(host).is_some() {
            return false;
        }
        let Some(mode) = self.attr(template, "shadowrootmode") else {
            return false;
        };
        if !mode.eq_ignore_ascii_case("open") && !mode.eq_ignore_ascii_case("closed") {
            return false;
        }
        let data = ShadowRootData {
            closed: mode.eq_ignore_ascii_case("closed"),
            declarative: true,
            delegates_focus: self.attr(template, "shadowrootdelegatesfocus").is_some(),
            serializable: self.attr(template, "shadowrootserializable").is_some(),
            clonable: self.attr(template, "shadowrootclonable").is_some(),
            manual_slot_assignment: self
                .attr(template, "shadowrootslotassignment")
                .is_some_and(|value| value.eq_ignore_ascii_case("manual")),
        };
        let root = self.attach_shadow(host);
        self.shadow_data.insert(root, data);
        // The tree builder keeps the template on its stack but does not insert
        // it in the light tree. Redirect subsequent parser insertions directly
        // into the shadow root, including nested declarations and raw text.
        if let NodeData::Element {
            template_contents, ..
        } = &mut self.nodes[template].data
        {
            *template_contents = Some(root);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::{DOCUMENT, SelectorList};

    fn query(dom: &Dom, root: NodeId, selector: &str) -> NodeId {
        dom.query(root, &SelectorList::parse(selector).unwrap(), true)[0]
    }

    #[test]
    fn declarative_shadow_parsing_preserves_scopes_slots_and_nested_roots() {
        let dom = Dom::parse_document(
            r#"<!doctype html><div id=host>
            <template shadowrootmode=OpEn shadowrootdelegatesfocus shadowrootserializable>
                <style>a { color: red }</style><a id=login href=/login>Log in</a>
                <slot></slot><section id=nested><template shadowrootmode=CLOSED>
                    <b>Nested navigation</b></template></section>
            </template><span id=light>Assigned</span>
            <template id=duplicate shadowrootmode=open><b>Inert</b></template>
        </div>"#,
        );
        let host = dom.get_by_id("host").unwrap();
        let root = dom.shadow_root(host).unwrap();
        let login = query(&dom, root, "#login");
        let slot = query(&dom, root, "slot");
        let nested = query(&dom, root, "#nested");
        assert!(
            dom.get_by_id("login").is_none(),
            "document queries stay scoped"
        );
        assert_eq!(dom.node(root).parent, None);
        assert_eq!(dom.parent_composed(root), Some(host));
        assert_eq!(dom.computed_value(login, "color").as_deref(), Some("red"));
        assert_eq!(
            dom.assigned_slot(dom.get_by_id("light").unwrap()),
            Some(slot)
        );
        assert!(dom.shadow_info(root).unwrap().1.delegates_focus);
        assert!(dom.shadow_info(root).unwrap().1.serializable);
        let inner = dom.shadow_root(nested).unwrap();
        assert!(dom.shadow_info(inner).unwrap().1.closed);
        assert!(dom.text_content(inner).contains("Nested navigation"));
        let templates = dom.query(host, &SelectorList::parse("template").unwrap(), false);
        assert_eq!(templates, vec![dom.get_by_id("duplicate").unwrap()]);
        assert_eq!(dom.shadow_data.len(), 2);
    }

    #[test]
    fn declarative_shadow_invalid_declarations_remain_inert() {
        let dom = Dom::parse_document(
            r#"<!doctype html>
            <a id=anchor><template shadowrootmode=open>Invalid host</template></a>
            <div id=bad><template shadowrootmode=" open ">Invalid mode</template></div>
            <div id=empty><template shadowrootmode>Missing mode</template></div>
            <svg><g id=svg><template shadowrootmode=open>Wrong namespace</template></g></svg>
            <font-face id=reserved><template shadowrootmode=open>Reserved name</template></font-face>
            <template id=inert><div id=inner><template shadowrootmode=open>Detached root</template></div></template>
        "#,
        );
        for id in ["anchor", "bad", "empty", "svg", "reserved"] {
            assert!(
                dom.shadow_root(dom.get_by_id(id).unwrap()).is_none(),
                "{id}"
            );
        }
        // A declaration inside ordinary template contents may attach there,
        // but that detached subtree never becomes document content/resources.
        assert!(dom.get_by_id("inner").is_none());
        assert!(!dom.serialize(DOCUMENT).contains("Detached root"));
    }

    #[test]
    fn declarative_shadow_is_disabled_for_inner_html_and_dom_parser() {
        let html = "<div id=host><template shadowrootmode=open><a>Hidden</a></template></div>";
        let mut dom = Dom::new();
        let fragment_host = dom.parse_fragment_into("body", html)[0];
        assert!(dom.shadow_root(fragment_host).is_none());
        assert_eq!(
            dom.tag_name(dom.children(fragment_host)[0]),
            Some("template")
        );
        let document = dom.parse_document_into(html);
        let host = query(&dom, document, "#host");
        assert!(dom.shadow_root(host).is_none());
        assert_eq!(dom.tag_name(dom.children(host)[0]), Some("template"));
    }

    #[test]
    fn declarative_shadow_resources_are_discovered_and_sheets_stay_scoped() {
        let html = r#"<!doctype html><style>a { color: blue }</style><a id=outside>Outside</a>
            <div id=one><template shadowrootmode=open><link rel=stylesheet href=nav.css>
                <a>First</a><script src=nav.js></script></template></div>
            <div id=two><template shadowrootmode=closed><link rel=stylesheet href=nav.css>
                <a>Second</a><style>b { color: green }</style></template></div>
            <template><link rel=stylesheet href=inert.css><script src=inert.js></script></template>"#;
        let mut dom = Dom::parse_document(html);
        assert_eq!(dom.stylesheet_links(), vec!["nav.css", "nav.css"]);
        assert_eq!(dom.inline_stylesheets().len(), 2);
        assert_eq!(dom.scripts().len(), 1);
        let resources = crate::js::external_resources(html);
        assert_eq!(
            resources.iter().filter(|r| r.source == "nav.css").count(),
            1
        );
        assert!(resources.iter().any(|r| r.source == "nav.js"));
        assert!(!resources.iter().any(|r| r.source.starts_with("inert")));
        dom.attach_external_sheets(&[("nav.css".into(), "a { color: red }".into())]);
        for id in ["one", "two"] {
            let root = dom.shadow_root(dom.get_by_id(id).unwrap()).unwrap();
            let link = query(&dom, root, "a");
            assert_eq!(dom.computed_value(link, "color").as_deref(), Some("red"));
        }
        assert_eq!(
            dom.computed_value(dom.get_by_id("outside").unwrap(), "color")
                .as_deref(),
            Some("blue")
        );
    }

    #[test]
    fn declarative_shadow_cloning_and_frame_transfer_keep_root_state() {
        let mut dom = Dom::parse_document(
            r#"<!doctype html>
            <div id=clone><template shadowrootmode=closed shadowrootclonable>
                <a href=/login>Log in</a></template></div>
            <div id=plain><template shadowrootmode=open>Not cloned</template></div>
            <iframe id=frame></iframe>"#,
        );
        let host = dom.get_by_id("clone").unwrap();
        for deep in [false, true] {
            let copy = dom.clone_subtree(host, deep);
            let root = dom.shadow_root(copy).unwrap();
            assert!(dom.shadow_info(root).unwrap().1.closed);
            assert!(dom.text_content(root).contains("Log in"));
        }
        let plain = dom.get_by_id("plain").unwrap();
        let copy = dom.clone_subtree(plain, true);
        assert!(dom.shadow_root(copy).is_none());
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            "<div id=header><template shadowrootmode=open><a>Frame navigation</a></template></div>",
            "https://example.test/",
        );
        let inner_host = query(&dom, frame, "#header");
        let root = dom.shadow_root(inner_host).unwrap();
        assert!(dom.text_content(root).contains("Frame navigation"));
        assert!(dom.is_connected(root));
    }
}
