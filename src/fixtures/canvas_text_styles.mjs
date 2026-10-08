// HTML #canvastextdrawingstyles and #text-preparation-algorithm (local WHATWG
// e5071a20): spacing, kerning, stretch, capitals, rendering, language and the
// "inherit" direction, measured through TextMetrics.
(function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    const close = (a, b, epsilon = 0.1) => Math.abs(a - b) < epsilon;
    const canvas = document.createElement("canvas"); canvas.width = 300; canvas.height = 60;
    const ctx = canvas.getContext("2d");
    check(ctx.letterSpacing === "0px" && ctx.wordSpacing === "0px" && ctx.fontKerning === "auto"
        && ctx.fontStretch === "normal" && ctx.fontVariantCaps === "normal"
        && ctx.textRendering === "auto" && ctx.lang === "inherit", "defaults");
    for (const name of ["letterSpacing", "wordSpacing", "fontKerning", "fontStretch",
        "fontVariantCaps", "textRendering", "lang"]) {
        const descriptor = Object.getOwnPropertyDescriptor(CanvasRenderingContext2D.prototype, name);
        check(descriptor && descriptor.enumerable && descriptor.get.name === "get " + name, "attribute " + name);
    }
    // letterSpacing: a CSS <length>, applied after every character.
    ctx.font = "10px sans-serif";
    const base = ctx.measureText("Hello World").width;
    for (const [value, serialized, delta] of [["3px", "3px", 33], ["-2px", "-2px", -22],
        ["1em", "1em", 110], ["0.5EM", "0.5em", 55], ["1in", "1in", 1056], ["-0.1cm", "-0.1cm", -41.57]]) {
        ctx.letterSpacing = value;
        check(ctx.letterSpacing === serialized, "letterSpacing serialization " + value + ": " + ctx.letterSpacing);
        const width = ctx.measureText("Hello World").width;
        check(close(width, base + delta, 0.2), "letterSpacing " + value + ": " + (width - base));
    }
    for (const invalid of ["", "1", "10%", "inherit", "1px 2px", "px", "1zz"]) {
        ctx.letterSpacing = "2px"; ctx.letterSpacing = invalid;
        check(ctx.letterSpacing === "2px", "ignored letterSpacing " + invalid);
    }
    ctx.letterSpacing = "0";
    check(ctx.letterSpacing === "0px", "unitless zero");
    // Font-relative units follow the font in effect when text is prepared.
    ctx.letterSpacing = "1em"; ctx.font = "20px sans-serif";
    const spaced = ctx.measureText("ab").width;
    ctx.letterSpacing = "0px";
    check(close(spaced - ctx.measureText("ab").width, 40), "em follows the current font");
    // wordSpacing: added to each word separator.
    ctx.font = "10px sans-serif";
    const words = ctx.measureText("a b c").width;
    ctx.wordSpacing = "5px";
    check(ctx.wordSpacing === "5px" && close(ctx.measureText("a b c").width - words, 10), "wordSpacing");
    ctx.wordSpacing = "1%";
    check(ctx.wordSpacing === "5px", "percentages are not lengths");
    ctx.save(); ctx.wordSpacing = "0px"; ctx.letterSpacing = "3px"; ctx.restore();
    check(ctx.wordSpacing === "5px" && ctx.letterSpacing === "0px", "spacing is drawing state");
    ctx.wordSpacing = "0px";
    // Enumerated attributes ignore other values, including other cases.
    for (const [name, value, rejected] of [["fontKerning", "none", "NONE"], ["fontStretch", "condensed", "50%"],
        ["fontVariantCaps", "all-small-caps", "Small-Caps"], ["textRendering", "optimizeSpeed", "optimizespeed"]]) {
        ctx[name] = value; ctx[name] = rejected;
        check(ctx[name] === value, name + " enumeration");
    }
    // The 'font' shorthand sets its font-stretch and font-variant-caps
    // longhands and resets font-kerning.
    ctx.font = "small-caps condensed 12px serif";
    check(ctx.fontVariantCaps === "small-caps" && ctx.fontStretch === "condensed" && ctx.fontKerning === "auto",
        "font sets its longhands");
    check(ctx.font === "small-caps condensed 12px serif", "font serialization " + ctx.font);
    ctx.fontStretch = "expanded"; ctx.fontVariantCaps = "normal";
    check(ctx.font === "expanded 12px serif", "font reflects its longhands " + ctx.font);
    ctx.font = "12px serif";
    check(ctx.fontStretch === "normal" && ctx.fontVariantCaps === "normal", "font resets its longhands");
    // Small capitals: lowercase letters become (native or synthesized) small
    // capitals; capitals and caseless characters are unchanged.
    ctx.font = "40px sans-serif";
    const lower = ctx.measureText("abc"), upperNormal = ctx.measureText("ABC1");
    ctx.fontVariantCaps = "all-small-caps";
    const allSmall = ctx.measureText("abc");
    check(allSmall.width !== lower.width || allSmall.actualBoundingBoxAscent !== lower.actualBoundingBoxAscent,
        "all-small-caps changes lowercase");
    ctx.fontVariantCaps = "small-caps";
    check(close(ctx.measureText("ABC1").width, upperNormal.width, 0.01), "small-caps leaves capitals");
    const smallCaps = ctx.measureText("x");
    ctx.fontVariantCaps = "normal";
    check(!close(smallCaps.width, ctx.measureText("x").width, 0.01), "small capitals replace lowercase");
    ctx.fontVariantCaps = "unicase";
    check(ctx.measureText("ABC1").width < upperNormal.width, "unicase makes capitals small");
    ctx.fontVariantCaps = "normal";
    // Kerning: disabling it never tightens the text.
    ctx.fontKerning = "normal"; const kerned = ctx.measureText("AVAVATAWAY").width;
    ctx.fontKerning = "none"; const unkerned = ctx.measureText("AVAVATAWAY").width;
    check(unkerned >= kerned - 0.01, "font-kerning none");
    ctx.fontKerning = "auto"; ctx.textRendering = "optimizeSpeed";
    check(close(ctx.measureText("AVAVATAWAY").width, unkerned, 0.01), "optimizeSpeed disables kerning");
    ctx.textRendering = "geometricPrecision";
    check(close(ctx.measureText("AVAVATAWAY").width, kerned, 0.01), "geometricPrecision keeps kerning");
    ctx.textRendering = "auto";
    // fontStretch selects a narrower face when one exists, never a wider one.
    ctx.fontStretch = "ultra-condensed"; const narrow = ctx.measureText("Stretch").width;
    ctx.fontStretch = "normal";
    check(narrow <= ctx.measureText("Stretch").width + 0.01, "font-stretch");
    // direction "inherit" uses the canvas element's directionality (HTML
    // #the-directionality), not CSS 'direction'.
    ctx.reset(); ctx.font = "20px sans-serif"; ctx.textAlign = "start";
    const ltr = ctx.measureText("Hi");
    canvas.setAttribute("dir", "rtl");
    const rtl = ctx.measureText("Hi");
    check(close(rtl.actualBoundingBoxLeft - ltr.actualBoundingBoxLeft, ltr.width, 0.01), "inherit from the dir attribute");
    canvas.removeAttribute("dir");
    const html = document.createElement("html"), body = document.createElement("body");
    document.appendChild(html); html.appendChild(body);
    const holder = document.createElement("div"); holder.setAttribute("dir", "rtl");
    body.appendChild(holder); holder.appendChild(canvas);
    check(close(ctx.measureText("Hi").actualBoundingBoxLeft, rtl.actualBoundingBoxLeft, 0.01), "inherited directionality");
    canvas.style.direction = "ltr";
    check(close(ctx.measureText("Hi").actualBoundingBoxLeft, rtl.actualBoundingBoxLeft, 0.01), "not CSS direction");
    ctx.direction = "ltr";
    check(close(ctx.measureText("Hi").actualBoundingBoxLeft, ltr.actualBoundingBoxLeft, 0.01), "explicit ltr");
    // The direction is the paragraph's base direction: trailing neutral
    // characters follow it (UAX #9), so the period's ink moves.
    function painted(text, direction) {
        ctx.reset(); ctx.font = "30px sans-serif"; ctx.direction = direction; ctx.textAlign = "left";
        ctx.fillText(text, 10, 40);
        return Array.from(ctx.getImageData(0, 0, 300, 60).data).join();
    }
    check(painted("A.", "rtl") !== painted("A.", "ltr"), "base direction reorders neutrals");
    check(painted("AB", "rtl") === painted("AB", "ltr"), "strong characters keep their order");
    // lang: a BCP 47 tag, "" or "inherit", stored as given.
    ctx.lang = "tr-TR"; check(ctx.lang === "tr-TR", "lang");
    ctx.lang = ""; check(ctx.lang === "", "explicitly unknown language");
    ctx.lang = "inherit";
    // TextMetrics: baselines measured from the textBaseline line.
    ctx.reset(); ctx.font = "20px sans-serif";
    const alphabetic = ctx.measureText("Hg");
    check(alphabetic.alphabeticBaseline === 0, "alphabetic baseline at alphabetic");
    check(close(alphabetic.emHeightAscent + alphabetic.emHeightDescent, 20, 1e-6), "em box is 1em");
    check(alphabetic.hangingBaseline > 0 && alphabetic.ideographicBaseline <= 0, "hanging above, ideographic below");
    for (const baseline of ["top", "hanging", "middle", "ideographic", "bottom"]) {
        ctx.textBaseline = baseline;
        const metrics = ctx.measureText("Hg");
        const shift = metrics.alphabeticBaseline;
        check(close(metrics.hangingBaseline - shift, alphabetic.hangingBaseline, 1e-6)
            && close(metrics.ideographicBaseline - shift, alphabetic.ideographicBaseline, 1e-6)
            && close(metrics.actualBoundingBoxAscent - shift, alphabetic.actualBoundingBoxAscent, 1e-6)
            && close(metrics.fontBoundingBoxDescent + shift, alphabetic.fontBoundingBoxDescent, 1e-6), "baseline " + baseline);
    }
    ctx.textBaseline = "top"; check(Math.abs(ctx.measureText("Hg").emHeightAscent) < 1e-9, "top is the em-over baseline");
    ctx.textBaseline = "hanging"; check(Math.abs(ctx.measureText("Hg").hangingBaseline) < 1e-9, "hanging");
    ctx.textBaseline = "ideographic"; check(Math.abs(ctx.measureText("Hg").ideographicBaseline) < 1e-9, "ideographic");
    ctx.textBaseline = "bottom"; check(Math.abs(ctx.measureText("Hg").emHeightDescent) < 1e-9, "bottom is the em-under baseline");
    ctx.textBaseline = "middle";
    const middle = ctx.measureText("Hg");
    check(close(middle.emHeightAscent, middle.emHeightDescent, 1e-6), "middle halves the em box");
    // Spacing reaches fillText: wider text paints further.
    function inkRight(spacing) {
        ctx.reset(); ctx.font = "20px sans-serif"; ctx.direction = "ltr"; ctx.letterSpacing = spacing;
        ctx.fillText("iiii", 5, 40);
        const p = ctx.getImageData(0, 0, 300, 60).data;
        for (let x = 299; x >= 0; x--) for (let y = 0; y < 60; y++) if (p[(y * 300 + x) * 4 + 3] > 128) return x;
        return -1;
    }
    const spacedInk = inkRight("10px"), plainInk = inkRight("0px");
    check(spacedInk >= plainInk + 25, "letterSpacing paints " + spacedInk + " " + plainInk);
    return "canvas-text-styles-ok";
})();
