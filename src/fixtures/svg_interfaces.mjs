(() => {
    // SVG 2 element interface hierarchy and bindings: types.html
    // #InterfaceSVGGraphicsElement, #InterfaceSVGGeometryElement, text.html
    // #InterfaceSVGTextContentElement, pservers.html#InterfaceSVGGradientElement,
    // struct.html#InterfaceSVGSVGElement, coords.html#InterfaceSVGTransformList;
    // Web IDL #interface-object, #es-attributes, #es-operations, #es-constants.
    function check(value, message) { if (!value) throw new Error(message); }
    function throwsType(fn, message) {
        try { fn(); } catch (e) { check(e instanceof TypeError, message + ': ' + e); return; }
        throw new Error(message + ': did not throw');
    }
    const SVG = 'http://www.w3.org/2000/svg';
    const proto = Object.getPrototypeOf, desc = Object.getOwnPropertyDescriptor;
    const tag = o => Object.prototype.toString.call(o).slice(8, -1);
    const parentOf = name => proto(globalThis[name].prototype).constructor.name;
    const chain = {
        SVGGraphicsElement: 'SVGElement', SVGGeometryElement: 'SVGGraphicsElement',
        SVGRectElement: 'SVGGeometryElement', SVGCircleElement: 'SVGGeometryElement',
        SVGEllipseElement: 'SVGGeometryElement', SVGLineElement: 'SVGGeometryElement',
        SVGPathElement: 'SVGGeometryElement', SVGPolylineElement: 'SVGGeometryElement',
        SVGPolygonElement: 'SVGGeometryElement', SVGTextContentElement: 'SVGGraphicsElement',
        SVGTextPositioningElement: 'SVGTextContentElement', SVGTextElement: 'SVGTextPositioningElement',
        SVGTSpanElement: 'SVGTextPositioningElement', SVGTextPathElement: 'SVGTextContentElement',
        SVGSVGElement: 'SVGGraphicsElement', SVGGElement: 'SVGGraphicsElement',
        SVGDefsElement: 'SVGGraphicsElement', SVGAElement: 'SVGGraphicsElement',
        SVGUseElement: 'SVGGraphicsElement', SVGImageElement: 'SVGGraphicsElement',
        SVGSwitchElement: 'SVGGraphicsElement', SVGForeignObjectElement: 'SVGGraphicsElement',
        SVGGradientElement: 'SVGElement', SVGLinearGradientElement: 'SVGGradientElement',
        SVGRadialGradientElement: 'SVGGradientElement', SVGSymbolElement: 'SVGElement',
        SVGMarkerElement: 'SVGElement', SVGClipPathElement: 'SVGElement', SVGStopElement: 'SVGElement',
    };
    for (const name of Object.keys(chain)) {
        check(parentOf(name) === chain[name], name + ' inherits from ' + parentOf(name));
        check(proto(globalThis[name]) === globalThis[chain[name]], name + ' interface object inheritance');
        check(globalThis[name].name === name && globalThis[name].length === 0, name + ' identity');
        check(tag(globalThis[name].prototype) === name, name + ' @@toStringTag');
        const d = desc(globalThis, name);
        check(d.writable && d.configurable && !d.enumerable, name + ' global descriptor');
        throwsType(() => new globalThis[name](), name + ' illegal constructor');
    }
    throwsType(() => new SVGElement(), 'SVGElement illegal constructor');
    check(typeof SVGGradientStopElement === 'undefined', 'no invented interface');

    // Element creation chooses the concrete interface; abstract interfaces
    // belong to no element.
    const created = {
        rect: 'SVGRectElement', path: 'SVGPathElement', text: 'SVGTextElement',
        tspan: 'SVGTSpanElement', textPath: 'SVGTextPathElement', svg: 'SVGSVGElement',
        g: 'SVGGElement', use: 'SVGUseElement', linearGradient: 'SVGLinearGradientElement',
        foreignObject: 'SVGForeignObjectElement', graphics: 'SVGElement', geometry: 'SVGElement',
        gradient: 'SVGElement', feGaussianBlur: 'SVGElement',
    };
    for (const name of Object.keys(created)) {
        const element = document.createElementNS(SVG, name);
        check(tag(element) === created[name], name + ' creates ' + tag(element));
        check(element instanceof SVGElement && !(element instanceof HTMLElement), name + ' namespace');
    }
    document.body.insertAdjacentHTML('beforeend',
        '<svg id="parsed"><circle id="pc" r="1"/><text id="pt">x</text></svg>');
    check(document.getElementById('pc') instanceof SVGGeometryElement, 'parsed geometry element');
    check(document.getElementById('pt') instanceof SVGTextPositioningElement, 'parsed text element');
    check(document.getElementById('parsed') instanceof SVGGraphicsElement, 'parsed svg element');

    // Attributes are enumerable accessors and operations enumerable writable
    // methods, with brand checks.
    const operations = [
        [SVGGraphicsElement, 'getBBox', 0], [SVGGraphicsElement, 'getCTM', 0],
        [SVGGraphicsElement, 'getScreenCTM', 0], [SVGGeometryElement, 'isPointInFill', 0],
        [SVGGeometryElement, 'isPointInStroke', 0], [SVGGeometryElement, 'getTotalLength', 0],
        [SVGGeometryElement, 'getPointAtLength', 1], [SVGTextContentElement, 'getNumberOfChars', 0],
        [SVGTextContentElement, 'getComputedTextLength', 0], [SVGTextContentElement, 'getSubStringLength', 2],
        [SVGSVGElement, 'createSVGPoint', 0], [SVGSVGElement, 'createSVGMatrix', 0],
        [SVGSVGElement, 'createSVGRect', 0], [SVGSVGElement, 'createSVGTransform', 0],
        [SVGSVGElement, 'createSVGTransformFromMatrix', 0],
    ];
    for (const [C, name, length] of operations) {
        const d = desc(C.prototype, name);
        check(d && d.writable && d.enumerable && d.configurable, name + ' operation descriptor');
        check(d.value.name === name && d.value.length === length, name + ' operation identity');
        throwsType(() => new d.value(), name + ' is not a constructor');
        throwsType(() => d.value.call({}), name + ' brand check');
        throwsType(() => d.value.call(document.createElement('div')), name + ' HTML brand check');
    }
    throwsType(() => SVGGeometryElement.prototype.getTotalLength.call(document.createElementNS(SVG, 'g')),
        'a group is not a geometry element');
    throwsType(() => SVGGraphicsElement.prototype.getBBox.call(document.createElementNS(SVG, 'linearGradient')),
        'a gradient is not a graphics element');
    for (const [C, name] of [[SVGGraphicsElement, 'transform'], [SVGGeometryElement, 'pathLength'],
            [SVGElement, 'ownerSVGElement'], [SVGElement, 'viewportElement']]) {
        const d = desc(C.prototype, name);
        check(d && d.enumerable && d.configurable && d.set === undefined && d.get.name === 'get ' + name,
            name + ' attribute descriptor');
    }
    for (const [name, value] of [['LENGTHADJUST_UNKNOWN', 0], ['LENGTHADJUST_SPACING', 1],
            ['LENGTHADJUST_SPACINGANDGLYPHS', 2]]) {
        const d = desc(SVGTextContentElement, name);
        check(d.value === value && !d.writable && d.enumerable && !d.configurable, name + ' constant');
        check(SVGTextContentElement.prototype[name] === value, name + ' prototype constant');
    }
    check(SVGTransform.SVG_TRANSFORM_ROTATE === 4 && SVGTransform.prototype.SVG_TRANSFORM_SKEWY === 6,
        'transform type constants');

    // struct.html#__svg__SVGSVGElement__createSVGPoint et al.: new detached
    // DOMPoint/DOMMatrix/DOMRect (Geometry 1 legacy aliases) and SVGTransform.
    const svg = document.createElementNS(SVG, 'svg');
    const point = svg.createSVGPoint(), matrix = svg.createSVGMatrix(), rect = svg.createSVGRect();
    check(point instanceof SVGPoint && point instanceof DOMPoint && point.x === 0 && point.y === 0 && point.w === 1,
        'createSVGPoint');
    check(point !== svg.createSVGPoint(), 'createSVGPoint returns a new object');
    point.x = 3; check(point.x === 3, 'SVGPoint is mutable');
    check(matrix instanceof SVGMatrix && matrix.isIdentity && matrix.is2D, 'createSVGMatrix');
    check(rect instanceof SVGRect && rect.x === 0 && rect.width === 0, 'createSVGRect');
    const transform = svg.createSVGTransform();
    check(transform instanceof SVGTransform && transform.type === 1 && transform.matrix.isIdentity,
        'createSVGTransform');
    transform.setTranslate(5, 6);
    check(transform.type === 2 && transform.matrix.e === 5 && transform.matrix.f === 6, 'setTranslate');
    transform.setScale(2, 3);
    check(transform.type === 3 && transform.matrix.a === 2 && transform.matrix.d === 3, 'setScale');
    transform.setRotate(90, 10, 10);
    const r = transform.matrix;
    check(transform.type === 4 && transform.angle === 90 && Math.abs(r.a) < 1e-12 && r.b === 1 &&
        r.c === -1 && Math.abs(r.e - 20) < 1e-12 && Math.abs(r.f) < 1e-12, 'setRotate about a point');
    transform.setSkewX(45);
    check(transform.type === 5 && Math.abs(transform.matrix.c - 1) < 1e-12, 'setSkewX');
    throwsType(() => transform.setTranslate(1), 'setTranslate arity');
    throwsType(() => transform.setTranslate(NaN, 1), 'setTranslate float conversion');
    const fromMatrix = svg.createSVGTransformFromMatrix({a: 2, e: 7});
    check(fromMatrix.type === 1 && fromMatrix.matrix.a === 2 && fromMatrix.matrix.e === 7,
        'createSVGTransformFromMatrix');

    // types.html#__svg__SVGGraphicsElement__transform reflects the transform
    // attribute; consolidate() is what d3-interpolate's parseSvg uses.
    const g = document.createElementNS(SVG, 'g');
    check(g.transform === g.transform && g.transform instanceof SVGAnimatedTransformList, '[SameObject] transform');
    const list = g.transform.baseVal;
    check(list instanceof SVGTransformList && list === g.transform.baseVal, 'baseVal list');
    check(list.numberOfItems === 0 && list.consolidate() === null, 'empty list');
    g.setAttribute('transform', 'translate(10, 20) scale(2) rotate(90)');
    check(list.numberOfItems === 3 && list.length === 3 && list.getItem(1).type === 3, 'list re-reads the attribute');
    check(list.getItem(0) === list.getItem(0), 'list items keep their identity');
    const consolidated = list.consolidate();
    const c = consolidated.matrix;
    check(consolidated.type === 1 && list.numberOfItems === 1 && Math.abs(c.a) < 1e-12 &&
        Math.abs(c.b - 2) < 1e-12 && Math.abs(c.c + 2) < 1e-12 && Math.abs(c.d) < 1e-12 &&
        c.e === 10 && c.f === 20, 'consolidate');
    check(/^matrix\(/.test(g.getAttribute('transform')), 'consolidate re-serializes the attribute');
    const appended = list.appendItem(svg.createSVGTransformFromMatrix({e: 1}));
    check(list.numberOfItems === 2 && g.getAttribute('transform').split('matrix').length === 3, 'appendItem');
    appended.setTranslate(3, 4);
    check(/translate\(3, 4\)$/.test(g.getAttribute('transform')), 'attached items re-serialize');
    try { list.getItem(5); throw new Error('getItem range'); } catch (e) { check(e.name === 'IndexSizeError', 'getItem range'); }
    try { g.transform.animVal.clear(); throw new Error('animVal read-only'); }
    catch (e) { check(e.name === 'NoModificationAllowedError', 'animVal read-only'); }
    list.clear();
    check(list.numberOfItems === 0 && g.getAttribute('transform') === '', 'clear');
    g.setAttribute('transform', 'translate(1 2) bogus(3)');
    check(list.numberOfItems === 0, 'an attribute in error is an empty list');
    const path = document.createElementNS(SVG, 'path');
    check(path.pathLength === path.pathLength && path.pathLength.baseVal === 0, 'pathLength default');
    path.setAttribute('pathLength', '120');
    check(path.pathLength.baseVal === 120 && path.pathLength.animVal === 120, 'pathLength reflects');
    path.pathLength.baseVal = 7.5;
    check(path.getAttribute('pathLength') === '7.5', 'pathLength setter');
    return 'svg-interfaces-ok';
})()
