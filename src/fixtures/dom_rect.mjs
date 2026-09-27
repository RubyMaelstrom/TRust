(() => {
    // Geometry 1 #DOMRect, #structured-serialization; Web IDL
    // #js-to-dictionary, #dfn-attribute-setter, #js-default-tojson.
    function check(value, message) { if (!value) throw new Error(message); }
    function typeError(fn, message) {
        try { fn(); } catch (e) { check(e instanceof TypeError, message + ': error brand'); return; }
        throw new Error(message + ': did not throw');
    }
    const desc = Object.getOwnPropertyDescriptor;
    const ro = new DOMRectReadOnly(-0, 7, -2, -3), rect = new DOMRect(1, 2, 3, 4);
    check(Reflect.ownKeys(ro).length === 0 && Reflect.ownKeys(rect).length === 0, 'private slots');
    check(Object.getPrototypeOf(DOMRect) === DOMRectReadOnly, 'constructor inheritance');
    check(Object.getPrototypeOf(DOMRect.prototype) === DOMRectReadOnly.prototype, 'prototype inheritance');
    check(DOMRect.length === 0 && DOMRectReadOnly.length === 0, 'optional constructor length');
    check(DOMRect.fromRect.length === 0 && DOMRectReadOnly.prototype.toJSON.length === 0, 'operation length');
    for (const ctor of [DOMRect, DOMRectReadOnly]) {
        const globalDescriptor = desc(globalThis, ctor.name);
        check(globalDescriptor.writable && globalDescriptor.configurable && !globalDescriptor.enumerable, 'global interface descriptor');
        const p = desc(ctor, 'prototype'), m = desc(ctor, 'fromRect');
        check(!p.writable && !p.enumerable && !p.configurable, 'interface prototype descriptor');
        check(m.writable && m.enumerable && m.configurable, 'static operation descriptor');
        typeError(() => ctor(), 'constructor requires new');
        typeError(() => new ctor(1n), 'constructor BigInt');
        typeError(() => new ctor(Symbol()), 'constructor Symbol');
        for (const value of [false, 2, 'x', 1n, Symbol()])
            typeError(() => ctor.fromRect(value), 'dictionary rejects primitives');
        check(ctor.fromRect(null).width === 0 && ctor.fromRect().x === 0, 'dictionary defaults');
        check(ctor.fromRect.call(null, {x: 8}).x === 8, 'static operation ignores receiver');
        typeError(() => new ctor.fromRect(), 'static operation not a constructor');
    }
    for (const name of ['x', 'y', 'width', 'height', 'top', 'right', 'bottom', 'left']) {
        const d = desc(DOMRectReadOnly.prototype, name);
        check(d.enumerable && d.configurable && d.set === undefined, 'readonly descriptor ' + name);
        check(d.get.length === 0 && d.get.name === 'get ' + name, 'getter identity ' + name);
        typeError(() => new d.get(), 'getter not constructor');
        for (const receiver of [null, undefined, 4, {}, Object.create(ro), new Proxy(rect, {})])
            typeError(() => d.get.call(receiver), 'readonly getter brand ' + name);
        if (['x', 'y', 'width', 'height'].includes(name)) {
            const m = desc(DOMRect.prototype, name);
            check(m.enumerable && m.configurable && m.set.length === 1 && m.get.length === 0, 'mutable descriptor');
            check(m.set.name === 'set ' + name, 'setter name');
            typeError(() => m.get.call(ro), 'mutable getter rejects readonly');
            let converted = false;
            typeError(() => m.set.call(ro, {valueOf() { converted = true; return 10; }}), 'setter checks brand');
            check(!converted, 'setter brand before conversion');
            m.set.call(rect, {valueOf() { return -5; }});
            check(m.get.call(rect) === -5, 'setter conversion');
            typeError(() => m.set.call(rect, 1n), 'setter BigInt');
            check(m.get.call(rect) === -5, 'throw leaves slot unchanged');
            m.set.call(rect);
            check(Number.isNaN(m.get.call(rect)), 'missing setter value is NaN');
        } else check(!desc(DOMRect.prototype, name), 'derived edges inherited');
    }
    let order = [];
    const args = ['x', 'y', 'width', 'height'].map((k, i) => ({valueOf() { order.push(k); return i + 1; }}));
    const numbers = new DOMRect(args[0], args[1], args[2], args[3]);
    check(order.join() === 'x,y,width,height', 'constructor conversion order');
    order = [];
    const newTarget = new Proxy(function Target() {}, {get(target, key) {
        if (key === 'prototype') order.push('prototype');
        return Reflect.get(target, key);
    }});
    const constructed = Reflect.construct(DOMRect, args, newTarget);
    check(order.join() === 'x,y,width,height,prototype', 'IDL conversions precede prototype lookup');
    check(DOMRectReadOnly.prototype.toJSON.call(constructed).x === 1, 'custom newTarget platform brand');
    order = [];
    const dict = new Proxy({}, {get(_, k) { order.push('get ' + k); return {valueOf() { order.push('number ' + k); return 6; }}; }});
    const copied = DOMRectReadOnly.fromRect(dict);
    check(order.join() === 'get height,number height,get width,number width,get x,number x,get y,number y', 'dictionary conversion order');
    check(copied.x === 6 && copied.height === 6, 'converted dictionary');
    const sentinel = {};
    order = [];
    try {
        DOMRect.fromRect({get height() { order.push('h'); return {valueOf() { throw sentinel; }}; }, get width() { order.push('w'); }});
        throw new Error('missing abrupt completion');
    } catch (e) { check(e === sentinel && order.join() === 'h', 'dictionary abrupt completion'); }
    const inherited = DOMRect.fromRect(Object.create({x: 9, height: null}));
    check(inherited.x === 9 && inherited.height === 0, 'inherited dictionary members');
    class Child extends DOMRect {}
    check(Child.fromRect() instanceof DOMRect && !(Child.fromRect() instanceof Child), 'static factory primary interface');
    check(new Child(4).x === 4 && new Child() instanceof Child, 'subclass construction');
    const readonlyWithMutablePrototype = Reflect.construct(DOMRectReadOnly, [13], DOMRect);
    check(desc(DOMRectReadOnly.prototype, 'x').get.call(readonlyWithMutablePrototype) === 13, 'primary interface does not follow prototype');
    typeError(() => desc(DOMRect.prototype, 'x').set.call(readonlyWithMutablePrototype, 3), 'readonly brand survives Reflect.construct');
    const oldSuper = Object.getPrototypeOf(DOMRect);
    try {
        Object.setPrototypeOf(DOMRect, function () { throw new Error('author superclass'); });
        check(new DOMRect(11).x === 11, 'IDL constructor does not invoke mutable superclass');
    } finally { Object.setPrototypeOf(DOMRect, oldSuper); }
    check(Object.is(ro.x, -0) && ro.left === -2 && Object.is(ro.right, -0), 'negative dimensions/signed zero');
    const zero = new DOMRect(-0, -0, 0, 0);
    check(Object.is(zero.left, -0) && Object.is(zero.top, -0) && Object.is(zero.right, 0) && Object.is(zero.bottom, 0), 'signed zero extrema');
    for (const value of [new DOMRect(NaN, NaN, 1, 1), new DOMRect(1, 1, NaN, NaN), new DOMRect(Infinity, -Infinity, -Infinity, Infinity)])
        check(['top', 'right', 'bottom', 'left'].every(k => Number.isNaN(value[k])), 'NaN propagates to edges');
    const toJSON = DOMRectReadOnly.prototype.toJSON;
    Object.defineProperty(numbers, 'x', {get() { throw new Error('author coordinate getter'); }, enumerable: true});
    Object.defineProperty(numbers, 'right', {get() { throw new Error('author edge getter'); }});
    let writes = 0;
    Object.defineProperty(Object.prototype, 'x', {set() { writes++; }, configurable: true});
    let json;
    try { json = toJSON.call(numbers); } finally { delete Object.prototype.x; }
    check(writes === 0 && json.x === 1 && json.right === 4, 'toJSON uses slots and own data properties');
    check(Object.keys(json).join() === 'x,y,width,height,top,right,bottom,left', 'toJSON property order');
    check(Object.getPrototypeOf(json) === Object.prototype && json !== toJSON.call(numbers), 'fresh ordinary JSON object');
    typeError(() => toJSON.call({}), 'toJSON brand');
    const special = new DOMRect(-0, NaN, Infinity, -Infinity);
    const graph = {a: special, b: special, readonly: ro}; graph.self = graph;
    const cloned = structuredClone(graph);
    check(cloned.self === cloned && cloned.a === cloned.b && cloned.a !== special, 'clone graph identity');
    check(Object.getPrototypeOf(cloned.a) === DOMRect.prototype && Object.getPrototypeOf(cloned.readonly) === DOMRectReadOnly.prototype, 'clone primary interfaces');
    check(Object.is(cloned.a.x, -0) && Number.isNaN(cloned.a.y) && cloned.a.width === Infinity && cloned.a.height === -Infinity, 'clone unrestricted doubles');
    check(structuredClone(numbers).x === 1, 'clone ignores author properties');
    let trapped = false;
    for (const target of [numbers, {}]) {
        try { structuredClone(new Proxy(target, {get() { trapped = true; throw new Error('proxy trap'); }})); }
        catch (e) { check(e.name === 'DataCloneError', 'proxy clone error'); continue; }
        throw new Error('proxy was cloned');
    }
    check(!trapped, 'clone proxy rejection precedes author traps');
    const rebranded = structuredClone(readonlyWithMutablePrototype);
    check(Object.getPrototypeOf(rebranded) === DOMRectReadOnly.prototype && rebranded.x === 13, 'clone uses primary interface not prototype');
    check(Reflect.ownKeys(cloned.a).length === 0, 'clone private slots');
    const channel = new MessageChannel();
    globalThis.rectangleMessageResult = 'pending';
    channel.port1.onmessage = e => {
        const r = e.data;
        rectangleMessageResult = Object.getPrototypeOf(r) === DOMRect.prototype
            && Object.is(r.x, -0) && Number.isNaN(r.y) && r.width === Infinity ? 'ok' : 'bad';
        channel.port1.close(); channel.port2.close();
    };
    channel.port2.postMessage(special);
    check(typeof __geometry_bind === 'undefined' && typeof __geometry_codec === 'undefined', 'bootstrap capabilities removed');
    check(typeof document === 'undefined' ? typeof SVGRect === 'undefined' : SVGRect === DOMRect, 'window-only legacy alias');
    return 'dom-rect-ok';
})()
