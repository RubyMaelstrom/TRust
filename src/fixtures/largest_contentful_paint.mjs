(() => {
    const assert = (value, message) => { if (!value) throw Error(message); };
    assert(typeof LargestContentfulPaint === 'function', 'Window exposes the interface');
    assert(PerformanceObserver.supportedEntryTypes.includes('largest-contentful-paint'), 'supported entry type');
    try { new LargestContentfulPaint(); throw Error('constructed'); }
    catch (error) { assert(error instanceof TypeError, 'illegal constructor'); }
    const observer = new PerformanceObserver(() => {});
    observer.observe({type:'largest-contentful-paint'});
    const element = id => { const node = document.createElement('p'); node.id = id; document.body.append(node); return node; };
    const first = element('first'), equal = element('equal'), tiny = element('tiny'), image = element('image');
    const origin = performance.timeOrigin, documentId = document.__id;
    const row = (node,size,width,height,url='',load=0) =>
        [node.__id,size,width,height,url,load,node.id,documentId];
    const record = rows => __trust.recordLargestContentfulPaint(rows,origin + 100,documentId);
    record([row(first,1000,50,20)]);
    let entries = observer.takeRecords();
    assert(entries.length === 1, 'first paint queues a candidate');
    const entry = entries[0];
    assert(entry instanceof LargestContentfulPaint && entry instanceof PerformanceEntry, 'prototype inheritance');
    assert(Object.prototype.toString.call(entry) === '[object LargestContentfulPaint]', 'class string');
    assert(entry.name === '' && entry.entryType === 'largest-contentful-paint' && entry.duration === 0, 'entry fields');
    assert(entry.size === 1000 && entry.loadTime === 0 && entry.element === first && entry.id === 'first' && entry.url === '', 'text fields');
    assert(Math.abs(entry.startTime - 100) < 0.11 && entry.renderTime === entry.startTime &&
        entry.paintTime === entry.startTime && entry.presentationTime === null, 'paint timestamps');
    const json = entry.toJSON();
    assert(json.element === first && json.size === 1000 && json.paintTime === entry.paintTime, 'default toJSON');
    assert(performance.getEntriesByType('largest-contentful-paint').length === 0 &&
        !performance.getEntries().includes(entry) && performance.getEntriesByName('', 'largest-contentful-paint').length === 0,
        'observer-only registry availability');
    record([row(first,9000,100,90),row(equal,1000,50,20),row(tiny,1100,52,21)]);
    assert(observer.takeRecords().length === 0, 'one paint per element, ties, and three-pixel tolerance');
    record([row(image,1500,60,30,'https://image.test/photo.png',origin + 50)]);
    entries = observer.takeRecords();
    assert(entries.length === 1 && entries[0].url === 'https://image.test/photo.png' &&
        Math.abs(entries[0].loadTime - 50) < 0.11, 'image URL and load time');
    first.remove();
    assert(entry.element === null && entry.id === 'first' && entry.size === 1000 && entry.toJSON().element === null,
        'detachment changes element exposure, preserving candidate fields');
    const shadow = document.createElement('div'); document.body.append(shadow);
    shadow.attachShadow({mode:'open'}).append(first);
    assert(entry.element === null, 'shadow tree elements are not exposed');
    shadow.after(first);
    assert(entry.element === first, 'reattachment to the same Document exposes it');
    const frame = document.createElement('iframe'); document.body.append(frame);
    const childGetter = Object.getOwnPropertyDescriptor(frame.contentWindow.LargestContentfulPaint.prototype,'element').get;
    assert(childGetter.call(entry) === first, 'borrowed getter uses the entry Document');
    frame.remove();
    const buffered = new PerformanceObserver(() => {});
    buffered.observe({type:'largest-contentful-paint',buffered:true});
    assert(buffered.takeRecords().length === 2, 'buffered observer receives prior candidates');
    window.dispatchEvent(new Event('scroll'));
    const afterSynthetic = element('after-synthetic');
    record([row(afterSynthetic,2000,100,20)]);
    assert(observer.takeRecords().length === 1, 'synthetic scroll does not terminate LCP');
    __trust.fireElementScroll(first.__id);
    __trust.runScrollSteps();
    record([row(element('after-stop'),9000,150,60)]);
    assert(observer.takeRecords().length === 0, 'trusted scroll permanently stops the metric');
    const getter = Object.getOwnPropertyDescriptor(LargestContentfulPaint.prototype,'element').get;
    try { getter.call({}); throw Error('unbranded receiver'); }
    catch (error) { assert(error instanceof TypeError, 'Web IDL receiver check'); }
    observer.disconnect(); buffered.disconnect();
    return 'lcp-api-ok';
})()
