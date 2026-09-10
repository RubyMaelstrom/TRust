// Original Web IDL/CSSOM binding regression; no network or challenge program.
(function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    function typeError(operation, Constructor = TypeError) {
        let error;
        try { operation(); } catch (caught) { error = caught; }
        check(error instanceof Constructor, 'Expected binding TypeError');
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const element = document.createElement('div');
    element.style.display = 'flex'; body.appendChild(element);
    const style = getComputedStyle;
    check(style.length === 1 && style.name === 'getComputedStyle', 'operation metadata');
    const lengthDescriptor = Object.getOwnPropertyDescriptor(style, 'length');
    check(lengthDescriptor.configurable && !lengthDescriptor.writable && !lengthDescriptor.enumerable,
        'operation length descriptor');
    check(!Object.hasOwn(style, 'prototype'), 'nonconstructible operation has no prototype');
    typeError(() => style());
    for (const invalid of [undefined, null, 0, false, 1n, Symbol('invalid'), 'div', {}, document,
        document.createTextNode('text'), Object.create(Element.prototype),
        new Proxy(element, {})]) typeError(() => style(invalid));
    typeError(() => new style(element));
    typeError(() => style.call({}, element));
    let touched = 0;
    const forged = Object.create(Element.prototype);
    Object.defineProperty(forged, '__id', {get() { touched++; return element.__id; }});
    typeError(() => style(forged));
    check(touched === 0, 'interface conversion must not inspect author getters');
    const trap = new Proxy({}, {getPrototypeOf() { touched++; throw Error('trap'); }});
    typeError(() => style(trap));
    check(touched === 0, 'interface conversion must not inspect Proxy prototypes');
    const pseudoError = {};
    let converted = 0;
    try { style(element, {toString() { converted++; throw pseudoError; }}); }
    catch (error) { check(error === pseudoError, 'preserve conversion error identity'); }
    check(converted === 1, 'convert optional pseudo-element argument');
    typeError(() => style(element, Symbol('invalid pseudo')));
    typeError(() => style(null, {toString() { touched++; return ''; }}));
    check(touched === 0, 'element conversion precedes pseudo-element conversion');
    check(style(element).display === 'flex', 'ordinary cascade');
    check(style.call(null, element).display === 'flex', 'null Window receiver');
    const weakGet = WeakMap.prototype.get;
    try {
        WeakMap.prototype.get = function () { throw Error('author WeakMap getter'); };
        check(style(element).display === 'flex', 'interface conversion uses captured intrinsic');
        typeError(() => style({}));
    } finally { WeakMap.prototype.get = weakGet; }
    const proto = Object.getPrototypeOf(element);
    Object.setPrototypeOf(element, null);
    check(style(element).display === 'flex', 'brand survives prototype replacement');
    Object.setPrototypeOf(element, proto);
    for (const namespace of ['http://www.w3.org/2000/svg', 'http://www.w3.org/1998/Math/MathML', null]) {
        const node = document.createElementNS(namespace, 'test');
        node.setAttribute('style', 'display:inline-block'); body.appendChild(node);
        check(style(node).display === 'inline-block', 'namespaced Element interface');
    }
    const parsed = document.createElement('section');
    parsed.innerHTML = '<span style="display:inline-grid"></span>'; body.appendChild(parsed);
    check(style(parsed.firstElementChild).display === 'inline-grid', 'parser wrapper');
    const clone = parsed.firstElementChild.cloneNode(true); body.appendChild(clone);
    check(style(clone).display === 'inline-grid', 'cloned wrapper');
    class CustomElement extends HTMLElement { constructor() { super(); this.style.display = 'flex'; } }
    customElements.define('x-computed-style-binding', CustomElement);
    for (const custom of [new CustomElement(), document.createElement('x-computed-style-binding')]) {
        body.appendChild(custom);
        check(style(custom).display === 'flex', 'custom-element creation/upgrade brand');
    }
    const frame = document.createElement('iframe'); body.appendChild(frame);
    const foreign = frame.contentWindow;
    foreign.eval('if (!document.documentElement) { const h=document.createElement("html"), b=document.createElement("body"); document.append(h); h.append(b); }');
    const other = foreign.document.createElement('div');
    other.style.display = 'grid'; foreign.document.body.appendChild(other);
    check(style(other).display === 'grid', 'same-Agent foreign element');
    check(foreign.getComputedStyle(element).display === 'flex', 'foreign method, local element');
    check(style.call(foreign, element).display === 'flex', 'same-origin WindowProxy receiver');
    typeError(() => foreign.getComputedStyle({}), foreign.TypeError);
    Object.defineProperty(Element, Symbol.hasInstance, {value: () => true, configurable: true});
    typeError(() => style({}));
    delete Element[Symbol.hasInstance];
    const live = style(other);
    other.style.display = 'block';
    check(live.display === 'block', 'computed declarations remain live');
    const id = element.__id;
    Object.defineProperty(element, '__id', {get() { touched++; throw Error('forged node id'); }, configurable: true});
    check(style(element).display === 'flex', 'native element identity, not public node id');
    check(touched === 0, 'valid interface conversion must not read public node id');
    Object.defineProperty(element, '__id', {value: id, writable: true, configurable: true});
    const opaque = document.createElement('iframe');
    opaque.src = 'data:text/html,<p>opaque</p>'; body.appendChild(opaque);
    __trust.hydrateFrames();
    let security;
    try { style.call(opaque.contentWindow, element); } catch (error) { security = error; }
    check(security instanceof DOMException && security.name === 'SecurityError', 'Window security check');
    check(globalThis.__element_slots === undefined, 'private binding consumed');
    return 'computed-style-bindings-ok';
})();
