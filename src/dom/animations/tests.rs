use super::*;

fn setup(css: &str, body: &str) -> Dom {
    let mut dom = Dom::parse_document(&format!(
        "<!doctype html><style>body{{margin:0}}{css}</style>{body}"
    ));
    dom.set_viewport_px(800., 600.);
    dom
}

fn value(dom: &Dom, id: &str, property: &str) -> Option<String> {
    dom.computed_value_resolved(dom.get_by_id(id).unwrap(), property)
}

#[test]
fn color_animation_origin_reaches_computed_values_and_inheritance() {
    let mut dom = setup(
        "#a{color:black;animation:fade 2s linear}
         @keyframes fade{from{color:rgb(0, 0, 0)}to{color:rgb(200, 100, 0)}}",
        "<div id=a>a<span id=b>b</span><p id=c style='color:green'>c</p></div>",
    );
    // Warm the caches with the base values first.
    assert_eq!(value(&dom, "b", "color").as_deref(), Some("black"));
    dom.update_css_animations(0.);
    assert_eq!(value(&dom, "a", "color").as_deref(), Some("rgb(0, 0, 0)"));
    dom.update_css_animations(1.);
    assert_eq!(
        value(&dom, "a", "color").as_deref(),
        Some("rgb(100, 50, 0)")
    );
    // CSS Cascade 5 #inheriting: the animated computed value inherits.
    assert_eq!(
        value(&dom, "b", "color").as_deref(),
        Some("rgb(100, 50, 0)")
    );
    assert_eq!(value(&dom, "c", "color").as_deref(), Some("green"));
    dom.update_css_animations(1.5);
    assert_eq!(
        value(&dom, "b", "color").as_deref(),
        Some("rgb(150, 75, 0)")
    );
    // Paint-only: no relayout was requested.
    assert_eq!(dom.take_css_animation_updates(), (true, false));
    // fill-mode none: after the active interval the base value returns.
    dom.update_css_animations(2.5);
    assert_eq!(value(&dom, "a", "color").as_deref(), Some("black"));
    assert_eq!(value(&dom, "b", "color").as_deref(), Some("black"));
}

#[test]
fn animated_rows_are_not_shared_with_equal_cascades() {
    // Two siblings with one cascade, started at different times: their
    // descendants inherit different animated colors.
    let mut dom = setup(
        ".x{animation:c 1s linear infinite}
         @keyframes c{from{color:rgb(0, 0, 0)}to{color:rgb(200, 0, 0)}}",
        "<div class=x id=a><i id=ai>a</i></div><div id=b><i id=bi>b</i></div>",
    );
    dom.update_css_animations(0.);
    let b = dom.get_by_id("b").unwrap();
    dom.set_attr(b, "class", "x");
    dom.update_css_animations(0.25);
    assert_eq!(value(&dom, "ai", "color").as_deref(), Some("rgb(50, 0, 0)"));
    assert_eq!(value(&dom, "bi", "color").as_deref(), Some("rgb(0, 0, 0)"));
    dom.update_css_animations(0.5);
    assert_eq!(
        value(&dom, "ai", "color").as_deref(),
        Some("rgb(100, 0, 0)")
    );
    assert_eq!(value(&dom, "bi", "color").as_deref(), Some("rgb(50, 0, 0)"));
}

#[test]
fn important_declarations_outrank_animations() {
    // CSS Cascade 5 #cascade-origin.
    let mut dom = setup(
        "#a{color:blue!important;background-color:red;animation:c 1s linear}
         #b{animation:c 1s linear}
         @keyframes c{from{color:rgb(0, 0, 0);background-color:rgb(0, 0, 0)}
                      to{color:rgb(200, 0, 0);background-color:rgb(200, 0, 0)}}",
        "<div id=a>a</div><div id=b style='background-color:green !important'>b</div>",
    );
    dom.update_css_animations(0.);
    dom.update_css_animations(0.5);
    assert_eq!(value(&dom, "a", "color").as_deref(), Some("blue"));
    assert_eq!(
        value(&dom, "a", "background-color").as_deref(),
        Some("rgb(100, 0, 0)")
    );
    assert_eq!(value(&dom, "b", "color").as_deref(), Some("rgb(100, 0, 0)"));
    assert_eq!(
        value(&dom, "b", "background-color").as_deref(),
        Some("green")
    );
}

