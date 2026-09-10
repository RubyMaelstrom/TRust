// Original same-Agent binding and Window security regressions, without network.
(function () {
    function check(ok,message){if(!ok)throw Error(message);}
    const html=document.createElement('html'), body=document.createElement('body');
    document.appendChild(html);html.appendChild(body);
    const frame=document.createElement('iframe');body.appendChild(frame);
    const foreign=frame.contentWindow, other=foreign.navigator;
    check(other!==navigator && foreign.Navigator!==Navigator,'per-Realm identity');
    check(Object.getPrototypeOf(other)===foreign.Navigator.prototype,'foreign prototype');
    for(const key of ['userAgent','languages','globalPrivacyControl','plugins','mimeTypes','permissions']) {
        const local=Object.getOwnPropertyDescriptor(Navigator.prototype,key).get;
        const remote=Object.getOwnPropertyDescriptor(foreign.Navigator.prototype,key).get;
        check(local.call(other)===other[key] && remote.call(navigator)===navigator[key],'cross-Realm Navigator getter '+key);
        let rejected=false;
        try {remote.call({});}catch(e){rejected=e instanceof foreign.TypeError;}
        check(rejected,'getter error belongs to function Realm '+key);
    }
    const getter=Object.getOwnPropertyDescriptor(globalThis,'navigator').get;
    check(getter.call(foreign)===other,'WindowProxy receiver');
    const raw=foreign.eval('globalThis');
    check(getter.call(raw)===other,'actual Window receiver');
    const foreignGetter=Object.getOwnPropertyDescriptor(foreign,'navigator').get;
    check(foreignGetter.call(globalThis)===navigator,'borrowed Window getter');
    for(const [property,C] of [['plugins',PluginArray],['mimeTypes',MimeTypeArray]]) {
        const list=other[property];
        check(C.prototype.item.call(list,0)===null,'cross-Realm indexed operation');
        check(C.prototype.namedItem.call(list,'absent')===null,'cross-Realm named operation');
        check(Object.getOwnPropertyDescriptor(C.prototype,'length').get.call(list)===0,'cross-Realm length');
        check(Object.getPrototypeOf(list)===foreign[C.name].prototype,'collection creation Realm');
    }
    const alias=Object.getOwnPropertyDescriptor(globalThis,'clientInformation');
    alias.set.call(foreign,23);
    check(foreign.clientInformation===23 && foreign.navigator===other,'cross-Realm Replaceable alias');
    const opaque=document.createElement('iframe');
    opaque.src='data:text/html,<p>opaque</p>';body.appendChild(opaque);
    __trust.hydrateFrames();
    for(const operation of [()=>getter.call(opaque.contentWindow),
        ()=>alias.get.call(opaque.contentWindow),()=>alias.set.call(opaque.contentWindow,1)]) {
        let rejected=false;
        try {operation();}catch(e){rejected=e.name==='SecurityError' && e instanceof DOMException;}
        check(rejected,'cross-origin Window security check');
    }
    // Navigators and their collections retained by script remain usable after
    // removal; active-document restrictions belong to the APIs requiring them.
    frame.remove();
    check(other.userAgent===navigator.userAgent && other.plugins.length===0,'retained Navigator');
    return 'navigator-realms-ok';
})();
