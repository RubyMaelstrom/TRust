//! Repeatable engine workloads, independent of a particular site's scripts.
//! Run optimized and without competing compiler/raster work. Wall times are
//! diagnostics; deterministic work bounds belong in ordinary regression tests.

use super::*;
use std::time::Instant;

fn repeated(s: &str, n: usize) -> String {
    s.repeat(n)
}

pub(super) fn nested(depth: usize, display: &str, positioned: bool) -> String {
    format!(
        "<style>body{{margin:0}}.nest{{display:{display};flex-direction:column;grid-template-columns:minmax(0,1fr);min-width:0}}.abs{{position:absolute;width:10px;height:10px}}</style>{}<div id=edit>alpha beta gamma delta{}</div>{}",
        repeated("<div class=nest>", depth),
        if positioned { "<i class=abs></i>" } else { "" },
        repeated("</div>", depth)
    )
}

fn workloads(count: usize) -> Vec<(String, String)> {
    let mut cases = Vec::new();
    for (name, css, markup) in [
        (
            "blocks",
            ".item{margin:3px;padding:2px}",
            "<div class=item>alpha beta gamma delta</div>",
        ),
        (
            "inlines",
            ".item{padding:2px}",
            "<span class=item>alpha <b>beta</b> gamma delta </span>",
        ),
        (
            "flex",
            "main{display:flex;flex-wrap:wrap}.item{flex:1 1 120px;padding:2px}",
            "<div class=item>alpha beta gamma delta</div>",
        ),
        (
            "grid",
            "main{display:grid;grid-template-columns:repeat(5,minmax(0,1fr))}.item{padding:2px}",
            "<div class=item>alpha beta gamma delta</div>",
        ),
        (
            "floats",
            ".item{float:left;width:120px;padding:2px}",
            "<div class=item>alpha beta gamma delta</div>",
        ),
        (
            "positioned",
            ".item{position:relative;width:120px}.item i{position:absolute;right:0;bottom:0}",
            "<div class=item>alpha beta gamma delta<i>badge</i></div>",
        ),
        (
            "tables",
            "table{border-collapse:collapse}td{padding:3px}",
            "<table class=item><tr><td>alpha beta</td><td>gamma delta</td></tr></table>",
        ),
        (
            "queries",
            "main{display:grid;grid-template-columns:repeat(5,1fr)}.item{container-type:inline-size}@container(width>100px){b{padding:3px}}",
            "<div class=item><b>alpha beta gamma delta</b></div>",
        ),
    ] {
        cases.push((name.into(), format!("<style>body{{margin:0}}{css}</style><main><div id=edit>alpha beta gamma delta</div>{}</main>", repeated(markup, count).replacen("class=item", "id=resize class=item", 1))));
    }
    cases.push(("shadow".into(), format!("<div id=host><template shadowrootmode=open><style>:host{{display:block}} main{{display:grid;grid-template-columns:repeat(5,1fr)}}slot{{display:contents}}</style><main><slot></slot></main></template><div id=edit>alpha beta</div>{}</div>", repeated("<div>slotted content</div>", count))));
    cases.push(("images".into(), format!("<style>main{{display:flex;flex-wrap:wrap}}img{{width:60px;height:auto}}</style><main><div id=edit>alpha beta</div>{}</main>", repeated("<div><img src=/image.png>caption</div>", count))));
    cases.push(("logical-clamp".into(), format!("<style>.item{{writing-mode:vertical-rl;direction:rtl;inline-size:140px;block-size:50px}}p{{line-clamp:2;width:80px}}</style><div id=edit>alpha beta</div>{}", repeated("<div class=item>sideways text <b>span</b></div><p>line clamped content with several wrapping words</p>", count/2))));
    for depth in [4, 8, 12] {
        for display in ["block", "flex", "grid"] {
            for positioned in [false, true] {
                cases.push((
                    format!("nested-{display}-{depth}-oof{positioned}"),
                    nested(depth, display, positioned),
                ));
            }
        }
    }
    cases
}

#[test]
fn shadow_character_data_invalidates_both_formatting_paths() {
    // DOM #concept-cd-replace preserves slottable identity. CSS Shadow 1
    // #flattening makes its formatting path cross assigned/forwarding slots.
    let base = Url::parse("https://layout.invalid/").unwrap();
    let vp = Viewport::new(640., 480.);
    let controls = ControlMap::new();
    let images = ImageSizes::new();
    for direct_text in [false, true] {
        let mut dom = Dom::parse_document(&format!(
            "<div id=host>{}{}</div>",
            if direct_text {
                "alpha"
            } else {
                "<div id=edit>alpha</div>"
            },
            "<div>independent sibling</div>".repeat(40)
        ));
        let host = dom.get_by_id("host").unwrap();
        let text = if direct_text {
            dom.children(host)[0]
        } else {
            dom.children(dom.get_by_id("edit").unwrap())[0]
        };
        let outer = dom.attach_shadow(host);
        let inner_host = dom.create_element("div");
        dom.append(outer, inner_host);
        let forwarding_slot = dom.create_element("slot");
        dom.append(inner_host, forwarding_slot);
        let inner = dom.attach_shadow(inner_host);
        let grid = dom.create_element("main");
        dom.set_attr(
            grid,
            "style",
            "display:grid;grid-template-columns:repeat(5,1fr)",
        );
        dom.append(inner, grid);
        let receiving_slot = dom.create_element("slot");
        dom.append(grid, receiving_slot);
        let shadow_text = dom.create_text("inside shadow");
        dom.append(grid, shadow_text);
        for (changed, data) in [
            (text, "different assigned text wraps over several lines"),
            (shadow_text, "different shadow text"),
        ] {
            let resources = (
                crate::font_system::page_font_epoch(),
                crate::img::svg_intrinsic_epoch(),
            );
            measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
            dom.set_text(changed, data);
            let warm = measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
            if resources
                == (
                    crate::font_system::page_font_epoch(),
                    crate::img::svg_intrinsic_epoch(),
                )
            {
                assert!(
                    warm.work.tree_builds <= 16,
                    "unrelated slotted siblings rebuilt: {:?}",
                    warm.work
                );
                assert!(
                    warm.work.item_hits >= 30,
                    "independent slotted items not reused: {:?}",
                    warm.work
                );
            }
            memo::tests::assert_cold(&mut dom, &base, vp, &[], &controls, &images);
        }
    }
}

