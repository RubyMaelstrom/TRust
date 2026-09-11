// HTML #the-storage-interface; Web IDL #js-legacy-platform-objects.
function check(condition, message) {
    if (!condition) throw new Error(message);
}
for (const storage of [localStorage, sessionStorage]) {
    storage.clear();
    storage.token = 'signed-in';
    check(storage.getItem('token') === 'signed-in', 'named setter writes the storage map');
    storage.setItem('token', 'renewed');
    check(storage.token === 'renewed', 'named getter sees method writes');
    check(storage.length === 1, 'one key shared by both APIs');
    check(storage.missing === undefined && storage.getItem('missing') === null, 'missing values');
    check('token' in storage && !('missing' in storage), 'named membership');
    check(Object.keys(storage).join(',') === 'token', 'only stored keys are own enumerable properties');
    const descriptor = Object.getOwnPropertyDescriptor(storage, 'token');
    check(descriptor.value === 'renewed' && descriptor.writable && descriptor.enumerable && descriptor.configurable, 'named descriptor');
    delete storage.token;
    check(storage.getItem('token') === null && storage.length === 0, 'named deletion removes the stored item');
    storage.setItem('getItem', 'hidden');
    storage.length = 'stored length';
    check(typeof storage.getItem === 'function' && storage.length === 2, 'prototype members mask stored names');
    check(storage.getItem('getItem') === 'hidden' && storage.getItem('length') === 'stored length', 'setters still store colliding names');
    check(!Object.keys(storage).includes('getItem') && !Object.hasOwn(storage, 'length'), 'masked names are not visible');
    delete storage.getItem;
    check(storage.getItem('getItem') === 'hidden', 'deleting an invisible named property preserves the map');
    storage[0] = 7;
    check(storage.getItem('0') === '7', 'numeric keys and DOMString conversion');
    const symbol = Symbol('expando');
    storage[symbol] = 42;
    check(storage[symbol] === 42 && storage.length === 3, 'symbols are ordinary properties');
    Object.defineProperty(storage, 'defined', {value: 8, configurable: true});
    check(storage.defined === '8' && storage.getItem('defined') === '8', 'defineProperty invokes the setter');
    check(!Reflect.defineProperty(storage, 'accessor', {get() { return 1; }}), 'named accessors rejected');
    check(!Reflect.preventExtensions(storage), 'storage must remain extensible');
    let threw = false;
    try { storage.token = Symbol('invalid'); } catch (error) { threw = error instanceof TypeError; }
    check(threw && storage.getItem('token') === null, 'symbol values reject DOMString conversion');
    storage.clear();
    check(storage.length === 0 && storage.defined === undefined && storage[symbol] === 42, 'clear only removes stored keys');
}
globalThis.storageNamedResult = 'ok';
