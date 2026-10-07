(function () {
    // HTML #loading-a-document and #read-xml: a navigation to an XML MIME
    // type (application/xhtml+xml and image/svg+xml included) creates a
    // Document built by the XML parser, with that content type.
    function assert(value, label) { if (!value) throw Error(label); }
    function drain() {
        let budget = 1000;
        while (__trust.hasPlatformTask() && budget-- > 0) __trust.runPlatformTask();
        assert(budget > 0, 'navigation task queue drains');
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const XHTML = 'http://www.w3.org/1999/xhtml';
    const cases = [
        ['application/xml', '<foo>Dummy XML <b/></foo>\n', 'foo', null, 'Dummy XML '],
        ['text/xml', '<?xml version="1.0"?><x:r xmlns:x="urn:x"><![CDATA[a<b]]></x:r>', 'r', 'urn:x', 'a<b'],
        ['application/xhtml+xml', '<!DOCTYPE html>\n<html xmlns="' + XHTML +
            '"><head><title>T</title></head><body/></html>\n', 'html', XHTML, 'T'],
        ['image/svg+xml', '<svg xmlns="http://www.w3.org/2000/svg"><title>S</title></svg>', 'svg',
            'http://www.w3.org/2000/svg', 'S'],
    ];
    for (const [type, source, localName, namespace, text] of cases) {
        const frame = document.createElement('iframe');
        body.appendChild(frame);
        let loads = 0;
        frame.onload = () => { loads++; };
        const url = URL.createObjectURL(new Blob([source], {type}));
        frame.src = url;
        __trust.hydrateFrames(); drain();
        const doc = frame.contentDocument;
        assert(doc.contentType === type, 'content type ' + doc.contentType);
        const root = doc.documentElement;
        assert(root && root.localName === localName && root.namespaceURI === namespace, 'root ' + type);
        assert(root.textContent === text, 'XML text content for ' + type + ': ' + JSON.stringify(root.textContent));
        assert(doc.createElement('x').namespaceURI === (type === 'application/xhtml+xml' ? XHTML : null),
            'createElement namespace in ' + type);
        assert(type !== 'application/xhtml+xml' || doc.doctype.name === 'html', 'XML doctype');
        assert(type !== 'text/xml' || root.firstChild.nodeType === 4, 'CDATA section');
        assert(loads === 1, 'one completed load');
        URL.revokeObjectURL(url);
        frame.remove();
    }
    const broken = document.createElement('iframe');
    body.appendChild(broken);
    broken.src = URL.createObjectURL(new Blob(['<a><b></a>'], {type: 'application/xml'}));
    __trust.hydrateFrames(); drain();
    const error = broken.contentDocument.documentElement;
    assert(error.localName === 'parsererror' &&
        error.namespaceURI === 'http://www.mozilla.org/newlayout/xml/parsererror.xml', 'XML parse error document');
    return 'frame-xml-documents-ok';
})()
