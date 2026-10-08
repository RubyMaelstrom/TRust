// SVG 2 types.html#ReflectingDOM, #InterfaceSVGLength, #InterfaceSVGAngle,
// #ListInterfaces and the SVGAnimated* interfaces; coords.html
// #InterfaceSVGPreserveAspectRatio; shapes.html#InterfaceSVGPointList;
// Filter Effects 1 and CSS Masking 1 DOM interfaces.
(() => {
    const check = (condition, message) => { if (!condition) throw new Error(message); };
    const thrown = (f) => { try { f(); return 'none'; } catch (e) { return e.name; } };
    const NS = 'http://www.w3.org/2000/svg', XLINK = 'http://www.w3.org/1999/xlink';
    const make = (name, parent) => { const e = document.createElementNS(NS, name); if (parent) parent.append(e); return e; };
    const svg = make('svg', document.body);
    svg.setAttribute('width', '200'); svg.setAttribute('height', '100');
    const rect = make('rect', svg);

    // Interfaces by case-sensitive local name.
    check(rect instanceof SVGRectElement && make('feGaussianBlur') instanceof SVGFEGaussianBlurElement, 'element interfaces');
    check(make('feFuncR') instanceof SVGComponentTransferFunctionElement && make('symbol') instanceof SVGGraphicsElement, 'base interfaces');
    check(!(make('RECT') instanceof SVGRectElement) && make('RECT') instanceof SVGElement, 'local names are case-sensitive');
    check(make('metadata') instanceof SVGMetadataElement && make('clipPath') instanceof SVGClipPathElement, 'more interfaces');
    check(typeof SVGAnimateElement === 'undefined' && make('animate').constructor === SVGElement, 'SMIL is not exposed');
    check(thrown(() => Object.getOwnPropertyDescriptor(SVGRectElement.prototype, 'x').get.call(make('circle'))) === 'TypeError', 'brand check');
    check(thrown(() => new SVGLength()) === 'TypeError' && thrown(() => new SVGAnimatedLength()) === 'TypeError', 'no constructors');

    // SVGAnimatedLength and SVGLength.
    const x = rect.x;
    check(x instanceof SVGAnimatedLength && rect.x === x && x.baseVal === x.baseVal && x.baseVal !== x.animVal, '[SameObject]');
    check(x.baseVal.value === 0 && x.baseVal.unitType === SVGLength.SVG_LENGTHTYPE_NUMBER && x.baseVal.valueAsString === '0', 'initial length');
    rect.setAttribute('x', '10%');
    check(x.baseVal.unitType === 2 && x.baseVal.valueInSpecifiedUnits === 10 && x.baseVal.valueAsString === '10%', 'percentage');
    check(x.baseVal.value === 20 && x.animVal.value === 20, 'horizontal percentage ' + x.baseVal.value);
    rect.setAttribute('y', '50%');
    check(rect.y.baseVal.value === 50, 'vertical percentage ' + rect.y.baseVal.value);
    const circle = make('circle', svg);
    circle.setAttribute('r', '10%');
    check(Math.abs(circle.r.baseVal.value - Math.sqrt((200 * 200 + 100 * 100) / 2) / 10) < 1e-3, 'diagonal percentage ' + circle.r.baseVal.value);
    x.baseVal.value = 5;
    check(rect.getAttribute('x') === '5', 'value setter ' + rect.getAttribute('x'));
    x.baseVal.newValueSpecifiedUnits(SVGLength.SVG_LENGTHTYPE_CM, 2.54);
    check(rect.getAttribute('x') === '2.54cm' && Math.abs(x.baseVal.value - 96) < 1e-3, 'newValueSpecifiedUnits ' + rect.getAttribute('x'));
    x.baseVal.convertToSpecifiedUnits(SVGLength.SVG_LENGTHTYPE_MM);
    check(rect.getAttribute('x') === '25.4mm', 'convertToSpecifiedUnits ' + rect.getAttribute('x'));
    x.baseVal.valueAsString = '3in';
    check(x.baseVal.value === 288 && x.baseVal.unitType === SVGLength.SVG_LENGTHTYPE_IN, 'valueAsString');
    check(thrown(() => { x.baseVal.valueAsString = 'bogus'; }) === 'SyntaxError', 'invalid valueAsString');
    check(thrown(() => x.baseVal.newValueSpecifiedUnits(0, 1)) === 'NotSupportedError', 'unknown unit type');
    check(thrown(() => { x.animVal.value = 1; }) === 'NoModificationAllowedError', 'animVal is read only');
    rect.setAttribute('x', '2em');
    rect.style.fontSize = '20px';
    check(x.baseVal.value === 40, 'em ' + x.baseVal.value);
    rect.setAttribute('width', 'oops');
    check(rect.width.baseVal.value === 0 && rect.width.baseVal.unitType === 1, 'invalid attribute is the initial value');
    rect.setAttribute('x', '1vw');
    check(x.baseVal.unitType === 0 && x.baseVal.valueAsString === '1vw', 'other CSS units are unknown');
    const detached = svg.createSVGLength();
    detached.valueAsString = '50%';
    check(detached.value === 50, 'detached percentage basis is 100');

    // viewBox: SVGAnimatedRect's DOMRects reflect and reserialize.
    svg.setAttribute('viewBox', '0 0 400 200');
    const viewBox = svg.viewBox.baseVal;
    check(viewBox instanceof DOMRect && viewBox.width === 400 && svg.viewBox.baseVal === viewBox, 'viewBox');
    rect.setAttribute('x', '10%');
    check(x.baseVal.value === 40, 'percentage of the viewBox user space ' + x.baseVal.value);
    viewBox.width = 800;
    check(svg.getAttribute('viewBox') === '0 0 800 200', 'viewBox reserialized ' + svg.getAttribute('viewBox'));
    svg.setAttribute('viewBox', '1 2 3 4');
    check(viewBox.x === 1 && viewBox.height === 4 && svg.viewBox.animVal.y === 2, 'viewBox is live');
    check(thrown(() => { svg.viewBox.animVal.x = 5; }) === 'NoModificationAllowedError' && viewBox.x === 1, 'viewBox animVal is read only');
    svg.removeAttribute('viewBox');
    check(viewBox.width === 0, 'removed viewBox');

    // SVGAnimatedPreserveAspectRatio.
    const par = svg.preserveAspectRatio;
    check(par.baseVal.align === SVGPreserveAspectRatio.SVG_PRESERVEASPECTRATIO_XMIDYMID && par.baseVal.meetOrSlice === 1, 'initial par');
    par.baseVal.align = SVGPreserveAspectRatio.SVG_PRESERVEASPECTRATIO_NONE;
    check(svg.getAttribute('preserveAspectRatio') === 'none meet', 'align setter ' + svg.getAttribute('preserveAspectRatio'));
    svg.setAttribute('preserveAspectRatio', 'xMinYMax slice');
    check(par.baseVal.align === 8 && par.animVal.meetOrSlice === 2, 'par reflects');
    check(thrown(() => { par.baseVal.align = 0; }) === 'TypeError' && thrown(() => { par.animVal.align = 1; }) === 'NoModificationAllowedError', 'par errors');

    // Numbers, integers, booleans, enumerations and strings.
    const stop = make('stop'); stop.setAttribute('offset', '50%');
    check(stop.offset.baseVal === 0.5, 'stop offset percentage');
    const path = make('path'); path.pathLength.baseVal = 10;
    check(path.getAttribute('pathLength') === '10' && path.pathLength.animVal === 10, 'pathLength');
    const gradient = make('linearGradient');
    check(gradient.gradientUnits.baseVal === SVGUnitTypes.SVG_UNIT_TYPE_OBJECTBOUNDINGBOX, 'gradientUnits initial');
    gradient.gradientUnits.baseVal = 1;
    check(gradient.getAttribute('gradientUnits') === 'userSpaceOnUse', 'enumeration setter');
    check(thrown(() => { gradient.gradientUnits.baseVal = 0; }) === 'TypeError' && thrown(() => { gradient.spreadMethod.baseVal = 9; }) === 'TypeError', 'enumeration errors');
    gradient.setAttribute('spreadMethod', 'bogus');
    check(gradient.spreadMethod.baseVal === SVGGradientElement.SVG_SPREADMETHOD_PAD, 'invalid keyword is the initial value');
    check(gradient.x2.baseVal.valueAsString === '100%' && gradient.y2.baseVal.valueAsString === '0%', 'gradient length initial values');
    gradient.gradientTransform.baseVal.appendItem(svg.createSVGTransform());
    check(gradient.getAttribute('gradientTransform') === 'matrix(1, 0, 0, 1, 0, 0)', 'gradientTransform ' + gradient.getAttribute('gradientTransform'));
    const radial = make('radialGradient'); radial.setAttribute('cx', '30%');
    check(radial.fx.baseVal.valueAsString === '30%' && radial.fy.baseVal.valueAsString === '50%', 'fx defaults to cx');
    const convolve = make('feConvolveMatrix');
    check(convolve.preserveAlpha.baseVal === false && convolve.orderX.baseVal === 3 && convolve.divisor.baseVal === 1, 'convolve initial');
    convolve.preserveAlpha.baseVal = true;
    convolve.setAttribute('order', '3 5'); convolve.setAttribute('kernelMatrix', '1 2 3');
    check(convolve.getAttribute('preserveAlpha') === 'true' && convolve.orderY.baseVal === 5 && convolve.targetY.baseVal === 2, 'integer pair');
    check(convolve.divisor.baseVal === 6 && convolve.kernelMatrix.baseVal.length === 3, 'divisor is the kernel sum');
    convolve.orderY.baseVal = 7;
    check(convolve.getAttribute('order') === '3 7', 'integer pair setter ' + convolve.getAttribute('order'));
    check(convolve.edgeMode.baseVal === SVGFEConvolveMatrixElement.SVG_EDGEMODE_DUPLICATE, 'edgeMode');
    const blur = make('feGaussianBlur');
    blur.setStdDeviation(2, 4);
    check(blur.stdDeviationX.baseVal === 2 && blur.stdDeviationY.baseVal === 4 && blur.getAttribute('stdDeviation') === '2 4', 'setStdDeviation');
    blur.setAttribute('stdDeviation', '3');
    check(blur.stdDeviationY.baseVal === 3, 'implicit second number');
    check(blur.edgeMode.baseVal === 3 && blur.in1.baseVal === '' && blur.x.baseVal.valueAsString === '0%' && blur.width.baseVal.valueAsString === '100%', 'filter primitive defaults');
    blur.in1.baseVal = 'SourceAlpha';
    check(blur.getAttribute('in') === 'SourceAlpha', 'in1 reflects in');
    check(make('filter').x.baseVal.valueAsString === '-10%' && make('mask').maskUnits.baseVal === 2, 'filter and mask');
    check(make('feTurbulence').numOctaves.baseVal === 1 && make('feDropShadow').dx.baseVal === 2, 'filter initial values');
    const use = make('use');
    use.setAttributeNS(XLINK, 'xlink:href', '#a');
    check(use.href.baseVal === '#a', 'xlink:href fallback');
    use.href.baseVal = '#b';
    check(use.getAttributeNS(XLINK, 'href') === '#b' && !use.hasAttribute('href'), 'xlink:href setter');
    use.setAttribute('href', '#c');
    check(use.href.baseVal === '#c' && use.href.animVal === '#c', 'href wins');
    check(rect.className instanceof SVGAnimatedString, 'className');
    rect.className.baseVal = 'a b';
    check(rect.getAttribute('class') === 'a b' && rect.classList.contains('b'), 'className reflects class');
    const a = make('a'); a.target.baseVal = '_blank';
    check(a.getAttribute('target') === '_blank' && a.target instanceof SVGAnimatedString, 'SVGAElement target');
    const image = make('feImage'); image.setAttribute('crossorigin', 'bogus');
    check(image.crossOrigin.baseVal === 'anonymous', 'crossOrigin limited to known values');

    // Marker orientation: SVGAnimatedAngle and orientType.
    const marker = make('marker');
    check(marker.orientType.baseVal === SVGMarkerElement.SVG_MARKER_ORIENT_ANGLE && marker.orientAngle.baseVal.value === 0, 'orient initial');
    marker.setAttribute('orient', '45deg');
    check(marker.orientAngle.baseVal.value === 45 && marker.orientAngle.baseVal.unitType === SVGAngle.SVG_ANGLETYPE_DEG, 'orient angle');
    marker.setAttribute('orient', '0.5turn');
    check(marker.orientAngle.baseVal.value === 180 && marker.orientAngle.baseVal.unitType === 0, 'turn is unknown');
    marker.setOrientToAuto();
    check(marker.orient === 'auto' && marker.orientType.baseVal === 1, 'setOrientToAuto');
    const angle = svg.createSVGAngle(); angle.newValueSpecifiedUnits(SVGAngle.SVG_ANGLETYPE_RAD, 1);
    marker.setOrientToAngle(angle);
    check(marker.getAttribute('orient') === '1rad' && Math.abs(marker.orientAngle.animVal.value - 57.29578) < 1e-3, 'setOrientToAngle');
    marker.orientType.baseVal = SVGMarkerElement.SVG_MARKER_ORIENT_ANGLE;
    check(marker.getAttribute('orient') === '0', 'orientType setter');
    check(marker.markerWidth.baseVal.value === 3 && marker.markerUnits.baseVal === 2, 'marker initial values');

    // List interfaces.
    const text = make('text', svg);
    text.setAttribute('rotate', '10 20,30');
    const rotate = text.rotate.baseVal;
    check(rotate instanceof SVGNumberList && rotate.length === 3 && rotate.getItem(1).value === 20 && rotate[2].value === 30, 'number list');
    const first = rotate.getItem(0);
    rotate.appendItem(svg.createSVGNumber());
    check(text.getAttribute('rotate') === '10 20 30 0', 'appendItem ' + text.getAttribute('rotate'));
    first.value = 5;
    check(text.getAttribute('rotate') === '5 20 30 0', 'item reserializes');
    text.setAttribute('rotate', '1');
    check(rotate.length === 1 && rotate.getItem(0) === first && first.value === 1, 'synchronization keeps items');
    check(thrown(() => text.rotate.animVal.appendItem(svg.createSVGNumber())) === 'NoModificationAllowedError', 'animVal list is read only');
    check(thrown(() => rotate.getItem(5)) === 'IndexSizeError', 'getItem range');
    const appended = rotate.appendItem(first);
    check(appended !== first && appended.value === 1, 'an attached item is copied');
    text.setAttribute('x', '10 20%');
    const xs = text.x.baseVal;
    check(xs.length === 2 && xs.getItem(1).unitType === 2 && xs.getItem(1).value === 40, 'length list ' + xs.getItem(1).value);
    xs.insertItemBefore(svg.createSVGLength(), 0);
    check(text.getAttribute('x') === '0 10 20%', 'insertItemBefore ' + text.getAttribute('x'));
    const polygon = make('polygon', svg);
    polygon.setAttribute('points', '0,0 10,10 20');
    const points = polygon.points;
    check(points instanceof SVGPointList && points.length === 2 && points.getItem(1).x === 10 && points.getItem(0) instanceof DOMPoint, 'point list');
    points.getItem(1).x = 15;
    check(polygon.getAttribute('points') === '0,0 15,10', 'point reserializes ' + polygon.getAttribute('points'));
    points.appendItem(svg.createSVGPoint());
    check(polygon.getAttribute('points') === '0,0 15,10 0,0', 'appended point');
    check(polygon.animatedPoints !== points && polygon.animatedPoints.length === 3, 'animatedPoints is distinct');
    check(thrown(() => { polygon.animatedPoints.getItem(0).x = 1; }) === 'NoModificationAllowedError', 'animatedPoints items are read only');
    const group = make('g', svg);
    group.systemLanguage.appendItem('en'); group.systemLanguage.appendItem('fr');
    check(group.getAttribute('systemLanguage') === 'en, fr' && group.systemLanguage.getItem(1) === 'fr', 'systemLanguage');
    group.setAttribute('requiredExtensions', 'http://a.example/ http://b.example/');
    check(group.requiredExtensions.length === 2 && group.requiredExtensions instanceof SVGStringList, 'requiredExtensions');

    // SVGStyleElement includes LinkStyle.
    const style = make('style', svg);
    style.textContent = 'rect { fill: red }';
    check(style.sheet instanceof CSSStyleSheet && style.sheet.ownerNode === style && style.sheet.cssRules.length === 1, 'SVG style sheet');
    return 'svg-animated-values-ok';
})()
