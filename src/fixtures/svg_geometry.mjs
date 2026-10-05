(() => {
    // SVG 2 geometry DOM methods, answered from the inline-SVG paint pipeline:
    // types.html#__svg__SVGGraphicsElement__getBBox (coords.html#BoundingBoxes),
    // #__svg__SVGGraphicsElement__getCTM, #__svg__SVGGraphicsElement__getScreenCTM,
    // #__svg__SVGGeometryElement__getTotalLength, #…__getPointAtLength,
    // #…__isPointInFill, #…__isPointInStroke and text.html
    // #InterfaceSVGTextContentElement. Expected values follow the spec text and
    // its bbox example; Chromium and Firefox agree on each of these.
    function check(value, message) { if (!value) throw new Error(message); }
    const near = (a, b, tolerance = 1e-3) => Math.abs(a - b) <= tolerance;
    const box = (r, x, y, w, h, tolerance) => r instanceof DOMRect &&
        near(r.x, x, tolerance) && near(r.y, y, tolerance) && near(r.width, w, tolerance) && near(r.height, h, tolerance);
    const show = r => r ? [r.x, r.y, r.width, r.height].join(',') : String(r);
    const matrix = (m, values, tolerance = 1e-9) => m instanceof DOMMatrix && m.is2D &&
        [m.a, m.b, m.c, m.d, m.e, m.f].every((value, i) => near(value, values[i], tolerance));
    const showMatrix = m => m ? [m.a, m.b, m.c, m.d, m.e, m.f].join(',') : String(m);
    document.body.style.margin = '0';
    document.body.innerHTML =
        '<div style="font-size:30px"><svg id="s" width="200" height="100" viewBox="0 0 100 50" ' +
        'style="position:absolute;left:7px;top:30px;padding:3px;border:2px solid black">' +
        '<rect id="r" x="10" y="20" width="30" height="40" transform="translate(5,5)"/>' +
        '<circle id="c" cx="50" cy="25" r="10" stroke="black" stroke-width="4"/>' +
        '<path id="curve" d="M0 0 C 0 100 100 100 100 0"/>' +
        '<path id="quad" d="M20 50 L35 100 H120 V50 Q70 10 20 50"/>' +
        '<g id="tg" transform="translate(100,10) scale(2)">' +
        '<rect x="1" y="2" width="3" height="4"/><rect x="10" y="20" width="1" height="1" transform="rotate(45)"/></g>' +
        // The coords.html#BoundingBoxes example (images/coords/bbox-calc.svg).
        '<defs id="defs-1"><rect id="rect-1" x="20" y="20" width="40" height="40" fill="blue"/></defs>' +
        '<g id="group-1"><use id="use-1" href="#rect-1" x="10" y="10"/>' +
        '<g id="group-2" display="none"><rect id="rect-2" x="10" y="10" width="100" height="100" fill="red"/></g></g>' +
        '<rect id="flat" x="5" y="6" width="0" height="10"/><rect id="invalid" x="5" y="6" width="-1" height="10"/>' +
        '<rect id="pct" x="10%" y="0" width="50%" height="20%"/>' +
        '<svg id="n" x="10" y="20" width="50" height="50" viewBox="0 0 10 10">' +
        '<rect id="nr" x="1" y="1" width="2" height="2" transform="rotate(90)"/></svg>' +
        '<path id="poly" d="M0 0 L30 40 L30 0" stroke-width="4"/>' +
        '<path id="ring" d="M0 0 H10 V10 H0 Z M2 2 H8 V8 H2 Z" fill-rule="evenodd"/>' +
        '<line id="ln" x1="0" y1="10" x2="100" y2="10" stroke-dasharray="10 10" stroke-width="2"/>' +
        '<g font-size="10"><text id="small" x="5" y="40">Hello <tspan id="world">world</tspan> again</text></g>' +
        '<text id="big" x="50" y="40" text-anchor="middle">Hi</text>' +
        '<text id="empty" x="1" y="1"></text>' +
        '<marker id="arrow" markerWidth="10" markerHeight="10" refX="0" refY="5" orient="auto" ' +
        'markerUnits="userSpaceOnUse"><path d="M0 0 L10 5 L0 10 Z"/></marker>' +
        '<path id="marked" d="M10 10 L50 10" stroke="black" marker-end="url(#arrow)"/>' +
        '<clipPath id="cp"><rect x="10" y="20" width="30" height="40"/></clipPath>' +
        '<rect id="clipped" x="0" y="0" width="100" height="100" clip-path="url(#cp)"/>' +
        '</svg></div>';
    const $ = id => document.getElementById(id);

    // Bounding boxes: user space, excluding the element's own transform.
    check(box($('r').getBBox(), 10, 20, 30, 40), 'rect ' + show($('r').getBBox()));
    check(box($('c').getBBox(), 40, 15, 20, 20), 'circle ' + show($('c').getBBox()));
    check(box($('c').getBBox({stroke: true}), 38, 13, 24, 24), 'circle stroke box');
    check(box($('c').getBBox({fill: false}), 0, 0, 0, 0), 'no parts');
    // Curves: the extrema, not the control points (y peaks at 75 for t = 1/2).
    check(box($('curve').getBBox(), 0, 0, 100, 75), 'cubic ' + show($('curve').getBBox()));
    // The spec's quadratic figure: Q70,10 peaks at y = 30, not at 10.
    check(box($('quad').getBBox(), 20, 30, 100, 70), 'quadratic ' + show($('quad').getBBox()));
    // A group: children with their transforms, tight around a rotated child.
    check(box($('tg').getBBox(), -11 * Math.SQRT1_2, 2, 4 + 11 * Math.SQRT1_2, 32 * Math.SQRT1_2 - 2, 1e-4),
        'group ' + show($('tg').getBBox()));
    check(box($('defs-1').getBBox(), 0, 0, 0, 0), 'defs-1');
    check(box($('rect-1').getBBox(), 20, 20, 40, 40), 'rect-1 (in defs)');
    check(box($('use-1').getBBox(), 30, 30, 40, 40), 'use-1 ' + show($('use-1').getBBox()));
    check(box($('group-1').getBBox(), 30, 30, 40, 40), 'group-1 ' + show($('group-1').getBBox()));
    check(box($('group-2').getBBox(), 10, 10, 100, 100), 'group-2 (display:none)');
    check(box($('rect-2').getBBox(), 10, 10, 100, 100), 'rect-2');
    check(box($('flat').getBBox(), 5, 6, 0, 10), 'zero-width rect ' + show($('flat').getBBox()));
    check(box($('invalid').getBBox(), 0, 0, 0, 0), 'negative width');
    check(box($('pct').getBBox(), 10, 0, 50, 10), 'percentages of the viewBox');
    const detached = document.createElementNS('http://www.w3.org/2000/svg', 'ellipse');
    detached.setAttribute('cx', '5'); detached.setAttribute('cy', '6');
    detached.setAttribute('rx', '2'); detached.setAttribute('ry', '3');
    check(box(detached.getBBox(), 3, 3, 4, 6), 'not in the document');
    // svg: its content in its own (viewBox) user space.
    check(box($('n').getBBox(), -3, 1, 2, 2), 'nested svg ' + show($('n').getBBox()));
    // SVGBoundingBoxOptions: markers and clipping paths only when requested.
    check(box($('marked').getBBox(), 10, 10, 40, 0), 'markers excluded ' + show($('marked').getBBox()));
    check(box($('marked').getBBox({markers: true}), 10, 5, 50, 10), 'markers ' + show($('marked').getBBox({markers: true})));
    check(box($('clipped').getBBox(), 0, 0, 100, 100), 'clip excluded');
    check(box($('clipped').getBBox({clipped: true}), 10, 20, 30, 40), 'clipped ' + show($('clipped').getBBox({clipped: true})));

    // Text: full glyph cells, advances equal to the text content lengths.
    const small = $('small'), world = $('world'), big = $('big');
    const smallBox = small.getBBox(), worldBox = world.getBBox(), bigBox = big.getBBox();
    check(smallBox.width > 0 && near(smallBox.x, 5) && smallBox.y < 40 && smallBox.y + smallBox.height > 40 &&
        smallBox.height > 10 && smallBox.height < 15, 'text box from the inherited font size ' + show(smallBox));
    check(near(smallBox.width, small.getComputedTextLength(), 1e-3), 'text box width is its advance');
    check(small.getNumberOfChars() === 17 && world.getNumberOfChars() === 5, 'addressable characters');
    check(near(small.getSubStringLength(6, 5), world.getComputedTextLength(), 1e-3), 'tspan advance');
    check(near(worldBox.x, 5 + small.getSubStringLength(0, 6), 1e-3) &&
        near(worldBox.width, world.getComputedTextLength(), 1e-3) && near(worldBox.y, smallBox.y) &&
        near(worldBox.height, smallBox.height), 'tspan glyph cells ' + show(worldBox));
    const parts = [0, 6, 11].map((start, i) => small.getSubStringLength(start, [6, 5, 6][i]));
    check(near(parts[0] + parts[1] + parts[2], small.getComputedTextLength(), 1e-3), 'substrings add up');
    check(near(small.getSubStringLength(16, 99), small.getSubStringLength(16, 1)), 'nchars clamps');
    try { small.getSubStringLength(17, 1); throw new Error('getSubStringLength range'); }
    catch (e) { check(e instanceof DOMException && e.name === 'IndexSizeError', 'IndexSizeError'); }
    check(bigBox.height > 30 && near(bigBox.x, 50 - bigBox.width / 2, 1e-3), 'inherited CSS font size, anchor');
    check(box($('empty').getBBox(), 0, 0, 0, 0) && $('empty').getNumberOfChars() === 0 &&
        $('empty').getComputedTextLength() === 0, 'empty text');

    // Matrices: to the nearest viewport (including its viewBox), and to the
    // document viewport through the outermost svg element's content box.
    check(matrix($('r').getCTM(), [2, 0, 0, 2, 10, 10]), 'rect CTM ' + showMatrix($('r').getCTM()));
    check(matrix($('s').getCTM(), [2, 0, 0, 2, 0, 0]), 'outermost svg CTM');
    check(matrix($('nr').getCTM(), [0, 5, -5, 0, 10, 20], 1e-12), 'nested viewport CTM ' + showMatrix($('nr').getCTM()));
    check(matrix($('n').getCTM(), [10, 0, 0, 10, 20, 40]), 'nested svg CTM');
    check(matrix($('r').getScreenCTM(), [2, 0, 0, 2, 22, 45]), 'screen CTM ' + showMatrix($('r').getScreenCTM()));
    check(matrix($('nr').getScreenCTM(), [0, 10, -10, 0, 32, 75], 1e-12), 'nested screen CTM ' + showMatrix($('nr').getScreenCTM()));
    check(matrix($('s').getScreenCTM(), [2, 0, 0, 2, 12, 35]), 'outermost screen CTM');
    check(detached.getCTM() === null && detached.getScreenCTM() === null, 'no matrix outside the document');
    const local = new DOMPoint(5, 5).matrixTransform($('r').getScreenCTM().inverse());
    check(near(local.x, -8.5) && near(local.y, -20), 'd3.pointer-style client to user space');

    // Paths: length, point at length, fill and stroke hit tests.
    const poly = $('poly');
    check(near(poly.getTotalLength(), 90), 'polyline length');
    const at = (d) => { const p = poly.getPointAtLength(d); return [p.x, p.y]; };
    check(poly.getPointAtLength(0) instanceof DOMPoint && near(at(25)[0], 15) && near(at(25)[1], 20) &&
        near(at(60)[0], 30) && near(at(60)[1], 30), 'point at length');
    check(near(at(-5)[0], 0) && near(at(500)[1], 0) && near(at(500)[0], 30), 'distance clamps');
    let threw = false;
    try { poly.getPointAtLength(NaN); } catch (e) { threw = e instanceof TypeError; }
    check(threw, 'float conversion');
    check(near($('c').getTotalLength(), 2 * Math.PI * 10, 0.05), 'circle length ' + $('c').getTotalLength());
    const start = $('c').getPointAtLength(0);
    check(near(start.x, 60) && near(start.y, 25), 'a circle starts at (cx + r, cy)');
    check(poly.isPointInFill({x: 25, y: 20}) && !poly.isPointInFill({x: 5, y: 20}), 'open subpath fills closed');
    check(poly.isPointInFill({x: 30, y: 10}) && poly.isPointInFill(new DOMPoint(0, 0)), 'points on the path');
    check(!poly.isPointInFill({x: NaN, y: 1}) && !$('quad').isPointInFill() && poly.isPointInFill(),
        'non-finite and default points');
    check($('ring').isPointInFill({x: 1, y: 5}) && !$('ring').isPointInFill({x: 5, y: 5}), 'evenodd');
    check(poly.isPointInStroke({x: 31.5, y: 20}) && !poly.isPointInStroke({x: 27.5, y: 20}),
        'stroke width, independent of the stroke paint');
    check($('ln').isPointInStroke({x: 5, y: 10.5}) && !$('ln').isPointInStroke({x: 15, y: 10}),
        'dash pattern');
    return 'svg-geometry-ok';
})()
