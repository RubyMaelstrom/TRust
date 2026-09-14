// DOM #concept-event-dispatch; HTML #concept-form-submit.
// Local official snapshots: DOM a2331a4, HTML e5071a2 (2026-09-06).
(function () {
    function assert(value, message) { if (!value) throw new Error(message); }
    const host = document.createElement('div');
    document.body.appendChild(host);
    const root = host.attachShadow({mode: 'open'});
    root.innerHTML = '<form><input name="username" required><x-submit></x-submit></form>';
    const form = root.querySelector('form');
    const input = root.querySelector('input');
    const component = root.querySelector('x-submit');
    component.attachShadow({mode: 'open'}).innerHTML = '<button><slot></slot></button><slot name="submit"></slot>';
    component.innerHTML = '<span>Submit</span><input type="submit" slot="submit" style="display:none">';
    const visible = component.shadowRoot.querySelector('button');
    const hidden = component.querySelector('input');
    const label = component.querySelector('span');
    hidden.addEventListener('click', e => e.stopPropagation());
    component.addEventListener('click', () => hidden.dispatchEvent(new PointerEvent('click')));
    const events = [];
    form.addEventListener('submit', e => {
        events.push([e instanceof SubmitEvent, e.submitter === hidden, e.isTrusted].join(':'));
        e.preventDefault();
    });
    input.value = 'dummy';
    __trust.click(visible.__id);
    assert(events.join() === 'true:true:true', 'forwarded non-bubbling PointerEvent must submit exactly once');
    __trust.click(label.__id);
    assert(events.length === 2, 'slotted label activates the same button');
    hidden.dispatchEvent(new Event('click'));
    assert(events.length === 2, 'plain Event click has no activation behavior');
    hidden.addEventListener('click', e => e.preventDefault(), {once: true});
    assert(!hidden.dispatchEvent(new MouseEvent('click', {cancelable: true})), 'canceled click result');
    assert(events.length === 2, 'canceled click does not submit');
    hidden.disabled = true;
    hidden.dispatchEvent(new PointerEvent('click'));
    assert(events.length === 2, 'disabled submit control does not submit');
    hidden.disabled = false;
    input.value = '';
    hidden.dispatchEvent(new PointerEvent('click'));
    assert(events.length === 2, 'shadow form validates required fields');
    hidden.formNoValidate = true;
    hidden.dispatchEvent(new PointerEvent('click'));
    assert(events.length === 3, 'submitter can bypass validation');

    const box = document.createElement('input');
    box.type = 'checkbox';
    form.appendChild(box);
    let checkedDuringClick = false, changed = 0;
    box.addEventListener('click', e => { checkedDuringClick = box.checked; e.preventDefault(); }, {once:true});
    box.addEventListener('change', () => changed++);
    box.dispatchEvent(new MouseEvent('click', {cancelable:true}));
    assert(checkedDuringClick && !box.checked && changed === 0, 'dispatchEvent runs pre-activation and cancellation');
    box.dispatchEvent(new PointerEvent('click'));
    assert(box.checked && changed === 1, 'dispatchEvent completes checkbox activation');

    const direct = document.createElement('button');
    const child = document.createElement('span');
    direct.appendChild(child);
    form.appendChild(direct);
    input.value = 'dummy';
    child.dispatchEvent(new MouseEvent('click'));
    assert(events.length === 3, 'non-bubbling click does not activate an ordinary ancestor');
    child.addEventListener('click', () => child.remove(), {once: true});
    child.dispatchEvent(new MouseEvent('click', {bubbles:true}));
    assert(events.length === 4, 'activation target is selected before a listener removes the child');
    host.remove();
    return 'shadow-form-activation-ok';
})()
