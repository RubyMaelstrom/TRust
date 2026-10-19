// HTML e5071a20 #structuredserializeinternal / #structureddeserialize;
// WebCrypto 811c24c #cryptokey-interface-serializable. Private serialization
// records must not run author hooks or disclose a non-extractable key handle.
const key = await crypto.subtle.importKey('raw', new Uint8Array(16), 'AES-GCM', false, ['encrypt']);
const channel = new MessageChannel(), transferred = new MessageChannel();
const define = Object.defineProperty, getOwn = Object.getOwnPropertyDescriptor;
const apply = Reflect.apply;
let hooks = 0, clone, received;
channel.port2.onmessage = event => { received = event; };
define(Object.prototype, 'toJSON', {configurable:true, value() {
    hooks++;
    if (Array.isArray(this) && this[0] === 'CK') this[1][1] = true;
    return this;
}});
try {
    clone = structuredClone({key, again:key});
    channel.port1.postMessage({key}, [transferred.port1]);
} finally { delete Object.prototype.toJSON; }
check(hooks === 0, 'private records never invoke toJSON');
check(clone.key === clone.again && clone.key !== key && !clone.key.extractable, 'key identity and extractability');
const control = globalThis.__trust || globalThis.__wkr;
const hasPortTask = () => globalThis.__trust ? control.hasPortMessageTask() : control.hasPortTask();
const runPortTask = () => globalThis.__trust ? control.runPortMessageTask() : control.runPortTask();
while (hasPortTask()) runPortTask();
check(received && !received.data.key.extractable && received.ports.length === 1, 'message key and port transfer');
const receivedPort = received.ports[0];
await rejects(crypto.subtle.exportKey('raw', clone.key), 'InvalidAccessError', 'cloned key remains non-extractable');
const iv = new Uint8Array(12), empty = new Uint8Array(0);
const expected = new Uint8Array(await crypto.subtle.encrypt({name:'AES-GCM',iv}, key, empty));
const actual = new Uint8Array(await crypto.subtle.encrypt({name:'AES-GCM',iv}, clone.key, empty));
check(actual.join() === expected.join(), 'cloned private key remains usable');

// Replace all operations that formerly received a private graph, slot map,
// transfer capability or key record. Restore before assertions/reporting.
const replacements = [
    [JSON,'stringify'], [JSON,'parse'], [Array.prototype,'push'],
    [Array.prototype,'map'], [Map.prototype,'set'], [Map.prototype,'get'],
    [Map.prototype,'has'], [Map.prototype,'forEach'], [Set.prototype,'add'],
    [Set.prototype,'has'], [WeakMap.prototype,'get'], [WeakMap.prototype,'set'],
    [Object,'defineProperty'], [Object,'getOwnPropertyDescriptor'], [Object,'keys'],
    [Object,'create'], [Reflect,'apply']
];
const saved = replacements.map(([object,name]) => getOwn(object,name));
let replacementCalls = 0;
function unexpected() { replacementCalls++; throw new Error('author hook received private data'); }
for (let i=0; i<replacements.length; i++)
    define(replacements[i][0], replacements[i][1], {...saved[i], value:unexpected});
try {
    clone = structuredClone(key);
    channel.port1.postMessage({key});
} finally {
    for (let i=0; i<replacements.length; i++) define(replacements[i][0], replacements[i][1], saved[i]);
}
check(replacementCalls === 0 && !clone.extractable, 'captured intrinsics protect private records');
while (hasPortTask()) runPortTask();
check(!received.data.key.extractable, 'message after replaced intrinsics');

// The worker slot map's own operations (Reflect.apply, WeakMap get/set,
// Object.create) are captured at bootstrap. Exercise actual platform-object
// operations that allocate and read those slots while they are replaced.
// This is the worker realm's internalsFor; Window uses privateSlots().
if (!globalThis.__trust) {
    const slotTargets = [[Reflect,'apply'], [WeakMap.prototype,'get'], [WeakMap.prototype,'set'], [Object,'create']];
    const slotSaved = slotTargets.map(([object,name]) => getOwn(object,name));
    let slotCalls = 0, faceStatus = 'none', faceFamily, eventSourceOk = false, slotError = '';
    function slotUnexpected() { slotCalls++; throw new Error('author hook received slot data'); }
    for (let i=0; i<slotTargets.length; i++)
        define(slotTargets[i][0], slotTargets[i][1], {...slotSaved[i], value:slotUnexpected});
    try {
        try {
            const face = new FontFace('private-slots', 'local(private-slots)');
            faceStatus = face.status; faceFamily = face.family;
        } catch (error) { slotError = 'FontFace:' + error.message; }
        try {
            eventSourceOk = new MessageEvent('message', {source:channel.port1}).source === channel.port1;
        } catch (error) { slotError += ' MessageEvent:' + error.message; }
        clone = structuredClone(key);
    } finally {
        for (let i=0; i<slotTargets.length; i++) define(slotTargets[i][0], slotTargets[i][1], slotSaved[i]);
    }
    check(slotCalls === 0 && faceStatus === 'unloaded' && typeof faceFamily === 'string' && eventSourceOk && !clone.extractable,
        'captured slot intrinsics serve platform objects ' + slotError);
}

