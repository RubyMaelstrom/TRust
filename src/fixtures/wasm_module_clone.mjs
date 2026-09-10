(function () {
    function assert(ok, message) { if (!ok) throw Error(message); }
    const bytes = new Uint8Array([
        0,97,115,109,1,0,0,0,1,5,1,96,0,1,127,3,2,1,0,7,10,1,6,
        97,110,115,119,101,114,0,0,10,6,1,4,0,65,42,11
    ]);
    const module = new WebAssembly.Module(bytes);
    bytes.fill(0);
    Object.defineProperty(module, 'expando', { enumerable:true, get() { throw Error('expando read'); } });
    Object.setPrototypeOf(module, null);
    const source = { module, again:module, map:new Map([[module,module]]) };
    source.self = source;
    const copy = structuredClone(source);
    assert(copy.self === copy && copy.module === copy.again, 'cycles and repeated identity');
    assert(copy.map.get(copy.module) === copy.module, 'Map identity');
    assert(copy.module !== module && copy.module instanceof WebAssembly.Module, 'new receiver Module');
    assert(!Object.hasOwn(copy.module, 'expando'), 'expandos excluded');
    assert(new WebAssembly.Instance(copy.module).exports.answer() === 42, 'immutable source bytes');
    assert(WebAssembly.Module.exports(module)[0].name === 'answer', 'internal brand ignores prototype');
    let forged = false;
    try { WebAssembly.Module.exports(Object.create(WebAssembly.Module.prototype)); }
    catch (error) { forged = error instanceof TypeError; }
    assert(forged, 'forged prototype is not Module');
    let storage = false;
    try { __sc_serialize({nested:new Set([module])}, true); }
    catch (error) { storage = error.name === 'DataCloneError'; }
    assert(storage, 'nested storage serialization rejected');
    assert(typeof __wasm_register_clone === 'undefined' && typeof __wasm_module_binding === 'undefined', 'bootstrap hooks private');
    return 'wasm-module-clone-ok';
})()
