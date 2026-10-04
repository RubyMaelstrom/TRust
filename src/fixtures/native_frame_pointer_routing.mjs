// Defines the setup and the input sequence separately: the test reads the
// child's parser-created node IDs from the native arena in between, so no
// Realm creates their wrappers before the routed input does.
globalThis.frameRoutingSetup = function () {
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    body.style.margin = '0';
    const frame = document.createElement('iframe');
    frame.style.border = '0';
    frame.setAttribute('width', '300'); frame.setAttribute('height', '150');
    // Parser-created content the child's own scripts never touch before input.
    frame.srcdoc = '<body style="margin:0"><p id=text style="margin:0;height:40px">Verify</p>' +
        '<input id=box type=checkbox style="margin:0;width:20px;height:20px"></body>';
    body.appendChild(frame); __trust.hydrateFrames();
    globalThis.routingFrame = frame;
};
globalThis.frameRoutingRun = function (textId, boxId) {
    function assert(value, label) { if (!value) throw Error(label); }
    const frame = globalThis.routingFrame, body = document.body;
    const child = frame.contentWindow;
    const log = [];
    for (const type of ['pointerover', 'pointermove', 'pointerout', 'pointerdown', 'pointerup', 'pointercancel', 'click'])
        window.addEventListener(type, e => { if (e.target === frame || e.target === window) log.push('top:' + type); }, true);
    window.addEventListener('blur', e => { if (e.target === window) log.push('top:window-blur'); });
    window.addEventListener('focus', e => { if (e.target === window) log.push('top:window-focus'); });
    child.eval(`
        globalThis.seen = [];
        globalThis.realmOK = true;
        for (const type of ['pointerover','pointermove','pointerout','pointerdown','pointerup','pointercancel','click','focus','blur'])
            window.addEventListener(type, e => {
                const t = e.target;
                if (t !== window && !(t instanceof Node)) realmOK = false;
                if (t !== window && t.nodeType === 1 && !(t instanceof HTMLElement)) realmOK = false;
                seen.push(type + ':' + (t === window ? 'window' : t.id || t.nodeName));
                if (type === 'click') globalThis.click = {x: e.clientX, y: e.clientY, ox: e.offsetX, oy: e.offsetY,
                    px: e.pageX, sx: e.screenX, type: e.pointerType};
                if (type === 'pointerdown') globalThis.down = {x: e.clientX, focus: document.hasFocus()};
            }, true);
    `);
    const rect = frame.getBoundingClientRect();
    child.seen.length = 0;
    // Pointer Events boundary/motion events: motion inside the child Document
    // is dispatched there only; the embedding Document sees the transition.
    __trust.hover(null, rect.left - 5, rect.top + 10);
    __trust.hover(textId, rect.left + 15, rect.top + 10);
    __trust.hover(textId, rect.left + 25, rect.top + 12);
    assert(child.seen.join('|') === 'pointerover:text|pointermove:text|pointermove:text',
        'child motion on an untouched parser node: ' + child.seen);
    assert(child.realmOK, 'child events target child-Realm wrappers');
    assert(log.join('|') === 'top:pointerover', 'embedding Document sees only the transition: ' + log);
    // A hit on the container below the child's boxes lands on its root element.
    child.seen.length = 0;
    __trust.hover(nodeIdOf(frame), rect.left + 200, rect.top + 140);
    assert(child.seen.join('|') === 'pointerout:text|pointerover:HTML|pointermove:HTML',
        'container content box hit: ' + child.seen);
    // HTML #focus-update-steps / #has-focus-steps across navigables.
    assert(!child.document.hasFocus() && document.hasFocus(), 'child unfocused before input');
    child.seen.length = 0; log.length = 0;
    const x = rect.left + 30.6, y = rect.top + 50.3;
    __trust.hover(boxId, x, y);
    __trust.pointerButton(boxId, true, x, y);
    // A frontend focus change without a pointer lock leaves the press running.
    __trust.releasePointerLock(false);
    __trust.pointerButton(boxId, false, x, y);
    __trust.click(boxId);
    assert(!child.seen.includes('pointercancel:box'), 'no pointercancel without a lock: ' + child.seen);
    assert(log.includes('top:window-blur') && !log.includes('top:pointercancel'), 'embedding window blur: ' + log);
    const order = child.seen.filter(s => /^(focus|pointerdown|pointerup|click)/.test(s)).join('|');
    assert(order === 'pointerdown:box|focus:window|focus:box|pointerup:box|click:box', 'child focus chain: ' + order);
    assert(child.down.focus === false && child.document.hasFocus() && document.hasFocus(), 'has focus steps');
    assert(document.activeElement === frame && child.document.activeElement.id === 'box', 'active elements');
    // Pointer Events #event-coordinates: click keeps integer coordinates.
    assert(child.down.x !== Math.floor(child.down.x), 'pointerdown keeps fractional coordinates');
    assert(child.click.x === Math.floor(child.click.x) && child.click.ox === Math.floor(child.click.ox) &&
        child.click.y === Math.floor(child.click.y) && child.click.oy === Math.floor(child.click.oy) &&
        child.click.px === Math.floor(child.click.px) && child.click.type === 'mouse',
        'integral click coordinates: ' + JSON.stringify(child.click));
    // Focus returns to the embedding Document.
    child.seen.length = 0; log.length = 0;
    const button = document.createElement('button'); button.textContent = 'top'; body.appendChild(button);
    __trust.focusPage(nodeIdOf(button));
    assert(child.seen.join('|') === 'blur:box|blur:window', 'child leaves the chain: ' + child.seen);
    assert(log.join('|') === 'top:window-focus', 'embedding window regains focus: ' + log);
    assert(!child.document.hasFocus() && document.activeElement === button, 'focus back in the embedding Document');
    // Touch Events: no legacy ontouch* handlers with maxTouchPoints 0.
    assert(navigator.maxTouchPoints === 0 && !('ontouchstart' in window) && !('ontouchstart' in document) &&
        !('ontouchstart' in child) && typeof TouchEvent === 'function', 'legacy touch handlers');
    return 'native-frame-pointer-routing-ok';

    function nodeIdOf(element) { return element.__id; }
};
