// Original regression: platform objects reachable by author script carry no
// TRust-internal properties. Internal state lives in internal slots (Web IDL
// platform objects), never in author-visible "__…" or "…trust…" properties.
(async function () {
    const standard = new Set(['__proto__', '__defineGetter__', '__defineSetter__', '__lookupGetter__',
        '__lookupSetter__', 'isTrusted']);
    const leaks = new Map();
    const seen = new WeakSet();
    function scan(label, object, depth = 0) {
        if (object === null || (typeof object !== 'object' && typeof object !== 'function')) return;
        // The engine's legacy RegExp statics are outside the platform.
        if (object === RegExp || seen.has(object)) return;
        seen.add(object);
        let keys;
        try { keys = Reflect.ownKeys(object); } catch (e) { return; }
        for (const key of keys) {
            if (typeof key !== 'string' || standard.has(key) || !(key.startsWith('__') || /trust/i.test(key))) continue;
            const list = leaks.get(key) || [];
            if (list.length < 3) list.push(label);
            leaks.set(key, list);
        }
        if (depth < 1) {
            let proto;
            try { proto = Object.getPrototypeOf(object); } catch (e) { proto = null; }
            for (let level = 0; proto && level < 10; level++, proto = Object.getPrototypeOf(proto))
                scan(label + '.__proto__' + level, proto, 1);
        }
    }
    const pending = [];
    const tick = () => new Promise(resolve => setTimeout(resolve, 0));
    const html = document.documentElement || document.appendChild(document.createElement('html'));
    const body = document.body || html.appendChild(document.createElement('body'));
    scan('window', globalThis); scan('document', document);
    for (const name of Object.getOwnPropertyNames(globalThis)) {
        let value;
        try { value = globalThis[name]; } catch (e) { continue; }
        if (typeof value === 'function' && /^[A-Z]/.test(name) && value !== RegExp) {
            scan(name, value); scan(name + '.prototype', value.prototype);
        } else if (value && typeof value === 'object') scan(name, value);
    }
    const tags = ['div', 'span', 'input', 'textarea', 'select', 'option', 'button', 'form', 'a', 'img', 'iframe',
        'canvas', 'video', 'audio', 'style', 'script', 'link', 'template', 'table', 'tr', 'td', 'ul', 'li', 'label',
        'dialog', 'details', 'summary', 'p', 'slot', 'meta', 'object', 'output', 'progress', 'meter', 'fieldset',
        'area', 'map', 'base', 'col', 'datalist', 'legend', 'optgroup', 'picture', 'source', 'track', 'time'];
    const elements = [];
    for (const tag of tags) {
        const element = document.createElement(tag);
        element.id = 'probe-' + tag; element.className = 'a b';
        element.setAttribute('data-x', '1'); element.style.color = 'red';
        element.title = 'title'; element.onclick = () => {};
        element.addEventListener('click', () => {});
        element.classList.toggle('c'); void element.dataset.x; void element.attributes.length;
        body.appendChild(element);
        void element.localName; void element.tagName; void element.namespaceURI; void element.prefix;
        elements.push(element);
    }
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    body.appendChild(svg); elements.push(svg);
    const form = document.getElementById('probe-form');
    const input = document.createElement('input'); input.type = 'checkbox'; input.name = 'check';
    const text = document.createElement('input'); text.name = 'field'; text.value = 'value';
    const textarea = document.createElement('textarea'); textarea.textContent = 'initial'; textarea.value = 'changed';
    form.append(input, text, textarea); input.checked = true; input.indeterminate = true;
    text.setCustomValidity('custom'); void text.validationMessage; form.reset();
    void form.elements.field; void form.elements.length; void form.elements.namedItem('check');
    const select = document.getElementById('probe-select');
    const one = document.createElement('option'), two = document.createElement('option');
    one.value = '1'; two.value = '2'; select.append(one, two); select.value = '2'; void select.selectedOptions;
    const button = document.getElementById('probe-button'); button.click();
    const dialog = document.getElementById('probe-dialog'); dialog.show(); dialog.close('done');
    const details = document.getElementById('probe-details'); details.open = true;
    const link = document.getElementById('probe-link'); link.rel = 'stylesheet'; void link.relList; void link.sheet;
    const style = document.getElementById('probe-style'); style.textContent = 'p { color: blue } @media screen { a { color: red } }';
    const sheet = style.sheet; void sheet.cssRules; sheet.insertRule('div { margin: 0 }', 0); void sheet.cssRules[0].cssText;
    const media = document.getElementById('probe-video'); media.volume = 0.5; media.muted = true; media.playbackRate = 2;
    void media.textTracks; void media.audioTracks; void media.videoTracks;
    const script = document.createElement('script'); script.async = false; body.appendChild(script);
    const constructed = new CSSStyleSheet(); constructed.replaceSync('a { color: green }');
    document.adoptedStyleSheets = [constructed];
    const host = document.createElement('div'); body.appendChild(host);
    const shadow = host.attachShadow({mode: 'open'}); shadow.innerHTML = '<slot></slot><p>shadow</p>';
    host.appendChild(document.createElement('span'));
    class ProbeElement extends HTMLElement { connectedCallback() { this.connected = true; } }
    customElements.define('probe-element', ProbeElement);
    const custom = document.createElement('probe-element'); body.appendChild(custom);
    const frame = document.createElement('iframe'); frame.srcdoc = '<p>frame</p>'; body.appendChild(frame);
    const sourced = document.createElement('iframe'); sourced.src = 'about:blank'; body.appendChild(sourced);
    void document.styleSheets.length; void document.fonts; void document.domain;
    const range = document.createRange(); range.selectNodeContents(body);
    const walker = document.createTreeWalker(body); walker.firstChild(); walker.nextSibling();
    const iterator = document.createNodeIterator(body); iterator.nextNode(); iterator.previousNode();
    const event = new CustomEvent('probe', {bubbles: true, detail: 1});
    body.addEventListener('probe', e => { e.stopPropagation(); void e.composedPath(); });
    body.dispatchEvent(event);
    const records = [];
    const mutations = new MutationObserver(list => records.push(...list));
    mutations.observe(body, {childList: true, attributes: true, subtree: true});
    body.appendChild(document.createElement('b')); body.setAttribute('data-y', '2');
    const intersection = new IntersectionObserver(entries => records.push(...entries));
    intersection.observe(button);
    const resize = typeof ResizeObserver === 'function' ? new ResizeObserver(entries => records.push(...entries)) : null;
    if (resize) resize.observe(button);
    const objects = [range, walker, iterator, event, mutations, intersection, resize, getSelection(),
        new URL('https://example.com/a?b=1#c'), new URLSearchParams('a=1'), new Headers({a: 'b'}),
        new FormData(form), new Blob(['x']), new File(['y'], 'f.txt'), new FileReader(),
        new TextDecoder(), new TextEncoder(), new AbortController(), AbortSignal.abort(),
        new MessageChannel(), new DOMParser().parseFromString('<p>x</p>', 'text/html'),
        new Request('https://example.com/', {method: 'POST', body: 'x'}), new Response('body'),
        document.implementation.createHTMLDocument('x'), new Image(), new Audio(),
        new FontFace('Probe', 'url(x.woff)'), new Notification('x'), new ClipboardItem({'text/plain': 'x'}),
        typeof URLPattern === 'function' ? new URLPattern({pathname: '/:id'}) : null,
        new XMLSerializer(), document.createDocumentFragment(), document.createComment('c'),
        document.getElementById('probe-canvas').getContext('2d'), new ImageData(1, 1),
        new SpeechSynthesisUtterance('x'), new OfflineAudioContext(1, 128, 8000)];
    const url = objects[8]; url.searchParams.append('d', '2'); url.search = '?e=3';
    const reader = objects[14]; reader.readAsText(objects[12]);
    objects[17].abort();
    const xhr = new XMLHttpRequest();
    xhr.open('GET', 'data:text/plain,hello', false); xhr.overrideMimeType('text/plain'); xhr.send();
    void xhr.responseText; void xhr.getAllResponseHeaders();
    const asyncXhr = new XMLHttpRequest(); asyncXhr.open('GET', 'data:text/plain,hi'); asyncXhr.send();
    objects.push(xhr, asyncXhr);
    pending.push(fetch('data:text/plain,body').then(response => { objects.push(response, response.headers); return response.text(); }));
    pending.push(objects[22].clone().text());
    const database = await new Promise((resolve, reject) => {
        const request = indexedDB.open('probe', 1);
        objects.push(request);
        request.onupgradeneeded = () => {
            const db = request.result, store = db.createObjectStore('items', {keyPath: 'id'});
            store.createIndex('name', 'name');
            objects.push(db, request.transaction, store, store.index('name'));
        };
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
    });
    const transaction = database.transaction('items', 'readwrite');
    const store = transaction.objectStore('items');
    store.put({id: 1, name: 'one'}); store.put({id: 2, name: 'two'});
    const cursorRequest = store.openCursor();
    await new Promise(resolve => {
        cursorRequest.onsuccess = () => {
            const cursor = cursorRequest.result;
            if (cursor) { objects.push(cursor, cursor.source); cursor.continue(); } else resolve();
        };
    });
    objects.push(database, transaction, store, store.index('name'), cursorRequest, IDBKeyRange.bound(1, 2));
    await new Promise(resolve => { transaction.oncomplete = resolve; });
    await Promise.all(pending);
    for (let i = 0; i < 5; i++) await tick();
    objects.push(...records, document.adoptedStyleSheets, constructed, sheet, sheet.cssRules, sheet.cssRules[0],
        sheet.media, sheet.cssRules[1] && sheet.cssRules[1].cssRules, document.styleSheets, document.fonts,
        frame.contentWindow, frame.contentDocument, sourced.contentDocument, shadow, custom, form.elements,
        form.elements.namedItem('check'), select.options, body.children, body.childNodes,
        document.querySelectorAll('div'), document.getElementsByClassName('a'), body.attributes,
        body.attributes[0], navigator.storage, navigator.serviceWorker, navigator.clipboard,
        navigator.mediaDevices, navigator.credentials, navigator.permissions, speechSynthesis,
        customElements, history, location, navigator, screen, performance, crypto, localStorage,
        sessionStorage, caches, globalThis.visualViewport);
    for (const element of elements.concat([input, text, textarea, script, host, custom])) {
        scan('<' + element.localName + '>', element);
        for (const part of ['classList', 'style', 'attributes', 'dataset', 'relList', 'sheet', 'shadowRoot'])
            try { scan('<' + element.localName + '>.' + part, element[part]); } catch (e) {}
    }
    for (const [index, object] of objects.entries()) {
        let name;
        try { name = Object.prototype.toString.call(object); } catch (e) { name = 'object'; }
        scan(index + ':' + name, object);
    }
    return [...leaks].map(([key, labels]) => key + ' <- ' + labels.join(', ')).sort().join('\n') || 'none';
})().then(value => globalThis.internalNamesResult = value,
          error => globalThis.internalNamesResult = 'ERROR:' + error.name + ':' + error.message + '\n' + error.stack);
