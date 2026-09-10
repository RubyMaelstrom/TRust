(function () {
    function assert(value, label) { if (!value) throw Error(label); }
    function drain() {
        let budget = 1000;
        while (__trust.hasPlatformTask() && budget-- > 0) __trust.runPlatformTask();
        assert(budget > 0, 'navigation task queue drains');
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const source = '\n<script>parent.textWasExecuted = true;<\/script>&amp;<img src="/unexpected">\r\nend\r\u0000é';
    const expected = source.replace(/\r\n?/g, '\n').replace(/\u0000/g, '\ufffd');
    const types = ['text/javascript', 'application/x-ecmascript', 'text/javascript1.5',
        'text/plain', 'text/css', 'text/vtt', 'application/json', 'application/example+json'];
    for (const type of types) {
        const frame = document.createElement('iframe');
        body.appendChild(frame);
        const initialWindow = frame.contentWindow, initialDocument = frame.contentDocument;
        assert(initialDocument.contentType === 'text/html', 'initial HTML document type');
        let loads = 0;
        frame.onload = event => {
            assert(event.isTrusted && event.target === frame, 'trusted iframe load');
            assert(frame.contentDocument.readyState === 'complete', 'load after document completion');
            loads++;
        };
        const url = URL.createObjectURL(new Blob([source], {type: type + ';charset=utf-8'}));
        frame.src = url;
        __trust.hydrateFrames(); drain();
        const doc = frame.contentDocument;
        assert(frame.contentWindow === initialWindow, 'first same-origin navigation reuses Window');
        assert(doc !== initialDocument && doc.defaultView === initialWindow, 'new active Document');
        assert(doc.URL === url && initialWindow.location.href === url, 'committed Blob URL');
        assert(initialWindow.location.origin === location.origin, 'Blob creator origin');
        assert(doc.contentType === type, 'MIME essence: ' + doc.contentType);
        assert(doc.body.children.length === 1 && doc.body.firstElementChild.localName === 'pre', 'text document structure');
        assert(doc.body.firstElementChild.textContent === expected, 'literal text, newline and NUL processing');
        assert(doc.querySelectorAll('script,img').length === 0 && !window.textWasExecuted, 'source is not HTML or executable');
        assert(loads === 1, 'one completed load');
        frame.srcdoc = '<body><p>HTML again</p><script>window.actualScriptRan = 1;<\/script>';
        __trust.hydrateFrames(); drain();
        assert(frame.contentDocument.contentType === 'text/html', 'type resets on HTML navigation');
        assert(doc.contentType === type, 'old Document retains its content type');
        assert(frame.contentWindow.actualScriptRan === 1, 'HTML scripts still execute');
        URL.revokeObjectURL(url);
        frame.remove();
    }
    const opaque = document.createElement('iframe');
    opaque.src = 'data:text/plain,' + encodeURIComponent(source);
    body.appendChild(opaque);
    __trust.hydrateFrames(); drain();
    assert(opaque.contentDocument === null, 'data navigation retains the same-origin boundary');
    // Host-side fixture inspection, not an author-visible cross-origin access.
    const child = opaque.__contentRealmWindow;
    assert(child.location.href === opaque.src && child.location.origin === 'null', 'opaque text document URL/origin');
    assert(child.document.contentType === 'text/plain' && child.document.body.textContent === expected, 'data text document');
    const outer = document.createElement('iframe');
    outer.srcdoc = '<body><script>' +
        'window.innerTextFrame = document.createElement("iframe");' +
        'innerTextFrame.src = URL.createObjectURL(new Blob(["x"], {type:"text/javascript"}));' +
        'document.body.appendChild(innerTextFrame);' +
        'URL.revokeObjectURL(innerTextFrame.src);' +
        'window.initialInnerWindow = innerTextFrame.contentWindow;' +
        '<\/script>';
    body.appendChild(outer);
    __trust.hydrateFrames(); drain();
    const nested = outer.contentWindow.innerTextFrame;
    assert(nested.contentDocument.contentType === 'text/javascript', 'nested Realm Blob document type');
    assert(nested.contentDocument.body.textContent === 'x', 'nested Realm Blob document text');
    assert(nested.contentWindow === outer.contentWindow.initialInnerWindow, 'nested initial Window reuse');
    const superseded = document.createElement('iframe');
    body.appendChild(superseded);
    const a = URL.createObjectURL(new Blob(['old'], {type:'text/plain'}));
    const b = URL.createObjectURL(new Blob(['new'], {type:'text/plain'}));
    superseded.src = a;
    superseded.src = b;
    URL.revokeObjectURL(a); URL.revokeObjectURL(b);
    drain();
    assert(superseded.contentDocument.body.textContent === 'new', 'latest queued URL keeps its entry across revocation');
    const revoked = document.createElement('iframe');
    const unavailable = URL.createObjectURL(new Blob(['must not load'], {type:'text/plain'}));
    URL.revokeObjectURL(unavailable);
    revoked.src = unavailable;
    body.appendChild(revoked); drain();
    assert(!revoked.contentDocument.querySelector('pre'), 'revocation before navigation prevents loading');
    return 'frame-text-documents-ok';
})()
