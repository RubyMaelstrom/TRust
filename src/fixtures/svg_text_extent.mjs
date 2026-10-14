(() => {
    // SVG 2 text.html#__svg__SVGTextContentElement__getExtentOfChar,
    // #TermAddressableCharacter, #TermFindTypographicCharacterForCharacter;
    // coords.html#TermFullGlyphCell; CSS Writing Modes #typeset-upright /
    // #typeset-sideways; Web IDL #js-operations / #js-unsigned-long;
    // Geometry Interfaces #DOMRect. The assertions compare measured cells,
    // so ordinary horizontal font advances/ascent/descent need no fixed values.
    function check(value, message) { if (!value) throw new Error(message); }
    function raises(callback, predicate, message) {
        try { callback(); } catch (error) {
            check(predicate(error), message + ': ' + error);
            return;
        }
        throw new Error(message + ': did not throw');
    }
    const typeError = (callback, message) => raises(callback, error => error instanceof TypeError, message);
    const rangeError = (callback, message) => raises(callback,
        error => error instanceof DOMException && error.name === 'IndexSizeError', message);
    const near = (a, b) => Math.abs(a - b) <= 1e-3;
    const same = (a, b) => near(a.x, b.x) && near(a.y, b.y) &&
        near(a.width, b.width) && near(a.height, b.height);
    const show = rect => [rect.x, rect.y, rect.width, rect.height].join(',');
    const SVG = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(SVG, 'svg');
    svg.setAttribute('width', '600'); svg.setAttribute('height', '400');
    svg.setAttribute('viewBox', '0 0 600 400');
    document.body.appendChild(svg);
    function text(content, attributes = {}, parent = svg) {
        const node = document.createElementNS(SVG, 'text');
        node.setAttribute('font-family', 'DejaVu Serif');
        node.setAttribute('font-size', '20');
        node.setAttribute('x', '13'); node.setAttribute('y', '70');
        for (const name of Object.keys(attributes)) {
            if (name === 'xml:space')
                node.setAttributeNS('http://www.w3.org/XML/1998/namespace', name, attributes[name]);
            else node.setAttribute(name, attributes[name]);
        }
        node.textContent = content;
        parent.appendChild(node);
        return node;
    }

    if (svgExtentTestSection === 'bindings') {
        const node = text('AB');
        const descriptor = Object.getOwnPropertyDescriptor(SVGTextContentElement.prototype, 'getExtentOfChar');
        check(descriptor && descriptor.enumerable && descriptor.writable && descriptor.configurable,
            'operation descriptor');
        const method = descriptor.value;
        check(method.name === 'getExtentOfChar' && method.length === 1 &&
            !Object.prototype.hasOwnProperty.call(method, 'prototype'), 'operation name, arity, constructor shape');
        typeError(() => node.getExtentOfChar(), 'required argument');
        typeError(() => new method(0), 'operation is not a constructor');
        let conversions = 0;
        const index = {valueOf() { conversions++; return 0; }};
        for (const receiver of [{}, SVGTextContentElement.prototype,
            Object.create(Object.getPrototypeOf(node)), document.createElementNS(SVG, 'rect'), new Proxy(node, {})]) {
            typeError(() => method.call(receiver, index), 'receiver brand');
        }
        check(conversions === 0, 'brand check precedes index conversion');
        typeError(() => node.getExtentOfChar(Symbol('index')), 'symbol conversion');
        typeError(() => node.getExtentOfChar(0n), 'bigint conversion');
        const first = node.getExtentOfChar(0), second = node.getExtentOfChar(1);
        check(first instanceof DOMRect && first instanceof SVGRect && first.width > 0 && first.height > 0,
            'fresh mutable SVGRect alias');
        for (const value of [undefined, null, NaN, Infinity, -Infinity, -0.9, 4294967296, -4294967296])
            check(same(node.getExtentOfChar(value), first), 'unsigned long zero conversion: ' + value);
        check(same(node.getExtentOfChar(4294967297.9), second), 'unsigned long modulo and truncation');
        check(same(node.getExtentOfChar(index), first) && conversions === 1, 'one numeric conversion');
        const sentinel = {};
        raises(() => node.getExtentOfChar({valueOf() { throw sentinel; }}), error => error === sentinel,
            'conversion exception propagates');
        rangeError(() => node.getExtentOfChar(-1), 'wrapped negative index');
        rangeError(() => node.getExtentOfChar(2), 'index at addressable count');
        rangeError(() => text('').getExtentOfChar(0), 'empty text index');
        rangeError(() => text('A', {display: 'none'}).getExtentOfChar(0), 'non-rendered text index');
        check(same(node.getExtentOfChar(0, index), first) && conversions === 1, 'extra argument ignored');
        check(node.getExtentOfChar(0) !== first, 'fresh object per call');
        const originalX = first.x;
        first.x = -900; first.width = 1;
        check(near(node.getExtentOfChar(0).x, originalX), 'rectangle mutation does not change layout');
        const snapshot = node.getExtentOfChar(0);
        node.setAttribute('x', '83');
        check(near(snapshot.x, originalX) && near(node.getExtentOfChar(0).x, 83), 'detached rectangle snapshot');
    } else if (svgExtentTestSection === 'clusters') {
        const nonBmp = text('\u{1F600}', {'font-family': 'DejaVu Sans'});
        check(nonBmp.getNumberOfChars() === 2 && same(nonBmp.getExtentOfChar(0), nonBmp.getExtentOfChar(1)),
            'surrogate pair shares one glyph cell');
        const combining = text('a\u0301');
        check(combining.getNumberOfChars() === 2 && same(combining.getExtentOfChar(0), combining.getExtentOfChar(1)),
            'combining mark shares the typographic cell');
        const ligature = text('fi');
        check(ligature.getNumberOfChars() === 2 && same(ligature.getExtentOfChar(0), ligature.getExtentOfChar(1)),
            'DejaVu Serif fi ligature shares the typographic cell');
        const bidi = text('\u05d0\u05d1A');
        const alef = bidi.getExtentOfChar(0), bet = bidi.getExtentOfChar(1);
        check(bidi.getNumberOfChars() === 3 && alef.x > bet.x, 'RTL cells addressed in logical order');
        const spans = text('');
        spans.innerHTML = '<tspan id="extent-prefix">AA</tspan><tspan id="extent-target">B</tspan>';
        const target = document.getElementById('extent-target');
        check(target.getNumberOfChars() === 1 && same(target.getExtentOfChar(0), spans.getExtentOfChar(2)),
            'tspan indexes start at zero while retaining parent placement');
        rangeError(() => target.getExtentOfChar(1), 'tspan excludes sibling characters');
        const divided = text('');
        divided.innerHTML = 'f<tspan id="extent-ligature-tail">i</tspan>';
        const tail = document.getElementById('extent-ligature-tail');
        check(tail.getNumberOfChars() === 1 && same(tail.getExtentOfChar(0), divided.getExtentOfChar(1)),
            'tspan tail retains the cell of a cluster crossing its source boundary');
        if (same(divided.getExtentOfChar(0), divided.getExtentOfChar(1)))
            check(tail.getComputedTextLength() === 0 && tail.getSubStringLength(0, 1) === 0,
                'later source character shares cell but does not own ligature advance');
    } else if (svgExtentTestSection === 'positions') {
        const plain = text('A'), base = plain.getExtentOfChar(0);
        check(near(base.x, 13) && base.y < 70 && base.y + base.height > 70, 'full alphabetic glyph cell');
        const gap = text('AA', {x: '10 200'});
        const left = gap.getExtentOfChar(0), right = gap.getExtentOfChar(1);
        check(near(left.x, 10) && near(right.x, 200) && near(left.width, base.width) && near(right.width, base.width),
            'explicit position gap is outside the cells: ' + show(left) + ' / ' + show(right));
        const shifted = text('A', {dx: '80'}).getExtentOfChar(0);
        check(near(shifted.x, 93) && near(shifted.width, base.width), 'dx shifts without enlarging cell');
        const spaced = text('AA', {'letter-spacing': '40'});
        const spaceFirst = spaced.getExtentOfChar(0), spaceNext = spaced.getExtentOfChar(1);
        check(near(spaceFirst.width, base.width) && near(spaceNext.x - spaceFirst.x, base.width + 40),
            'letter spacing positions cells but is outside glyph extent');
        check(same(text('A', {stroke: 'black', 'stroke-width': '64'}).getExtentOfChar(0), base),
            'glyph extent excludes stroke');
        const rotated = text('A', {rotate: '90'}).getExtentOfChar(0);
        check(near(rotated.width, base.height) && near(rotated.height, base.width) && near(rotated.y, 70) &&
            near(rotated.x, 13 - (base.y + base.height - 70)), 'rotated cell tight bounds: ' + show(rotated));
        const diagonal = text('A', {rotate: '45'}).getExtentOfChar(0);
        check(near(diagonal.width, (base.width + base.height) * Math.SQRT1_2) &&
            near(diagonal.height, diagonal.width), '45 degree cell quad bounds');
        const group = document.createElementNS(SVG, 'g');
        group.setAttribute('transform', 'translate(300 200) scale(3)'); svg.appendChild(group);
        const local = text('A', {transform: 'translate(70 30) rotate(45)'}, group);
        svg.setAttribute('viewBox', '0 0 300 200');
        check(same(local.getExtentOfChar(0), base), 'caller user coordinates exclude own, ancestor and viewport transforms');
        const scaledNode = text('A', {textLength: String(base.width * 2), lengthAdjust: 'spacingAndGlyphs'});
        const scaled = scaledNode.getExtentOfChar(0);
        check(near(scaled.width, base.width * 2) && near(scaled.height, base.height), 'textLength scales the glyph cell');
        check(near(scaledNode.getComputedTextLength(), base.width * 2) &&
            near(scaledNode.getSubStringLength(0, 1), base.width * 2), 'textLength scales formatted advance');
        const path = document.createElementNS(SVG, 'path');
        path.id = 'extent-line'; path.setAttribute('d', 'M0 50 H500'); svg.appendChild(path);
        const pathText = text('', {x: '0'});
        pathText.innerHTML = '<textPath href="#extent-line" startOffset="10">A</textPath>';
        const pathCell = pathText.getExtentOfChar(0);
        // The shared SVG renderer places textPath glyphs with a 0.5-unit
        // arc-length solver tolerance. Character geometry must expose that
        // actual positioned cell exactly, including its fractional origin.
        check(same(pathCell, pathText.getBBox()) && Math.abs(pathCell.x - 10) <= 0.5 &&
            near(pathCell.y, base.y - 20) &&
            near(pathCell.width, base.width) && near(pathCell.height, base.height),
            'textPath cell follows path position: ' + show(pathCell));
    } else if (svgExtentTestSection === 'vertical') {
        const horizontal = text('A').getExtentOfChar(0);
        const sideways = text('A', {'writing-mode': 'vertical-rl'}).getExtentOfChar(0);
        check(near(sideways.width, horizontal.height) && near(sideways.height, horizontal.width),
            'sideways vertical glyph uses horizontal metrics: ' + show(sideways));
        // DejaVu Serif has no vertical metrics. Copyright has UAX50 U
        // orientation, so SVG 2 requires a one-EM-square fallback cell.
        const upright = text('\u00a9', {'writing-mode': 'vertical-rl'}).getExtentOfChar(0);
        check(near(upright.width, 20) && near(upright.height, 20) && near(upright.x, 3) && near(upright.y, 70),
            'upright fallback EM cell at central baseline: ' + show(upright));
        const rotated = text('\u00a9', {'writing-mode': 'vertical-rl', rotate: '45'}).getExtentOfChar(0);
        check(near(rotated.width, 20 * Math.SQRT2) && near(rotated.height, 20 * Math.SQRT2),
            'upright rotated cell bounds');
    } else if (svgExtentTestSection === 'addressable') {
        const collapsed = text('  A   B  ');
        check(collapsed.getNumberOfChars() === 3 && collapsed.getExtentOfChar(1).width > 0,
            'collapsed whitespace indexing includes surviving space');
        rangeError(() => collapsed.getExtentOfChar(3), 'collapsed whitespace bounds');
        const spaces = text('  ', {'xml:space': 'preserve'});
        const first = spaces.getExtentOfChar(0), next = spaces.getExtentOfChar(1);
        check(spaces.getNumberOfChars() === 2 && first.width > 0 && first.height > 0 &&
            near(next.x - first.x, first.width) && near(first.width, next.width),
            'preserved spaces retain glyph cells without outlines');
        const discarded = text('A\u00ad\u2067B\u2069');
        check(discarded.getNumberOfChars() === 2 && discarded.getExtentOfChar(1).width > 0,
            'discarded soft hyphen and bidi controls are not addressable');
        rangeError(() => discarded.getExtentOfChar(2), 'discarded control bounds');
        const hidden = text('');
        hidden.innerHTML = 'A<tspan display="none">hidden</tspan>B';
        check(hidden.getNumberOfChars() === 2 && hidden.getExtentOfChar(1).width > 0,
            'display:none descendants are not addressable');
        check(text('A', {visibility: 'hidden'}).getExtentOfChar(0).width > 0,
            'visibility:hidden text still has a glyph cell');
    } else {
        throw new Error('unknown SVG extent fixture section');
    }
    return 'svg-text-extent-' + svgExtentTestSection + '-ok';
})()
