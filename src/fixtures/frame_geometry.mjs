(function () {
    function assert(value, message) { if (!value) throw Error(message); }
    function rect(element, expected, message) {
        const r = element.getBoundingClientRect(), got = [r.x, r.y, r.width, r.height];
        assert(got.every((n, i) => Math.abs(n - expected[i]) < 0.02), message + ': ' + got);
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    body.style.cssText = 'margin:0;height:1000px';
    const frame = document.createElement('iframe');
    frame.style.cssText = 'position:absolute;left:100px;top:80px;width:300px;height:200px;border:3px solid;padding:0';
    frame.srcdoc = '<style>body{margin:0}#target{position:absolute;left:30px;top:40px;width:50px;height:20px}</style><div id="target"></div>';
    body.appendChild(frame); __trust.hydrateFrames();
    const child = frame.contentWindow, target = child.document.getElementById('target');
    assert(child.innerWidth === 300 && child.innerHeight === 200, 'initial child Window uses its content-box viewport');
    rect(frame, [100, 80, 306, 206], 'embedding border box');
    rect(target, [30, 40, 50, 20], 'child rectangle uses its own viewport');
    assert(child.document.elementFromPoint(35, 45) === target, 'child rectangle and hit test agree');
    frame.style.left = '170px'; frame.style.top = '120px';
    rect(target, [30, 40, 50, 20], 'moving the embedding element does not move child client coordinates');
    __trust.setScroll(0, 50);
    rect(target, [30, 40, 50, 20], 'parent scrolling does not move child client coordinates');
    rect(frame, [170, 70, 306, 206], 'parent scrolling moves its embedding rectangle');
    frame.style.padding = '7px 11px';
    rect(frame, [170, 70, 328, 220], 'padded embedding border box');
    rect(target, [30, 40, 50, 20], 'padding is outside the child viewport');
    assert(child.document.elementFromPoint(35, 45) === target, 'padded frame hit testing');
    let clicked = null;
    target.addEventListener('click', event => clicked = [event.clientX, event.clientY, event.isTrusted]);
    __trust.pointerButton(target.__id, true, 170 + 3 + 11 + 35, 70 + 3 + 7 + 45);
    __trust.pointerButton(target.__id, false, 170 + 3 + 11 + 35, 70 + 3 + 7 + 45);
    __trust.click(target.__id);
    assert(clicked && clicked.join() === '35,45,true', 'native input uses the same content-box origin: ' + clicked);
    const nested = child.document.createElement('iframe');
    nested.style.cssText = 'position:absolute;left:90px;top:60px;width:100px;height:90px;border:2px solid;padding:5px';
    nested.srcdoc = '<style>body{margin:0}#inner{position:absolute;left:9px;top:12px;width:20px;height:18px}</style><div id="inner"></div>';
    child.document.body.appendChild(nested); child.__trust.hydrateFrames();
    const inner = nested.contentDocument.getElementById('inner');
    assert(nested.contentWindow.innerWidth === 100 && nested.contentWindow.innerHeight === 90, 'initial nested viewport excludes borders and padding');
    rect(nested, [90, 60, 114, 104], 'nested embedding rectangle uses parent frame coordinates');
    rect(inner, [9, 12, 20, 18], 'nested child rectangle uses its own viewport');
    assert(nested.contentDocument.elementFromPoint(12, 16) === inner, 'nested frame hit testing');
    let nestedClick = null;
    inner.addEventListener('click', event => nestedClick = [event.clientX, event.clientY, event.isTrusted]);
    const x = 170 + 3 + 11 + 90 + 2 + 5 + 12;
    const y = 70 + 3 + 7 + 60 + 2 + 5 + 16;
    __trust.pointerButton(inner.__id, true, x, y);
    __trust.pointerButton(inner.__id, false, x, y);
    __trust.click(inner.__id);
    assert(nestedClick && nestedClick.join() === '12,16,true', 'nested native coordinates: ' + nestedClick);
    const pointerEvents = [];
    for (const type of ['pointerover', 'mouseover', 'pointerenter', 'mouseenter', 'pointermove', 'mousemove',
        'pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click', 'pointerout', 'mouseout', 'pointerleave', 'mouseleave']) {
        inner.addEventListener(type, event => pointerEvents.push({type,
            clientX: event.clientX, clientY: event.clientY, screenX: event.screenX, screenY: event.screenY,
            trusted: event.isTrusted, view: event.view}));
    }
    // The native lane can supply screen coordinates independently of client
    // coordinates; neither iframe border/padding nor parent scrolling alters them.
    __trust.hover(inner.__id, x, y, 500, 600);
    __trust.pointerButton(inner.__id, true, x, y, 500, 600);
    __trust.pointerButton(inner.__id, false, x, y, 500, 600);
    __trust.click(inner.__id);
    __trust.hover(null, x, y, 500, 600);
    assert(pointerEvents.map(event => event.type).join() === 'pointerover,mouseover,pointerenter,mouseenter,pointermove,mousemove,pointerdown,mousedown,pointerup,mouseup,click,pointerout,mouseout,pointerleave,mouseleave', 'native pointer/mouse screen coordinate sequence');
    for (const event of pointerEvents) {
        assert(event.clientX === 12 && event.clientY === 16, event.type + ' nested client coordinates');
        assert(event.screenX === 500 && event.screenY === 600, event.type + ' screen coordinates cross frames unchanged');
        assert(event.trusted && event.view === nested.contentWindow, event.type + ' owning Window and trust');
    }
    pointerEvents.length = 0;
    __trust.pointerButton(inner.__id, true, x, y, 0, -20);
    __trust.pointerButton(inner.__id, false, x, y, 0, -20);
    __trust.click(inner.__id);
    assert(pointerEvents.length === 5 && pointerEvents.every(event => event.screenX === 0 && event.screenY === -20),
        'explicit zero and negative screen coordinates are not replaced by client coordinates');
    // Borrowing another realm's getter must still use the target's Document.
    const getter = Element.prototype.getBoundingClientRect;
    assert(getter.call(inner).top === 12, 'cross-realm getter uses the owning viewport');
    // Percentages are resolved against the containing block width, not parsed
    // as a pixel number at the JavaScript boundary.
    frame.style.padding = '2%';
    rect(target, [30, 40, 50, 20], 'percentage padding preserves child coordinates');
    assert(child.document.elementFromPoint(35, 45) === target, 'percentage-padded frame hit testing');
    frame.style.display = 'none';
    __trust.updateFrameResizes();
    rect(frame, [0, 0, 0, 0], 'non-rendered iframe has no border box even after scrolling');
    rect(target, [0, 0, 0, 0], 'non-rendered child has no border box');
    assert(frame.getClientRects().length === 0 && target.getClientRects().length === 0, 'no boxes means an empty rectangle list');
    assert(child.document.elementFromPoint(1, 1) === null, 'non-rendered child viewport is zero sized');
    assert(child.innerWidth === 0 && child.innerHeight === 0, 'hidden child Window dimensions update to zero');
    frame.style.display = 'block';
    frame.style.width = '0'; frame.style.height = '0';
    __trust.updateFrameResizes();
    assert(child.document.elementFromPoint(1, 1) === null, 'zero-size iframe does not fall back to 300x150');
    return 'frame-geometry-ok';
})()
