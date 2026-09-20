(function () {
    function assert(value, label) { if (!value) throw Error(label); }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const frame = document.createElement('iframe');
    frame.srcdoc = '<button id="button">Check</button><input id="field">';
    body.appendChild(frame); __trust.hydrateFrames();
    const child = frame.contentWindow;
    child.eval(`
        globalThis.seen = [];
        const button = document.getElementById('button');
        for (const type of ['pointerdown','mousedown','pointerup','mouseup','click'])
            button.addEventListener(type, e => seen.push([type, e.isTrusted,
                e.view === window, e.clientX, e.clientY, e.buttons].join(':')));
        button.addEventListener('click', e => {
            globalThis.clickAliases = e.x === e.clientX && e.y === e.clientY;
            globalThis.clickHitTest = !!document.elementFromPoint(e.x, e.y);
        });
        button.addEventListener('mouseover', e => globalThis.hovered = e.isTrusted);
        button.addEventListener('mouseout', e => globalThis.left = e.isTrusted);
        document.getElementById('field').addEventListener('keydown', e => {
            globalThis.keyView = e.isTrusted && e.view === window && e.key === 'Enter';
            e.preventDefault();
        });
        document.getElementById('field').addEventListener('input', e => globalThis.edited = e.target.value);
        globalThis.cancelDown = e => e.preventDefault();
    `);
    const button = child.document.getElementById('button'), field = child.document.getElementById('field');
    assert(child.document instanceof child.Document && child.document instanceof child.Node,
        'child Document interface inheritance');
    assert(child.document.parentNode === null && child.document.ownerDocument === null &&
        child.document.nextSibling === null && child.document.previousSibling === null,
        'child Document is a tree root');
    assert(child.document.documentElement.parentNode === child.document &&
        button.getRootNode() === child.document && button.ownerDocument === child.document,
        'child DOM tree ends at its Document');
    assert(child.document.querySelector('html') === child.document.documentElement &&
        child.document.getElementsByTagName('html')[0] === child.document.documentElement,
        'document selectors include the document element');
    assert(__trust.clickables().includes(button.__id), 'child click listener discovery');
    assert(__trust.hoverables().includes(button.__id), 'child hover listener discovery');
    const rect = frame.getBoundingClientRect();
    const x = rect.left + frame.clientLeft + 10, y = rect.top + frame.clientTop + 12;
    __trust.hover(button.__id, x, y);
    __trust.pointerButton(button.__id, true, x, y);
    assert(document.activeElement === frame && child.document.activeElement === button, 'focus chain: ' +
        [document.activeElement && document.activeElement.localName,
         child.document.activeElement && child.document.activeElement.localName,
         child.document.activeElement && child.document.activeElement.__id, button.__id, child.seen]);
    __trust.pointerButton(button.__id, false, x, y);
    __trust.click(button.__id);
    assert(child.seen.join('|') === [
        'pointerdown:true:true:10:12:1', 'mousedown:true:true:10:12:1',
        'pointerup:true:true:10:12:0', 'mouseup:true:true:10:12:0',
        'click:true:true:10:12:0'
    ].join('|'), 'native input order/realm/coordinates: ' + child.seen);
    assert(child.clickAliases && child.clickHitTest, 'native child pointer aliases feed finite hit-test coordinates');
    __trust.hover(null, x + 400, y);
    assert(child.hovered && child.left, 'iframe enter/exit');
    __trust.focusPage(field.__id);
    assert(__trust.key(field.__id, 'Enter', 'Enter', false, false, false, false, false, false), 'key cancellation');
    assert(child.keyView, 'keyboard realm');
    __trust.formSet(field.__id, 'hello', null);
    assert(child.edited === 'hello', 'native edit in child realm');
    __trust.focusPage(null);
    child.seen.length = 0;
    button.addEventListener('pointerdown', child.cancelDown);
    let moves = [];
    button.addEventListener('pointermove', () => moves.push('pointer'));
    button.addEventListener('mousemove', () => moves.push('mouse'));
    __trust.pointerButton(button.__id, true, x, y);
    __trust.hover(button.__id, x, y);
    assert(moves.join() === 'pointer', 'canceled pointerdown suppresses compatibility mousemove');
    __trust.pointerButton(button.__id, false, x, y);
    __trust.click(button.__id);
    assert(child.seen.map(s => s.split(':')[0]).join() === 'pointerdown,pointerup,click', 'canceled pointer suppresses compatibility mouse only');
    assert(child.document.activeElement !== button, 'canceled mousedown does not focus on click');
    // A percent-sized embedded slider must retain both its painted endpoint
    // and its document mouseup target beyond the 300px default object width.
    body.style.width = '400px';
    frame.setAttribute('width', '100%'); frame.setAttribute('height', '150'); frame.style.border = '0';
    child.eval(`
        document.body.style.margin = '0';
        document.body.innerHTML = '<div id="track" style="width:360px;height:40px">' +
            '<button id="thumb" style="width:40px;height:40px">Drag</button></div>';
        globalThis.dragging = false; globalThis.dragMoves = 0;
        document.getElementById('thumb').addEventListener('mousedown', () => dragging = true);
        document.addEventListener('mousemove', () => { if (dragging) dragMoves++; });
        document.addEventListener('mouseup', event => {
            dragging = false; globalThis.releasedAt = event.clientX;
        });
    `);
    const thumb = child.document.getElementById('thumb'), track = child.document.getElementById('track');
    const sliderRect = frame.getBoundingClientRect();
    assert(sliderRect.width === 400 && child.innerWidth === 400,
        'percentage iframe viewport: ' + sliderRect.width + '/' + child.innerWidth + '/' + frame.getAttribute('width'));
    __trust.pointerButton(thumb.__id, true, sliderRect.left + 20, sliderRect.top + 20);
    __trust.hover(track.__id, sliderRect.left + 340, sliderRect.top + 20);
    assert(child.dragging && child.dragMoves === 1, 'embedded slider drag');
    __trust.pointerButton(track.__id, false, sliderRect.left + 340, sliderRect.top + 20);
    __trust.hover(track.__id, sliderRect.left + 350, sliderRect.top + 20);
    assert(!child.dragging && child.releasedAt === 340 && child.dragMoves === 1,
        'document mouseup releases the thumb and later motion does not drag');
    return 'native-frame-input-ok';
})()
