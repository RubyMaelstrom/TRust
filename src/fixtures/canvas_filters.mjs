// HTML #dom-context-2d-filter and #drawing-model with Filter Effects 1 filter
// functions and SVG filter references (local WHATWG e5071a20, CSSWG 81c27f68).
(function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    const canvas = document.createElement("canvas"); canvas.width = 40; canvas.height = 20;
    const ctx = canvas.getContext("2d");
    const px = (x, y) => Array.from(ctx.getImageData(x, y, 1, 1).data);
    const near = (a, b, tolerance = 2) => a.length === b.length && a.every((v, i) => Math.abs(v - b[i]) <= tolerance);
    // The filter attribute: "none" or a <filter-value-list>; others ignored.
    check(ctx.filter === "none", "default filter");
    ctx.filter = "blur(2px) grayscale(50%)";
    check(ctx.filter === "blur(2px) grayscale(50%)", "the given string is kept");
    for (const invalid of ["", "inherit", "initial", "blur(-1px)", "blur(1px) bogus", "blur(10%)",
        "drop-shadow(1px)", "url(#a) 3", "none blur(1px)"]) {
        ctx.filter = invalid;
        check(ctx.filter === "blur(2px) grayscale(50%)", "ignored " + invalid);
    }
    ctx.filter = null; ctx.filter = undefined;
    check(ctx.filter === "blur(2px) grayscale(50%)", "null and undefined are unparseable");
    ctx.filter = "none"; check(ctx.filter === "none", "none disables");
    ctx.filter = "invert(1)"; ctx.save(); ctx.filter = "none"; ctx.restore();
    check(ctx.filter === "invert(1)", "the filter is drawing state");
    ctx.filter = "blur(1em)"; check(ctx.filter === "blur(1em)", "relative lengths keep their text");
    // Color matrix functions operate on unpremultiplied sRGB.
    const fill = (filter, color = "rgb(255, 0, 0)") => {
        ctx.reset(); ctx.filter = filter; ctx.fillStyle = color; ctx.fillRect(0, 0, 10, 10); return px(5, 5);
    };
    check(near(fill("invert(1)"), [0, 255, 255, 255]), "invert");
    check(near(fill("invert(50%)"), [128, 128, 128, 255], 1), "invert amount");
    check(near(fill("grayscale(1)"), [54, 54, 54, 255]), "grayscale");
    check(near(fill("opacity(0.5)"), [255, 0, 0, 128], 1), "opacity");
    check(near(fill("brightness(0)"), [0, 0, 0, 255]), "brightness");
    check(near(fill("contrast(0)", "rgb(10, 200, 30)"), [128, 128, 128, 255], 1), "contrast");
    check(near(fill("sepia(1)", "rgb(100, 100, 100)"), [135, 120, 94, 255], 2), "sepia");
    check(near(fill("hue-rotate(180deg)", "rgb(255, 0, 0)"), [0, 109, 109, 255], 2), "hue-rotate");
    check(near(fill("saturate(0)", "rgb(255, 0, 0)"), [54, 54, 54, 255]), "saturate");
    check(near(fill("invert(1) invert(1)"), [255, 0, 0, 255]), "functions apply in order");
    // Global alpha applies before the filter (drawing model step 3).
    ctx.reset(); ctx.globalAlpha = 0.5; ctx.filter = "opacity(1)"; ctx.fillStyle = "red"; ctx.fillRect(0, 0, 10, 10);
    check(near(px(5, 5), [255, 0, 0, 128], 1), "global alpha");
    // blur(): a Gaussian of the given standard deviation spreads the shape.
    ctx.reset(); ctx.filter = "blur(2px)"; ctx.fillStyle = "black"; ctx.fillRect(15, 0, 10, 20);
    const edge = px(15, 10)[3], inside = px(20, 10)[3], outside = px(10, 10)[3], far = px(5, 10)[3];
    check(edge > 64 && edge < 192 && inside > 240 && outside > 0 && outside < 64 && far === 0, "blur " + [edge, inside, outside, far]);
    // drop-shadow(): the blurred, offset alpha in the shadow color, under the input.
    check(near(fill("drop-shadow(15px 0 0 blue)"), [255, 0, 0, 255]), "drop-shadow keeps the input");
    check(near(px(20, 5), [0, 0, 255, 255]), "drop-shadow offset");
    // A is infinite: content outside the canvas casts a shadow into it.
    ctx.reset(); ctx.filter = "drop-shadow(12px 0 0 lime)"; ctx.fillStyle = "red"; ctx.fillRect(-10, 0, 5, 5);
    check(near(px(4, 2), [0, 255, 0, 255]) && px(0, 2)[3] === 0, "offscreen source content");
    // Filters apply to images, text and strokes too.
    const image = document.createElement("canvas"); image.width = 4; image.height = 4;
    const imageContext = image.getContext("2d"); imageContext.fillStyle = "red"; imageContext.fillRect(0, 0, 4, 4);
    ctx.reset(); ctx.filter = "invert(1)"; ctx.drawImage(image, 0, 0);
    check(near(px(2, 2), [0, 255, 255, 255]), "drawImage");
    ctx.reset(); ctx.filter = "invert(1)"; ctx.font = "20px sans-serif"; ctx.fillStyle = "red"; ctx.fillText("M", 2, 18);
    const glyph = ctx.getImageData(0, 0, 40, 20).data; let cyan = 0;
    for (let i = 0; i < glyph.length; i += 4) if (glyph[i + 3] === 255) { cyan++; check(glyph[i] === 0 && glyph[i + 1] === 255, "text filter"); }
    check(cyan > 10, "filtered text paints");
    ctx.reset(); ctx.filter = "invert(1)"; ctx.strokeStyle = "red"; ctx.lineWidth = 4; ctx.strokeRect(5, 5, 10, 10);
    check(near(px(5, 10), [0, 255, 255, 255]), "stroke");
    // Shadows are rendered from the filtered image B.
    ctx.reset(); ctx.filter = "invert(1)"; ctx.shadowColor = "blue"; ctx.shadowOffsetX = 20; ctx.fillStyle = "red";
    ctx.fillRect(0, 0, 10, 10);
    check(near(px(5, 5), [0, 255, 255, 255]) && near(px(25, 5), [0, 0, 255, 255]), "shadow from B");
    // clearRect, putImageData and the current path are unaffected.
    ctx.reset(); ctx.fillStyle = "red"; ctx.fillRect(0, 0, 40, 20); ctx.filter = "blur(5px)"; ctx.clearRect(10, 5, 10, 10);
    check(px(15, 10)[3] === 0 && near(px(9, 10), [255, 0, 0, 255]), "clearRect is unfiltered");
    ctx.putImageData(new ImageData(new Uint8ClampedArray([0, 0, 255, 255]), 1, 1), 15, 10);
    check(near(px(15, 10), [0, 0, 255, 255]) && px(16, 10)[3] === 0, "putImageData is unfiltered");
    // url(): a same-document SVG filter, resolved for each drawing.
    const html = document.createElement("html"), body = document.createElement("body");
    document.appendChild(html); html.appendChild(body);
    body.innerHTML = '<svg width="0" height="0"><filter id="green" x="0" y="0" width="1" height="1"'
        + ' color-interpolation-filters="sRGB"><feColorMatrix id="matrix" type="matrix"'
        + ' values="0 0 0 0 0  0 0 0 0 1  0 0 0 0 0  0 0 0 1 0"/></filter></svg>';
    check(near(fill("url(#green)"), [0, 255, 0, 255]), "SVG filter reference");
    document.getElementById("matrix").setAttribute("values", "0 0 0 0 0  0 0 0 0 0  0 0 0 0 1  0 0 0 1 0");
    check(near(fill("url(#green)"), [0, 0, 255, 255]), "a changed SVG filter is used by the next draw");
    check(near(fill("url(#green) invert(1)"), [255, 255, 0, 255]), "references chain with functions");
    // A missing reference ignores the whole chain; an external document that
    // is not loaded draws without filtering.
    check(near(fill("invert(1) url(#missing)"), [255, 0, 0, 255]), "missing reference");
    check(near(fill("url(other.svg#green)"), [255, 0, 0, 255]), "external reference");
    body.innerHTML = '<div id="notfilter"></div>';
    check(near(fill("url(#notfilter)"), [255, 0, 0, 255]), "non-filter reference");
    // OffscreenCanvas contexts filter too.
    const offscreen = new OffscreenCanvas(4, 4).getContext("2d");
    offscreen.filter = "invert(1)"; offscreen.fillStyle = "red"; offscreen.fillRect(0, 0, 4, 4);
    check(near(Array.from(offscreen.getImageData(1, 1, 1, 1).data), [0, 255, 255, 255]), "OffscreenCanvas filter");
    return "canvas-filters-ok";
})();
