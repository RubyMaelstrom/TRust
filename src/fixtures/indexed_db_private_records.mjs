// IndexedDB f704918 #clone-value / #object-store-storage-operation and
// WebCrypto 811c24c #cryptokey-interface-serializable: database records must
// keep a serialized CryptoKey private throughout storage and retrieval.
const request = request => new Promise((resolve,reject) => {
    request.onsuccess=()=>resolve(request.result); request.onerror=()=>reject(request.error);
});
const complete = transaction => new Promise((resolve,reject) => {
    transaction.oncomplete=resolve; transaction.onabort=()=>reject(transaction.error);
});
const open = indexedDB.open('clone-private-record-regression',1);
open.onupgradeneeded = () => {
    const store=open.result.createObjectStore('records');
    store.createIndex('label','label',{unique:true});
};
const db=await request(open);
const key=await crypto.subtle.importKey('raw',new Uint8Array(16),'AES-GCM',false,['encrypt']);
const apply=Reflect.apply, define=Object.defineProperty, descriptor=Object.getOwnPropertyDescriptor;
const includes=String.prototype.includes, isArray=Array.isArray;
const leaks=new Set(), add=Set.prototype.add;
function inspect(value,label,depth=0) {
    if (depth>6 || value===null || typeof value!=='object') return;
    if (isArray(value)) {
        if (descriptor(value,'0')?.value==='CK') apply(add,leaks,[label]);
        for (let i=0; i<value.length && i<32; i++) inspect(descriptor(value,i)?.value,label,depth+1);
    } else {
        for (let i=0; i<fields.length; i++) {
            const field=descriptor(value,fields[i]);
            if (!field || !Object.hasOwn(field,'value')) continue;
            if (typeof field.value==='string' && apply(includes,field.value,['"CK"'])) apply(add,leaks,[label]);
            else inspect(field.value,label,depth+1);
        }
    }
}
const fields=['value','wire','stores','records','data','key'];
const targets=['push','map','filter','find','findIndex','sort','slice','splice','forEach','reverse',Symbol.iterator];
const originals=targets.map(name=>descriptor(Array.prototype,name));
const assign=Object.assign;
define(Object.prototype,'toJSON',{configurable:true,value(){inspect(this,'toJSON');return this;}});
for (let i=0; i<targets.length; i++) {
    const name=targets[i], original=originals[i].value;
    define(Array.prototype,name,{...originals[i],value:function(...args) {
        inspect(this,'Array.'+String(name));
        inspect(args,'Array.'+String(name));
        return apply(original,this,args);
    }});
}
Object.assign=function(...args) {inspect(args,'Object.assign');return apply(assign,Object,args);};
let stored, indexed, all, cursorValue;
try {
    let tx=db.transaction('records','readwrite'), done=complete(tx);
    tx.objectStore('records').put({label:'first',key},1);
    tx.objectStore('records').put({label:'second',key},2);
    await done;
    tx=db.transaction('records','readonly'); done=complete(tx);
    const store=tx.objectStore('records');
    const one=request(store.get(1)), list=request(store.getAll()), secondary=request(store.index('label').get('second'));
    const cursor=request(store.openCursor());
    stored=await one; all=await list; indexed=await secondary; cursorValue=(await cursor).value;
    await done;
    tx=db.transaction('records','readwrite'); done=complete(tx);
    tx.objectStore('records').delete(1);
    tx.objectStore('records').put({label:'replacement',key},2);
    await done;
} finally {
    delete Object.prototype.toJSON;
    for (let i=0; i<targets.length; i++) define(Array.prototype,targets[i],originals[i]);
    Object.assign=assign; db.close();
}
check(leaks.size===0,'private records reached '+Array.from(leaks).join(','));
check(all.length===2 && indexed.label==='second' && cursorValue.label==='first','record/index/cursor results');
check(!stored.key.extractable && !indexed.key.extractable && !cursorValue.key.extractable,'stored key policy');
await rejects(crypto.subtle.exportKey('raw',stored.key),'InvalidAccessError','stored key remains non-extractable');
await request(indexedDB.deleteDatabase('clone-private-record-regression'));
