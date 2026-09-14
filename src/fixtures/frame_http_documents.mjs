(function () {
    // HTML #process-a-navigate-response, #read-html, #read-text:
    // a received HTTP response is distinct from a network error. Its status
    // does not suppress document parsing, scripts, or ordinary load events.
    function assert(value, label) { if (!value) throw Error(label); }
    function drain() {
        let budget = 1000;
        while (__trust.hasPlatformTask() && budget-- > 0) __trust.runPlatformTask();
        assert(budget > 0 && !__trust.hasInitialFramesPending(), 'navigation tasks finish');
        assert(__trust.takeErrors() === '', 'no frame script errors');
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const messages = [];
    addEventListener('message', event => messages.push(event));
    function response(url, status, type, text, finalURL = url) {
        frameNavigationResponses[url] = [status, type, text, '', '', finalURL, location.href];
    }
    function makeFrame(url) {
        const frame = document.createElement('iframe');
        if (url) frame.src = url;
        body.appendChild(frame);
        return frame;
    }
    for (const status of [200, 201, 302, 404, 429, 500]) {
        const url = new URL('/frame-' + status, location.href).href;
        response(url, status, 'text/html; charset=utf-8', '<body><p>HTTP ' + status + '</p>' +
            '<script>window.parsedStatus=' + status + ';parent.postMessage(' + status + ',"*");<\/script>');
        const frame = makeFrame();
        const initialWindow = frame.contentWindow, initialDocument = frame.contentDocument;
        let loads = 0;
        frame.onload = event => {
            assert(event.isTrusted && event.target === frame, 'trusted container load');
            assert(frame.contentDocument.readyState === 'complete', 'load after parsing');
            assert(frame.contentWindow.parsedStatus === status, 'script precedes load');
            loads++;
        };
        frame.src = url;
        drain();
        const doc = frame.contentDocument;
        assert(doc !== initialDocument && frame.contentWindow === initialWindow, 'first same-origin Window reuse');
        assert(doc.URL === url && doc.contentType === 'text/html', 'HTTP document URL and type');
        assert(doc.querySelector('p').textContent === 'HTTP ' + status, 'HTTP body rendered');
        assert(loads === 1, 'one HTTP document load');
        const event = messages.pop();
        assert(event && event.data === status && event.origin === location.origin &&
            event.source === frame.contentWindow, 'HTTP document sends message');
        frame.remove();
    }
    const crossURL = 'https://frames.example.net/error';
    const finalURL = 'https://frames.example.net/final-error';
    response(crossURL, 429, 'text/html', '<body><script>parent.postMessage("cross-error","*");<\/script>', finalURL);
    const cross = makeFrame(crossURL);
    drain();
    const event = messages.pop();
    assert(cross.contentDocument === null, 'error document preserves same-origin access restrictions');
    assert(event && event.data === 'cross-error' && event.origin === 'https://frames.example.net' &&
        event.source === cross.contentWindow, 'cross-origin error document can message its parent');
    assert(cross.__contentRealmWindow.document.URL === finalURL, 'final response URL committed');
    cross.remove();

    const textURL = new URL('/plain-error', location.href).href;
    const source = '<script>parent.unexpectedHTTPExecution=true;<\/script>&amp;';
    response(textURL, 500, 'text/plain', source);
    const textFrame = makeFrame(textURL);
    drain();
    assert(textFrame.contentDocument.contentType === 'text/plain' &&
        textFrame.contentDocument.body.textContent === source && !window.unexpectedHTTPExecution,
        'error status does not override MIME-based text handling');
    textFrame.remove();

    // 204/205 leave the current Document and its URL intact, do not fire load,
    // and must release the parent Document's navigation/load-delay reservation.
    for (const status of [204, 205]) {
        const frame = document.createElement('iframe');
        frame.srcdoc = '<body><p>Keep this document</p>';
        body.appendChild(frame);
        drain();
        const previousWindow = frame.contentWindow, previousDocument = frame.contentDocument;
        let loads = 0;
        frame.onload = () => loads++;
        previousWindow.addEventListener('load', () => loads++);
        const url = new URL('/no-content-' + status, location.href).href;
        response(url, status, 'text/html', '<body><script>parent.unexpectedHTTPExecution=true;<\/script>');
        frame.removeAttribute('srcdoc');
        frame.src = url;
        drain();
        assert(frame.contentWindow === previousWindow && frame.contentDocument === previousDocument,
            'no-content response retains Window and Document');
        assert(previousDocument.URL === 'about:srcdoc' &&
            previousDocument.querySelector('p').textContent === 'Keep this document', 'no-content retains URL and body');
        assert(loads === 0 && !window.unexpectedHTTPExecution, 'no-content response has no new document events or scripts');
        frame.remove();
    }
    return 'frame-http-documents-ok';
})()
