// Cookie Store (54559c0c) and Web IDL: shared jar, creation URL, asynchronous
// operations, dictionary order, secure exposure and change-event payloads.
(async () => {
    const assert = (value, message) => { if (!value) throw new Error(message); };
    const rejects = async (run, name = 'TypeError') => {
        try { await run(); } catch (error) { assert(error.name === name, String(error)); return; }
        throw new Error('operation unexpectedly resolved');
    };
    const store = cookieStore;
    assert(store === cookieStore && store instanceof EventTarget, 'same object and EventTarget');
    assert(Object.getPrototypeOf(store) === CookieStore.prototype, 'CookieStore prototype');
    assert(store.get.length === 0 && store.getAll.length === 0 && store.set.length === 1 && store.delete.length === 1, 'IDL arities');
    globalThis.cookieChanges = [];
    store.onchange = event => {
        assert(event.isTrusted && event.target === store, 'trusted change target');
        assert(Object.isFrozen(event.changed) && Object.isFrozen(event.deleted), 'frozen event lists');
        cookieChanges.push(...event.changed.map(cookie => ['changed', cookie.name, cookie.value]),
            ...event.deleted.map(cookie => ['deleted', cookie.name, cookie.value]));
    };
    const source = [{name:'x', value:'one'}];
    const event = new CookieChangeEvent('change', {changed:source, deleted:[{name:'gone'}]});
    source[0].value='two'; source.push({name:'extra'});
    assert(event.changed.length === 1 && event.changed[0].value === 'one', 'event copies dictionaries');
    assert(event.changed === event.changed && event.deleted[0].value === undefined, 'same event arrays');
    assert(!event.isTrusted, 'constructed event is untrusted');
    let closed=false;
    const invalidCookies={ [Symbol.iterator](){return {next(){return {done:false,value:1};},return(){closed=true;return {};}};} };
    await rejects(()=>new CookieChangeEvent('change',{changed:invalidCookies}));
    assert(!closed,'IDL sequence conversion does not close the iterator on a bad item');
    let defaultsRead=0;
    Object.defineProperty(Object.prototype,'changed',{configurable:true,get(){defaultsRead++;return [{name:'inherited'}];}});
    try { assert(new CookieChangeEvent('change',null).changed.length===0,'null dictionary has no inherited members'); }
    finally { delete Object.prototype.changed; }
    assert(defaultsRead===0,'empty dictionaries avoid prototype reads');
    await rejects(() => CookieStore.prototype.get.call({}));
    await rejects(() => store.get());
    await rejects(() => store.set());
    await rejects(() => store.set({name:'missing'}));
    await rejects(() => store.set('bad=name','v'));
    await rejects(() => store.set('x','bad;value'));
    await rejects(() => store.set({name:'x',value:'v',domain:'.test'}));
    await rejects(() => store.set({name:'x',value:'v',expires:Infinity}));
    await rejects(() => store.set({name:'x',value:'v',expires:0,maxAge:1}));
    await rejects(() => store.set('__Http-secret','v'));
    await rejects(() => store.set({name:'__Host-x',value:'v',path:'/a'}));
    const order = [], options = {};
    for (const [name,value] of Object.entries({domain:null,expires:null,maxAge:null,name:'ordered',partitioned:false,path:'/',sameSite:'strict',value:'ok'}))
        Object.defineProperty(options,name,{get(){order.push(name);return value;}});
    await store.set(options);
    assert(order.join() === 'domain,expires,maxAge,name,partitioned,path,sameSite,value', 'dictionary conversion order');
    await store.set(' \talpha ', ' one\t ');
    assert((await store.get(' alpha ')).value === 'one', 'cookie whitespace normalization');
    assert(document.cookie.includes('alpha=one'), 'CookieStore writes reach document.cookie');
    document.cookie = 'scoped=from-document; Secure; Path=/a';
    assert((await store.get('scoped')).value === 'from-document', 'document writes reach CookieStore');
    await store.set({name:'alpha',value:'partition',partitioned:true});
    assert((await store.getAll('alpha')).length === 2, 'partitioned identities coexist');
    await store.delete({name:'alpha',partitioned:true});
    assert((await store.get('alpha')).value === 'one', 'partition deletion preserves ordinary cookie');
    await store.delete('alpha');
    assert(await store.get('alpha') === null, 'delete string uses default partition');
    const original = location.href;
    history.replaceState(null,'','/elsewhere');
    assert(!document.cookie.includes('scoped=from-document'), 'document.cookie follows live URL');
    assert((await store.get('scoped')).value === 'from-document', 'CookieStore retains creation URL');
    await rejects(() => store.getAll({url:location.href}));
    assert((await store.getAll({url:original+'#fragment'})).some(c => c.name === 'scoped'), 'creation URL ignores fragment');
    assert((await store.get('ordered')).value === 'ok', 'unrelated cookies survive');
    let serialized=0;
    Object.defineProperty(Object.prototype,'toJSON',{configurable:true,value(){serialized++;throw new Error('private cookie data reached toJSON');}});
    try {
        await store.set('prototype-safe','value');
        assert((await store.get('prototype-safe')).value==='value','private cookie records ignore toJSON');
        await store.delete('prototype-safe');
    } finally { delete Object.prototype.toJSON; }
    assert(serialized===0,'author serialization hooks did not run');
    const channel=new MessageChannel();
    Object.defineProperty(Object.prototype,'cookieStore',{configurable:true,value:{kind:'CookieChangeEvent',changed:['forged']}});
    try { await rejects(()=>Object.getOwnPropertyDescriptor(CookieChangeEvent.prototype,'changed').get.call(channel.port1)); }
    finally { delete Object.prototype.cookieStore;channel.port1.close();channel.port2.close(); }
    globalThis.cookieStoreResult = 'cookie-store-ok';
})().catch(error => { globalThis.cookieStoreResult = String(error.stack || error); });
