// XMLHttpRequest Standard conformance probes shared by Window and
// DedicatedWorkerGlobalScope (run by platform_fixture_in_window_and_secure_worker;
// `check`, `throws` and `rejects` come from that wrapper).
const isWindow = typeof document !== 'undefined';
const finished = xhr => new Promise(resolve => xhr.addEventListener('loadend', () => resolve(xhr)));
const send = (method, url, configure) => {
    const xhr = new XMLHttpRequest();
    xhr.open(method, url);
    if (configure) configure(xhr);
    const done = finished(xhr);
    xhr.send();
    return done;
};

// #interface-xmlhttprequest
check(typeof XMLHttpRequest === 'function' && typeof XMLHttpRequestUpload === 'function' &&
    typeof XMLHttpRequestEventTarget === 'function', 'interfaces exposed');
check(('responseXML' in XMLHttpRequest.prototype) === isWindow, 'responseXML is [Exposed=Window]');
check(Object.prototype.toString.call(new XMLHttpRequest()) === '[object XMLHttpRequest]', 'class string');
check(XMLHttpRequest.LOADING === 3 && new XMLHttpRequest().DONE === 4, 'state constants');
throws(() => new XMLHttpRequestUpload(), 'TypeError', 'XMLHttpRequestUpload is not constructible');
throws(() => XMLHttpRequest.prototype.open.call({}, 'GET', 'data:,x'), 'TypeError', 'brand check');
check(Object.getOwnPropertyNames(new XMLHttpRequest()).length === 0, 'no own properties');

// #the-open()-method
const opened = new XMLHttpRequest();
throws(() => opened.open('GET'), 'TypeError', 'open requires two arguments');
throws(() => opened.open('G ET', 'data:,x'), 'SyntaxError', 'method must be a token');
throws(() => opened.open('TRACE', 'data:,x'), 'SecurityError', 'forbidden method');
throws(() => opened.open('GET', 'http://['), 'SyntaxError', 'URL parse failure');
throws(() => opened.open('G\u0100T', 'data:,x'), 'TypeError', 'method is a ByteString');
check(opened.readyState === 0, 'failed open leaves UNSENT');
let openEvents = 0;
opened.onreadystatechange = () => openEvents++;
opened.open('GET', 'data:,x');
opened.open('GET', 'data:,y');
check(opened.readyState === 1 && openEvents === 1, 'readystatechange only on the first open');

// #the-setrequestheader()-method
const headers = new XMLHttpRequest();
throws(() => headers.setRequestHeader('X-A', 'b'), 'InvalidStateError', 'setRequestHeader before open');
headers.open('POST', 'data:,x');
throws(() => headers.setRequestHeader('X A', 'b'), 'SyntaxError', 'invalid header name');
throws(() => headers.setRequestHeader('X-A', 'b\r\nc'), 'SyntaxError', 'invalid header value');
headers.setRequestHeader('Cookie', 'ignored');
headers.setRequestHeader('X-A', ' padded\t');

// #the-withcredentials-attribute, #the-timeout-attribute and #the-responsetype-attribute
const attributes = new XMLHttpRequest();
attributes.withCredentials = 1;
check(attributes.withCredentials === true, 'withCredentials converts to boolean');
attributes.timeout = 12.9;
check(attributes.timeout === 12, 'timeout is an unsigned long');
attributes.responseType = 'bogus';
check(attributes.responseType === '', 'invalid responseType enum value is ignored');
attributes.responseType = 'document';
check(attributes.responseType === (isWindow ? 'document' : ''), 'document responseType only in a Window');
const syncTimeout = new XMLHttpRequest();
syncTimeout.open('GET', 'data:,x', false);
if (isWindow) throws(() => { syncTimeout.timeout = 5; }, 'InvalidAccessError', 'sync timeout in a Window');
else { syncTimeout.timeout = 5; check(syncTimeout.timeout === 5, 'sync timeout in a worker'); }

// Event handler attributes are [LegacyTreatNonObjectAsNull].
const handlers = new XMLHttpRequest();
const handlerObject = {};
handlers.onload = handlerObject;
handlers.onerror = 5;
check(handlers.onload === handlerObject && handlers.onerror === null && handlers.upload.onload === null,
    'event handler attribute values');