#[test]
fn missing_keyframes_use_underlying_values_and_fill_modes_apply() {
    // CSS Animations 1 #keyframes: an absent 0%/100% keyframe takes the
    // computed value; #animation-fill-mode before/after the active interval.
    let mut dom = setup(
        "#a{background-color:rgb(0, 0, 200);animation:to-red 2s linear 1s both}
         #b{background-color:rgb(0, 0, 200);animation:to-red 2s linear 1s}
         @keyframes to-red{to{background-color:rgb(200, 0, 0)}}",
        "<div id=a></div><div id=b></div>",
    );
    dom.update_css_animations(0.);
    // Delay phase: backwards fill applies the (implicit) 0% keyframe.
    assert_eq!(
        value(&dom, "a", "background-color").as_deref(),
        Some("rgb(0, 0, 200)")
    );
    assert_eq!(
        value(&dom, "b", "background-color").as_deref(),
        Some("rgb(0, 0, 200)")
    );
    dom.update_css_animations(2.);
    assert_eq!(
        value(&dom, "a", "background-color").as_deref(),
        Some("rgb(100, 0, 100)")
    );
    dom.update_css_animations(5.);
    assert_eq!(
        value(&dom, "a", "background-color").as_deref(),
        Some("rgb(200, 0, 0)")
    );
    assert_eq!(
        value(&dom, "b", "background-color").as_deref(),
        Some("rgb(0, 0, 200)")
    );
}

#[test]
fn direction_iterations_easing_and_play_state() {
    let mut dom = setup(
        "#alt{animation:w 1s linear 3 alternate both}
         #steps{animation:w 1s steps(2, end) infinite}
         #paused{animation:w 4s linear -1s paused}
         @keyframes w{from{width:0px}to{width:100px}}",
        "<div id=alt></div><div id=steps></div><div id=paused></div>",
    );
    dom.update_css_animations(0.);
    dom.update_css_animations(1.25);
    // The second iteration plays in reverse.
    assert_eq!(value(&dom, "alt", "width").as_deref(), Some("75px"));
    assert_eq!(value(&dom, "steps", "width").as_deref(), Some("0px"));
    // A paused animation holds its (negative-delay) current time.
    assert_eq!(value(&dom, "paused", "width").as_deref(), Some("25px"));
    dom.update_css_animations(1.75);
    assert_eq!(value(&dom, "steps", "width").as_deref(), Some("50px"));
    assert_eq!(value(&dom, "paused", "width").as_deref(), Some("25px"));
    dom.update_css_animations(9.);
    // Three iterations ending forwards: the third ends at 100%.
    assert_eq!(value(&dom, "alt", "width").as_deref(), Some("100px"));

    // CSS Animations 1 #timing-functions: a keyframe's own timing function
    // eases only the interval it starts.
    let mut dom = setup(
        "#kf{animation:k 1s linear}
         @keyframes k{0%{width:0px;animation-timing-function:steps(1, end)}50%{width:50px}
                      100%{width:100px}}",
        "<div id=kf></div>",
    );
    dom.update_css_animations(10.);
    dom.update_css_animations(10.25);
    assert_eq!(value(&dom, "kf", "width").as_deref(), Some("0px"));
    dom.update_css_animations(10.75);
    assert_eq!(value(&dom, "kf", "width").as_deref(), Some("75px"));
}

