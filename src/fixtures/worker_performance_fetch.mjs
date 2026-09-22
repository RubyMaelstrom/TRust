// Resource Timing #marking-resource-timing, Fetch #fetch-finale/#tao-check,
// Performance Timeline #queue-the-performanceobserver-task, HTML worker loop.
// Loaded as a real classic, module, or blob worker by the native integration test.
onmessage = async event => {
    try {
        const check = (ok, text) => { if (!ok) throw Error(name + ': ' + text); };
        const {home, cross, earliestOrigin} = event.data;
        check(performance.timeOrigin + 0.1 >= earliestOrigin, 'worker creation time origin');
        check(performance.getEntriesByName('parent-only').length === 0, 'independent entry buffer');
        const observed = [];
        let microtask = false, markAfterMicrotask = false, finishResources;
        const resourcesReady = new Promise(resolve => { finishResources = resolve; });
        const observer = new PerformanceObserver(list => {
            for (const entry of list.getEntries()) {
                if (entry.entryType === 'mark') markAfterMicrotask = microtask;
                if (entry.entryType === 'resource') observed.push(entry);
            }
            if (observed.length === 3) finishResources();
        });
        observer.observe({entryTypes:['mark','resource']});
        performance.mark(name + '-ready');
        queueMicrotask(() => { microtask = true; });
        const urls = [home + '/payload?' + name, cross + '/allowed?' + name, cross + '/opaque?' + name];
        for (const url of urls) {
            const response = await fetch(url);
            check(response.status === 200 && await response.text() === 'body', 'fetch completion');
        }
        await resourcesReady;
        observer.disconnect();
        check(markAfterMicrotask, 'observer is a task after microtasks');
        const entries = performance.getEntriesByType('resource').filter(entry => entry.initiatorType === 'fetch');
        check(entries.length === 3, 'all fetches recorded in this worker');
        for (let i = 0; i < entries.length; i++) {
            const entry = entries[i];
            check(entry instanceof PerformanceResourceTiming && entry instanceof PerformanceEntry, 'entry realm brand');
            check(entry === observed[i] && entry.name === urls[i], 'observer shares ordered timeline entries');
            check(entry.startTime >= 0 && entry.responseEnd >= entry.startTime && entry.responseEnd <= performance.now(), 'relative timestamps');
            check(Math.abs(entry.duration - (entry.responseEnd - entry.startTime)) < 0.001, 'duration');
            check(entry.responseStatus === 200 && entry.decodedBodySize === 4, 'CORS-visible body and status');
            if (i < 2) {
                check(entry.responseStart >= entry.startTime && entry.requestStart >= entry.startTime &&
                    entry.nextHopProtocol === 'http/1.1' && entry.transferSize === 304, 'same-origin or TAO-authorized detail');
            } else {
                check(entry.responseStart === 0 && entry.requestStart === 0 && entry.domainLookupStart === 0 &&
                    entry.connectStart === 0 && entry.nextHopProtocol === '' && entry.transferSize === 0, 'TAO-denied timing privacy');
            }
        }
        postMessage(name + ':ok');
    } catch (error) {
        postMessage('ERROR:' + error.message);
    }
};
