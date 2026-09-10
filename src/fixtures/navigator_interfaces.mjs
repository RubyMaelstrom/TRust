// Original HTML Navigator/WorkerNavigator and Web IDL binding regressions.
(function () {
    function check(ok,message){if(!ok)throw Error(message);}
    function throws(name,fn){try{fn();}catch(e){check(e.name===name,'wrong error '+e.name);return;}throw Error('missing '+name);}
    const worker=typeof document==='undefined', name=worker?'WorkerNavigator':'Navigator';
    const Constructor=globalThis[name], nav=navigator;
    check(typeof Constructor==='function','Navigator interface object');
    check(nav instanceof Constructor && Object.getPrototypeOf(nav)===Constructor.prototype,'Navigator prototype');
    check(Object.prototype.toString.call(nav)==='[object '+name+']','Navigator tag');
    throws('TypeError',()=>new Constructor());throws('TypeError',()=>Constructor());
    const properties=['appCodeName','appName','appVersion','platform','product','userAgent','language','languages','onLine','hardwareConcurrency','globalPrivacyControl','permissions'];
    if(!worker)properties.push('vendor','vendorSub','productSub','cookieEnabled','maxTouchPoints','webdriver','plugins','mimeTypes','pdfViewerEnabled');
    for(const key of properties) {
        const descriptor=Object.getOwnPropertyDescriptor(Constructor.prototype,key);
        check(descriptor && descriptor.enumerable && descriptor.configurable && typeof descriptor.get==='function' && descriptor.set===undefined,'Navigator readonly descriptor '+key);
        check(!Object.hasOwn(nav,key),'Navigator attribute belongs to prototype '+key);
        check(!Object.hasOwn(descriptor.get,'prototype'),'IDL getter is not a constructor '+key);
        throws('TypeError',()=>Reflect.construct(descriptor.get,[]));
        throws('TypeError',()=>descriptor.get.call({}));
        throws('TypeError',()=>descriptor.get.call(Object.create(Constructor.prototype)));
        throws('TypeError',()=>descriptor.get.call(new Proxy(nav,{})));
    }
    check(nav.appCodeName==='Mozilla' && nav.appName==='Netscape' && nav.product==='Gecko','HTML compatibility constants');
    check(nav.userAgent==='TRust/0.1' && nav.appVersion==='','identity is not spoofed');
    check(Object.isFrozen(nav.languages) && nav.languages===nav.languages && nav.language===nav.languages[0],'stable frozen languages');
    throws('TypeError',function(){'use strict';nav.userAgent='changed';});
    throws('TypeError',function(){'use strict';globalThis.navigator={};});
    const globalDescriptor=Object.getOwnPropertyDescriptor(globalThis,'navigator');
    check(globalDescriptor.get.call(null)===nav && globalDescriptor.get.call(undefined)===nav,'global getter null-this substitution');
    throws('TypeError',()=>globalDescriptor.get.call({}));
    check(typeof globalThis.__navigator_binding==='undefined','private binding consumed');
    if(worker) {
        check(typeof Navigator==='undefined' && typeof PluginArray==='undefined','Window-only interfaces not exposed in Worker');
        for(const key of ['productSub','vendor','vendorSub','plugins','mimeTypes','pdfViewerEnabled','javaEnabled','taintEnabled','oscpu','webdriver','maxTouchPoints'])check(!(key in nav),'Window-only attribute '+key);
    } else {
        check(clientInformation===nav,'clientInformation aliases associated Navigator');
        const alias=Object.getOwnPropertyDescriptor(globalThis,'clientInformation');
        globalThis.clientInformation=17;check(clientInformation===17 && navigator===nav,'Replaceable alias does not replace Navigator');
        Object.defineProperty(globalThis,'clientInformation',alias);
        check(nav.vendor==='' && nav.vendorSub==='' && nav.productSub==='20100101' && nav.taintEnabled()===false,'Gecko compatibility mode, not Gecko engine identity');
        check(nav.pdfViewerEnabled===false && nav.javaEnabled()===false,'unavailable inline PDF and Java');
        throws('TypeError',()=>new Plugin());throws('TypeError',()=>new MimeType());
        for(const [list,C] of [[nav.plugins,PluginArray],[nav.mimeTypes,MimeTypeArray]]) {
            check(list instanceof C && !Array.isArray(list) && list.length===0,'empty legacy collection type');
            check(list=== (C===PluginArray?nav.plugins:nav.mimeTypes),'SameObject collection');
            check(list[Symbol.iterator]===Array.prototype.values && Array.from(list).length===0,'IDL indexed iterator');
            check(list.item(0)===null && list.item(-1)===null && list.namedItem('absent')===null,'empty collection lookup');
            throws('TypeError',()=>new C());throws('TypeError',()=>list.item());throws('TypeError',()=>list.namedItem());
            throws('TypeError',()=>list.item(1n));throws('TypeError',()=>list.namedItem(Symbol()));
            let count=0;
            check(list.item({valueOf(){count++;return 0;}})===null && count===1,'numeric conversion once');
            count=0;check(list.namedItem({toString(){count++;return 'absent';}})===null && count===1,'string conversion once');
            throws('TypeError',()=>C.prototype.item.call({}, {valueOf(){count++;return 0;}}));check(count===1,'collection brand before conversion');
            check(!Reflect.defineProperty(list,'0',{value:'no'}) && !Reflect.set(list,'0','no'),'indexed properties cannot be created');
            const OriginalString=globalThis.String;
            try {
                globalThis.String=()=>{throw Error('internal index check called author String');};
                check(!Reflect.defineProperty(list,'4294967294',{value:'no'}),'last array index remains protected');
                check(Reflect.defineProperty(list,'4294967295',{value:7,configurable:true}),'non-index numeric expando');
                delete list['4294967295'];
            } finally {globalThis.String=OriginalString;}
            const receiver={};check(Reflect.set(list,'0','elsewhere',receiver) && receiver[0]==='elsewhere','foreign receiver ordinary set');
            check(!Reflect.preventExtensions(list) && Object.isExtensible(list),'legacy platform object remains extensible');
            throws('TypeError',()=>Object.freeze(list));
            list.example=3;check(list.example===3 && Object.keys(list).join(',')==='example','ordinary expando preserved');delete list.example;
        }
        check(nav.plugins.refresh()===undefined,'refresh empty collection');
    }
    return 'navigator-interfaces-ok';
})();
