(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function throws(C, callback) {
        try { callback(); } catch (error) { check(error instanceof C, 'exception type'); return; }
        throw Error('expected exception');
    }
    const entry = performance.getEntriesByType('navigation')[0];
    check(entry && performance.getEntriesByType('navigation').length === 1, 'one navigation');
    check(entry instanceof PerformanceNavigationTiming && entry instanceof PerformanceResourceTiming &&
        entry instanceof PerformanceEntry, 'navigation inheritance');
    check(entry.name === location.href && entry.entryType === 'navigation' && entry.startTime === 0, 'entry identity');
    check(entry.type === 'reload' && performance.navigation.type === 1, 'native navigation type');
    check(entry.duration === 0 && entry.loadEventEnd === 0 && entry.domInteractive === 0, 'unfinished lifecycle');
    check(entry.responseEnd > 0 && entry.responseEnd <= performance.now(), 'native response precedes script');
    check(entry.encodedBodySize === 24 && entry.decodedBodySize === 48 && entry.transferSize === 324, 'byte counts');
    check(entry.contentType === 'text/html' && entry.nextHopProtocol === 'http/1.1' && entry.responseStatus === 200, 'transport metadata');
    check(entry.secureConnectionStart === 0, 'HTTP has no TLS handshake');
    for (const C of [PerformanceEntry, PerformanceResourceTiming, PerformanceNavigationTiming]) throws(TypeError, () => new C());
    const startGetter = Object.getOwnPropertyDescriptor(PerformanceResourceTiming.prototype, 'requestStart').get;
    for (const name of ['unloadEventStart','unloadEventEnd','domInteractive','domContentLoadedEventStart',
        'domContentLoadedEventEnd','domComplete','loadEventStart','loadEventEnd','redirectCount','criticalCHRestart']) {
        const d=Object.getOwnPropertyDescriptor(PerformanceNavigationTiming.prototype,name);
        check(d.get.name==='get '+name && d.get.length===0 && d.set===undefined &&
            d.enumerable && d.configurable,'navigation IDL getter '+name);
    }
    throws(TypeError, () => startGetter.call({}));
    throws(TypeError, () => startGetter.call(new PerformanceMark('not-resource')));
    throws(TypeError, () => startGetter.call(new Proxy(entry, {})));
    check(Object.keys(PerformanceEntry.prototype.toJSON.call(entry)).sort().join(',') ===
        'duration,entryType,name,startTime', 'base JSON has only base IDL fields');
    const json = entry.toJSON();
    check(json.loadEventEnd === 0 && json.responseEnd === entry.responseEnd && json.type === 'reload', 'derived JSON');
    check(Object.keys(json).slice(-12).join(',')==='unloadEventStart,unloadEventEnd,domInteractive,domContentLoadedEventStart,'+
        'domContentLoadedEventEnd,domComplete,loadEventStart,loadEventEnd,type,redirectCount,criticalCHRestart,notRestoredReasons','navigation IDL JSON order');
    const name = entry.name; entry.name = 'overwritten'; check(entry.name === name, 'readonly attribute');
    const nowBefore = performance.now(), event = new Event('original-clock-test'), nowAfter = performance.now();
    check(event.timeStamp >= nowBefore && event.timeStamp <= nowAfter, 'event clock origin');
    const savedFloor = Math.floor;
    try { Math.floor = () => { throw Error('author Math.floor'); }; check(performance.now() >= nowAfter, 'captured clock intrinsic'); }
    finally { Math.floor = savedFloor; }
    performance.mark('original-marker'); performance.clearMarks(); performance.clearMeasures();
    check(performance.getEntries()[0] === entry, 'User Timing clear preserves navigation');
    globalThis.navigationTimingLog = [];
    const observer = new PerformanceObserver(list => {
        check(list.getEntries().length === 1 && list.getEntries()[0] === entry, 'navigation observer identity');
        check(entry.duration === entry.loadEventEnd && entry.loadEventEnd > 0, 'observer after load');
        navigationTimingLog.push('observer'); observer.disconnect();
    });
    observer.observe({type: 'navigation'});
    const buffered = new PerformanceObserver(() => { throw Error('drained observer callback'); });
    buffered.observe({type: 'navigation', buffered: true});
    check(buffered.takeRecords()[0] === entry, 'buffered navigation available before load'); buffered.disconnect();
    document.dispatchEvent(new Event('DOMContentLoaded'));
    dispatchEvent(new Event('load'));
    check(entry.domContentLoadedEventStart === 0 && entry.loadEventEnd === 0, 'synthetic events cannot finish navigation');
    document.addEventListener('DOMContentLoaded', () => {
        check(document.readyState === 'interactive' && entry.domInteractive > 0, 'DOM readiness boundary');
        check(entry.domContentLoadedEventStart >= entry.domInteractive && entry.domContentLoadedEventEnd === 0, 'DOM dispatch boundary');
        navigationTimingLog.push('dom');
    });
    addEventListener('load', () => {
        check(document.readyState === 'complete' && entry.domComplete >= entry.domContentLoadedEventEnd, 'complete readiness');
        check(entry.loadEventStart >= entry.domComplete && entry.loadEventEnd === 0, 'load dispatch boundary');
        navigationTimingLog.push('load');
    });
    globalThis.navigationTimingFinish = () => {
        check(entry.loadEventEnd >= entry.loadEventStart && entry.duration === entry.loadEventEnd, 'final duration');
        check(entry.toJSON().loadEventEnd === entry.loadEventEnd, 'JSON reads live entry');
        check(Math.abs(performance.timeOrigin + entry.responseEnd - performance.timing.responseEnd) < 1.1, 'legacy epoch coherence');
        check(navigationTimingLog.join(',') === 'dom,load,observer', 'observer task order');
        check(performance.getEntriesByName(location.href, 'navigation')[0] === entry, 'name lookup');
        return 'navigation-timing-ok';
    };
})();
