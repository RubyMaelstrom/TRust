(function () {
    function assert(value, label) { if (!value) throw Error(label); }
    function drain() {
        let budget = 1000;
        while (__trust.hasPlatformTask() && budget-- > 0) __trust.runPlatformTask();
        assert(budget > 0, 'message tasks finish');
    }
    const received = [];
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    addEventListener('message', event => received.push(event));
    const frame = document.createElement('iframe');
    document.body.appendChild(frame);
    const child = frame.contentWindow;
    child.eval(`
        globalThis.received = [];
        addEventListener('message', event => received.push(event));
        parent.postMessage({kind:'top-level', date:new Date(123), map:new Map([['a',7]])}, '*');
        function send() { parent.postMessage({kind:'function'}, '*'); }
        send();
        Reflect.apply(parent.postMessage, parent, [{kind:'apply'}, '*']);
        parent.postMessage.bind(parent)({kind:'bound'}, '*');
    `);
    assert(received.length === 0, 'asynchronous delivery');
    drain();
    assert(received.length === 4, 'child-to-parent delivery: ' + received.length);
    for (const event of received) {
        assert(event.isTrusted, 'posted message is delivered by the user agent');
        assert(event.source === child, 'child source');
        assert(event.origin === location.origin, 'inherited about:blank origin');
        assert(Object.getPrototypeOf(event.data) === Object.prototype, 'receiver object realm');
    }
    assert(received[0].data.date instanceof Date && +received[0].data.date === 123, 'Date clone');
    assert(received[0].data.map instanceof Map && received[0].data.map.get('a') === 7, 'Map clone');
    const data = { value: 7, bytes: new Uint8Array([1, 2, 3]).buffer };
    data.self = data;
    child.postMessage(data);
    data.value = 9;
    new Uint8Array(data.bytes)[0] = 99;
    child.postMessage('filtered', 'https://not-the-target.invalid');
    let invalid = false;
    try { child.postMessage('bad origin', 'not a URL'); }
    catch (error) { invalid = error.name === 'SyntaxError'; }
    assert(invalid, 'invalid targetOrigin throws');
    let uncloneable = false;
    try { child.postMessage(function () {}, '*'); }
    catch (error) { uncloneable = error.name === 'DataCloneError'; }
    assert(uncloneable, 'uncloneable message throws synchronously');
    drain();
    assert(child.received.length === 1, 'parent-to-child target filtering');
    const event = child.received[0];
    assert(event.source === window, 'parent source');
    assert(event.data.self === event.data && event.data.value === 7, 'send-time graph clone');
    assert(new Uint8Array(event.data.bytes).join() === '1,2,3', 'send-time buffer clone');
    assert(Object.getPrototypeOf(event.data.bytes) === child.ArrayBuffer.prototype, 'receiver buffer realm');
    const opaque = document.createElement('iframe');
    opaque.src = 'data:text/html,' + encodeURIComponent(`<script>
        parent.postMessage('opaque-wildcard', '*');
        parent.postMessage('opaque-default');
        addEventListener('message', event => parent.postMessage('reply:' + event.data, '*'));
    <\/script>`);
    document.body.appendChild(opaque);
    __trust.hydrateFrames();
    drain();
    const opaqueEvents = received.filter(event => typeof event.data === 'string');
    assert(opaqueEvents.length === 1, 'opaque default must not reach parent');
    assert(opaqueEvents[0].origin === 'null', 'opaque sender origin');
    assert(opaqueEvents[0].source === opaque.contentWindow, 'cross-origin source proxy');
    opaque.contentWindow.postMessage('ping', '*');
    drain();
    assert(received.some(event => event.data === 'reply:ping'), 'cross-origin proxy reply');
    const channel = new MessageChannel();
    const queued = { value: 1 };
    channel.port1.postMessage(queued);
    queued.value = 2;
    child.eval(`
        globalThis.portReceived = [];
        addEventListener('message', event => {
            if (!event.data || event.data.kind !== 'port') return;
            const port = event.ports[0];
            if (!(port instanceof MessagePort) || event.data.port !== port)
                throw Error('receiver port realm/graph');
            port.onmessage = e => {
                portReceived.push([e.data.value, e.isTrusted,
                    Object.getPrototypeOf(e.data) === Object.prototype, e.target === port].join('|'));
                port.postMessage('ack');
            };
        });
    `);
    const acknowledgements = [];
    channel.port1.onmessage = e => acknowledgements.push(e.data);
    child.postMessage({kind:'port', port:channel.port2}, '*', [channel.port2]);
    let detached = false;
    try { structuredClone(channel.port2, {transfer:[channel.port2]}); }
    catch (e) { detached = e.name === 'DataCloneError'; }
    assert(detached, 'source port detaches at send time');
    drain();
    assert(child.portReceived.join() === '1|true|true|true', 'queued message follows transfer: ' + child.portReceived);
    assert(acknowledgements.join() === 'ack', 'bidirectional transferred channel');
    const cycle = {port:channel.port1}; cycle.self = cycle;
    const copy = structuredClone(cycle, {transfer:new Set([channel.port1])});
    assert(copy.self === copy && copy.port instanceof MessagePort, 'cyclic port clone');
    const pending = new MessageChannel();
    let duplicate = false;
    try { structuredClone(null, {transfer:[pending.port1,pending.port1]}); }
    catch (e) { duplicate = e.name === 'DataCloneError'; }
    assert(duplicate, 'duplicate transfer rejected');
    let untransferred = false;
    try { structuredClone(pending.port1); }
    catch (e) { untransferred = e.name === 'DataCloneError'; }
    assert(untransferred, 'ports require transfer list');
    const buffer = new Uint8Array([3,4]).buffer;
    const bufferCopy = structuredClone({buffer}, {transfer:[buffer]});
    assert(buffer.byteLength === 0 && new Uint8Array(bufferCopy.buffer).join() === '3,4', 'buffer transfer detaches');
    const lateBuffer = new Uint8Array([1]).buffer;
    const lateCopy = structuredClone({buffer:lateBuffer, get mutate() {
        new Uint8Array(lateBuffer)[0] = 9; return true;
    }}, {transfer:[lateBuffer]});
    assert(new Uint8Array(lateCopy.buffer)[0] === 9, 'transfer takes bytes after graph serialization');
    const closed = new MessageChannel().port1;
    closed.close();
    let visited = false, closedError = false;
    try { structuredClone({get value() { visited = true; }}, {transfer:[closed]}); }
    catch (e) { closedError = e.name === 'DataCloneError'; }
    assert(visited && closedError, 'detached transfer rejected after graph serialization');
    const moving = new MessageChannel();
    let moved, delivered;
    moving.port2.onmessage = e => delivered = e.data.value;
    moving.port1.postMessage({get value() {
        moved = structuredClone(moving.port1, {transfer:[moving.port1]}); return 17;
    }});
    while (__trust.hasPlatformTask()) __trust.runPlatformTask();
    assert(delivered === 17 && moved instanceof MessagePort, 'target captured before getter transfers source');
    return 'window-messages-ok';
})()
