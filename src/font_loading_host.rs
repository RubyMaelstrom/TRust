//! CSS Font Loading 3 #font-face-load and #font-face-set-load, with CSS Fonts
//! 4 #font-style-matching / #src-desc (CSSWG snapshot 81c27f686901).
//! Script owns FontFace identity and promises; the host owns loading, decoding,
//! descriptor matching and installing the resulting fonts in their document.

use super::*;
use parley::fontique::{Descriptor, FontStyle, FontWeight, FontWidth};
use unicode_casefold::UnicodeCaseFold as _;

pub(super) fn query(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let op = host_arg_string(ctx, args, 0);
    if op == "epoch" {
        return Ok(Value::from_string(
            host_dom(ctx).borrow().font_source_epoch().to_string(),
        ));
    }
    if op == "scopes" || op == "pending" || op == "needed" {
        if ctx
            .host::<HostState>()
            .is_some_and(|state| state.worker_self.is_some())
        {
            return scopes(ctx, _this, args);
        }
        return guard_node_arguments(
            ctx,
            _this,
            args,
            ("font source scopes", &[1], NodeDenial::Throw),
            scopes,
        );
    }
    if op == "buffer" {
        let bytes = args
            .get(1)
            .and_then(|value| ctx.buffer_source_bytes(value, false))
            .ok_or_else(|| {
                ctx.make_error(
                    "TypeError",
                    "FontFace source is not an available non-shared BufferSource",
                )
            })?;
        return ctx.make_array_buffer(&bytes);
    }
    let text = host_arg_string(ctx, args, 1);
    let extra = host_arg_string(ctx, args, 2);
    let result = if op == "match" {
        let sample = host_arg_string(ctx, args, 3);
        let faces = serde_json::from_str::<Vec<String>>(&extra).unwrap_or_default();
        serde_json::json!(matching_faces(&text, &sample, &faces))
    } else {
        serde_json::json!(crate::dom::font_face_descriptor(&text, &extra))
    };
    Ok(Value::from_string(result.to_string()))
}

fn scopes(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let op = host_arg_string(ctx, args, 0);
    if op == "needed"
        && ctx
            .host::<HostState>()
            .is_none_or(|state| state.worker_self.is_none())
    {
        // CSS Font Loading #font-face-set-ready waits for layout, including
        // newly discovered fallback faces after another font has loaded.
        // Readiness needs current font/layout inputs, not the visual bounds
        // of compositor-only transforms. Ordinary rendering opportunities
        // also call this discovery path; forcing CSSOM geometry here would
        // undo compositor retention on every animation frame.
        ensure_host_geometry_for(ctx, "font loading readiness", None, false);
    }
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    let document = if ctx
        .host::<HostState>()
        .is_some_and(|state| state.worker_self.is_some())
    {
        Some(dom.document())
    } else {
        host_arg_node(&dom, &args[1..], 0)
    };
    if op == "needed" {
        let needed = document
            .map(|document| dom.needed_font_members(document))
            .unwrap_or_default();
        return Ok(Value::from_string(serde_json::to_string(&needed).unwrap()));
    }
    if op == "pending" {
        let pending = ctx.host::<HostState>().is_some_and(|state| {
            document
                .and_then(|document| node_window_context(state, &dom, document))
                .is_some_and(|context| {
                    state
                        .pending_stylesheets
                        .get(&context)
                        .copied()
                        .unwrap_or(0)
                        != 0
                })
        });
        return Ok(Value::from_string(pending.to_string()));
    }
    let sources = document
        .map(|document| dom.font_source_scopes(document))
        .unwrap_or_default();
    Ok(Value::from_string(serde_json::to_string(&sources).unwrap()))
}

