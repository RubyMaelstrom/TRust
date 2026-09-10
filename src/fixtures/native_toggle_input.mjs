(function () {
    function assert(value, message) { if (!value) throw Error(message); }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const frame = document.createElement('iframe');
    frame.style.cssText = 'position:absolute;left:100px;top:80px;width:300px;height:200px;border:0';
    frame.srcdoc = '<input id="box" type="checkbox"><input id="first" name="g" type="radio" checked><input id="second" name="g" type="radio">';
    body.appendChild(frame); __trust.hydrateFrames();
    const child = frame.contentWindow, box = child.document.getElementById('box');
    const events = [], clicks = [];
    for (const type of ['click', 'input', 'change']) box.addEventListener(type, event => {
        events.push([type, event.isTrusted, box.checked, box.indeterminate].join(':'));
        if (type === 'click') clicks.push({view: event.view, trusted: event.isTrusted,
            clientX: event.clientX, clientY: event.clientY, screenX: event.screenX, screenY: event.screenY});
    });
    // A UA activation must not run an author-installed own setter (for
    // example a framework's tracker for script writes to checkedness).
    const checked = Object.getOwnPropertyDescriptor(child.HTMLInputElement.prototype, 'checked');
    let scriptWrites = 0;
    Object.defineProperty(box, 'checked', {
        configurable: true, get() { return checked.get.call(this); },
        set(value) { scriptWrites++; checked.set.call(this, value); }
    });
    function nativeClick(target) {
        __trust.pointerButton(target.__id, true, 114, 96);
        __trust.pointerButton(target.__id, false, 114, 96);
        return __trust.click(target.__id);
    }
    box.indeterminate = true;
    nativeClick(box);
    assert(events.join('|') === 'click:true:true:false|input:true:true:false|change:true:true:false', 'native event sequence: ' + events);
    assert(scriptWrites === 0, 'native checkedness does not call the author setter');
    const cancel = event => event.preventDefault();
    box.addEventListener('click', cancel);
    events.length = 0; box.indeterminate = true;
    assert(nativeClick(box), 'canceled native activation');
    assert(events.join() === 'click:true:false:false' && box.checked && box.indeterminate, 'cancellation restores full state and skips input/change');
    assert(scriptWrites === 0, 'canceled activation also bypasses author setter');
    box.removeEventListener('click', cancel);
    events.length = 0; box.click();
    assert(events.join('|') === 'click:false:false:false|input:true:false:false|change:true:false:false', 'script click remains untrusted: ' + events);
    // Assert outside the listeners: dispatch reports callback exceptions instead
    // of propagating them to the caller, as required by the DOM standard.
    assert(clicks.length === 3, 'observed all native and script clicks');
    for (const event of clicks) {
        assert(event.view === child, 'checkbox event view');
        if (event.trusted) {
            assert(event.clientX === 14 && event.clientY === 16, 'native checkbox client coordinates');
            assert(event.screenX === 114 && event.screenY === 96, 'native screen coordinates do not become frame-local');
        } else {
            assert(event.clientX === 0 && event.clientY === 0 && event.screenX === 0 && event.screenY === 0,
                'script click does not inherit native pointer coordinates');
        }
    }
    const first = child.document.getElementById('first'), second = child.document.getElementById('second');
    second.addEventListener('click', cancel);
    nativeClick(second);
    assert(first.checked && !second.checked, 'canceled native radio restores its group');
    second.removeEventListener('click', cancel);
    nativeClick(second);
    assert(!first.checked && second.checked, 'native radio group commit');
    box.disabled = true; events.length = 0;
    __trust.click(box.__id);
    assert(events.length === 0, 'disabled checkbox is not activated');
    for (const mode of ['open', 'closed']) {
        const host = child.document.createElement('div');
        child.document.body.appendChild(host);
        const root = host.attachShadow({mode});
        const inside = child.document.createElement('input');
        inside.type = 'checkbox'; root.appendChild(inside);
        let clicks = 0;
        inside.addEventListener('click', event => {
            assert(event.isTrusted && event.view === child, 'shadow control native realm/trust');
            assert(event.clientX === 14 && event.clientY === 16, 'shadow control native coordinates');
            clicks++;
        });
        nativeClick(inside);
        assert(clicks === 1 && inside.checked, mode + ' shadow tree routes native input to its owning iframe');
        assert(root.parentNode === null && root.host === host &&
            (mode !== 'closed' || host.shadowRoot === null), 'native routing does not expose the shadow root');
    }
    return 'native-toggle-input-ok';
})()