#[test]
fn play_state_pauses_and_resumes_the_current_time() {
    let mut dom = setup(
        "#a{animation:w 4s linear}.paused{animation-play-state:paused}
         @keyframes w{from{width:0px}to{width:100px}}",
        "<div id=a></div>",
    );
    let a = dom.get_by_id("a").unwrap();
    dom.update_css_animations(0.);
    dom.update_css_animations(1.);
    dom.set_attr(a, "class", "paused");
    dom.update_css_animations(1.);
    dom.update_css_animations(3.);
    assert_eq!(value(&dom, "a", "width").as_deref(), Some("25px"));
    dom.set_attr(a, "class", "");
    dom.update_css_animations(3.);
    dom.update_css_animations(4.);
    assert_eq!(value(&dom, "a", "width").as_deref(), Some("50px"));
}

#[test]
fn layout_properties_relayout_and_font_size_composes() {
    let mut dom = setup(
        "#a{font-size:10px;animation:grow 1s linear}
         @keyframes grow{from{font-size:10px;letter-spacing:0px}to{font-size:30px;letter-spacing:4px}}
         #b{font-size:2em}",
        "<div id=a><span id=b>x</span></div>",
    );
    dom.update_css_animations(0.);
    let _ = dom.take_css_animation_updates();
    let epoch = dom.layout_presentation_epoch();
    dom.update_css_animations(0.5);
    assert_eq!(dom.take_css_animation_updates(), (false, true));
    assert_ne!(dom.layout_presentation_epoch(), epoch);
    let b = dom.get_by_id("b").unwrap();
    assert_eq!(dom.font_px(b), 40.);
    assert_eq!(value(&dom, "b", "letter-spacing").as_deref(), Some("2px"));

    // CSS Fonts 4 #font-size-prop: `em` keyframes are relative to the
    // parent, and an absent keyframe uses the computed size.
    let mut dom = setup(
        "#p{font-size:10px}#a{font-size:2em;animation:em 1s linear}
         @keyframes em{to{font-size:4em}}",
        "<div id=p><div id=a>a</div></div>",
    );
    let a = dom.get_by_id("a").unwrap();
    dom.update_css_animations(0.);
    dom.update_css_animations(0.5);
    assert_eq!(dom.font_px(a), 30.);
    // A discrete property flips at the midpoint from its underlying
    // (here initial) value (CSS Values 4 #discrete).
    let mut dom = setup(
        "#a{animation:s 1s linear}@keyframes s{to{border-top-style:dashed}}",
        "<div id=a>a</div>",
    );
    dom.update_css_animations(0.);
    dom.update_css_animations(0.25);
    assert_eq!(
        value(&dom, "a", "border-top-style").as_deref(),
        Some("none")
    );
    dom.update_css_animations(0.75);
    assert_eq!(
        value(&dom, "a", "border-top-style").as_deref(),
        Some("dashed")
    );
}

#[test]
fn keyframe_values_resolve_var_currentcolor_and_shorthands() {
    let mut dom = setup(
        "#a{--glow:rgb(0, 0, 200);color:rgb(200, 0, 0);animation:g 1s linear}
         @keyframes g{from{border:2px solid currentcolor;text-shadow:0 0 4px var(--glow)}
                      to{border:2px solid rgb(0, 0, 0);text-shadow:0 0 8px var(--glow)}}",
        "<div id=a>a</div>",
    );
    dom.update_css_animations(0.);
    dom.update_css_animations(0.5);
    assert_eq!(
        value(&dom, "a", "border-top-color").as_deref(),
        Some("rgb(100, 0, 0)")
    );
    assert_eq!(
        value(&dom, "a", "text-shadow").as_deref(),
        Some("rgb(0, 0, 200) 0px 0px 6px")
    );
}

