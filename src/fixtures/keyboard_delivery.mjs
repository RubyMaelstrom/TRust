// UI Events keyboard targets, keydown/keypress/keyup order, legacy key
// attributes; HTML focus chains. Exercise native entrypoints, not dispatchEvent.
(() => {
    function assert(value, message) { if (!value) throw Error(message); }
    const canvas = document.createElement('canvas');
    canvas.tabIndex = -1;
    document.body.appendChild(canvas);
    __trust.focusPage(canvas.__id);
    assert(document.activeElement === canvas, 'native canvas focus');
    let seen = [];
    const record = e => seen.push(e);
    for (const type of ['keydown', 'keypress', 'keyup'])
        document.addEventListener(type, record);
    function key(value, code, up = false, location = 0, repeat = false, composing = false,
        shift = false, ctrl = false, alt = false, meta = false) {
        return __trust.key(null, value, code, repeat, composing, shift, ctrl, alt, meta, up, location);
    }
    key('a', 'KeyA'); key('a', 'KeyA', true);
    assert(seen.map(e => e.type).join() === 'keydown,keypress,keyup', 'character event order');
    assert(seen.map(e => e.keyCode).join() === '65,97,65', 'down/up virtual codes vs character code');
    assert(seen.map(e => e.charCode).join() === '0,97,0', 'charCode only on keypress');
    for (const e of seen) {
        assert(e instanceof KeyboardEvent && e.isTrusted && e.view === window, 'native keyboard context');
        assert(e.target === canvas && e.bubbles && e.composed && e.cancelable, 'canvas key bubbles');
        assert(e.key === 'a' && e.code === 'KeyA' && e.location === 0 && e.detail === 0, 'key identity');
        assert(e.which === e.keyCode && !e.repeat && !e.isComposing, 'native legacy/state fields');
    }
    seen = [];
    canvas.addEventListener('keydown', e => e.preventDefault(), { once: true });
    assert(key(' ', 'Space'), 'keydown cancellation reaches native default');
    key(' ', 'Space', true);
    assert(seen.map(e => e.type).join() === 'keydown,keyup', 'cancellation suppresses press but not release');
    assert(seen.every(e => e.keyCode === 32), 'space virtual key');
    seen = [];
    canvas.addEventListener('keypress', e => e.preventDefault(), { once: true });
    assert(key('Enter', 'Enter'), 'keypress cancellation reaches native default');
    key('Enter', 'Enter', true);
    assert(seen.map(e => e.charCode).join() === '0,13,0', 'Enter character sequence');

    for (const [value, code, virtual, location] of [
        ['ArrowLeft', 'ArrowLeft', 37, 0], ['Backspace', 'Backspace', 8, 0],
        ['Shift', 'ShiftRight', 16, 2], ['F12', 'F12', 123, 0],
        ['1', 'Numpad1', 97, 3], ['!', 'Digit1', 49, 0],
        ['?', 'Slash', 191, 0], ['z', 'KeyY', 90, 0],
        ['Meta', 'MetaLeft', 91, 1], ['Unidentified', '', 0, 0],
    ]) {
        seen = [];
        key(value, code, false, location, true, false, true, false, true);
        assert(seen[0].keyCode === virtual && seen[0].code === code, 'virtual/physical identity: ' + code);
        assert(seen[0].location === location && seen[0].repeat && seen[0].shiftKey && seen[0].altKey,
            'location, repeat, modifiers: ' + code);
    }
    seen = [];
    key('a', 'KeyA', false, 0, false, true);
    assert(seen.length === 1 && seen[0].isComposing && seen[0].keyCode === 229, 'IME down has no keypress');
    seen = [];
    key('a', 'KeyA', false, 0, false, false, false, true);
    assert(seen.length === 1 && seen[0].ctrlKey, 'command chord has no character event');
    seen = [];
    key('🦄', '');
    assert(seen[1].charCode === 0x1f984 && seen[1].code === '', 'Unicode text does not invent physical key');

    const input = document.createElement('input');
    document.body.appendChild(input);
    canvas.addEventListener('keydown', () => input.focus(), { once: true });
    seen = [];
    key('ArrowRight', 'ArrowRight'); key('ArrowRight', 'ArrowRight', true);
    assert(seen[0].target === canvas && seen[1].target === input, 'release resolves updated focus');
    input.remove(); seen = [];
    key('b', 'KeyB');
    assert(seen[0].target === document.body, 'removed focus falls back to body');

    const host = document.createElement('div');
    document.body.appendChild(host);
    const shadow = host.attachShadow({ mode: 'open' });
    shadow.appendChild(canvas);
    canvas.focus();
    let innerTarget = null;
    canvas.addEventListener('keyup', e => innerTarget = e.target, { once: true });
    let retargeted = false;
    host.addEventListener('keyup', e => {
        retargeted = e.target === host && e.composedPath()[0] === canvas;
    }, { once: true });
    key('a', 'KeyA', true);
    assert(innerTarget === canvas && retargeted, 'shadow focus uses inner anchor with normal retargeting');

    const frame = document.createElement('iframe');
    frame.srcdoc = '<canvas id="game" tabindex="-1"></canvas>';
    document.body.appendChild(frame); __trust.hydrateFrames();
    const child = frame.contentWindow;
    child.eval(`
        globalThis.keys = [];
        for (const type of ['keydown', 'keypress', 'keyup'])
            document.addEventListener(type, e => {
                if (!(e instanceof KeyboardEvent && e.isTrusted && e.view === window
                    && e.target === document.getElementById('game'))) throw Error('child key context');
                keys.push(e.type + ':' + e.keyCode);
                if (e.type === 'keydown') e.preventDefault();
            });
    `);
    __trust.focusPage(child.document.getElementById('game').__id);
    assert(document.activeElement === frame, 'focused child anchors parent frame');
    seen = [];
    assert(key('a', 'KeyA'), 'child cancellation crosses host boundary');
    key('a', 'KeyA', true);
    assert(child.keys.join() === 'keydown:65,keyup:65', 'focused child receives down and up');
    assert(seen.length === 0, 'child events do not bubble into parent document');
    __trust.focusPage(null);
    key('a', 'KeyA');
    assert(seen[0].target === document.body && child.keys.length === 2, 'leaving frame restores viewport input');

    frame.remove(); host.remove();
    for (const type of ['keydown', 'keypress', 'keyup']) document.removeEventListener(type, record);
    return 'keyboard-delivery-ok';
})();
