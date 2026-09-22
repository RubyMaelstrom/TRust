// Local HR-Time 1f0b9fa, User Timing a449dbe, Performance Timeline 8aa7b1d,
// Resource Timing 1f9ef25: worker-specific exposure and event-loop contracts.
(function () {
    const check = (ok, text) => { if (!ok) throw Error(text); };
    const throws = (name, fn) => {
        let error; try { fn(); } catch (e) { error = e; }
        check(error && error.name === name, 'expected ' + name);
    };
    const p = performance;
    check(p instanceof Performance && p instanceof EventTarget, 'Performance EventTarget');
    check(Object.prototype.toString.call(p) === '[object Performance]', 'Performance tag');
    check(typeof window === 'undefined' && typeof document === 'undefined' &&
        typeof PerformanceNavigationTiming === 'undefined' && !('timing' in p) &&
        !('navigation' in p), 'workers have no Window navigation APIs');
    check(typeof __performance_binding === 'undefined' && typeof __performance_adapter === 'undefined', 'installation hooks removed');
    check(p.getEntriesByType('navigation').length === 0, 'no navigation entries');
    const ignored = new PerformanceObserver(() => { throw Error('unsupported navigation delivery'); });
    ignored.observe({type:'navigation', buffered:true});
    check(!__wkr.hasPerformanceTask(), 'unsupported observation does not queue');
    ignored.disconnect();
    const now = p.now(), origin = p.timeOrigin;
    check(now >= 0 && p.now() >= now && p.toJSON().timeOrigin === origin, 'monotonic worker clock');
    throws('TypeError', () => new Performance());
    throws('TypeError', () => Performance.prototype.now.call({}));
    throws('TypeError', () => Object.getOwnPropertyDescriptor(Performance.prototype, 'timeOrigin').get.call({}));
    // User Timing deliberately allows creating these names in workers, but
    // its legacy name-to-timestamp conversion remains Window-only.
    const named = p.mark('navigationStart', {startTime:12.5});
    check(named.startTime === 12.5, 'worker mark name is not reserved');
    throws('TypeError', () => p.measure('legacy-window-name', 'navigationStart'));
    p.clearMarks();
    let retained;
    const eventOrder = [];
    p.addEventListener('resourcetimingbufferfull', event => {
        eventOrder.push('capture');
        check(event.eventPhase === Event.AT_TARGET && event.currentTarget === p && event.target === p,
            'buffer event is at its Performance target');
        check(event.isTrusted && !event.bubbles && !event.cancelable && !event.composed,
            'buffer event flags');
        check(event.timeStamp >= 0 && event.timeStamp <= p.now(), 'worker-relative event clock');
        check(event.composedPath().length === 1 && event.composedPath()[0] === p, 'flat event path');
        retained = event;
    }, {capture:true, once:true});
    p.onresourcetimingbufferfull = () => { eventOrder.push('handler'); p.setResourceTimingBufferSize(2); };
    p.addEventListener('resourcetimingbufferfull', () => eventOrder.push('listener'), {once:true});
    p.setResourceTimingBufferSize(0);
    __wkr.recordResourceTiming({name:'https://example.org/overflow',startTime:origin+1,responseEnd:origin+2});
    check(eventOrder.length === 0, 'overflow is asynchronous');
    __wkr.runPerformanceTask();
    check(eventOrder.join() === 'capture,handler,listener', 'buffer event listener order');
    check(retained.currentTarget === null && retained.eventPhase === Event.NONE && retained.composedPath().length === 0,
        'dispatch state cleaned up');
    check(p.getEntriesByType('resource').length === 1, 'buffer recovers');
    p.onresourcetimingbufferfull = null; p.clearResourceTimings(); p.setResourceTimingBufferSize(250);
    return 'worker-performance-ok';
})();
