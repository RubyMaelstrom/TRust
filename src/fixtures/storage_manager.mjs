// Original Storage Standard StorageManager and Service Workers container
// regressions for secure Window and Worker contexts.
(async function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    function throws(name, fn) {
        try { fn(); } catch (e) { check(e.name === name, 'wrong error ' + e.name); return e; }
        throw Error('missing ' + name);
    }
    async function rejects(name, promise) {
        check(promise instanceof Promise, 'operation returns a Promise');
        try { await promise; } catch (e) { check(e.name === name, 'wrong rejection ' + e.name + ': ' + e.message); return e; }
        throw Error('missing rejection ' + name);
    }
    const worker = typeof document === 'undefined';
    const NavigatorInterface = worker ? WorkerNavigator : Navigator;
    check(isSecureContext, 'secure context');
    for (const key of ['storage', 'serviceWorker']) {
        const descriptor = Object.getOwnPropertyDescriptor(NavigatorInterface.prototype, key);
        check(descriptor && descriptor.get && !descriptor.set && descriptor.enumerable && descriptor.configurable,
            'Navigator attribute ' + key);
        throws('TypeError', () => descriptor.get.call({}));
        check(!Object.hasOwn(navigator, key), 'attribute is not an own property ' + key);
    }
    const storage = navigator.storage;
    check(storage === navigator.storage && storage instanceof StorageManager, 'SameObject StorageManager');
    check(Object.prototype.toString.call(storage) === '[object StorageManager]', 'StorageManager tag');
    check(Object.getOwnPropertyNames(storage).length === 0, 'no own StorageManager properties');
    throws('TypeError', () => new StorageManager());
    check(worker === !('persist' in storage), 'persist() is Window-only');
    for (const key of ['persisted', 'estimate', 'getDirectory'])
        check(Object.getOwnPropertyDescriptor(StorageManager.prototype, key).enumerable, 'enumerable ' + key);
    await rejects('TypeError', StorageManager.prototype.estimate.call({}));
    await rejects('TypeError', StorageManager.prototype.persisted.call(null));
    check(await storage.persisted() === false, 'best-effort bucket is not persisted');
    const estimate = await storage.estimate();
    check(Object.getPrototypeOf(estimate) === Object.prototype && Object.keys(estimate).join() === 'usage,quota',
        'StorageEstimate dictionary');
    check(estimate.quota === (5 + 64 + 64) * 1024 * 1024, 'quota is the shelf endpoint quota sum: ' + estimate.quota);
    if (!worker) {
        const empty = estimate.usage;
        localStorage.setItem('ab', 'cdef');
        const after = await storage.estimate();
        check(after.usage === empty + 12, 'localStorage usage counts UTF-16 bytes: ' + empty + ' -> ' + after.usage);
        check(await storage.persist() === false, 'persistence cannot be granted');
        const status = await navigator.permissions.query({name: 'persistent-storage'});
        check(status.state === 'denied', 'persist() requested the permission');
    }
    await rejects('SecurityError', storage.getDirectory());

    const container = navigator.serviceWorker;
    check(container === navigator.serviceWorker && container instanceof ServiceWorkerContainer
        && container instanceof EventTarget, 'SameObject ServiceWorkerContainer');
    check(Object.getOwnPropertyNames(container).length === 0, 'no own ServiceWorkerContainer properties');
    throws('TypeError', () => new ServiceWorkerContainer());
    check(container.controller === null, 'no controller');
    const ready = container.ready;
    check(ready instanceof Promise && ready === container.ready, 'ready promise is stable');
    let settled = false;
    ready.then(() => { settled = true; }, () => { settled = true; });
    check(await container.getRegistration() === undefined, 'no registration');
    check(await container.getRegistration('./page') === undefined, 'relative client URL');
    await rejects('SecurityError', container.getRegistration('https://other.example/'));
    await rejects('TypeError', container.getRegistration('https://[bad'));
    const registrations = await container.getRegistrations();
    check(Array.isArray(registrations) && registrations.length === 0 && Object.isFrozen(registrations),
        'frozen empty registration list');
    await rejects('TypeError', container.register());
    await rejects('TypeError', container.register('ftp://example.com/sw.js'));
    await rejects('TypeError', container.register('/a%2Fb/sw.js'));
    await rejects('TypeError', container.register('/sw.js', {type: 'shared'}));
    await rejects('TypeError', container.register('/sw.js', {scope: 'data:text/plain,x'}));
    await rejects('SecurityError', container.register('/sw.js'));
    await rejects('SecurityError', container.register('sw.js', {scope: './', updateViaCache: 'none'}));
    await rejects('TypeError', ServiceWorkerContainer.prototype.register.call({}, '/sw.js'));
    check(container.startMessages() === undefined, 'startMessages');
    let messages = 0;
    container.onmessage = () => messages++;
    check(typeof container.onmessage === 'function' && container.oncontrollerchange === null, 'event handlers');
    container.dispatchEvent(new Event('message'));
    check(messages === 1, 'onmessage handler listens');
    await Promise.resolve();
    check(!settled, 'ready never settles');
    for (const object of [storage, container, StorageManager.prototype, ServiceWorkerContainer.prototype])
        check(!Object.getOwnPropertyNames(object).some(name => name.startsWith('__') || /trust/i.test(name)),
            'no internals on ' + object);
    return 'storage-manager-ok';
})().then(value => globalThis.storageResult = value,
          error => globalThis.storageResult = 'ERROR:' + error.name + ':' + error.message);