// An asynchronous request's states and events (#the-send()-method, #handle-response-end-of-body).
const order = [];
const ordered = await send('GET', 'data:text/plain;charset=utf-8,hello#fragment', xhr => {
    xhr.onreadystatechange = () => order.push('rs' + xhr.readyState);
    for (const type of ['loadstart', 'progress', 'load', 'loadend'])
        xhr.addEventListener(type, event => order.push(type + ':' + event.loaded + '/' + event.total +
            (event.lengthComputable ? '' : '?') + (event instanceof ProgressEvent ? '' : '!')));
});
check(order.join(' ') === 'loadstart:0/0? rs2 rs3 progress:5/0? progress:5/0? rs4 load:5/0? loadend:5/0?',
    'event order: ' + order.join(' '));
check(ordered.status === 200 && ordered.statusText === 'OK' && ordered.responseText === 'hello' &&
    ordered.response === 'hello', 'text response');
check(ordered.responseURL === 'data:text/plain;charset=utf-8,hello', 'responseURL excludes the fragment');
check(ordered.getResponseHeader('CONTENT-TYPE') === 'text/plain;charset=utf-8' && ordered.getResponseHeader('x') === null,
    'getResponseHeader');
check(ordered.getAllResponseHeaders() === 'content-type: text/plain;charset=utf-8\r\n', 'getAllResponseHeaders');
throws(() => { ordered.responseType = 'text'; }, 'InvalidStateError', 'responseType once done');
throws(() => ordered.overrideMimeType('text/plain'), 'InvalidStateError', 'overrideMimeType once done');

// Response types (#the-response-attribute).
const buffer = await send('GET', 'data:application/octet-stream;base64,AAEC', xhr => { xhr.responseType = 'arraybuffer'; });
check(buffer.response instanceof ArrayBuffer && buffer.response.byteLength === 3 && buffer.response === buffer.response &&
    new Uint8Array(buffer.response)[2] === 2, 'arraybuffer response');
throws(() => buffer.responseText, 'InvalidStateError', 'responseText needs a text response type');
const blob = await send('GET', 'data:text/x-probe;a=b,abc', xhr => { xhr.responseType = 'blob'; });
check(blob.response instanceof Blob && blob.response.size === 3 && blob.response.type === 'text/x-probe;a=b',
    'blob response type is the final MIME type');
check(await blob.response.text() === 'abc', 'blob response bytes');
const json = await send('GET', 'data:application/json,%EF%BB%BF%7B%22a%22%3A%5B1%5D%7D', xhr => { xhr.responseType = 'json'; });
check(json.response && json.response.a[0] === 1 && json.response === json.response, 'json response strips the BOM');
const badJSON = await send('GET', 'data:application/json,%7B', xhr => { xhr.responseType = 'json'; });
check(badJSON.response === null, 'invalid JSON is null');
const utf16JSON = await send('GET', 'data:application/json,%FF%FE%7B%00%7D%00', xhr => { xhr.responseType = 'json'; });
check(utf16JSON.response === null, 'JSON is decoded as UTF-8 only');
if (typeof SharedArrayBuffer === 'function') {
    const shared = new XMLHttpRequest();
    shared.open('POST', 'data:,x');
    throws(() => shared.send(new SharedArrayBuffer(4)), 'TypeError', 'a SharedArrayBuffer body');
}

// Text decoding (#text-response and #final-charset).
const legacy = await send('GET', 'data:text/plain;charset=windows-1252,%80');
check(legacy.responseText === '\u20ac', 'response charset');
const overridden = await send('GET', 'data:text/plain,%80', xhr => xhr.overrideMimeType('text/plain;charset=windows-1252'));
check(overridden.responseText === '\u20ac' && overridden.getResponseHeader('Content-Type') === 'text/plain',
    'override MIME type charset');
const bom = await send('GET', 'data:text/plain;charset=windows-1252,%FF%FEa%00');
check(bom.responseText === 'a', 'a byte order mark wins over the charset');
const xmlDeclared = await send('GET', 'data:text/xml,' + encodeURIComponent('<?xml version="1.0" encoding="windows-1252"?><r/>') + '%80');
check(xmlDeclared.responseText.endsWith('<r/>\u20ac'), 'XML encoding declaration');
const replacement = await send('GET', 'data:text/plain;charset=iso-2022-kr,abc');
check(replacement.responseText === '\ufffd', 'replacement encoding');

