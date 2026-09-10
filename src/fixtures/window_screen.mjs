(function () {
    function assert(value, message) { if (!value) throw Error(message); }
    // CSSOM View #dom-window-screenx/#dom-window-screeny; Web IDL #Replaceable.
    assert([screenX, screenLeft, screenY, screenTop].join() === '-137,-137,245,245', 'native CSS coordinates');
    const descriptor = Object.getOwnPropertyDescriptor(window, 'screenX');
    assert(descriptor.enumerable && descriptor.configurable && descriptor.get && descriptor.set, 'replaceable descriptor');
    assert(descriptor.get.name === 'get screenX' && descriptor.get.length === 0 &&
        descriptor.set.name === 'set screenX' && descriptor.set.length === 1, 'Web IDL accessor metadata');
    for (const receiver of [{}, Object.create(window), 1]) {
        let failed = false;
        try { descriptor.get.call(receiver); } catch (error) { failed = error instanceof TypeError; }
        assert(failed, 'getter requires a Window');
        failed = false;
        try { descriptor.set.call(receiver, 9); } catch (error) { failed = error instanceof TypeError; }
        assert(failed, 'setter requires a Window');
    }
    assert(descriptor.get.call(null) === -137, 'null receiver uses the relevant global');
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const frame = document.createElement('iframe');
    frame.style.cssText = 'position:absolute;left:80px;top:90px;width:300px;height:200px';
    frame.srcdoc = '<button id="target">range</button>';
    body.appendChild(frame); __trust.hydrateFrames();
    const child = frame.contentWindow;
    globalThis.__screenChild = child;
    assert(child.screenX === -137 && child.screenLeft === -137 && child.screenY === 245 && child.screenTop === 245,
        'iframe offsets are not client-window offsets');
    assert(descriptor.get.call(child) === -137, 'borrowed getter accepts same-origin Window');
    __trust.setScroll(10, 20);
    assert(child.screenX === screenX && child.screenY === screenY, 'viewport scrolling does not move the client window');
    const rect = child.document.getElementById('target').getBoundingClientRect();
    const event = new child.MouseEvent('mousedown', {
        clientX: rect.x + 1, clientY: rect.y + 1,
        screenX: rect.x + 1 + child.screenX, screenY: rect.y + 1 + child.screenY,
    });
    assert(Number.isFinite(event.screenX) && Number.isFinite(event.screenY), 'finite synthetic coordinates');
    for (const value of [NaN, Infinity, -Infinity]) {
        let failed = false;
        try { new MouseEvent('click', {screenX: value}); } catch (error) { failed = error instanceof TypeError; }
        assert(failed, 'MouseEvent finite-double validation remains strict');
    }
    screenX = 'author';
    const own = Object.getOwnPropertyDescriptor(window, 'screenX');
    assert(own.value === 'author' && own.writable && own.enumerable && own.configurable, 'assignment replaces without numeric conversion');
    assert(screenLeft === -137 && child.screenX === -137, 'replacement is independent of aliases and other Windows');
    return 'window-screen-ok';
})();