let setterCalls = 0;
define(Array.prototype, '0', {configurable:true, set(value) {
    setterCalls++; throw new Error('private list reached an array setter');
}});
try { clone = structuredClone(key); }
finally { delete Array.prototype[0]; }
check(setterCalls === 0 && !clone.extractable, 'private lists use CreateDataProperty');

// Property descriptors used to create internal slots are also private
// Records. ToPropertyDescriptor must not consult inherited get/set members.
let descriptorCalls=0;
define(Object.prototype,'get',{__proto__:null,configurable:true,get() {
    descriptorCalls++; return undefined;
}});
try { clone=structuredClone(key); }
finally { delete Object.prototype.get; }
check(descriptorCalls===0 && !clone.extractable,'internal property descriptors');

// A record for a different platform interface must not acquire a new brand
// through Object.prototype; these examples already have real private slots.
define(Object.prototype, 'cryptoKey', {configurable:true, value:{extractable:true}});
define(Object.prototype, 'messagePort', {configurable:true, value:{id:1,detached:false}});
try {
    throws(() => apply(getOwn(CryptoKey.prototype,'extractable').get, channel.port1, []), 'TypeError', 'CryptoKey own brand');
    throws(() => apply(MessagePort.prototype.start, key, []), 'TypeError', 'MessagePort own brand');
} finally {
    delete Object.prototype.cryptoKey; delete Object.prototype.messagePort;
}

let reads = 0;
const payload = {a:1, get b() { reads++; delete this.c; return 2; }, c:3};
define(payload,'toJSON',{value() { throw new Error('ordinary object toJSON called'); }});
define(payload,'__proto__',{value:{safe:true},enumerable:true});
const copied = structuredClone(payload);
check(reads === 1 && copied.b === 2 && !Object.hasOwn(copied,'c'), 'own-property snapshot and deletion');
check(Object.getPrototypeOf(copied) === Object.prototype && copied.__proto__.safe, '__proto__ is an own data property');
const sparse = []; sparse.length=4; sparse[2]=key; sparse.extra=sparse;
const sparseClone = structuredClone(sparse);
check(sparseClone.length === 4 && !(0 in sparseClone) && sparseClone.extra === sparseClone, 'array holes and cycles');

const map = new Map(), set = new Set();
const mapEntry = {get value() { map.clear(); map.set('new',9); return 1; }};
const setEntry = {get value() { set.clear(); set.add('new'); return 2; }};
map.set('first',mapEntry); map.set('last',3);
set.add(setEntry); set.add('last');
map.forEach = set.forEach = () => { throw new Error('author collection method called'); };
const mapClone = structuredClone(map), setClone = structuredClone(set);
check(mapClone.size === 2 && mapClone.get('first').value === 1 && mapClone.get('last') === 3, 'MapData snapshot');
check(setClone.size === 2 && setClone.has('last') && !setClone.has('new'), 'SetData snapshot');
const invalidDate = new Date(NaN), date = new Date(1234), regexp = /a/gi;
date.getTime = () => { throw new Error('author Date method called'); };
define(regexp,'source',{get() { throw new Error('author RegExp source read'); }});
define(regexp,'flags',{get() { throw new Error('author RegExp flags read'); }});
check(Number.isNaN(structuredClone(invalidDate).getTime()) && structuredClone(date).getTime() === 1234, 'DateValue slot');
check(structuredClone(regexp).toString() === '/a/gi', 'RegExp slots');
const regexpPrototypeClone=structuredClone(RegExp.prototype);
check(Object.getPrototypeOf(regexpPrototypeClone)===Object.prototype && Object.keys(regexpPrototypeClone).length===0,
    'RegExp.prototype is an ordinary object');

channel.port1.close(); channel.port2.close(); transferred.port2.close(); receivedPort.close();
