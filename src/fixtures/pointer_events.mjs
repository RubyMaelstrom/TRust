(function () {
    function assert(value, message) { if (!value) throw Error(message); }
    function throws(fn, message) {
        let error; try { fn(); } catch (e) { error = e; }
        assert(error && error.name === 'TypeError', message);
    }
    const defaults = {pointerId:0, width:1, height:1, pressure:0, tangentialPressure:0,
        tiltX:0, tiltY:0, twist:0, altitudeAngle:Math.PI/2, azimuthAngle:0,
        pointerType:'', isPrimary:false, persistentDeviceId:0};
    for (const options of [undefined, null, {}]) {
        const event = new PointerEvent('pointermove', options);
        for (const key of Object.keys(defaults)) assert(event[key] === defaults[key], 'default ' + key);
        assert(event.detail === 0 && event.button === 0 && event.buttons === 0 && event.view === null, 'inherited defaults');
        assert(event.ctrlKey === false && event.altKey === false && event.shiftKey === false && event.metaKey === false, 'modifier defaults');
        assert(!event.isTrusted && event instanceof MouseEvent && Object.prototype.toString.call(event) === '[object PointerEvent]', 'interface identity');
        assert(event.getCoalescedEvents().length === 0 && event.getPredictedEvents().length === 0, 'empty lists');
    }
    throws(() => new PointerEvent(), 'required type');
    throws(() => new PointerEvent(Symbol()), 'DOMString type');
    throws(() => new PointerEvent('x', 1), 'dictionary type');
    let inheritedReads = 0;
    Object.defineProperty(Object.prototype, 'pointerId', {configurable:true,
        get() { inheritedReads++; return 123; }});
    try {
        assert(new PointerEvent('x').pointerId === 0, 'omitted dictionary does not read Object.prototype');
        assert(new PointerEvent('x', undefined).pointerId === 0, 'undefined dictionary does not read Object.prototype');
        assert(new PointerEvent('x', null).pointerId === 0 && inheritedReads === 0, 'null dictionary does not read Object.prototype');
        assert(new PointerEvent('x', {}).pointerId === 123 && inheritedReads === 1, 'explicit dictionary reads inherited members');
    } finally { delete Object.prototype.pointerId; }
    throws(() => new PointerEvent('x', {pointerType:Symbol()}), 'DOMString pointerType');
    for (const key of ['width', 'height', 'pressure', 'tangentialPressure', 'altitudeAngle', 'azimuthAngle']) {
        for (const value of [NaN, Infinity, -Infinity, 1n]) throws(() => new PointerEvent('x', {[key]:value}), 'finite ' + key);
    }
    throws(() => new PointerEvent('x', {pressure:1e100}), 'float overflow');
    const values = new PointerEvent('x', {pointerId:4294967297.9, twist:-1.9,
        pressure:0.1, tangentialPressure:-0.1, pointerType:123, isPrimary:1});
    assert(values.pointerId === 1 && values.twist === -1, 'long conversion');
    assert(values.pressure === Math.fround(0.1) && values.tangentialPressure === Math.fround(-0.1), 'float conversion');
    assert(values.pointerType === '123' && values.isPrimary, 'string/boolean conversion');
    assert(Object.is(new PointerEvent('x', {pressure:-0}).pressure, -0), 'negative zero');
    const tilt = new PointerEvent('x', {tiltX:45, tiltY:-30});
    assert(Math.abs(tilt.altitudeAngle - 0.7137243789447657) < 1e-12 && Math.abs(tilt.azimuthAngle - 5.759586531581287) < 1e-12, 'tilt to spherical');
    const spherical = new PointerEvent('x', {altitudeAngle:Math.PI/4, azimuthAngle:Math.PI});
    assert(spherical.tiltX === -45 && spherical.tiltY === 0, 'spherical to tilt');
    for (const [azimuthAngle, x, y] of [[0,90,0],[Math.PI/2,0,90],[Math.PI,-90,0],[3*Math.PI/2,0,-90],[Math.PI/4,90,90]]) {
        const event = new PointerEvent('x', {altitudeAngle:0, azimuthAngle});
        assert(event.tiltX === x && event.tiltY === y, 'horizontal transducer');
    }
    const explicit = new PointerEvent('x', {tiltX:10, tiltY:20, altitudeAngle:0.4, azimuthAngle:0.6});
    assert(explicit.tiltX === 10 && explicit.tiltY === 20 && explicit.altitudeAngle === 0.4 && explicit.azimuthAngle === 0.6, 'explicit angles preserved');
    for (const key of Object.keys(defaults)) {
        const descriptor = Object.getOwnPropertyDescriptor(PointerEvent.prototype, key);
        assert(descriptor && descriptor.get.name === 'get ' + key && descriptor.get.length === 0 && !descriptor.set && descriptor.enumerable && descriptor.configurable, 'IDL descriptor ' + key);
        assert(!Object.prototype.hasOwnProperty.call(descriptor.get, 'prototype'), 'getter has no prototype ' + key);
        throws(() => Reflect.construct(function () {}, [], descriptor.get), 'getter is not a constructor ' + key);
        throws(() => descriptor.get.call({}), 'getter brand ' + key);
        throws(() => descriptor.get.call(Object.create(PointerEvent.prototype)), 'forged brand ' + key);
        assert(!Object.prototype.hasOwnProperty.call(values, key), 'private attribute ' + key);
    }
    const source = [values], event = new PointerEvent('x', {coalescedEvents:source, predictedEvents:source});
    source.length = 0;
    assert(event.getCoalescedEvents()[0] === values && event.getPredictedEvents()[0] === values, 'constructor list snapshots');
    event.getCoalescedEvents().length = 0;
    assert(event.getCoalescedEvents().length === 1 && !('coalescedEvents' in event), 'fresh list, no expando');
    for (const key of ['coalescedEvents', 'predictedEvents']) {
        for (const value of [null, 1, 'text', {}, [{}], [new MouseEvent('x')]]) throws(() => new PointerEvent('x', {[key]:value}), 'sequence ' + key);
    }
    for (const name of ['getCoalescedEvents', 'getPredictedEvents']) {
        assert(PointerEvent.prototype[name].length === 0, 'method arity');
        assert(!Object.prototype.hasOwnProperty.call(PointerEvent.prototype[name], 'prototype'), 'method has no prototype ' + name);
        throws(() => Reflect.construct(function () {}, [], PointerEvent.prototype[name]), 'method is not a constructor ' + name);
        throws(() => PointerEvent.prototype[name].call({}), 'method brand');
    }
    assert(Object.getOwnPropertyNames(PointerEvent.prototype).join(',') === Object.keys(defaults).join(',') + ',getCoalescedEvents,getPredictedEvents,constructor', 'interface member definition order');
    const reads = [], options = new Proxy({}, {get(target, key) { reads.push(key); }});
    new PointerEvent('x', options);
    assert(reads.join(',') === 'bubbles,cancelable,composed,detail,view,altKey,ctrlKey,metaKey,modifierAltGraph,modifierCapsLock,modifierFn,modifierFnLock,modifierHyper,modifierNumLock,modifierScrollLock,modifierSuper,modifierSymbol,modifierSymbolLock,shiftKey,button,buttons,clientX,clientY,movementX,movementY,relatedTarget,screenX,screenY,altitudeAngle,azimuthAngle,coalescedEvents,height,isPrimary,persistentDeviceId,pointerId,pointerType,predictedEvents,pressure,tangentialPressure,tiltX,tiltY,twist,width', 'Web IDL dictionary read order: ' + reads);
    const effectful = {pointerId:1, height:{valueOf() { effectful.pointerId = 9; return 2; }}};
    assert(new PointerEvent('x', effectful).pointerId === 9, 'conversion happens before the next member Get');
    let laterReads = 0;
    throws(() => new PointerEvent('x', {height:Infinity, get pointerId() { laterReads++; return 1; }}), 'conversion failure');
    assert(laterReads === 0, 'conversion failure stops subsequent member Gets');
    const warmed = {pointerId:7, height:2};
    for (let i = 0; i < 120; i++) assert(new PointerEvent('x', warmed).pointerId === 7, 'warm dictionary');
    Object.defineProperty(warmed, 'pointerId', {get() { return 8; }});
    Object.setPrototypeOf(warmed, {pointerType:'pen'});
    const changed = new PointerEvent('x', warmed);
    assert(changed.pointerId === 8 && changed.pointerType === 'pen', 'named caches preserve accessor/prototype mutations');
    let unknownReads = 0;
    const ignored = new PointerEvent('x', {get notInTheDictionary() { unknownReads++; return 1; }});
    assert(!('notInTheDictionary' in ignored) && unknownReads === 0, 'unknown init members ignored');
    assert(!('modifierAltGraph' in ignored), 'modifier dictionary is not public attributes');
    const iterator = Array.prototype[Symbol.iterator];
    try {
        Array.prototype[Symbol.iterator] = function () { throw Error('author array iterator'); };
        assert(new PointerEvent('x').getPredictedEvents().length === 0, 'private field lists do not invoke author iterators');
        assert(event.getCoalescedEvents()[0] === values, 'list result does not invoke author iterators');
    } finally { Array.prototype[Symbol.iterator] = iterator; }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const button = document.createElement('button'); body.appendChild(button);
    const seen = [];
    for (const type of ['pointermove', 'pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click'])
        button.addEventListener(type, event => seen.push(event));
    __trust.hover(button.__id, 20, 30, 120, 130);
    __trust.pointerButton(button.__id, true, 20, 30, 120, 130);
    __trust.pointerButton(button.__id, false, 20, 30, 120, 130);
    __trust.click(button.__id);
    assert(seen.map(event => event.type).join(',') === 'pointermove,pointerdown,mousedown,pointerup,mouseup,click', 'native sequence');
    for (const event of seen) {
        assert(event.isTrusted && event.view === window && event.clientX === 20 && event.screenX === 120, 'native coordinates/trust');
        assert(event.detail === (event.type.startsWith('pointer') ? 0 : 1), 'native detail ' + event.type);
        if (event instanceof PointerEvent) {
            assert(event.width === 1 && event.altitudeAngle === Math.PI/2, 'native pointer defaults');
            assert(event.pointerId === 1 && event.pointerType === 'mouse', 'native pointer identity');
        } else assert(!('pointerId' in event), 'compatibility mouse interface');
    }
    const move = seen[0], leaf = move.getCoalescedEvents()[0];
    assert(move.button === -1 && leaf && leaf.isTrusted && leaf !== move && leaf.timeStamp <= move.timeStamp && leaf.clientX === move.clientX, 'real movement coalesced sample');
    assert(leaf.getCoalescedEvents().length === 0 && move.getPredictedEvents().length === 0, 'no invented predictions');
    assert(seen[1].pressure === 0.5 && seen[3].pressure === 0, 'native pressure');
    assert(seen[5].isPrimary === false && seen[5].pressure === 0, 'click pointer-specific defaults');
    const nativeEvent = seen[5];
    const weakHas = WeakSet.prototype.has, weakAdd = WeakSet.prototype.add, weakDelete = WeakSet.prototype.delete;
    try {
        WeakSet.prototype.has = () => true;
        assert(!new PointerEvent('x').isTrusted, 'author cannot forge trust through WeakSet.has');
        WeakSet.prototype.has = () => false;
        assert(nativeEvent.isTrusted, 'author cannot hide native trust through WeakSet.has');
        WeakSet.prototype.delete = () => false;
        button.dispatchEvent(nativeEvent);
        assert(!nativeEvent.isTrusted, 'redispatch clears trust independently of author intrinsics');
        WeakSet.prototype.add = () => {};
        __trust.click(button.__id);
        assert(seen[seen.length - 1].isTrusted, 'native event creation uses internal trust state');
    } finally {
        WeakSet.prototype.has = weakHas; WeakSet.prototype.add = weakAdd; WeakSet.prototype.delete = weakDelete;
    }
    seen.length = 0; button.click();
    assert(seen.length === 1 && !seen[0].isTrusted && seen[0].detail === 0 && seen[0].pointerId === -1 && seen[0].pointerType === '' && !seen[0].isPrimary && seen[0].clientX === 0, 'script click is not mouse input');
    const frame = document.createElement('iframe'); frame.srcdoc = '<body></body>'; body.appendChild(frame); __trust.hydrateFrames();
    const childEvent = new frame.contentWindow.PointerEvent('x', {pointerId:12});
    const borrowed = Object.getOwnPropertyDescriptor(PointerEvent.prototype, 'pointerId').get;
    assert(borrowed.call(childEvent) === 12, 'cross-realm getter brand');
    assert(new PointerEvent('x', {coalescedEvents:[childEvent]}).getCoalescedEvents()[0] === childEvent, 'cross-realm sequence identity');
    Object.setPrototypeOf(childEvent, null);
    assert(borrowed.call(childEvent) === 12, 'brand independent of prototype');
    assert(typeof globalThis.__pointer_event_slots === 'undefined', 'host slots are private');
    return 'pointer-events-ok';
})()