#[test]
fn display_none_cancels_and_events_follow_phases() {
    let mut dom = setup(
        "#a{animation:w 1s linear 0.5s 2}.gone{display:none}
         @keyframes w{to{width:100px}}",
        "<div id=a></div>",
    );
    let a = dom.get_by_id("a").unwrap();
    dom.update_css_animations(0.);
    assert!(dom.take_css_animation_events().is_empty());
    assert_eq!(dom.css_animation_next_change(), Some(0.5));
    dom.update_css_animations(0.6);
    let start = dom.take_css_animation_events();
    assert_eq!(start, vec![(a, "w".into(), "animationstart", 0.)]);
    dom.update_css_animations(1.6);
    let iteration = dom.take_css_animation_events();
    assert_eq!(iteration, vec![(a, "w".into(), "animationiteration", 1.)]);
    dom.set_attr(a, "class", "gone");
    dom.update_css_animations(1.75);
    let cancel = dom.take_css_animation_events();
    assert_eq!(cancel.len(), 1);
    assert_eq!(cancel[0].2, "animationcancel");
    assert!((cancel[0].3 - 1.25).abs() < 1e-9);
    assert!(!dom.css_animations_keep_alive());

    let mut dom = setup(
        "#a{animation:w 1s linear}@keyframes w{to{width:100px}}",
        "<div id=a></div>",
    );
    dom.update_css_animations(0.);
    dom.update_css_animations(3.);
    let events = dom.take_css_animation_events();
    let kinds = events.iter().map(|event| event.2).collect::<Vec<_>>();
    assert_eq!(kinds, ["animationstart", "animationend"]);
    assert_eq!(events[1].3, 1.);
}

#[test]
fn frame_targets_report_running_paint_only_animations() {
    let mut dom = setup(
        "#a{animation:c 1s infinite}#b{animation:w 1s infinite}#c{animation:c 1s infinite paused}
         #d{animation:c 1s 5s}
         @keyframes c{to{color:red}}@keyframes w{to{width:10px}}",
        "<p id=a>a</p><p id=b>b</p><p id=c>c</p><p id=d>d</p>",
    );
    dom.update_css_animations(0.);
    let mut targets = dom.css_animation_frame_targets();
    targets.sort_by_key(|target| target.node);
    let a = dom.get_by_id("a").unwrap();
    let b = dom.get_by_id("b").unwrap();
    assert_eq!(
        targets,
        vec![
            FrameTarget {
                node: a,
                paint_only: true
            },
            FrameTarget {
                node: b,
                paint_only: false
            }
        ]
    );
    // The delayed animation needs a wake when it starts, not frames now.
    assert_eq!(dom.css_animation_next_change(), Some(5.));
    assert!(dom.css_animations_keep_alive());
}

#[test]
fn unanimated_documents_do_no_animation_work() {
    let mut dom = setup("p{color:red}", "<p id=a>a</p>");
    dom.update_css_animations(0.);
    dom.update_css_animations(1.);
    assert!(dom.animations.elements.is_empty());
    assert!(dom.animations.values.is_empty());
    assert!(!dom.animations.keyframes.get());
    let a = dom.get_by_id("a").unwrap();
    dom.set_attr(a, "class", "x");
    dom.flush_style_invalidations();
    assert!(dom.animations.invalid.borrow().is_empty());
    assert!(!dom.css_animations_declared());
}

#[test]
fn inherited_colors_relayout_subtrees_that_rasterize_inline_svg() {
    // Inline SVG resolves `currentColor` when it is rasterized during box
    // construction, so an inherited color change below it is not paint-only.
    let mut dom = setup(
        "#a{animation:c 1s linear infinite}#b{animation:c 1s linear infinite}
         @keyframes c{from{color:rgb(0, 0, 0)}to{color:rgb(200, 0, 0)}}",
        "<div id=a><svg><circle r=4 fill=currentColor /></svg></div><div id=b><p id=p>p</p></div>",
    );
    dom.update_css_animations(0.);
    let _ = dom.take_css_animation_updates();
    dom.update_css_animations(0.25);
    // `a` relayouts; `b` only repaints.
    assert_eq!(dom.take_css_animation_updates(), (true, true));
    assert_eq!(dom.animations.last_layout_changes, 1);
    assert_eq!(dom.animations.last_paint_changes, 1);
    let p = dom.get_by_id("p").unwrap();
    let svg = dom.create_element_ns("http://www.w3.org/2000/svg", None, "svg");
    dom.append(p, svg);
    dom.update_css_animations(0.5);
    assert_eq!(dom.animations.last_layout_changes, 2);
    assert_eq!(dom.animations.last_paint_changes, 0);
}