// blob: URLs, Content-Length and ranges (Fetch #scheme-fetch).
const blobURL = URL.createObjectURL(new Blob(['abcdef'], {type: 'text/x'}));
const lengths = [];
const fromBlob = await send('GET', blobURL, xhr => xhr.addEventListener('load', event => lengths.push(event.total, event.lengthComputable)));
check(fromBlob.responseText === 'abcdef' && lengths.join() === '6,true', 'blob: URL with a Content-Length');
check(fromBlob.getAllResponseHeaders() === 'content-length: 6\r\ncontent-type: text/x\r\n', 'sorted response headers');
const ranged = await send('GET', blobURL, xhr => xhr.setRequestHeader('Range', 'bytes=1-2'));
check(ranged.status === 206 && ranged.statusText === 'Partial Content' && ranged.responseText === 'bc' &&
    ranged.getResponseHeader('Content-Range') === 'bytes 1-2/6', 'blob: URL range request');
URL.revokeObjectURL(blobURL);

// Network errors (#handle-errors and #request-error-steps).
const errorEvents = [];
const failed = await send('GET', blobURL, xhr => {
    xhr.onreadystatechange = () => errorEvents.push('rs' + xhr.readyState);
    xhr.onerror = event => errorEvents.push('error:' + event.loaded + '/' + event.total);
    xhr.onload = () => errorEvents.push('load');
});
check(errorEvents.join(' ') === 'rs4 error:0/0' && failed.status === 0 && failed.responseText === '' &&
    failed.getAllResponseHeaders() === '' && failed.responseURL === '', 'revoked blob: URL is a network error');
const crossOrigin = await send('GET', 'about:blank');
check(crossOrigin.status === 0 && crossOrigin.readyState === 4, 'about: in "cors" mode is a network error');

// HEAD has a null body.
const head = await send('HEAD', 'data:text/plain,abc');
check(head.status === 200 && head.responseText === '', 'HEAD response body is null');

// #the-abort()-method
const abortEvents = [];
const aborted = new XMLHttpRequest();
aborted.open('GET', 'data:,late');
aborted.onreadystatechange = () => abortEvents.push('rs' + aborted.readyState);
for (const type of ['abort', 'loadend', 'load', 'error'])
    aborted.addEventListener(type, () => abortEvents.push(type));
aborted.upload.onabort = () => abortEvents.push('upload-abort');
aborted.send();
aborted.abort();
check(abortEvents.join(' ') === 'rs4 abort loadend' && aborted.readyState === 0 && aborted.status === 0,
    'abort after send: ' + abortEvents.join(' '));
await send('GET', 'data:,other');
check(abortEvents.join(' ') === 'rs4 abort loadend', 'an aborted fetch delivers nothing');
const credentials = new XMLHttpRequest();
credentials.open('GET', 'data:,x');
credentials.send();
throws(() => { credentials.withCredentials = true; }, 'InvalidStateError', 'withCredentials after send');
throws(() => credentials.send(), 'InvalidStateError', 'send twice');
credentials.abort();

// Upload events: a body with upload listeners, then without.
const uploadEvents = [];
const uploading = new XMLHttpRequest();
uploading.open('POST', 'data:,ok');
for (const type of ['loadstart', 'progress', 'load', 'loadend'])
    uploading.upload.addEventListener(type, event => uploadEvents.push(type + ':' + event.loaded + '/' + event.total));
const uploaded = finished(uploading);
uploading.send(new Uint8Array([1, 2, 3]));
await uploaded;
check(uploadEvents.join(' ') === 'loadstart:0/3 progress:3/3 load:3/3 loadend:3/3', 'upload events: ' + uploadEvents.join(' '));
let ignoredUpload = false;
await send('GET', 'data:,ok', xhr => { xhr.upload.onloadstart = () => { ignoredUpload = true; }; });
check(!ignoredUpload, 'no upload events without a body');

// Synchronous requests.
const sync = new XMLHttpRequest();
const syncEvents = [];
sync.onreadystatechange = () => syncEvents.push('rs' + sync.readyState);
sync.onprogress = () => syncEvents.push('progress');
sync.onload = () => syncEvents.push('load');
sync.open('GET', 'data:,synchronous', false);
sync.send();
check(sync.responseText === 'synchronous' && syncEvents.join(' ') === 'rs1 rs4 load', 'synchronous request: ' + syncEvents.join(' '));
const syncFailure = new XMLHttpRequest();
syncFailure.open('GET', 'about:blank', false);
throws(() => syncFailure.send(), 'NetworkError', 'synchronous network error throws');
check(syncFailure.readyState === 4, 'synchronous network error leaves DONE');
if (!isWindow) {
    const syncBuffer = new XMLHttpRequest();
    syncBuffer.open('GET', 'data:,ab', false);
    syncBuffer.responseType = 'arraybuffer';
    syncBuffer.send();
    check(syncBuffer.response.byteLength === 2, 'a worker sync request may set responseType');
}
