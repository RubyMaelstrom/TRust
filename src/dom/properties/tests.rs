use super::*;

fn compute(syntax: &str, value: &str) -> Option<String> {
    Syntax::parse(syntax)
        .unwrap()
        .compute(value, &Context::validation())
}

// CSS Values 5 #substitution guards last only for the current invocation;
// independent style reads must not acquire dependencies on earlier reads.
// Exercise a page-sized walk and mutation in both debug and release tests.
#[test]
fn property_resolution_preserves_styles_across_repeated_reads_and_mutation() {
    let mut html = String::from(
        r#"<style>
        @property --space {syntax:'<length>';inherits:true;initial-value:1px}
        html {font-size:62.5%;--ink:#2c2c2c;--paper:#fbfbfd;--family:Arial,sans-serif}
        body {font-size:14px;line-height:1.5;--space:2em}
        div {color:var(--ink);background-color:var(--paper);font-family:var(--family);
             width:var(--space);--alpha:1;opacity:var(--alpha)}
        #cycle {--space:var(--space);--bad:var(--bad);height:var(--bad,7px)}
        </style><body><div id=cycle></div>"#,
    );
    for n in 0..320 {
        html.push_str(&format!("<div id=n{n}>Example text</div>"));
    }
    let mut dom = Dom::parse_document(&html);
    let nodes: Vec<_> = (0..320)
        .map(|n| dom.get_by_id(&format!("n{n}")).unwrap())
        .collect();
    let cycle = dom.get_by_id("cycle").unwrap();
    let root = dom.style_scope_root_element(cycle).unwrap();
    for pass in 0..3 {
        // A real cycle must default without contaminating subsequent reads.
        assert_eq!(dom.resolve_vars(cycle, "var(--bad,7px)"), "7px");
        for &node in &nodes {
            assert_eq!(dom.font_px(root), 10.);
            assert_eq!(dom.font_px(node), 14.);
            for (property, expected) in [
                ("color", if pass == 0 { "#2c2c2c" } else { "#123456" }),
                ("background-color", "#fbfbfd"),
                ("font-family", "Arial,sans-serif"),
                ("line-height", "1.5"),
                ("width", "28px"),
                ("opacity", "1"),
            ] {
                assert_eq!(
                    dom.computed_value_resolved(node, property).as_deref(),
                    Some(expected),
                    "pass {pass}, node {node}, {property}"
                );
            }
        }
        if pass == 0 {
            dom.set_attr(root, "style", "--ink:#123456");
        }
    }
}

#[test]
fn registered_syntax_components_and_computed_types() {
    for (syntax, value, expected) in [
        ("<length>", "1in", "96px"),
        ("<length>", "0", "0px"),
        ("<number>", "calc(6 / 2)", "3"),
        ("<integer>", "calc(-1.5)", "-1"),
        ("<angle>", "0.5turn", "180deg"),
        ("<time>", "125ms", "0.125s"),
        ("<resolution>", "192dpi", "2dppx"),
        ("<percentage>", "25%", "25%"),
        (
            "<length-percentage>",
            "calc(2in + 10%)",
            "calc(10% + 192px)",
        ),
        ("<length>+", "1in 2px", "96px 2px"),
        ("<number>#", "1,2, 3", "1, 2, 3"),
        ("small | <length>", "small", "small"),
        ("<custom-ident>", "MiXeD", "MiXeD"),
        ("<string>", "'a b'", "\"a b\""),
        ("<color>", "red", "rgb(255, 0, 0)"),
        (
            "<transform-list>",
            "translate(1in, 2px) rotate(.5turn)",
            "translate(96px, 2px) rotate(180deg)",
        ),
        ("*", "", ""),
        ("*", "default", "default"),
    ] {
        assert_eq!(
            compute(syntax, value).as_deref(),
            Some(expected),
            "{syntax}: {value}"
        );
    }
    for (syntax, value) in [
        ("<length>", "1"),
        ("<length>", "calc(0)"),
        ("<length>", "10%"),
        ("<integer>", "1.5"),
        ("<integer>", "1.0"),
        ("<number>", "calc(1px + 2)"),
        ("<number>#", "1 2"),
        ("<number>+", "1,2"),
        ("<length>", "calc(1px+ 2px)"),
        ("<custom-ident>", "default"),
        ("small", "SMALL"),
        ("<color>", "garbage"),
        ("<transform-list>", "none"),
        ("<url>", "foo"),
    ] {
        assert_eq!(compute(syntax, value), None, "{syntax}: {value}");
    }
    for syntax in [
        "",
        "<Length>",
        "<unknown>",
        "<length>++",
        "<length> #",
        "<transform-list>+",
        "* | a",
        "a || b",
        "inherit",
        "a /*comment*/ | b",
    ] {
        assert!(Syntax::parse(syntax).is_none(), "{syntax}");
    }
}

#[test]
fn registered_properties_compute_before_inheritance_and_substitution() {
    let dom = Dom::parse_document(
        r#"<style>
        @property --size { syntax: '<length>'; inherits: true; initial-value: 1px; }
        @property --own { syntax: '<length>'; inherits: false; initial-value: 3px; }
        @property --empty { initial-value: ; }
        #p {font-size:20px; --size:2em; --own:5em; --untyped:2em}
        #c {font-size:10px; width:var(--size); height:var(--own)}
        #c::before {--size:3em; font-size:4px; width:var(--size); height:var(--own)}
        </style><div id=p><div id=c></div></div>"#,
    );
    let p = dom.get_by_id("p").unwrap();
    let c = dom.get_by_id("c").unwrap();
    assert_eq!(dom.custom_prop(p, "--size").as_deref(), Some("40px"));
    assert_eq!(dom.custom_prop(c, "--size").as_deref(), Some("40px"));
    assert_eq!(dom.custom_prop(c, "--own").as_deref(), Some("3px"));
    assert_eq!(dom.custom_prop(c, "--untyped").as_deref(), Some("2em"));
    assert_eq!(dom.custom_prop(c, "--empty").as_deref(), Some(""));
    assert_eq!(
        dom.computed_value_resolved(c, "width").as_deref(),
        Some("40px")
    );
    assert_eq!(
        dom.pseudo_layout_value(c, PseudoEl::Before, "width")
            .as_deref(),
        Some("12px")
    );
    assert_eq!(
        dom.pseudo_layout_value(c, PseudoEl::Before, "height")
            .as_deref(),
        Some("3px")
    );
}

#[test]
fn registered_properties_late_registration_and_cssom_mutation() {
    let mut dom = Dom::parse_document(
        "<style id=s>@property --x {syntax:'<length>';initial-value:1px;inherits:false} #c{--x:2em}</style><div id=c style='font-size:10px;width:var(--x)'></div>",
    );
    let c = dom.get_by_id("c").unwrap();
    let sheet = dom.get_by_id("s").unwrap();
    assert_eq!(dom.custom_prop(c, "--x").as_deref(), Some("20px"));
    dom.set_cssom_sheet(
        sheet,
        "@property --x {syntax:'<number>';initial-value:7;inherits:false} #c{--x:2em}".into(),
        "".into(),
        false,
    );
    assert_eq!(dom.custom_prop(c, "--x").as_deref(), Some("7"));
    dom.register_property(DOCUMENT, "--x", "<length>", false, Some("8px".into()), None)
        .unwrap();
    assert_eq!(dom.custom_prop(c, "--x").as_deref(), Some("20px"));
    assert_eq!(
        dom.register_property(DOCUMENT, "--x", "invalid", false, None, None),
        Err("InvalidModificationError")
    );
    assert_eq!(
        dom.register_property(
            DOCUMENT,
            "--bad",
            "<length>",
            false,
            Some("1em".into()),
            None
        ),
        Err("SyntaxError")
    );
    assert!(
        dom.register_property(DOCUMENT, "--bad", "*", true, None, None)
            .is_ok()
    );
    dom.set_cssom_sheet(sheet, "".into(), "".into(), false);
    assert_eq!(dom.custom_prop(c, "--x").as_deref(), Some("8px"));
}

#[test]
fn registered_properties_cycles_and_token_boundaries() {
    let dom = Dom::parse_document(
        r#"<style>
        @property --x {syntax:'<length>';initial-value:3px;inherits:false}
        @property --n {syntax:'<number>';initial-value:2;inherits:false}
        #self {--x:var(--x);width:var(--x,99px)}
        #fallback {--ok:blue;--x:var(--ok,var(--x));width:var(--x,99px)}
        #font {--x:2em;font-size:var(--x);width:var(--x)}
        #split {--raw:var(--n)px;--x:var(--n)px;width:var(--x)}
        </style><div style='font-size:20px'><div id=self></div><div id=fallback></div><div id=font></div><div id=split></div></div>"#,
    );
    for id in ["self", "fallback", "font", "split"] {
        let node = dom.get_by_id(id).unwrap();
        assert_eq!(dom.custom_prop(node, "--x").as_deref(), Some("3px"), "{id}");
    }
    let font = dom.get_by_id("font").unwrap();
    assert_eq!(dom.font_px(font), 20.);
    assert_eq!(
        dom.custom_prop(dom.get_by_id("split").unwrap(), "--raw")
            .as_deref(),
        Some("2/**/px")
    );
}

#[test]
fn registered_rules_optional_descriptors_and_multiple_names() {
    let rule=PropertyRule::parse("--a, --b", "syntax:'<length>'; syntax:'bad |'; inherits:false;inherits:banana;initial-value:1in;initial-value:red;unknown:yes;").unwrap();
    assert_eq!(rule.names, ["--a", "--b"]);
    assert!(!rule.registration.inherits);
    assert_eq!(rule.registration.initial.as_deref(), Some("1in"));
    let defaults = PropertyRule::parse("--a", "").unwrap();
    assert!(defaults.registration.inherits);
    assert_eq!(defaults.registration.syntax_text, "*");
    assert_eq!(defaults.registration.initial, None);
    assert!(PropertyRule::parse("--a, bad", "").is_none());
}

#[test]
fn registered_images_transforms_and_math_validate_nested_grammars() {
    for (syntax, value, expected) in [
        ("<length>", "calc(1in * 2px / 4px)", "48px"),
        ("<number>", "sin(90deg)", "1"),
        ("<number>", "pow(2,3)", "8"),
        ("<length>", "round(line-width, .1px)", "1px"),
        ("<length>", "round(line-width, 2.9px)", "2px"),
        ("<length>", "round(line-width, .1px, 2px)", "2px"),
        ("<length>", "mod(-18px,5px)", "2px"),
        (
            "<length-percentage>",
            "clamp(none, 10%, 2in)",
            "clamp(none, 10%, 192px)",
        ),
        (
            "<length-percentage>",
            "round(up, 50%, 10px)",
            "round(up, 50%, 10px)",
        ),
        (
            "<image>",
            "linear-gradient(to right in srgb, red 1in, blue)",
            "linear-gradient(to right in srgb, rgb(255, 0, 0) 96px, rgb(0, 0, 255))",
        ),
        (
            "<image>",
            "radial-gradient(circle 1in at left top, red, blue)",
            "radial-gradient(circle 96px at left top, rgb(255, 0, 0), rgb(0, 0, 255))",
        ),
        (
            "<image>",
            "conic-gradient(from .5turn, red 0, blue 100%)",
            "conic-gradient(from 180deg, rgb(255, 0, 0) 0deg, rgb(0, 0, 255) 100%)",
        ),
        (
            "<image>",
            "cross-fade(red 20%, blue)",
            "cross-fade(rgb(255, 0, 0) 20%, rgb(0, 0, 255) 80%)",
        ),
        ("<image>", "image(red)", "image(rgb(255, 0, 0))"),
        (
            "<image>",
            "-webkit-image-set('a' 2x)",
            "image-set(url(\"a\") 2dppx)",
        ),
        ("<color>", "rgb(calc(100 + 20) 0 0)", "rgb(120, 0, 0)"),
        ("<transform-function>", "scale(50%, 2)", "scale(0.5, 2)"),
    ] {
        assert_eq!(
            compute(syntax, value).as_deref(),
            Some(expected),
            "{syntax}: {value}"
        );
    }
    for (syntax, value) in [
        ("<image>", "radial-gradient(banana, red, blue)"),
        ("<image>", "radial-gradient(circle 10px 20px, red, blue)"),
        ("<image>", "radial-gradient(ellipse 10px, red, blue)"),
        ("<image>", "linear-gradient(to right left, red, blue)"),
        ("<image>", "linear-gradient(in srgb longer hue, red, blue)"),
        ("<image>", "conic-gradient(from 1px, red, blue)"),
        ("<image>", "linear-gradient(red, 20%, 40%, blue)"),
        ("<image>", "linear-gradient(red, blue, 20%)"),
        ("<image>", "image-set(image-set('a' 1x) 2x)"),
        ("<image>", "cross-fade(red 101%, blue)"),
        ("<image>", "image(red, blue)"),
        ("<url>", "url('x' 2px)"),
        ("<transform-function>", "translate(1px 2px)"),
        ("<transform-function>", "matrix(1,2,3,4,5)"),
        ("<length>", "round(line-width, 2)"),
    ] {
        assert!(compute(syntax, value).is_none(), "{syntax}: {value}");
    }
}

#[test]
fn registered_cycle_results_are_read_order_independent_and_mutable() {
    for unit in ["em", "rem", "lh", "rlh"] {
        for font_first in [false, true] {
            let html = format!(
                "<style>@property --x {{syntax:'<length>';initial-value:3px;inherits:false}} html {{--x:2{unit};font-size:var(--x);line-height:var(--x)}} </style><p id=c></p>"
            );
            let mut dom = Dom::parse_document(&html);
            let root = dom.document_element().unwrap();
            if font_first {
                assert_eq!(dom.font_px(root), FONT_SIZE_INITIAL, "{unit}");
            }
            assert_eq!(
                dom.custom_prop(root, "--x").as_deref(),
                Some("3px"),
                "{unit} first={font_first}"
            );
            assert_eq!(dom.font_px(root), FONT_SIZE_INITIAL, "{unit}");
            dom.set_attr(root, "style", "font-size:10px;line-height:20px;--x:2em");
            assert_eq!(
                dom.custom_prop(root, "--x").as_deref(),
                Some("20px"),
                "mutated {unit}"
            );
        }
    }
    let mut html = String::from("<div id=c style='--n0:1px;");
    for n in 1..40 {
        html.push_str(&format!("--n{n}:var(--n{},var(--n{}));", n - 1, n - 1));
    }
    html.push_str("width:var(--n39)'></div>");
    let dom = Dom::parse_document(&html);
    let c = dom.get_by_id("c").unwrap();
    assert_eq!(dom.custom_prop(c, "--n39").as_deref(), Some("1px"));
}

#[test]
fn registered_urls_keep_the_winning_stylesheet_base() {
    let mut dom = Dom::parse_document(
        r#"<link id=a rel=stylesheet href='https://example.test/a/style.css'>
        <link id=b rel=stylesheet href='https://example.test/b/style.css'><div id=c></div>"#,
    );
    dom.set_doc_url(Some(
        url::Url::parse("https://example.test/page/index.html").unwrap(),
    ));
    let a = dom.get_by_id("a").unwrap();
    let b = dom.get_by_id("b").unwrap();
    let c = dom.get_by_id("c").unwrap();
    dom.set_cssom_sheet(a,"@property --a {syntax:'<url>';inherits:false;initial-value:url(init.png)} #c {--a:url(image.png)}".into(),"".into(),false);
    dom.set_cssom_sheet(b,"@property --b {syntax:'<url>';inherits:false;initial-value:url(init.png)} #c {--b:var(--a)}".into(),"".into(),false);
    assert_eq!(
        dom.custom_prop(c, "--a").as_deref(),
        Some("url(\"https://example.test/a/image.png\")")
    );
    assert_eq!(
        dom.custom_prop(c, "--b").as_deref(),
        Some("url(\"https://example.test/a/image.png\")")
    );
    dom.set_attr(c, "style", "--a:initial;--b:url(inline.png)");
    assert_eq!(
        dom.custom_prop(c, "--a").as_deref(),
        Some("url(\"https://example.test/a/init.png\")")
    );
    assert_eq!(
        dom.custom_prop(c, "--b").as_deref(),
        Some("url(\"https://example.test/page/inline.png\")")
    );
    dom.register_property(DOCUMENT, "--image", "<image>", false, None, None)
        .unwrap();
    dom.set_attr(c, "style", "--image:image-set('' 1x, '#local' 2x)");
    assert_eq!(
        dom.custom_prop(c, "--image").as_deref(),
        Some("image-set(url(\"\") 1dppx, url(\"#local\") 2dppx)")
    );
}

#[test]
fn registered_names_are_css_identifiers_and_custom_values_remain_case_sensitive() {
    let dom = Dom::parse_document(
        r#"<style>@\70 roperty --a\:b {syntax:'<number>';inherits:false;initial-value:2} #c {--a\:b:3}</style><div id=c style='width:calc(var(--a\:b) * 1px)'></div>"#,
    );
    let c = dom.get_by_id("c").unwrap();
    assert_eq!(dom.custom_prop(c, "--a:b").as_deref(), Some("3"));
    assert_eq!(
        dom.computed_value_resolved(c, "width").as_deref(),
        Some("calc(3 * 1px)")
    );
}

#[test]
fn registered_frames_have_separate_registries_and_reset_on_navigation() {
    let mut dom = Dom::parse_document(
        "<style>@property --x {syntax:'<number>';inherits:false;initial-value:1}</style><iframe id=f></iframe>",
    );
    let frame = dom.get_by_id("f").unwrap();
    dom.install_frame_document(frame,"<style>@property --x {syntax:'<number>';inherits:false;initial-value:2}</style><p id=child></p>","https://frame.test/").unwrap();
    let child = dom.get_by_id("child").unwrap();
    assert_eq!(dom.custom_prop(frame, "--x").as_deref(), Some("1"));
    assert_eq!(dom.custom_prop(child, "--x").as_deref(), Some("2"));
    dom.register_property(frame, "--x", "<number>", false, Some("3".into()), None)
        .unwrap();
    dom.set_adopted_sheets(
        frame,
        vec![(
            "@property --adopted {initial-value:old-document}".into(),
            None,
        )],
    );
    assert_eq!(dom.custom_prop(frame, "--x").as_deref(), Some("1"));
    assert_eq!(dom.custom_prop(child, "--x").as_deref(), Some("3"));
    assert_eq!(
        dom.custom_prop(child, "--adopted").as_deref(),
        Some("old-document")
    );
    dom.install_frame_document(frame, "<p id=next></p>", "https://frame.test/next")
        .unwrap();
    assert_eq!(dom.custom_prop(dom.get_by_id("next").unwrap(), "--x"), None);
    assert_eq!(
        dom.custom_prop(dom.get_by_id("next").unwrap(), "--adopted"),
        None
    );
}

#[test]
fn registered_numeric_ranges_preserve_valid_calculations() {
    for (syntax, value, expected) in [
        ("<number>", "calc(NaN)", "0"),
        ("<length>", "calc(0px / 0)", "0px"),
        ("<number>", "calc(1 / infinity)", "0"),
        (
            "<number>",
            "calc(1 / calc(1 / -infinity))",
            "-340282350000000000000000000000000000000",
        ),
        (
            "<number>",
            "calc(3e38 * 2)",
            "340282350000000000000000000000000000000",
        ),
        ("<resolution>", "calc(-1dppx)", "0dppx"),
        ("<url>", "src('')", "src(\"\")"),
        ("<url>", "url()", "url(\"\")"),
        (
            "<transform-function>",
            "perspective(calc(-1px))",
            "perspective(0px)",
        ),
        (
            "<image>",
            "cross-fade(red calc(110%), blue)",
            "cross-fade(rgb(255, 0, 0) 100%, rgb(0, 0, 255) 0%)",
        ),
        (
            "<image>",
            "radial-gradient(circle calc(-1px), red)",
            "radial-gradient(circle 0px, rgb(255, 0, 0))",
        ),
        (
            "<image>",
            "radial-gradient(circle 20%, red)",
            "radial-gradient(circle 20%, rgb(255, 0, 0))",
        ),
        (
            "<image>",
            "radial-gradient(closest-side farthest-side, red)",
            "radial-gradient(closest-side farthest-side, rgb(255, 0, 0))",
        ),
    ] {
        assert_eq!(
            compute(syntax, value).as_deref(),
            Some(expected),
            "{syntax}: {value}"
        );
    }
    for (syntax, value) in [
        ("<resolution>", "-1dppx"),
        ("<transform-function>", "perspective(-1px)"),
        ("<image>", "radial-gradient(circle -1px, red)"),
        (
            "<image>",
            "radial-gradient(circle closest-side farthest-side, red)",
        ),
    ] {
        assert_eq!(compute(syntax, value), None, "{syntax}: {value}");
    }
}

#[test]
fn registered_rules_keep_strings_and_global_conditional_definitions() {
    let dom = Dom::parse_document(
        r#"<style>
        @property --string {syntax:'<string>';initial-value:'/*literal*/';inherits:false}
        @container (width > 90000px) {@property --container {initial-value:container}}
        @scope (.missing) {@property --scope {initial-value:scope}}
        @media print {@property --media {initial-value:print}}
        @supports (display:impossible) {@property --supports {initial-value:impossible}}
        </style><div id=c></div>"#,
    );
    let c = dom.get_by_id("c").unwrap();
    for (name, expected) in [
        ("--string", Some("\"/*literal*/\"")),
        ("--container", Some("container")),
        ("--scope", Some("scope")),
        ("--media", None),
        ("--supports", None),
    ] {
        assert_eq!(dom.custom_prop(c, name).as_deref(), expected, "{name}");
    }
}

#[test]
fn registered_css_wide_fallbacks_obey_the_registration_inherit_flag() {
    let dom = Dom::parse_document(
        r#"<style>
        @property --yes {syntax:'<number>';inherits:true;initial-value:2}
        @property --no {syntax:'<number>';inherits:false;initial-value:3}
        #p {--yes:7;--no:8}
        #initial {--yes:var(--missing,initial);--no:var(--missing,initial)}
        #inherit {--yes:var(--missing,inherit);--no:var(--missing,inherit)}
        #unset {--yes:var(--missing,unset);--no:var(--missing,unset)}
        #invalid {--yes:red;--no:red}
        </style><div id=p><div id=initial></div><div id=inherit></div><div id=unset></div><div id=invalid></div></div>"#,
    );
    for (id, yes, no) in [
        ("initial", "2", "3"),
        ("inherit", "7", "8"),
        ("unset", "7", "3"),
        ("invalid", "7", "3"),
    ] {
        let node = dom.get_by_id(id).unwrap();
        assert_eq!(dom.custom_prop(node, "--yes").as_deref(), Some(yes), "{id}");
        assert_eq!(dom.custom_prop(node, "--no").as_deref(), Some(no), "{id}");
    }
    assert_eq!(
        compute("<color>", "rgb(none 0 0 / none)").as_deref(),
        Some("color(srgb none 0 0 / none)")
    );
    assert_eq!(
        compute("<color>", "hsl(none 100% 50%)").as_deref(),
        Some("hsl(none 100% 50%)")
    );
    assert_eq!(compute("<color>", "rgb(10%, 0, 0)"), None);
    assert_eq!(compute("<color>", "rgb(none, 0, 0)"), None);
}