fn matching_faces(font: &str, text: &str, declarations: &[String]) -> Option<Vec<usize>> {
    use crate::dom::cssom::operation;
    if matches!(
        font.trim().to_ascii_lowercase().as_str(),
        "initial" | "inherit" | "unset" | "revert" | "revert-layer"
    ) || operation("supports", "font", font) != serde_json::Value::Bool(true)
    {
        return None;
    }
    let expanded = operation("expand", "font", font);
    let parts = expanded.as_array()?;
    let get = |name| {
        parts
            .iter()
            .find(|pair| pair[0].as_str() == Some(name))
            .and_then(|pair| pair[1].as_str())
    };
    let family = get("font-family")?;
    let families = svgtypes::parse_font_families(family).ok()?;
    let mut requested = crate::font_system::FaceDescriptors::default();
    requested.apply(
        "font-stretch",
        get("font-stretch")
            .or_else(|| get("font-width"))
            .unwrap_or("normal"),
    );
    let width = match requested.width {
        Descriptor::Value([value, _]) => value,
        _ => 1.,
    };
    let weight = match get("font-weight").unwrap_or("normal") {
        "bolder" => 700.,
        "lighter" => 100.,
        value => crate::layout2::css_font_weight(value).unwrap_or(400.),
    };
    let style = get("font-style").unwrap_or("normal");
    let style = if style == "italic" {
        FontStyle::Italic
    } else if style.starts_with("oblique") {
        FontStyle::Oblique(
            style
                .split_whitespace()
                .nth(1)
                .and_then(crate::text::typography::parse_angle_deg),
        )
    } else {
        FontStyle::Normal
    };
    let faces: Vec<_> = declarations
        .iter()
        .map(|text| crate::http::font_face_descriptors(text))
        .collect();
    let mut matched = Vec::new();
    for family in families {
        // Generic keywords select system families, not downloadable faces
        // whose quoted family happens to have that spelling.
        let svgtypes::FontFamily::Named(family) = family else {
            continue;
        };
        let folded = family.case_fold().collect::<String>();
        let candidates: Vec<_> = faces
            .iter()
            .enumerate()
            .filter_map(|(index, face)| {
                let face = face.as_ref()?;
                (face.family.case_fold().collect::<String>() == folded).then_some((index, face))
            })
            .collect();
        let descriptors: Vec<_> = candidates
            .iter()
            .map(|(_, face)| face.descriptors.matching())
            .collect();
        for which in parley::fontique::match_face_descriptors(
            &descriptors,
            FontWidth::from_ratio(width),
            style,
            FontWeight::new(weight),
        ) {
            let (index, face) = candidates[which];
            if !matched.contains(&index)
                && text.chars().any(|character| {
                    face.unicode_range.is_empty()
                        || face
                            .unicode_range
                            .iter()
                            .any(|&(start, end)| (start..=end).contains(&(character as u32)))
                })
            {
                matched.push(index);
            }
        }
    }
    Some(matched)
}

/// `load(id, declarations, baseURL, bytes?)`. Binary/data/local sources can
/// finish synchronously here, but script always settles them on a font task.
/// Network sources report through FontFaceDone in the requesting realm.
pub(super) fn load(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let request = args.first().and_then(Value::as_num_opt).unwrap_or(0.) as u64;
    let declarations = host_arg_string(ctx, args, 1);
    let source = host_arg_string(ctx, args, 2);
    let options = serde_json::from_str::<serde_json::Value>(&source).ok();
    let base = options
        .as_ref()
        .and_then(|v| v["base"].as_str())
        .unwrap_or(&source);
    let css = options
        .as_ref()
        .and_then(|v| v["css"].as_bool())
        .unwrap_or(false);
    let bytes = args
        .get(3)
        .and_then(|value| ctx.buffer_source_bytes(value, false));
    let Some(face) = crate::http::font_face_descriptors(&declarations) else {
        return Ok(Value::Bool(false));
    };
    if let Some(bytes) = bytes {
        let font = crate::font_system::PageFont::from_web_resource(face.family, bytes)
            .and_then(|font| font.select_face(None))
            .filter(|font| ttf_parser::Face::parse(&font.bytes, 0).is_ok());
        return match font {
            Some(font) => ctx.make_array_buffer(&font.bytes),
            None => Ok(Value::Bool(false)),
        };
    }
    // No network/thread is needed for data URLs or installed local faces.
    let mut only_local = true;
    for source in &face.sources {
        let font = match source {
            crate::http::CssFontSource::Local(name) => {
                crate::font_system::PageFont::from_local(face.family.clone(), name)
            }
            crate::http::CssFontSource::Url(url) if url.starts_with("data:") => {
                crate::img::decode_data_url(url).and_then(|bytes| {
                    crate::font_system::PageFont::from_web_resource(face.family.clone(), bytes)
                })
            }
            _ => {
                only_local = false;
                break;
            }
        };
        if let Some(font) = font.filter(|font| ttf_parser::Face::parse(&font.bytes, 0).is_ok()) {
            return ctx.make_array_buffer(&font.bytes);
        }
    }
    if only_local {
        return Ok(Value::Bool(false));
    }
    let context = ctx.host_job_context();
    let document = {
        let state = ctx.host::<HostState>().expect("font host");
        window_document(state, &state.dom.borrow(), context)
            .unwrap_or_else(|| state.dom.borrow().document())
    };
    let client_url = request_client_url(ctx);
    // HTML #set-up-a-worker-environment-settings-object separates the API
    // base URL from the inherited origin (notably for blob: workers).
    // Fetch #concept-request-origin uses that origin for CORS/credentials.
    let client = request_origin_for_context(
        ctx.host_mut::<HostState>().expect("font host"),
        context,
        &client_url,
    );
    let cookies = request_cookie_context(ctx);
    let environment = ModuleFetchEnvironment::of(ctx, context);
    let base = url::Url::parse(base).unwrap_or_else(|_| client_url.clone());
    let Some((handle, cache, events, traversable, referrer)) =
        ctx.host_mut::<HostState>().and_then(|state| {
            let policy = environment_referrer_policy(state, context);
            let referrer = css
                .then(|| {
                    state
                        .dom
                        .borrow()
                        .font_sheet_referrer(document, &base, policy)
                })
                .flatten()
                .unwrap_or_else(|| crate::http::SheetReferrer {
                    source: environment_referrer_source(state, context),
                    policy,
                    document_policy: policy,
                    service_worker: None,
                })
                .with_service_worker(service_worker_host::fetch::for_context(state, context));
            let network = state.network.as_ref()?;
            let events = state.task_events.clone()?;
            let result = (
                network.handle.clone(),
                network.cache.clone(),
                events,
                state.traversables.selected,
                referrer,
            );
            state.pending_fonts += 1;
            Some(result)
        })
    else {
        return Ok(Value::Bool(false));
    };
    let loading_cache = cache.clone();
    cache.spawn(&handle, async move {
        let loaded = crate::http::load_font_face(
            face,
            &base,
            &client,
            referrer,
            Some(&loading_cache),
            &cookies,
            &environment.blobs,
            &environment.storage_key,
        )
        .await;
        let (source, bytes) = match loaded {
            Some((source, font)) => (source, Some(font.bytes)),
            None => (None, None),
        };
        let _ = events.send(TrustJsHostTask::FontFaceDone {
            traversable,
            context,
            document,
            request,
            source,
            bytes,
        });
    });
    Ok(Value::Null)
}

