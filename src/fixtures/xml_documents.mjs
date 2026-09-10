// HTML #dom-domparser-parsefromstring, DOM #interface-xmldocument,
// XHR #document-response; XML 1.0 and Namespaces in XML 1.0.
(() => {
    function assert(value, message) { if (!value) throw new Error(message); }
    function throws(fn, message) {
        try { fn(); } catch (error) { assert(error instanceof TypeError, message + ': ' + error); return; }
        throw new Error(message + ': did not throw');
    }
    const parser = new DOMParser();
    const doc = parser.parseFromString('<?xml version="1.0" encoding="UTF-16"?><?game load?><Root xmlns:p="urn:game"><p:Title Code="x">Zork &amp; more</p:Title><![CDATA[<raw>]]><!--note--></Root>', 'application/xml');
    assert(doc instanceof XMLDocument && doc instanceof Document && !(document instanceof XMLDocument), 'XML document interface');
    assert(Object.prototype.toString.call(doc) === '[object XMLDocument]', 'XML document tag');
    assert(doc.contentType === 'application/xml' && doc.defaultView === null, 'XML MIME and no window');
    assert(doc.documentElement.nodeName === 'Root' && doc.documentElement.namespaceURI === null, 'XML root/case/namespace: ' + doc.documentElement.nodeName + ' ' + doc.documentElement.namespaceURI + ' ' + doc.documentElement.textContent);
    assert(doc.body === null && doc.head === null && doc.children.length === 1, 'no HTML wrappers');
    const root = doc.documentElement, title = root.firstElementChild;
    assert(title.nodeName === 'p:Title' && title.prefix === 'p' && title.localName === 'Title' && title.namespaceURI === 'urn:game', 'expanded name');
    assert(title.getAttribute('Code') === 'x' && title.textContent === 'Zork & more', 'attributes and entities');
    assert(title.getAttribute('code') === null && root.getAttribute('xmlns:p') === 'urn:game', 'XML attribute case and qualified names');
    assert(title.ownerDocument === doc && title.firstChild.ownerDocument === doc, 'native document ownership');
    const cdata = title.nextSibling, comment = cdata.nextSibling, pi = doc.firstChild;
    assert(cdata instanceof CDATASection && cdata instanceof Text && cdata.nodeType === 4, 'CDATA interface');
    assert(cdata.nodeName === '#cdata-section' && cdata.data === '<raw>', 'CDATA data');
    assert(pi instanceof ProcessingInstruction && pi.nodeType === 7 && pi.target === 'game' && pi.nodeName === 'game', 'PI identity');
    assert(pi.data === 'load' && comment instanceof Comment && comment.data === 'note', 'PI/comment data');
    cdata.appendData('tail'); pi.data = 'ready';
    assert(cdata.textContent === '<raw>tail' && pi.textContent === 'ready' && root.textContent === 'Zork & more<raw>tail', 'character data mutation');
    assert(cdata.cloneNode(true) instanceof CDATASection && pi.cloneNode().target === 'game', 'XML character node clones');
    const created = doc.createElement('MiXeD');
    assert(created.localName === 'MiXeD' && created.namespaceURI === null && created.ownerDocument === doc, 'XML createElement');
    assert(doc.createTextNode('x').ownerDocument === doc && doc.createComment('x').ownerDocument === doc, 'detached created node owners');
    const xhtml = parser.parseFromString('<html xmlns="http://www.w3.org/1999/xhtml"><body><table/></body></html>', 'application/xhtml+xml');
    assert(xhtml.contentType === 'application/xhtml+xml' && xhtml.documentElement.nodeName === 'html', 'XHTML is XML');
    const table = xhtml.documentElement.firstElementChild.firstElementChild;
    assert(table instanceof HTMLTableElement && table.insertRow().insertCell().nodeName === 'td', 'HTML table API inside XML');
    const html = parser.parseFromString('<title>hello</title><p>body</p>', 'text/html');
    assert(html instanceof Document && !(html instanceof XMLDocument) && html.documentElement.nodeName === 'HTML', 'HTML parser unchanged');
    const plain = new Document();
    assert(plain instanceof Document && !(plain instanceof XMLDocument) && plain.documentElement === null && plain.contentType === 'application/xml', 'Document constructor distinct from XMLDocument');
    throws(() => new XMLDocument(), 'XMLDocument is not constructible');
    throws(() => parser.parseFromString('<root/>'), 'required MIME type');
    throws(() => parser.parseFromString('<root/>', 'TEXT/XML'), 'MIME enum is case-sensitive');
    throws(() => parser.parseFromString('<root/>', 'text/plain'), 'unsupported MIME');
    for (const text of ['<root>', '<a/><b/>', '<a><b></a>', '<p:a/>', '<a x="1" x="2"/>', '<a xmlns:p="urn:x" xmlns:q="urn:x" p:v="1" q:v="2"/>', '<a>&unknown;</a>']) {
        const broken = parser.parseFromString(text, 'text/xml');
        assert(broken instanceof XMLDocument && broken.documentElement.localName === 'parsererror' && broken.documentElement.namespaceURI === 'http://www.mozilla.org/newlayout/xml/parsererror.xml', 'XML parse error: ' + text);
    }
    const entity = parser.parseFromString('<!DOCTYPE root [<!ENTITY game "Zork">]><root>&game;</root>', 'text/xml');
    assert(entity.documentElement.textContent === 'Zork', 'bounded internal entities');
    const sequence = parser.parseFromString('<r>a<![CDATA[b]]>c<![CDATA[d]]></r>', 'text/xml').documentElement.childNodes;
    assert(sequence.length === 4 && sequence[0].nodeType === 3 && sequence[1].nodeType === 4 && sequence[2].nodeType === 3 && sequence[3].nodeType === 4, 'CDATA boundaries retained');

    // Exercise the real response assembly, without network variability.
    function response(type, body, responseType) {
        const xhr = new XMLHttpRequest();
        xhr.open('GET', 'https://example.com/game.xml');
        xhr.responseType = responseType;
        xhr.__inFlight = true;
        xhr.__finish([200, type, body, null]);
        return xhr;
    }
    const xhr = response('text/xml; charset=utf-8', '<metadata><emulator>dosbox</emulator></metadata>', 'document');
    assert(xhr.response instanceof XMLDocument && xhr.response === xhr.responseXML && xhr.response === xhr.response, 'shared cached XHR document');
    assert(xhr.response.documentElement.firstElementChild.nodeName === 'emulator', 'archive metadata keeps XML names');
    assert(xhr.response.URL === 'https://example.com/game.xml' && xhr.response.documentURI === xhr.response.URL, 'XHR document URL');
    const brokenXHR = response('application/xml', '<broken>', 'document');
    assert(brokenXHR.response === null && brokenXHR.responseXML === null, 'XHR parser failure yields null');
    assert(response('application/json', '{}', 'document').response === null, 'non-document MIME returns null');
    assert(response('application/xhtml+xml', '<html xmlns="http://www.w3.org/1999/xhtml"/>', 'document').response instanceof XMLDocument, 'XHTML XHR is XML');
    assert(response('text/html', '<p>hi</p>', '').responseXML === null, 'default responseType excludes HTML');
    const override = new XMLHttpRequest();
    override.open('GET', 'data:text/plain,%80', false);
    override.overrideMimeType('Text/Plain; charset="windows-1252"trailing; charset=utf-8');
    override.send();
    assert(override.responseText === '€', 'override encoding uses first quoted parameter');
    assert(override.getResponseHeader('content-type') === 'text/plain', 'override does not change response header');
    let stateError = '';
    try { override.overrideMimeType('text/xml'); } catch (error) { stateError = error.name; }
    assert(stateError === 'InvalidStateError', 'override rejected once done');
    throws(() => new XMLHttpRequest().overrideMimeType(), 'override required argument');
    throws(() => new XMLHttpRequest().overrideMimeType(Symbol()), 'override DOMString conversion');
    const overrideXML = new XMLHttpRequest();
    overrideXML.open('GET', 'data:text/plain,%3Cr%2F%3E', false);
    overrideXML.overrideMimeType('application/xml'); overrideXML.send();
    assert(overrideXML.responseXML instanceof XMLDocument && overrideXML.responseXML.documentElement.localName === 'r', 'override selects XML parsing');
    const invalidMime = new XMLHttpRequest();
    invalidMime.open('GET', 'data:text/xml,%3Cr%2F%3E', false);
    invalidMime.overrideMimeType('not a MIME type'); invalidMime.send();
    assert(invalidMime.responseXML === null, 'invalid override falls back to octet-stream');
    const bom = new XMLHttpRequest();
    bom.open('GET', 'data:text/plain,%FF%FE%41%00', false);
    bom.overrideMimeType('text/plain;charset=utf-8'); bom.send();
    assert(bom.responseText === 'A', 'BOM takes precedence over override charset');
    return 'xml-documents-ok';
})();