#[test]
#[ignore]
fn layout_engine_workload_matrix() {
    let filter = std::env::var("TRUST_LAYOUT_BENCH_FILTER").unwrap_or_default();
    let profile_enabled = std::env::var_os("TRUST_LAYOUT_BENCH_PROFILE").is_some();
    let iterations = std::env::var("TRUST_LAYOUT_BENCH_ITERATIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(7)
        .max(1);
    let base = Url::parse("https://layout.invalid/").unwrap();
    let vp = Viewport::new(960., 640.);
    let controls = ControlMap::new();
    let mut images = ImageSizes::from([("https://layout.invalid/image.png".to_owned(), (120, 80))]);
    // Initialize process-wide fonts/shaping before measuring a layout.
    measure_retained_layout(
        &Dom::parse_document("warm fonts"),
        &base,
        vp,
        &[],
        &controls,
        &images,
    );
    let count = std::env::var("TRUST_LAYOUT_BENCH_BOXES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(220);
    for (name, html) in workloads(count) {
        if !filter.is_empty() && !name.contains(&filter) {
            continue;
        }
        let mut dom = Dom::parse_document(&html);
        let text = dom
            .children(dom.get_by_id("edit").unwrap())
            .into_iter()
            .find(|&n| matches!(dom.node(n).data, crate::dom::NodeData::Text(_)))
            .unwrap();
        for phase in ["cold", "warm", "edit", "resize", "resource"] {
            if phase == "resize" && dom.get_by_id("resize").is_none() {
                continue;
            }
            if phase == "resource" && name != "images" {
                continue;
            }
            let n = if phase == "cold" { 1 } else { iterations };
            let mut samples = Vec::new();
            for i in 0..n {
                if phase == "edit" {
                    dom.set_text(text, &format!("alpha beta gamma delta {i}"));
                }
                if phase == "resize" {
                    dom.set_attr(
                        dom.get_by_id("resize").unwrap(),
                        "style",
                        &format!("width:{}px", 80 + (i % 2) * 80),
                    );
                }
                if phase == "resource" {
                    images.insert(
                        "https://layout.invalid/image.png".to_owned(),
                        (120, 80 + (i % 2) as u32 * 80),
                    );
                }
                let start = Instant::now();
                let calculate =
                    || measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
                let (layout, operations) = if profile_enabled {
                    diagnostics::measure(calculate)
                } else {
                    (calculate(), diagnostics::Profile::default())
                };
                samples.push((
                    start.elapsed().as_secs_f64() * 1e6,
                    layout.work,
                    layout.boxes.len(),
                    operations,
                ));
            }
            samples.sort_by(|a, b| a.0.total_cmp(&b.0));
            let (median, work, boxes, profile) = samples[n / 2];
            eprintln!(
                "ENGINE_BENCH {name} {phase} median_us={:.1} max_us={:.1} boxes={boxes} flow_us={} tree_us={} hits={} intrinsic_hits={} tree_builds={} passes={} uncacheable={}",
                median,
                samples[n - 1].0,
                work.flow.as_micros(),
                work.tree.as_micros(),
                work.item_hits,
                work.intrinsic_hits,
                work.tree_builds,
                work.passes,
                profile.uncacheable
            );
            if profile_enabled {
                eprint!("ENGINE_OPS {name} {phase}");
                profile.print();
            }
        }
    }
}

#[test]
fn incremental_layout_matches_fresh_across_formatting_models() {
    let base = Url::parse("https://layout.invalid/").unwrap();
    let vp = Viewport::new(640., 480.);
    let controls = ControlMap::new();
    let mut images = ImageSizes::from([("https://layout.invalid/image.png".to_owned(), (120, 80))]);
    for (name, html) in workloads(24) {
        let mut dom = Dom::parse_document(&html);
        let edit = dom.get_by_id("edit").unwrap();
        let text = dom.children(edit)[0];
        for step in 0..4 {
            measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
            match step {
                0 => dom.set_text(
                    text,
                    "a longer line that changes intrinsic contributions and can wrap",
                ),
                1 => dom.set_attr(edit, "style", "font-size:21px;padding:3%;height:40px"),
                2 => {
                    let child = dom.create_element("span");
                    dom.set_attr(
                        child,
                        "style",
                        "display:inline-block;width:13px;height:20px",
                    );
                    dom.append(edit, child);
                }
                _ => {
                    if name == "images" {
                        images.insert("https://layout.invalid/image.png".to_owned(), (50, 160));
                    }
                    if let Some(resize) = dom.get_by_id("resize") {
                        dom.set_attr(resize, "style", "width:160px;position:relative;left:3px");
                    }
                }
            }
            // The shared assertion covers CSSOM boxes/tracks/scroll extents,
            // graphical display lists/hit testing, and terminal rows.
            memo::tests::assert_cold(&mut dom, &base, vp, &[], &controls, &images);
        }
    }
}
