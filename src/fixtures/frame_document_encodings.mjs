(function () {
    // HTML #read-html / #read-text with #encoding-sniffing-algorithm, and
    // DOM #dom-document-characterset. The top-level Document's encoding is
    // windows-1250, set natively before this fixture runs.
    function assert(value, label) { if (!value) throw Error(label); }
    function drain() {
        let budget = 1000;
        while (__trust.hasPlatformTask() && budget-- > 0) __trust.runPlatformTask();
        assert(budget > 0 && !__trust.hasInitialFramesPending(), 'navigation tasks finish');
        assert(__trust.takeErrors() === '', 'no frame script errors');
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    assert(document.characterSet === 'windows-1250' && document.charset === 'windows-1250' &&
        document.inputEncoding === 'windows-1250', 'top-level encoding');

    const ascii = text => Array.from(text, c => c.charCodeAt(0));
    function response(path, type, bytes, origin = location.href) {
        const url = new URL(path, origin).href;
        frameNavigationResponses[url] = [200, type, '', new Uint8Array(bytes).buffer, '', url, location.href];
        return url;
    }
    function load(url) {
        const frame = document.createElement('iframe');
        frame.src = url;
        body.appendChild(frame);
        drain();
        // Cross-origin documents are inspected through the test-only realm.
        return frame.__contentRealmWindow.document;
    }
    function check(doc, encoding, text, label) {
        assert(doc.characterSet === encoding, label + ': characterSet ' + doc.characterSet);
        assert(doc.body.textContent === text, label + ': text ' + JSON.stringify(doc.body.textContent));
    }

    // Step 4: a supported transport-layer charset is certain.
    check(load(response('/koi8', 'text/html; charset=koi8-r',
        [...ascii('<meta charset=gbk><p>'), 0xC1])), 'KOI8-R', 'а', 'transport');
    // Step 5: the prescan finds a declaration in the first kilobyte.
    check(load(response('/meta', 'text/html', [...ascii('<meta charset=windows-1251><p>'), 0xE6])),
        'windows-1251', 'ж', 'prescan');
    check(load(response('/pragma', 'text/html', [...ascii(
        '<meta http-equiv=Content-Type content="text/html; charset=windows-1253"><p>'), 0xE1])),
        'windows-1253', 'α', 'http-equiv pragma');
    // #change-the-encoding: a declaration after the first kilobyte.
    check(load(response('/late', 'text/html', [...ascii('<head><title>' + ' '.repeat(1100) +
        '</title><meta charset="&#119;indows-1251"></head><p>'), 0xE6])),
        'windows-1251', 'ж', 'late declaration');
    // Step 6: a same-origin container lends its encoding; another origin does not.
    check(load(response('/inherit', 'text/html', [...ascii('<p>'), 0xB9])),
        'windows-1250', 'ą', 'same-origin inheritance');
    check(load(response('/inherit', 'text/html', [...ascii('<p>'), 0xB9], 'https://frames.example.net/')),
        'windows-1252', '¹', 'cross-origin default');
    check(load('data:text/html,<p>%B9'), 'windows-1252', '¹', 'opaque data: default');
    // Step 1: a byte order mark is certain.
    check(load(response('/bom', 'text/html; charset=koi8-r', [0xFF, 0xFE, ...Array.from(
        '<p>ж', c => [c.charCodeAt(0) & 0xFF, c.charCodeAt(0) >> 8]).flat()])),
        'UTF-16LE', 'ж', 'byte order mark');
    // Text documents skip the prescan but keep the transport charset.
    const plain = load(response('/plain', 'text/plain; charset=windows-1253', [...ascii('<p>'), 0xE1]));
    assert(plain.characterSet === 'windows-1253' && plain.body.textContent === '<p>α', 'text/plain');
    const json = load(response('/json', 'application/json', [0x22, 0xC3, 0xA9, 0x22]));
    assert(json.characterSet === 'UTF-8' && json.body.textContent === '"é"', 'JSON is UTF-8');
    // XML MIME types follow XML: the declaration, else UTF-8 (never the container's).
    const xhtml = '<html xmlns="http://www.w3.org/1999/xhtml"><body><p>', end = ascii('</p></body></html>');
    check(load(response('/xhtml', 'application/xhtml+xml',
        [...ascii('<?xml version="1.0" encoding="koi8-r"?>' + xhtml), 0xC1, ...end])),
        'KOI8-R', 'а', 'XML declaration');
    check(load(response('/xhtml-default', 'application/xhtml+xml', [...ascii(xhtml), 0xC3, 0xA9, ...end])),
        'UTF-8', 'é', 'XML default');
    // A srcdoc document's source is already decoded text (HTML #charset).
    const srcdoc = document.createElement('iframe');
    srcdoc.srcdoc = '<p>ж';
    body.appendChild(srcdoc);
    drain();
    check(srcdoc.contentDocument, 'UTF-8', 'ж', 'srcdoc');
    // Documents created from strings or by APIs keep DOM's UTF-8 default.
    assert(new DOMParser().parseFromString('<meta charset=gbk>', 'text/html').characterSet === 'UTF-8', 'DOMParser');
    assert(document.implementation.createHTMLDocument('').characterSet === 'UTF-8', 'createHTMLDocument');
    assert(document.implementation.createDocument(null, '').inputEncoding === 'UTF-8', 'createDocument');
    assert(new Document().charset === 'UTF-8', 'new Document');
    // DOM #concept-node-clone copies the encoding.
    assert(document.cloneNode().characterSet === 'windows-1250', 'cloned document');
    let threw = false;
    try { Object.getOwnPropertyDescriptor(Document.prototype, 'characterSet').get.call(body); }
    catch (error) { threw = error instanceof TypeError; }
    assert(threw, 'characterSet brand check');
    return 'frame-document-encodings-ok';
})()
