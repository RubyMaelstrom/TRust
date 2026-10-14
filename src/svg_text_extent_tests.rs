//! SVG 2 glyph-cell geometry through the Window bindings, in every execution tier.
//! Local SVG2 snapshot c403ca46; Web IDL snapshot 8f182624; Geometry 1 snapshot
//! csswg-drafts@81c27f68. The fixture cites the normative clauses it exercises.
use super::tests::{configured_engine, string_value};
use super::*;

fn check_svg_extent(section: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let dom = Rc::new(RefCell::new(Dom::parse_document(
            "<!doctype html><html><body></body></html>",
        )));
        let mut engine =
            configured_engine(HostState::new(dom, Rc::new(RealmClock::new())), DEFAULT_URL);
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            &format!("globalThis.svgExtentTestSection = {section:?}"),
            "SVG extent fixture section",
        )
        .unwrap();
        assert_eq!(
            string_value(&mut engine, include_str!("fixtures/svg_text_extent.mjs")),
            format!("svg-text-extent-{section}-ok"),
            "{tier:?}, section={section}",
        );
    }
}

#[test]
fn svg_get_extent_of_char_has_web_idl_bindings_and_detached_rectangles() {
    check_svg_extent("bindings");
}

#[test]
fn svg_get_extent_of_char_maps_utf16_and_logical_shaping_clusters() {
    check_svg_extent("clusters");
}

#[test]
fn svg_get_extent_of_char_preserves_positions_and_local_rotated_cells() {
    check_svg_extent("positions");
}

#[test]
fn svg_get_extent_of_char_measures_sideways_and_upright_vertical_cells() {
    check_svg_extent("vertical");
}

#[test]
fn svg_get_extent_of_char_includes_preserved_spaces_and_filters_discarded_text() {
    check_svg_extent("addressable");
}
