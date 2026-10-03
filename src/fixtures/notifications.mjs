// Original Notifications API, Permissions and Web IDL binding regressions.
// TRust has no notification platform or permission prompt: the permission
// starts as "default" ("prompt"), a request resolves "denied", and a
// constructed notification is never shown.
(async function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    function throws(name, fn) {
        try { fn(); } catch (e) { check(e.name === name, 'wrong error ' + e.name + ' for ' + fn); return e; }
        throw Error('missing ' + name + ' for ' + fn);
    }
    async function rejects(name, promise) {
        check(promise instanceof Promise, 'operation returns a Promise');
        try { await promise; } catch (e) { check(e.name === name, 'wrong rejection ' + e.name); return e; }
        throw Error('missing rejection ' + name);
    }
    const worker = typeof document === 'undefined';
    check(typeof Notification === 'function' && Object.getPrototypeOf(Notification) === EventTarget,
        'Notification inherits EventTarget');
    check(Notification.length === 1 && Notification.name === 'Notification', 'constructor length and name');
    check(Object.prototype.toString.call(Notification.prototype) === '[object Notification]', 'toStringTag');
    const own = Object.getOwnPropertyDescriptor(globalThis, 'Notification');
    check(own && !own.enumerable && own.writable && own.configurable, 'interface object property');
    for (const key of ['permission', 'maxActions']) {
        const descriptor = Object.getOwnPropertyDescriptor(Notification, key);
        check(descriptor && descriptor.get && !descriptor.set && descriptor.enumerable && descriptor.configurable,
            'static readonly attribute ' + key);
    }
    check(Notification.permission === 'default', 'initial permission is default');
    check(Notification.maxActions === 0, 'no notification actions without a platform');
    check(worker === !('requestPermission' in Notification), 'requestPermission is Window-only');
    for (const key of ['title', 'dir', 'lang', 'body', 'navigate', 'tag', 'image', 'icon', 'badge', 'vibrate',
        'timestamp', 'renotify', 'silent', 'requireInteraction', 'data', 'actions', 'onclick', 'onshow',
        'onerror', 'onclose']) {
        const descriptor = Object.getOwnPropertyDescriptor(Notification.prototype, key);
        check(descriptor && descriptor.enumerable && descriptor.configurable && typeof descriptor.get === 'function',
            'prototype attribute ' + key);
        throws('TypeError', () => descriptor.get.call({}));
    }
    check(Object.getOwnPropertyDescriptor(Notification.prototype, 'close').enumerable, 'close is enumerable');
    throws('TypeError', () => Notification('x'));
    throws('TypeError', () => new Notification());
    throws('TypeError', () => new Notification('x', 1));
    throws('TypeError', () => new Notification('x', {dir: 'up'}));
    throws('TypeError', () => new Notification('x', {renotify: true}));
    throws('TypeError', () => new Notification('x', {silent: true, vibrate: 200}));
    throws('TypeError', () => new Notification('x', {actions: [{action: 'a', title: 'A'}]}));
    throws('TypeError', () => new Notification('x', {actions: [{action: 'a'}]}));
    throws('DataCloneError', () => new Notification('x', {data: () => 1}));
    const reads = [];
    const tracked = new Proxy({}, {get(target, key) { if (typeof key === 'string') reads.push(key); return undefined; }});
    new Notification('order', tracked);
    check(reads.join() === 'actions,badge,body,data,dir,icon,image,lang,navigate,renotify,requireInteraction,silent,tag,timestamp,vibrate',
        'dictionary members are read in lexicographic order: ' + reads.join());
    const data = {nested: [1, 2]};
    const before = Date.now();
    const note = new Notification('Hello \uD800', {
        body: 'Body', dir: 'rtl', lang: 'en', tag: 'tag-1', renotify: true, data,
        icon: 'icon.png', image: 'https://[bad', navigate: '/next', badge: '//cdn.example/b.png',
        vibrate: [1, 20000, 3, 4, 5, 6, 7, 8, 9, 10, 11], requireInteraction: 1,
    });
    check(note instanceof Notification && note instanceof EventTarget, 'instance brand');
    check(note.title === 'Hello \uD800' && note.body === 'Body' && note.dir === 'rtl' && note.lang === 'en',
        'DOMString attributes');
    check(note.tag === 'tag-1' && note.renotify === true && note.requireInteraction === true && note.silent === null,
        'boolean and nullable attributes');
    const base = worker ? location.href : document.baseURI;
    check(note.icon === new URL('icon.png', base).href && note.image === '' && note.navigate === new URL('/next', base).href
        && note.badge === new URL('//cdn.example/b.png', base).href, 'URLs parse against the API base URL');
    check(note.vibrate === note.vibrate && Object.isFrozen(note.vibrate) && note.vibrate.length === 10
        && note.vibrate[1] === 10000, 'validated and normalized frozen vibration pattern');
    check(note.actions === note.actions && Object.isFrozen(note.actions) && note.actions.length === 0, 'frozen actions');
    check(note.data !== data && note.data === note.data && note.data.nested[1] === 2, 'structured-cloned data');
    check(note.timestamp >= before - 1 && note.timestamp <= Date.now() + 1, 'fallback timestamp is the wall time');
    check(new Notification('t', {timestamp: 2 ** 40}).timestamp === 2 ** 40, 'explicit EpochTimeStamp');
    check(new Notification('t', {vibrate: 7}).vibrate.join() === '7', 'single vibration value');
    check(new Notification('t').vibrate.length === 0 && new Notification('t').data === null, 'empty defaults');
    check(Object.getOwnPropertyNames(note).length === 0, 'no own properties on a notification');
    check(note.close() === undefined, 'closing an unshown notification');
    const events = [];
    note.addEventListener('error', event => events.push('listener:' + event.isTrusted + ':' + (event.target === note)));
    note.onerror = function (event) { events.push('handler:' + (this === note)); };
    note.onshow = () => events.push('show');
    check(typeof note.onerror === 'function' && note.onclick === null, 'event handler attributes');
    const errorSeen = new Promise(resolve => note.addEventListener('error', resolve));
    await errorSeen;
    check(events.join() === 'listener:true:true,handler:true', 'error event instead of show: ' + events.join());

    // Permissions integration: "notifications" is a supported powerful feature.
    const status = await navigator.permissions.query({name: 'notifications'});
    check(status instanceof PermissionStatus && Object.getPrototypeOf(PermissionStatus) === EventTarget,
        'PermissionStatus brand');
    check(status.name === 'notifications' && status.state === 'prompt', 'query reports prompt');
    check(Object.getOwnPropertyNames(status).length === 0, 'no own properties on a PermissionStatus');
    throws('TypeError', () => new PermissionStatus());
    let nameReads = 0;
    const second = await navigator.permissions.query({get name() { nameReads++; return 'notifications'; }});
    check(nameReads === 2 && second !== status, 'typed descriptor conversion reads name again');
    const storageStatus = await navigator.permissions.query({name: 'persistent-storage'});
    check(storageStatus.name === 'persistent-storage' && storageStatus.state === 'prompt', 'persistent-storage');
    if (!worker) {
        await rejects('TypeError', Notification.requestPermission(1));
        const changes = [];
        status.onchange = event => changes.push(event.type + ':' + event.isTrusted + ':' + status.state);
        let callback = null;
        const result = await Notification.requestPermission(value => { callback = value; });
        check(result === 'denied' && callback === 'denied', 'request without a permission UI is denied');
        check(Notification.permission === 'denied', 'permission store records the decision');
        await new Promise(resolve => status.addEventListener('change', resolve));
        check(changes.join() === 'change:true:denied' && status.state === 'denied', 'status change: ' + changes.join());
        const later = await navigator.permissions.query({name: 'notifications'});
        check(later.state === 'denied', 'later query reports denied');
        check(await Notification.requestPermission() === 'denied', 'repeated request');
    }
    return 'notifications-ok';
})().then(value => globalThis.notificationResult = value,
          error => globalThis.notificationResult = 'ERROR:' + error.name + ':' + error.message);
