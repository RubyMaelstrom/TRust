// Original Permissions #query-method / Web IDL object and dictionary checks.
(async function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    async function rejects(name, operation) {
        let promise;
        try { promise=operation(); } catch(e) { throw Error('synchronous permission error'); }
        check(promise instanceof Promise,'query returns a Promise');
        try { await promise; } catch(e) { check(e.name===name,'wrong rejection '+e.name); return e; }
        throw Error('unsupported permission unexpectedly fulfilled');
    }
    const permissions=navigator.permissions;
    // None of these powerful APIs is implemented by the browser. Pretending
    // the browser could prompt for one is not an honest capability report.
    for (const name of ['not-a-real-permission','geolocation','camera','microphone','notifications','midi','clipboard-read'])
        await rejects('TypeError',()=>permissions.query({name}));
    for (const descriptor of [undefined,null,1,'camera',Symbol(),{}, {name:undefined}, {name:Symbol()}])
        await rejects('TypeError',()=>permissions.query(descriptor));
    let reads=0, conversions=0;
    await rejects('TypeError',()=>permissions.query({get name() { reads++; return {
        toString() { conversions++; return 'not-supported'; }
    }; }}));
    check(reads===1 && conversions===1,'root descriptor conversion occurs once');
    const sentinel=Error('original getter error');
    check(await rejects('Error',()=>permissions.query({get name() { throw sentinel; }}))===sentinel,'getter rejection preserves identity');
    let probed=false;
    await rejects('TypeError',()=>permissions.query.call({}, {get name(){probed=true;return 'camera';}}));
    check(!probed,'receiver brand is checked before dictionary access');
    check(permissions instanceof Permissions && Object.prototype.toString.call(permissions)==='[object Permissions]','Permissions brand');
    check(navigator.permissions===permissions,'SameObject getter');
    try { navigator.permissions={}; } catch(e) { check(e.name==='TypeError','readonly getter error'); }
    check(navigator.permissions===permissions,'readonly permissions');
    try { new Permissions(); throw Error('constructed Permissions'); } catch(e) { check(e.name==='TypeError','illegal Permissions constructor'); }
    check(typeof globalThis.__permissions_binding==='undefined','private binding consumed');
    return 'permissions-unsupported-ok';
})().then(value=>globalThis.permissionResult=value,
          error=>globalThis.permissionResult='ERROR:'+error.name+':'+error.message);
