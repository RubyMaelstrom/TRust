// CSSOM View Screen/Window dimensions, Web IDL attributes and Replaceable,
// HTML integration-with-idl. Values come from actual viewport geometry.
(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function throws(name, callback, C = globalThis[name]) {
        try { callback(); }
        catch (error) {
            check(error.name === name && error instanceof C, 'wrong exception: ' + error);
            return;
        }
        throw Error('missing ' + name);
    }
    const actual = screen;
    check(typeof Screen === 'function' && Screen.name === 'Screen' && Screen.length === 0, 'interface object');
    check(actual instanceof Screen && Object.getPrototypeOf(actual) === Screen.prototype, 'Screen prototype');
    check(Object.prototype.toString.call(actual) === '[object Screen]', 'Screen tag');
    check(Object.getPrototypeOf(Screen.prototype) === Object.prototype, 'interface prototype parent');
    throws('TypeError', () => Screen());
    throws('TypeError', () => new Screen());
    const attributes = ['availWidth', 'availHeight', 'width', 'height', 'colorDepth', 'pixelDepth'];
    for (const name of attributes) {
        const descriptor = Object.getOwnPropertyDescriptor(Screen.prototype, name);
        check(descriptor && descriptor.enumerable && descriptor.configurable &&
            typeof descriptor.get === 'function' && descriptor.set === undefined, 'readonly attribute ' + name);
        check(!Object.hasOwn(actual, name), 'attribute is inherited ' + name);
        check(descriptor.get.name === 'get ' + name && descriptor.get.length === 0, 'getter metadata ' + name);
        check(Function.prototype.toString.call(descriptor.get).includes('[native code]'), 'native getter reflection ' + name);
        throws('TypeError', () => Reflect.construct(descriptor.get, []));
        for (const receiver of [null, undefined, window, {}, Screen.prototype, Object.create(actual), new Proxy(actual, {})])
            throws('TypeError', () => descriptor.get.call(receiver));
        throws('TypeError', function () { 'use strict'; actual[name] = 1; });
        check(Number.isInteger(actual[name]), 'integer attribute ' + name);
    }
    check(actual.width === 640 && actual.height === 384 && actual.availWidth === 640 &&
        actual.availHeight === 384 && actual.colorDepth === 24 && actual.pixelDepth === 24, 'viewport screen area');
    const descriptors = {};
    for (const name of ['screen', 'innerWidth', 'innerHeight', 'outerWidth', 'outerHeight']) {
        const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
        descriptors[name] = descriptor;
        check(descriptor && descriptor.enumerable && descriptor.configurable &&
            typeof descriptor.get === 'function' && typeof descriptor.set === 'function', 'replaceable Window attribute ' + name);
        check(descriptor.get.name === 'get ' + name && descriptor.get.length === 0 &&
            descriptor.set.name === 'set ' + name && descriptor.set.length === 1, 'accessor metadata ' + name);
        check(descriptor.get.call(null) === globalThis[name] && descriptor.get.call(undefined) === globalThis[name], 'null receiver ' + name);
        for (const receiver of [{}, Object.create(window), 1]) {
            throws('TypeError', () => descriptor.get.call(receiver));
            throws('TypeError', () => descriptor.set.call(receiver, 1));
        }
        throws('TypeError', () => Reflect.construct(descriptor.get, []));
        throws('TypeError', () => Reflect.construct(descriptor.set, []));
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const frame = document.createElement('iframe');
    frame.style.cssText = 'width:300px;height:200px;border:0';
    body.appendChild(frame);
    const child = frame.contentWindow, childScreen = child.screen;
    check(child.Screen !== Screen && childScreen !== actual && childScreen instanceof child.Screen, 'per-Realm Screen');
    check(child.innerWidth === 300 && child.innerHeight === 200 && childScreen.width === 300 && childScreen.height === 200, 'nested viewport area');
    check(child.matchMedia('(device-width: 300px)').matches && child.matchMedia('(width: 300px)').matches, 'consistent nested media queries');
    const screenWidth = Object.getOwnPropertyDescriptor(Screen.prototype, 'width').get;
    const foreignWidth = Object.getOwnPropertyDescriptor(child.Screen.prototype, 'width').get;
    check(screenWidth.call(childScreen) === 300 && foreignWidth.call(actual) === 640, 'cross-Realm Screen getters');
    throws('TypeError', () => screenWidth.call(child));
    throws('TypeError', () => foreignWidth.call({}), child.TypeError);
    check(descriptors.screen.get.call(child) === childScreen && descriptors.innerWidth.get.call(child) === 300, 'WindowProxy getter receiver');
    const foreignInner = Object.getOwnPropertyDescriptor(child, 'innerWidth');
    check(foreignInner.get.call(window) === 640, 'borrowed Window getter reads receiver');
    Object.setPrototypeOf(childScreen, null);
    check(screenWidth.call(childScreen) === 300, 'brand survives prototype replacement');
    Object.setPrototypeOf(childScreen, child.Screen.prototype);
    frame.style.width = '351px';
    check(child.innerWidth === 351 && childScreen.width === 351, 'nested geometry is live before resize event');
    __trust.updateFrameResizes();
    const replacement = {valueOf() { throw Error('replacement was converted'); }};
    for (const name of Object.keys(descriptors)) {
        globalThis[name] = replacement;
        const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
        check(descriptor.value === replacement && descriptor.writable && descriptor.enumerable && descriptor.configurable, 'assignment replaces ' + name);
    }
    let resized = 0;
    window.addEventListener('resize', () => resized++);
    __trust.setViewport(812, 456);
    check(resized === 1 && innerWidth === replacement && screen === replacement, 'resize preserves author replacements');
    check(descriptors.innerWidth.get.call(window) === 812 && descriptors.innerHeight.get.call(window) === 456, 'saved accessors read live viewport');
    check(actual.width === 812 && actual.height === 456 && descriptors.screen.get.call(window) === actual, 'SameObject screen remains live');
    check(document.documentElement.clientWidth === 812 && document.documentElement.clientHeight === 456, 'DOM geometry ignores replaced Window properties');
    check(matchMedia('(width: 812px)').matches && matchMedia('(device-width: 812px)').matches, 'media queries ignore replaced Window properties');
    __trust.setViewport(812, 456);
    check(resized === 1, 'unchanged viewport does not fire resize');
    const originalRound = Math.round;
    try {
        Math.round = () => { throw Error('getter invoked author rounding'); };
        check(actual.width === 812 && descriptors.innerWidth.get.call(window) === 812, 'binding captures intrinsic rounding');
    } finally { Math.round = originalRound; }
    for (const name of Object.keys(descriptors)) Object.defineProperty(globalThis, name, descriptors[name]);
    foreignInner.set.call(child, replacement);
    frame.style.width = '389px';
    __trust.updateFrameResizes();
    check(child.innerWidth === replacement && foreignInner.get.call(child) === 389 && childScreen.width === 389, 'child replacement and live geometry coexist');
    const opaque = document.createElement('iframe');
    opaque.src = 'data:text/html,<p>opaque</p>'; body.appendChild(opaque);
    __trust.hydrateFrames();
    for (const name of Object.keys(descriptors)) {
        throws('SecurityError', () => descriptors[name].get.call(opaque.contentWindow), DOMException);
        throws('SecurityError', () => descriptors[name].set.call(opaque.contentWindow, 1), DOMException);
    }
    throws('SecurityError', () => screenWidth.call(opaque.contentWindow), DOMException);
    frame.style.display = 'none';
    check(foreignInner.get.call(child) === 0 && childScreen.width === 0 && childScreen.height === 0, 'hidden viewport is zero');
    check(typeof globalThis.__screen_binding === 'undefined', 'private binding consumed');
    return 'screen-interfaces-ok';
})();
