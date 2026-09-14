// HTML #dom-form-elements/#dom-form-nameditem; Web IDL #legacy-platform-objects.
// Local official snapshots: HTML e5071a2, Web IDL 8f18262 (2026-09-06).
(function () {
    function assert(value, message) { if (!value) throw new Error(message); }
    const host = document.createElement('div');
    document.body.appendChild(host);
    const root = host.attachShadow({mode:'closed'});
    root.innerHTML = '<form id="account"><input name="username" id="email" required>' +
        '<input name="password" type="password"><input name="remember" type="checkbox" checked>' +
        '<input name="choice" type="radio" value="a"><input name="choice" type="radio" value="b">' +
        '<input name="submit"><input name="image" type="image"><img name="logo"></form>' +
        '<input form="account" name="outside" value="external">';
    const form = root.querySelector('form');
    const email = root.querySelector('#email');
    const controls = form.elements;
    assert(controls.length === 7, 'controls are rooted in the form tree and exclude image inputs');
    assert(form.username === email && form.email === email && form[0] === email, 'named and indexed access');
    assert(email.form === form && form.outside.form === form, 'nearest and explicit form owners in shadow tree');
    assert(form.submit === root.querySelector('input[name=submit]'), 'named controls override prototype methods');
    assert(form.logo === root.querySelector('img') && form.image === undefined, 'legacy image fallback');
    assert(!('form' in form.logo), 'image form ownership does not expose a form IDL attribute');
    const foreign = document.createElementNS('http://www.w3.org/2000/svg', 'input');
    foreign.setAttribute('name', 'foreign');
    form.appendChild(foreign);
    assert(form.foreign === undefined && controls.length === 7, 'listed controls must be HTML elements');
    assert(!form.reportValidity(), 'shadow form required validation');
    email.value = 'dummy';
    assert(form.reportValidity(), 'valid shadow form');
    const radios = form.choice;
    assert(radios instanceof RadioNodeList && radios.length === 2, 'duplicate names return a live RadioNodeList');
    radios[1].remove();
    assert(radios.length === 1 && form.choice === radios[0], 'duplicate names track removal');
    const descriptor = Object.getOwnPropertyDescriptor(form, 'username');
    assert(descriptor.value === email && !descriptor.writable && !descriptor.enumerable && descriptor.configurable, 'named descriptor');
    assert('username' in form && Object.getOwnPropertyNames(form).includes('username') && !Object.keys(form).includes('username'), 'named reflection');
    assert(!Reflect.set(form, 'username', 1) && !Reflect.deleteProperty(form, 'username'), 'named controls are read only');
    email.name = 'renamed';
    assert(form.username === email && form.renamed === email, 'past names map preserves a renamed control');
    email.remove();
    assert(form.username === undefined && form.renamed === undefined, 'past names are cleared when the owner changes');
    const outer = document.createElement('form');
    outer.id = 'account';
    document.body.appendChild(outer);
    assert(form.outside.form === form, 'explicit association cannot cross shadow roots');
    const blocker = document.createElement('div');
    blocker.id = 'account';
    root.insertBefore(blocker, form);
    assert(root.querySelector('[name=outside]').form === null, 'first matching id must identify a form');
    blocker.remove();
    assert(controls.outside.value === 'external', 'live collection reflects form reassociation');
    const detached = document.createElement('form');
    detached.innerHTML = '<input name="detached"><input name="explicit" form="missing">';
    assert(detached.length === 2 && detached.explicit.form === detached, 'detached form tree uses ancestor association');
    host.remove(); outer.remove();
    return 'form-named-properties-ok';
})()
