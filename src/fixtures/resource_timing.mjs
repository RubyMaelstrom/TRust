// Original Resource Timing interface/task tests. Network measurements are
// independently exercised by the native localhost integration tests.
(function () {
    const check = (ok, message) => { if (!ok) throw Error(message); };
    const p = performance, t = p.timeOrigin;
    const put = n => __trust.recordResourceTiming({name:'https://original.test/'+n,
        initiatorType:'fetch',startTime:t+10,fetchStart:t+10,responseEnd:t+20,
        requestStart:t+11,responseStart:t+15,encodedBodySize:5,decodedBodySize:5,
        transferSize:305,responseStatus:200,nextHopProtocol:'http/1.1',
        renderBlockingStatus:'non-blocking'});
    const run = () => { let count=0; while (__trust.hasPerformanceTask()) {
        check(++count<20,'performance tasks do not spin'); __trust.runPerformanceTask();
    }};
    p.clearResourceTimings(); p.setResourceTimingBufferSize(2);
    const events=[], received=[], counts=[];
    const observer = new PerformanceObserver((list, self, options) => {
        check(self===observer,'observer identity');
        received.push(...list.getEntries()); counts.push(options.droppedEntriesCount);
        events.push('observer');
    });
    observer.observe({type:'resource'});
    p.onresourcetimingbufferfull = function (event) {
        check(this===p && event.target===p && event.isTrusted,'native buffer event');
        events.push('full'); p.setResourceTimingBufferSize(4);
    };
    put(1); put(2); put(3);
    check(events.length===0 && p.getEntriesByType('resource').length===2,'async overflow');
    run();
    check(events.join(',')==='observer,full' && received.length===3 && counts[0]===1,'observer/full FIFO');
    check(p.getEntriesByType('resource').length===3,'recover secondary buffer');
    const e=received[0];
    check(e instanceof PerformanceResourceTiming && e instanceof PerformanceEntry &&
        e.entryType==='resource' && e.startTime===10 && e.duration===10 &&
        e.requestStart===11 && e.responseStart===15 && e.responseEnd===20 &&
        e.initiatorType==='fetch' && e.workerStart===0 && e.transferSize===305,'native scalar conversion');
    check(JSON.parse(JSON.stringify(e)).responseStatus===200,'resource JSON');
    let bad=0;
    try { new PerformanceResourceTiming(); } catch(e) { bad += e instanceof TypeError; }
    try { p.setResourceTimingBufferSize(); } catch(e) { bad += e instanceof TypeError; }
    try { p.setResourceTimingBufferSize(1n); } catch(e) { bad += e instanceof TypeError; }
    try { Performance.prototype.clearResourceTimings.call({}); } catch(e) { bad += e instanceof TypeError; }
    check(bad===4,'IDL conversion and brand');
    const mark=p.mark('original-kept-mark');
    p.clearResourceTimings();
    check(p.getEntriesByType('resource').length===0 && p.getEntriesByName(mark.name)[0]===mark,'clear only resources');
    p.onresourcetimingbufferfull=null; p.setResourceTimingBufferSize(0); put(4); run();
    check(received.length===4 && p.getEntriesByType('resource').length===0,'full buffer does not prevent live observation');
    check(!__trust.hasPerformanceTask(),'unhandled overflow quiesces');
    p.setResourceTimingBufferSize(-1); put(5); run();
    check(p.getEntriesByType('resource').length===1,'unsigned long conversion');
    const buffered = new PerformanceObserver(()=>{});
    buffered.observe({type:'resource',buffered:true});
    check(buffered.takeRecords().length===1,'buffered observation');
    buffered.disconnect(); observer.disconnect();
    p.clearResourceTimings(); p.clearMarks(mark.name); p.setResourceTimingBufferSize(250); run();
    // The entry owns a snapshot, not the input record or a mutable public JSON
    // object. Exercise every supported scalar, including absent timestamps.
    const numbers=['workerStart','redirectStart','redirectEnd','fetchStart',
        'domainLookupStart','domainLookupEnd','connectStart','connectEnd','secureConnectionStart',
        'requestStart','firstInterimResponseStart','finalResponseHeadersStart','responseStart','responseEnd',
        'transferSize','encodedBodySize','decodedBodySize','responseStatus'];
    const strings=['initiatorType','deliveryType','nextHopProtocol','renderBlockingStatus','contentType','contentEncoding'];
    for(const name of numbers.concat(strings)) {
        const d=Object.getOwnPropertyDescriptor(PerformanceResourceTiming.prototype,name);
        check(d.get.name==='get '+name && d.get.length===0 && d.set===undefined &&
            d.enumerable && d.configurable,'resource IDL getter '+name);
    }
    const input={name:'https://original.test/snapshot',startTime:t+10}, expected={};
    for(let i=0;i<numbers.length;i++) {
        expected[numbers[i]]=i<14 ? (i%3===0 ? 0 : 20+i) : 100+i;
        input[numbers[i]]=i<14 && expected[numbers[i]]!==0 ? t+expected[numbers[i]] : expected[numbers[i]];
    }
    for(let i=0;i<strings.length;i++) input[strings[i]]=expected[strings[i]]='original-'+i;
    __trust.recordResourceTiming(input);
    const snapshot=p.getEntriesByName(input.name)[0];
    for(const name of numbers.concat(strings)) {
        input[name]='changed';check(snapshot[name]===expected[name],'snapshot getter '+name);
    }
    const json=snapshot.toJSON();
    check(Object.keys(json).length===28 && json.startTime===10 && json.duration===23,'complete scalar JSON');
    check(Object.keys(json).join(',')==='name,entryType,startTime,duration,initiatorType,deliveryType,nextHopProtocol,'+
        'workerStart,redirectStart,redirectEnd,fetchStart,domainLookupStart,domainLookupEnd,connectStart,connectEnd,'+
        'secureConnectionStart,requestStart,finalResponseHeadersStart,firstInterimResponseStart,responseStart,responseEnd,'+
        'transferSize,encodedBodySize,decodedBodySize,responseStatus,renderBlockingStatus,contentType,contentEncoding','resource IDL JSON order');
    for(const name of numbers.concat(strings)) {
        check(json[name]===expected[name],'snapshot JSON '+name);json[name]='changed';
    }
    check(snapshot.responseEnd===33 && snapshot.toJSON().responseEnd===33,'JSON does not alias entry');
    check(Object.keys(snapshot).length===0,'backing tuple is not public');
    const get=Object.getOwnPropertyDescriptor(PerformanceResourceTiming.prototype,'responseEnd').get;
    bad=0;try{get.call({})}catch(e){bad+=e instanceof TypeError}
    try{PerformanceResourceTiming.prototype.toJSON.call({})}catch(e){bad+=e instanceof TypeError}
    check(bad===2,'resource getter and JSON brands');
    // IDL serialization defines own properties and internal traversal never
    // calls author-installed array iterators or prototype setters.
    const define=Object.defineProperty, iterator=Array.prototype[Symbol.iterator];
    let touched=0, safeJSON;
    try {
        define(Object.prototype,'responseEnd',{configurable:true,set(){touched++}});
        Array.prototype[Symbol.iterator]=function(){throw Error('author iterator')};
        safeJSON=snapshot.toJSON();
    } finally {
        Array.prototype[Symbol.iterator]=iterator;delete Object.prototype.responseEnd;
    }
    check(touched===0 && safeJSON.responseEnd===33,'private traversal and own JSON fields');
    p.clearResourceTimings();run();
    document.body.setAttribute('data-resource-interface','ok');
})();