/// Register all font-source members before layout, including unloaded faces.
/// Their member identities let shaping request the precise FontFace object.
pub(super) fn install(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    #[derive(serde::Deserialize)]
    struct Registration {
        member: u64,
        scope: usize,
        css: String,
        base: String,
        connected: bool,
        status: String,
    }
    let dom = host_dom(ctx);
    let Some(document) = host_arg_node(&dom.borrow(), args, 0) else {
        return Ok(Value::Undefined);
    };
    let declarations = serde_json::from_str::<Vec<Registration>>(&host_arg_string(ctx, args, 1))
        .unwrap_or_default();
    let buffers = args.get(2).cloned().unwrap_or(Value::Undefined);
    let mut fonts = Vec::new();
    for (index, registration) in declarations.iter().enumerate() {
        if registration.scope != document
            && dom.borrow().owner_document(registration.scope) != Some(document)
        {
            continue;
        }
        let Some(mut face) = crate::http::font_face_descriptors(&registration.css) else {
            continue;
        };
        if let Ok(base) = url::Url::parse(&registration.base) {
            for source in &mut face.sources {
                if let crate::http::CssFontSource::Url(source) = source
                    && let Ok(resolved) = base.join(source)
                {
                    *source = resolved.to_string();
                }
            }
        }
        let value = ctx.member_get(&buffers, &index.to_string())?;
        let bytes = if registration.status == "loaded" {
            ctx.buffer_source_bytes(&value, false)
                .map(std::sync::Arc::from)
                .or_else(|| {
                    dom.borrow()
                        .font_registration_data(document, registration.member)
                })
        } else {
            None
        };
        fonts.push(crate::font_system::FontRegistration {
            key: crate::font_system::FontKey {
                scope: registration.scope,
                member: registration.member,
                script: true,
            },
            face,
            css: registration.connected,
            failed: registration.status == "error",
            data: bytes,
        });
    }
    dom.borrow_mut().set_script_fonts(document, fonts);
    Ok(Value::Undefined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_face_matching_uses_style_order_ranges_and_sample_text() {
        let face = |family, weight, width, style, range| {
            format!(
                "font-family:'{family}';src:url(font.woff2);font-weight:{weight};font-stretch:{width};font-style:{style};unicode-range:{range}"
            )
        };
        let faces = [
            face("Example", "400", "normal", "normal", "U+0-7F"),
            face("Example", "400", "normal", "normal", "U+400-4FF"),
            face("Example", "600 800", "normal", "italic", "U+0-10FFFF"),
            face("Other", "400", "normal", "normal", "U+0-10FFFF"),
        ];
        assert_eq!(
            matching_faces("12px Example", "aЖ", &faces),
            Some(vec![0, 1])
        );
        assert_eq!(matching_faces("12px Example", "a", &faces), Some(vec![0]));
        assert_eq!(
            matching_faces("italic 700 12px Example", "a", &faces),
            Some(vec![2])
        );
        assert_eq!(
            matching_faces("12px Example, Other", "a", &faces),
            Some(vec![0, 3])
        );
        assert_eq!(matching_faces("12px Missing", "a", &faces), Some(vec![]));
        assert_eq!(matching_faces("12px Example", "", &faces), Some(vec![]));
        for invalid in ["inherit", "initial", "var(--font)", "12", "bold Example"] {
            assert_eq!(matching_faces(invalid, "a", &faces), None, "{invalid}");
        }
    }
}
