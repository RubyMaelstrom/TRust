// UI Events #event-type-keydown, #event-type-keypress, #keypress-event-order;
// HTML #implicit-submission. The native Enter path must reach legacy search
// handlers before deciding whether a form/newline default is allowed.
(() => {
    const failures = [];
    function assert(value, message) {
        if (!value) failures.push(message);
    }
    function enter(target, composing = false) {
        return __trust.key(target.__id, 'Enter', 'Enter', false, composing,
            false, false, false, false);
    }
    const host = document.createElement('search-widget');
    document.body.appendChild(host);
    const root = host.attachShadow({ mode: 'open' });
    root.innerHTML = '<form><input name="q"><button>Search</button></form>';
    const input = root.querySelector('input');
    input.focus();
    const order = [];
    input.addEventListener('keydown', event => {
        order.push('down');
        assert(event.isTrusted, 'native keydown');
    });
    input.addEventListener('keypress', event => {
        order.push('press');
        assert(event instanceof KeyboardEvent && event.isTrusted, 'native keypress');
        assert(event.key === 'Enter' && event.code === 'Enter', 'Enter identity');
        assert(event.keyCode === 13 && event.charCode === 13 && event.which === 13,
            'legacy Enter codes');
        assert(event.bubbles && event.cancelable && event.composed, 'keypress flags');
        assert(event.view === window && event.target === input, 'keypress context');
    });
    host.addEventListener('keypress', event => {
        order.push('host');
        assert(event.target === host && event.composedPath()[0] === input,
            'keypress crosses shadow root with retargeting');
    });
    assert(enter(input) === false, 'real form retains native submission default');
    assert(order.join(',') === 'down,press,host', 'keydown precedes keypress');

    order.length = 0;
    input.addEventListener('keydown', event => event.preventDefault(), { once: true });
    assert(enter(input) === true, 'canceled keydown suppresses native default');
    assert(order.join(',') === 'down', 'canceled keydown suppresses keypress');

    order.length = 0;
    input.addEventListener('keypress', event => event.preventDefault(), { once: true });
    assert(enter(input) === true, 'canceled keypress suppresses native default');
    assert(order.join(',') === 'down,press,host', 'keypress still bubbles when canceled');

    order.length = 0;
    assert(enter(input, true) === true, 'IME Enter does not submit a form');
    assert(order.join(',') === 'down', 'IME input does not generate keypress');

    order.length = 0;
    __trust.key(input.__id, 'ArrowLeft', 'ArrowLeft', false, false,
        false, false, false, false);
    assert(order.join(',') === 'down', 'non-character key does not generate keypress');

    // A form-free component can react asynchronously or ignore Enter. Its
    // editor grouping in the frontend is not an HTML form to submit.
    root.appendChild(input);
    order.length = 0;
    assert(enter(input) === true, 'formless Enter has no native submission default');
    assert(order.join(',') === 'down,press,host', 'formless input still receives keypress');

    // A shadow input is not owned by a form surrounding its host.
    const outer = document.createElement('form');
    document.body.appendChild(outer);
    outer.appendChild(host);
    assert(enter(input) === true, 'form ownership does not cross shadow roots');

    const textarea = document.createElement('textarea');
    document.body.appendChild(textarea);
    textarea.focus();
    assert(enter(textarea) === false, 'textarea retains the native newline default');
    textarea.addEventListener('keypress', event => event.preventDefault());
    assert(enter(textarea) === true, 'keypress can cancel the native newline');

    host.remove();
    outer.remove();
    textarea.remove();
    if (failures.length) throw new Error(failures.join('; '));
    return 'keyboard-enter-ok';
})();
