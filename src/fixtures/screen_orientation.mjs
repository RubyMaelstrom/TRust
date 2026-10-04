// Screen Orientation #screenorientation-interface with the specification's
// anti-fingerprinting mitigations, Web IDL operations and attributes, and the
// screen orientation change steps across navigables.
(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function throws(name, callback) {
        try { callback(); }
        catch (error) { check(error.name === name, 'wrong exception: ' + error); return; }
        throw Error('missing ' + name);
    }
    const orientation = screen.orientation;
    check(typeof ScreenOrientation === 'function' && ScreenOrientation.name === 'ScreenOrientation', 'interface object');
    throws('TypeError', () => new ScreenOrientation());
    check(Object.getPrototypeOf(ScreenOrientation.prototype) === EventTarget.prototype, 'EventTarget inheritance');
    check(orientation instanceof ScreenOrientation && screen.orientation === orientation, 'SameObject orientation');
    check(Object.prototype.toString.call(orientation) === '[object ScreenOrientation]', 'class string');
    const screenOrientation = Object.getOwnPropertyDescriptor(Screen.prototype, 'orientation');
    check(screenOrientation.enumerable && screenOrientation.get.name === 'get orientation' && !screenOrientation.set, 'Screen attribute');
    throws('TypeError', () => screenOrientation.get.call({}));
    for (const [name, length] of [['lock', 1], ['unlock', 0]]) {
        const d = Object.getOwnPropertyDescriptor(ScreenOrientation.prototype, name);
        check(d.writable && d.enumerable && d.configurable && d.value.length === length && d.value.name === name, 'operation ' + name);
        throws('TypeError', () => new d.value());
    }
    for (const name of ['type', 'angle']) {
        const d = Object.getOwnPropertyDescriptor(ScreenOrientation.prototype, name);
        check(d.enumerable && d.configurable && d.get.name === 'get ' + name && !d.set, 'readonly attribute ' + name);
        throws('TypeError', () => d.get.call(EventTarget.prototype));
    }
    check(orientation.type === 'landscape-primary' && orientation.angle === 0, 'initial landscape screen');
    check(orientation.unlock() === undefined, 'unlock without a lock');
    throws('TypeError', () => orientation.unlock.call({}));
    const results = globalThis.orientationLockResults = [];
    const record = promise => {
        check(promise instanceof Promise, 'lock returns a Promise');
        promise.then(() => results.push('resolved'), e => results.push(e.name + (e instanceof DOMException ? ':dom' : '')));
    };
    record(orientation.lock('portrait'));
    record(orientation.lock());
    record(orientation.lock('sideways'));
    record(orientation.lock(Symbol()));
    record(orientation.lock.call({}, 'any'));
    check(orientation.onchange === null, 'onchange default');
    const changes = [];
    orientation.onchange = e => changes.push('top:' + e.type + ':' + orientation.type + ':' + orientation.angle);
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const frame = document.createElement('iframe');
    frame.style.cssText = 'width:300px;height:200px;border:0';
    body.appendChild(frame);
    const child = frame.contentWindow, childOrientation = child.screen.orientation;
    check(childOrientation instanceof child.ScreenOrientation && childOrientation !== orientation, 'per-Realm orientation');
    check(childOrientation.type === 'landscape-primary' && childOrientation.angle === 0, 'child orientation follows the screen');
    childOrientation.addEventListener('change', () => changes.push('child:' + childOrientation.type + ':' + childOrientation.angle));
    __trust.setViewport(400, 700);
    check(orientation.type === 'landscape-primary' && changes.length === 0, 'slots change in a queued task');
    while (__trust.hasPlatformTask()) __trust.runPlatformTask();
    check(changes.join('|') === 'top:change:portrait-primary:90|child:portrait-primary:90', 'portrait change: ' + changes);
    check(matchMedia('(orientation: portrait)').matches && child.matchMedia('(device-height: 700px)').matches, 'media queries agree');
    changes.length = 0;
    __trust.setViewport(410, 700);
    while (__trust.hasPlatformTask()) __trust.runPlatformTask();
    check(changes.length === 0, 'only a portrait/landscape flip fires change');
    __trust.setViewport(900, 500);
    while (__trust.hasPlatformTask()) __trust.runPlatformTask();
    check(changes.join('|') === 'top:change:landscape-primary:0|child:landscape-primary:0', 'landscape change: ' + changes);
    orientation.onchange = null;
    check(orientation.onchange === null, 'onchange cleared');
    return 'screen-orientation-ok';
})();
