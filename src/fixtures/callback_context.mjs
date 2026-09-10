// HTML HostMake/CallJobCallback, Web IDL callback context, HTML timers.
// Source identity is separate from the receiver/callback Realm and task owner.
(function () {
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const frame = document.createElement('iframe');
    body.appendChild(frame);
    const opaque = document.createElement('iframe');
    globalThis.callbackContextTrace = [];
    globalThis.callbackContextIntervalCount = 0;
    globalThis.callbackContextAssimilated = false;
    globalThis.callbackContextInvalid = 0;
    for (const invalid of [undefined, null, 7, {}]) {
        try { queueMicrotask(invalid); }
        catch (error) { if (error instanceof TypeError) callbackContextInvalid++; }
    }
    queueMicrotask(() => ({ then() { callbackContextAssimilated = true; } }));
    addEventListener('message', event => {
        const kind = event.data;
        if (typeof kind !== 'string' || !kind.startsWith('context-')) return;
        const fromOpaque = kind.startsWith('context-opaque-');
        const fromParent = kind === 'context-parent-function';
        const source = fromOpaque ? opaque.contentWindow : fromParent ? window : frame.contentWindow;
        const origin = fromOpaque ? 'null' : location.origin;
        callbackContextTrace.push(kind + ':' + (event.source === source) + ':' + (event.origin === origin));
        if (kind === 'context-child-interval' && ++callbackContextIntervalCount === 2)
            frame.contentWindow.clearInterval(frame.contentWindow.interval);
    });
    globalThis.parentFunction = function () { postMessage('context-parent-function', '*'); };
    globalThis.deferredContext = new Promise(resolve => globalThis.releaseContext = resolve);
    frame.contentWindow.eval(`
        Promise.resolve().then(parent.postMessage.bind(parent, 'context-child-promise', '*'));
        parent.deferredContext.then(parent.postMessage.bind(parent, 'context-child-pending', '*'));
        parent.queueMicrotask(parent.postMessage.bind(parent, 'context-child-parent-microtask', '*'));
        queueMicrotask(function () {
            'use strict';
            if (arguments.length !== 0 || this !== undefined) throw Error('microtask this/arguments');
            parent.postMessage('context-child-microtask-arguments', '*');
        });
        setTimeout(parent.postMessage.bind(parent, 'context-child-timer', '*'), 0);
        parent.setTimeout(parent.postMessage.bind(parent, 'context-child-parent-timer', '*'), 0);
        setTimeout(parent.parentFunction, 0);
        globalThis.interval = setInterval(parent.postMessage.bind(parent, 'context-child-interval', '*'), 1);
        setTimeout(function (a, b) {
            if (this !== window || a !== 3 || b !== 7) throw Error('timer this/arguments');
            parent.postMessage('context-child-arguments', '*');
        }, 0, 3, 7);
    `);
    opaque.src = 'data:text/html,' + encodeURIComponent(`<script>
        Promise.resolve().then(parent.postMessage.bind(parent, 'context-opaque-promise', '*'));
        setTimeout(parent.postMessage.bind(parent, 'context-opaque-timer', '*'), 0);
    <\/script>`);
    body.appendChild(opaque);
    __trust.hydrateFrames();
    releaseContext();
    return 'callback-context-pending';
})()
