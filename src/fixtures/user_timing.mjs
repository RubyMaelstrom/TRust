// Original User Timing / Performance Timeline regressions; no network or site data.
(function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    function throws(name, fn) {
        try { fn(); } catch (error) { check(error.name === name, 'expected ' + name + ', got ' + error.name); return; }
        throw Error('missing ' + name);
    }
    performance.clearMarks(); performance.clearMeasures();
    const detail = { number: 7, bytes: new Uint8Array([1, 2, 3]) }; detail.self = detail;
    const first = performance.mark('first', { startTime: 17.25, detail });
    check(first && first.entryType === 'mark', 'mark must return a recorded PerformanceMark');
    check(first instanceof PerformanceMark && first instanceof PerformanceEntry, 'mark interfaces');
    check(first.name === 'first' && first.startTime === 17.25 && first.duration === 0, 'mark fields');
    check(first.detail !== detail && first.detail.self === first.detail && first.detail.bytes[1] === 2, 'structured-cloned cyclic mark detail');
    detail.bytes[1] = 9; check(first.detail.bytes[1] === 2, 'detail bytes do not alias source');
    const json = first.toJSON();
    check(json.name === 'first' && json.entryType === 'mark' && json.startTime === 17.25 && json.duration === 0 && !Object.hasOwn(json,'detail'), 'entry JSON includes only the declaring interface attributes');
    check(performance.getEntries().includes(first) && performance.getEntriesByName('first', 'mark')[0] === first, 'record identity');
    check(performance.getEntriesByType('MARK').length === 0 && performance.getEntriesByName('absent').length === 0, 'exact filtering');
    const detached = new PerformanceMark('constructor-only', { startTime: 3 });
    check(detached.detail === null && performance.getEntriesByName('constructor-only').length === 0, 'constructor does not record');
    throws('TypeError', () => new PerformanceEntry());
    throws('TypeError', () => new PerformanceMeasure());
    throws('TypeError', () => PerformanceMark('without-new'));
    throws('TypeError', () => performance.mark());
    throws('TypeError', () => new PerformanceMark());
    throws('TypeError', () => performance.mark(Symbol()));
    throws('TypeError', () => performance.mark('bad', 1));
    for (const startTime of [-1, NaN, Infinity, -Infinity, 1n, Symbol()])
        throws('TypeError', () => performance.mark('bad', { startTime }));
    throws('DataCloneError', () => performance.mark('bad', { detail() {} }));
    throws('TypeError', () => performance.mark('bad', { startTime: -1, detail() {} }));
    check(performance.getEntriesByName('bad').length === 0, 'failed marks do not record');
    check(performance.mark(undefined, { startTime: 0 }).name === 'undefined', 'required DOMString undefined');
    check(performance.mark('constructor', null).name === 'constructor', 'ordinary names and null dictionary');
    const order = [];
    performance.mark({ toString() { order.push('name'); return 'ordered'; } }, {
        get detail() { order.push('detail'); return { get value() { order.push('clone'); return 1; } }; },
        get startTime() { order.push('startTime'); return { valueOf() { order.push('number'); return 2; } }; }
    });
    check(order.join(',') === 'name,detail,startTime,number,clone', 'dictionary conversion precedes clone');
    const a = performance.mark('repeat', { startTime: 50 });
    const b = performance.mark('repeat', { startTime: 4 });
    const measured = performance.measure('span', { start: 'repeat', end: 10, detail: { ok: true } });
    check(measured instanceof PerformanceMeasure && measured instanceof PerformanceEntry, 'measure interfaces');
    check(measured.startTime === 50 && measured.duration === -40 && measured.detail.ok === true, 'most recent timestamp and measure detail');
    check(performance.measure('negative', { start: 10, end: 4 }).duration === -6, 'negative measured durations are valid');
    check(performance.measure('duration-end', { duration: 3, end: 10 }).startTime === 7, 'duration/end');
    check(performance.measure('start-duration', { start: 10, duration: 3 }).duration === 3, 'start/duration');
    check(performance.measure('legacy', 'repeat', 'first').duration === -32.75, 'legacy string marks');
    check(performance.measure('empty', null).startTime === 0, 'null measure options');
    for (const options of [{ detail: 1 }, { duration: 3 }, { start: 1, end: 2, duration: 1 }, { start: -1 }, { end: Infinity }])
        throws('TypeError', () => performance.measure('invalid', options));
    throws('TypeError', () => performance.measure('invalid', { start: 0 }, 'first'));
    throws('SyntaxError', () => performance.measure('absent', 'absent'));
    throws('TypeError', () => performance.measure());
    throws('DataCloneError', () => performance.measure('invalid', { start: 0, detail: Symbol() }));
    const sorted = performance.getEntriesByType('mark');
    check(sorted.indexOf(b) < sorted.indexOf(first) && sorted.indexOf(first) < sorted.indexOf(a), 'chronological retrieval');
    sorted.length = 0; check(performance.getEntriesByType('mark').length > 0, 'retrieval returns an independent array');
    performance.clearMarks('repeat');
    check(performance.getEntriesByName('repeat').length === 0 && performance.getEntriesByName('span')[0] === measured, 'clearMarks does not erase measures');
    throws('SyntaxError', () => performance.measure('cleared', 'repeat'));
    performance.clearMeasures('span'); check(performance.getEntriesByName('span').length === 0, 'clearMeasures by name');
    if (typeof document !== 'undefined') {
        throws('SyntaxError', () => performance.mark('navigationStart'));
        throws('SyntaxError', () => new PerformanceMark('responseEnd'));
        check(performance.measure('navigation', 'navigationStart', 'navigationStart').duration === 0, 'legacy navigationStart mapping');
    }
    for (const method of ['mark', 'measure', 'getEntries', 'getEntriesByType', 'getEntriesByName', 'clearMarks', 'clearMeasures']) {
        check(!Object.hasOwn(performance[method], 'prototype'), 'operation is not constructible: ' + method);
        throws('TypeError', () => performance[method].call({}, 'test'));
    }
    const start = Object.getOwnPropertyDescriptor(PerformanceEntry.prototype, 'startTime');
    check(start && start.enumerable && start.configurable && !start.set, 'readonly Web IDL getter');
    throws('TypeError', () => start.get.call(Object.create(PerformanceEntry.prototype)));
    throws('TypeError', () => start.get.call(new Proxy(first, {})));
    Object.setPrototypeOf(first, null); check(start.get.call(first) === 17.25, 'brand survives prototype replacement');
    performance.clearMarks(undefined); performance.clearMeasures();
    check(performance.getEntriesByType('mark').length === 0 && performance.getEntriesByType('measure').length === 0, 'clear all user entries');
    return 'user-timing-ok';
})();
