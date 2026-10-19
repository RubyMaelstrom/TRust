// W3C Service Workers 92aba3be, client-context interfaces. Private records
// carry their creating realm's transport, so borrowed operations retain the
// receiver's settings object and task destination.
(function (g) {
    "use strict";
    const adapter = g.__service_worker_adapter;
    delete g.__service_worker_adapter;
    if (!adapter || !adapter.secure) return;
    const apply = Reflect.apply, construct = Reflect.construct, define = Object.defineProperty;
    const create = Object.create, freeze = Object.freeze, names = Object.getOwnPropertyNames;
    const weakGet = WeakMap.prototype.get, weakSet = WeakMap.prototype.set;
    const hasOwn = Object.prototype.hasOwnProperty;
    const slots = __platform_slots("internals", new WeakMap());
    const PromiseCtor = Promise, react = __promise_react, reject = Promise.reject;
    const TypeErrorCtor = TypeError, DOMExceptionCtor = g.DOMException, EventTargetCtor = adapter.EventTarget;
    const parse = JSON.parse, stringify = JSON.stringify, toWellFormed = String.prototype.toWellFormed;
    const split = String.prototype.split, test = RegExp.prototype.test, encodedSeparator=/%2f|%5c/i;
    const iterator = Symbol.iterator, setPrototype=Object.setPrototypeOf;
    const workers = create(null), registrations = create(null), pending = create(null);
    const own = value => { const shared=apply(weakGet, slots, [value]);return shared&&apply(hasOwn,shared,["serviceWorker"])&&shared.serviceWorker; };
    const remember = (value, record) => {
        let shared=apply(weakGet,slots,[value]);
        if(!shared){shared=create(null);apply(weakSet,slots,[value,shared]);}
        define(shared,"serviceWorker",{value:record,writable:true,enumerable:true,configurable:true});
    };
    const string = value => `${value}`;
    const usv = value => apply(toWellFormed, string(value), []);
    const object = value => value !== null && (typeof value === "object" || typeof value === "function");
    const rejected = error => apply(reject, PromiseCtor, [error]);
    function record(kind) { const r = create(null); r.kind=kind; r.handlers=create(null); r.transport=request; r.adapter=adapter; return r; }
    function brand(value, kind) {
        const r = object(value) && own(value);
        if (!r || r.kind !== kind) throw new TypeErrorCtor("Illegal " + kind + " invocation");
        return r;
    }
    function dictionary(value) {
        if (value == null) return create(null);
        if (!object(value)) throw new TypeErrorCtor("Expected a dictionary");
        return value;
    }
    function enumeration(value, allowed) {
        const converted = string(value);
        for (let i=0;i<allowed.length;i++) if (allowed[i] === converted) return converted;
        throw new TypeErrorCtor("Invalid enumeration value");
    }
    function exception(error) {
        return error.name === "TypeError" ? new TypeErrorCtor(error.message) : new DOMExceptionCtor(error.message, error.name);
    }
    function native(op, input) {
        const result = parse(adapter.native(op, stringify(input)));
        if (result.error) throw exception(result.error);
        return result.value;
    }
    function request(op, input, convert) {
        return new PromiseCtor((yes, no) => {
            let id;
            try { id = native(op, input); } catch (error) { no(error); return; }
            pending[id] = {yes,no,convert};
        });
    }
    function transport(op, input, convert) {
        // Job equivalence depends on actual public promise settlement, after
        // thenable assimilation, rather than invocation of its resolve callback.
        let id;
        const promise = new PromiseCtor((yes,no) => {
            try { id=native(op,input); } catch(error) {
                // Offline DOM-only evaluations have no browser network/task
                // coordinator. Their registration map is empty; mutations
                // still fail explicitly instead of creating inert workers.
                if(error.name==="InvalidStateError"&&(op==="getRegistration"||op==="getRegistrations"))
                    adapter.queue(()=>yes(convert(op==="getRegistrations"?[]:null)));
                else if(error.name==="SecurityError"&&op==="register")adapter.queue(()=>no(error));
                else no(error);
                return;
            }
            pending[id]={yes,no,convert};
        });
        if (id !== undefined) {
            const acknowledge = () => { const input=create(null); input.id=id; native("settled",input); };
            react(promise, acknowledge, acknowledge);
        }
        return promise;
    }
    function inputFor(r) {
        const input=create(null); input.base=r.adapter.baseURL();
        if(r.kind==="ServiceWorkerRegistration") {input.registration=r.id;input.scope=r.scope;}
        return input;
    }
    function registrationURL(text, base) {
        const parsed=__url_parse(text,base);
        if(!parsed)throw new TypeErrorCtor("Invalid service-worker URL");
        // URL tuple indices are the URL host boundary's protocol/path fields.
        if((parsed[1]!=="https:"&&parsed[1]!=="http:")||apply(test,encodedSeparator,[parsed[5]]))
            throw new TypeErrorCtor("Unsupported service-worker URL");
        return apply(split,parsed[0],["#",1])[0];
    }
    function worker(info) {
        if (info === null || info === undefined) return null;
        let value=workers[info.id];
        if (!value) {
            value=construct(EventTargetCtor,[],ServiceWorker);
            const r=record("ServiceWorker"); r.id=info.id;r.scriptURL=info.scriptURL;r.state=info.state;r.revision=info.revision;
            remember(value,r); workers[info.id]=value;
        }
        return value;
    }
    function registration(info) {
        if (info === null || info === undefined) return undefined;
        let value=registrations[info.id];
        if (!value) {
            value=construct(EventTargetCtor,[],ServiceWorkerRegistration);
            const r=record("ServiceWorkerRegistration");r.id=info.id;r.scope=info.scope;r.revision=-1;
            r.transport=transport;r.convert=registration;
            r.makeCookies=()=>{
                const value=create(CookieStoreManager.prototype), manager=record("CookieStoreManager");
                manager.owner=r;remember(value,manager);return value;
            };
            remember(value,r);registrations[info.id]=value;
        }
        const r=own(value);
        if(info.revision>=r.revision) {
            r.installing=worker(info.installing);r.waiting=worker(info.waiting);r.active=worker(info.active);
            r.updateViaCache=info.updateViaCache;r.revision=info.revision;r.registered=info.registered;
        }
        return value;
    }
    class ServiceWorker extends EventTargetCtor {
        constructor() { throw new TypeErrorCtor("Illegal constructor"); }
        get scriptURL() { return brand(this,"ServiceWorker").scriptURL; }
        get state() { return brand(this,"ServiceWorker").state; }
    }
    // ServiceWorker/postMessage: serialize first (rethrowing), with the
    // incumbent global as the source. Windows capture the incumbent through
    // the native callback boundary; a worker Realm is its own incumbent.
    function postToWorker(r, incumbent, message, options) {
        const packet = r.adapter.serializeMessage(message, options);
        const input = create(null);
        input.worker = r.id; input.packet = packet; input.incumbent = incumbent;
        native("postWorker", input);
    }
    define(ServiceWorker.prototype, "postMessage", {writable:true, enumerable:true, configurable:true,
        value: adapter.callback ? adapter.callback(function (source, receiver, args, incumbent) {
            const r = brand(receiver, "ServiceWorker");
            if (!args.length) throw new TypeErrorCtor("postMessage requires a message");
            postToWorker(r, r.adapter.incumbentContext(incumbent), args[0], args[1]);
        }, "postMessage", 1) : {postMessage(message) {
            const r = brand(this, "ServiceWorker");
            if (!arguments.length) throw new TypeErrorCtor("postMessage requires a message");
            postToWorker(r, r.adapter.context, message, arguments[1]);
        }}.postMessage});
    function clientRecord(value) {
        const r=object(value)&&own(value);
        if(!r||(r.kind!=="Client"&&r.kind!=="WindowClient"))throw new TypeErrorCtor("Illegal Client invocation");
        return r;
    }
    function client(info) {
        if(info==null)return undefined;
        const window=info.type==="window", value=create(window?WindowClient.prototype:Client.prototype);
        const r=record(window?"WindowClient":"Client");r.info=info;
        if(window)r.ancestors=freeze(info.ancestorOrigins);
        remember(value,r);return value;
    }
    class Client {
        constructor(){throw new TypeErrorCtor("Illegal constructor");}
        get url(){return clientRecord(this).info.url;}
        get id(){return clientRecord(this).info.id;}
        get type(){return clientRecord(this).info.type;}
        get frameType(){return clientRecord(this).info.frameType;}
        // Client/postMessage: serialize first with this Client's relevant
        // settings (the service worker Realm), then enqueue in parallel.
        postMessage(message){
            const r=clientRecord(this);
            if(!arguments.length)throw new TypeErrorCtor("postMessage requires a message");
            const packet=r.adapter.serializeMessage(message,arguments[1]);
            const input=create(null);input.client=r.info.id;input.packet=packet;
            native("postClient",input);
        }
    }
    class WindowClient extends Client {
        constructor(){throw new TypeErrorCtor("Illegal constructor");}
        get visibilityState(){return brand(this,"WindowClient").info.visibilityState;}
        get focused(){return brand(this,"WindowClient").info.focused;}
        get ancestorOrigins(){return brand(this,"WindowClient").ancestors;}
    }
    class Clients {
        constructor(){throw new TypeErrorCtor("Illegal constructor");}
        get(id){
            try{
                const r=brand(this,"Clients");
                if(arguments.length<1)throw new TypeErrorCtor("get requires an id");
                const input=create(null);input.id=string(id);
                return r.transport("getClient",input,client);
            }catch(error){return rejected(error);}
        }
        matchAll(options=undefined){
            try{
                const r=brand(this,"Clients"),dict=dictionary(options),input=create(null);
                input.includeUncontrolled=!!dict.includeUncontrolled;
                const type=dict.type;input.type=type===undefined?"window":enumeration(type,["window","worker","sharedworker","all"]);
                return r.transport("matchClients",input,values=>{
                    const list=[];for(let i=0;i<values.length;i++)define(list,i,{value:client(values[i]),writable:true,enumerable:true,configurable:true});
                    return freeze(list);
                });
            }catch(error){return rejected(error);}
        }
        claim(){
            try{const r=brand(this,"Clients");return r.transport("claim",create(null),()=>undefined);}
            catch(error){return rejected(error);}
        }
    }
    class ServiceWorkerRegistration extends EventTargetCtor {
        constructor() { throw new TypeErrorCtor("Illegal constructor"); }
        get installing() { return brand(this,"ServiceWorkerRegistration").installing; }
        get waiting() { return brand(this,"ServiceWorkerRegistration").waiting; }
        get active() { return brand(this,"ServiceWorkerRegistration").active; }
        get scope() { return brand(this,"ServiceWorkerRegistration").scope; }
        get updateViaCache() { return brand(this,"ServiceWorkerRegistration").updateViaCache; }
        update() {
            try {const r=brand(this,"ServiceWorkerRegistration");return r.transport("update",inputFor(r),r.convert);}
            catch(error) {return rejected(error);}
        }
        unregister() {
            try {const r=brand(this,"ServiceWorkerRegistration");return r.transport("unregister",inputFor(r),value=>value);}
            catch(error) {return rejected(error);}
        }
    }
    function subscriptions(value) {
        if(!object(value))throw new TypeErrorCtor("Subscriptions must be a sequence");
        const method=value[iterator];
        if(typeof method!=="function")throw new TypeErrorCtor("Subscriptions must be iterable");
        const iter=apply(method,value,[]);
        if(!object(iter))throw new TypeErrorCtor("Invalid iterator");
        const next=iter.next, result=[];setPrototype(result,null);
        for(;;){
            const step=apply(next,iter,[]);
            if(!object(step))throw new TypeErrorCtor("Invalid iterator result");
            if(step.done)return result;
            const dict=dictionary(step.value), item=create(null);
            const name=dict.name;if(name!==undefined)item.name=usv(name);
            const url=dict.url;if(url!==undefined)item.url=usv(url);
            result[result.length]=item;
        }
    }
    class CookieStoreManager {
        constructor(){throw new TypeErrorCtor("Illegal constructor");}
        subscribe(values){
            try {
                const r=brand(this,"CookieStoreManager");
                if(arguments.length<1)throw new TypeErrorCtor("subscribe requires subscriptions");
                const converted=subscriptions(values),input=inputFor(r.owner);input.subscriptions=converted;
                return r.owner.transport("cookieSubscribe",input,()=>undefined);
            } catch(error){return rejected(error);}
        }
        unsubscribe(values){
            try {
                const r=brand(this,"CookieStoreManager");
                if(arguments.length<1)throw new TypeErrorCtor("unsubscribe requires subscriptions");
                const converted=subscriptions(values),input=inputFor(r.owner);input.subscriptions=converted;
                return r.owner.transport("cookieUnsubscribe",input,()=>undefined);
            } catch(error){return rejected(error);}
        }
        getSubscriptions(){
            try {const r=brand(this,"CookieStoreManager");return r.owner.transport("cookieSubscriptions",inputFor(r.owner),value=>value);}
            catch(error){return rejected(error);}
        }
    }
    if(adapter.window||adapter.service){
        const getter={get cookies(){
            const r=brand(this,"ServiceWorkerRegistration");
            if(!r.cookies)r.cookies=r.makeCookies();
            return r.cookies;
        }};
        define(ServiceWorkerRegistration.prototype,"cookies",Object.getOwnPropertyDescriptor(getter,"cookies"));
    }
    class ServiceWorkerContainer extends EventTargetCtor {
        constructor() {throw new TypeErrorCtor("Illegal constructor");}
        get controller() {return brand(this,"ServiceWorkerContainer").controller;}
        get ready() {
            const r=brand(this,"ServiceWorkerContainer");
            if (!r.ready) r.makeReady();
            return r.ready;
        }
        register(scriptURL, options=undefined) {
            try {
                const r=brand(this,"ServiceWorkerContainer");
                if(arguments.length<1)throw new TypeErrorCtor("register requires a script URL");
                scriptURL=usv(scriptURL);
                const dict=dictionary(options), input=inputFor(r);input.script=scriptURL;
                const scope=dict.scope; if(scope!==undefined)input.scope=usv(scope);
                const type=dict.type;input.kind=type===undefined?"classic":enumeration(type,["classic","module"]);
                const cache=dict.updateViaCache;input.cache=cache===undefined?"imports":enumeration(cache,["imports","all","none"]);
                input.script=registrationURL(input.script,input.base);
                if(input.scope!==undefined)input.scope=registrationURL(input.scope,input.base);
                if(r.adapter.origin()==="null")return new PromiseCtor((_,no)=>r.adapter.queue(()=>no(new DOMExceptionCtor("Service-worker storage requires a non-opaque origin","SecurityError"))));
                return r.transport("register",input,r.convert);
            } catch(error) {return rejected(error);}
        }
        getRegistration(clientURL="") {
            try {
                const r=brand(this,"ServiceWorkerContainer"),input=inputFor(r);input.url=usv(clientURL);
                const parsed=__url_parse(input.url,input.base);
                if(!parsed)throw new TypeErrorCtor("Invalid client URL");
                if(parsed[8]!==r.adapter.origin()||parsed[8]==="null")throw new DOMExceptionCtor("Client URL is not same origin","SecurityError");
                return r.transport("getRegistration",input,r.convert);
            }
            catch(error) {return rejected(error);}
        }
        getRegistrations() {
            try {const r=brand(this,"ServiceWorkerContainer");if(r.adapter.origin()==="null")throw new DOMExceptionCtor("Service-worker storage requires a non-opaque origin","SecurityError");return r.transport("getRegistrations",inputFor(r),r.convertMany);}
            catch(error) {return rejected(error);}
        }
        startMessages() {brand(this,"ServiceWorkerContainer");enableMessages();}
    }
    function handlers(C, types) {
        for(let i=0;i<types.length;i++) {
            const type=types[i];
            const descriptor={enumerable:true,configurable:true,
                get(){const r=brand(this,C.name);return r.handlers[type]?r.handlers[type].value:null;},
                set(value){
                    const r=brand(this,C.name);
                    if(C===ServiceWorkerContainer&&type==="message")enableMessages();
                    if(!object(value))value=null;
                    let handler=r.handlers[type];
                    if(value===null){if(handler)r.adapter.remove(this,type,handler.listener);delete r.handlers[type];return;}
                    if(handler){handler.value=value;return;}
                    const target=this;
                    handler=r.handlers[type]={value,listener(event){if(typeof handler.value==="function")apply(handler.value,target,[event]);}};
                    r.adapter.add(this,type,handler.listener);
                }};
            define(descriptor.get,"name",{value:"get on"+type,configurable:true});
            define(descriptor.set,"name",{value:"set on"+type,configurable:true});
            define(C.prototype,"on"+type,descriptor);
        }
    }
    handlers(ServiceWorker,["statechange","error"]);
    handlers(ServiceWorkerRegistration,["updatefound"]);
    handlers(ServiceWorkerContainer,["controllerchange","message","messageerror"]);
    const interfaces=[ServiceWorker,ServiceWorkerRegistration,ServiceWorkerContainer];
    if(adapter.window||adapter.service)interfaces[interfaces.length]=CookieStoreManager;
    if(adapter.service){interfaces[interfaces.length]=Client;interfaces[interfaces.length]=WindowClient;interfaces[interfaces.length]=Clients;}
    for(const C of interfaces) {
        for(const name of names(C.prototype))if(name!=="constructor")define(C.prototype,name,{enumerable:true});
        define(C,"prototype",{writable:false});
        define(C.prototype,Symbol.toStringTag,{value:C.name,configurable:true});
        define(g,C.name,{value:C,writable:true,configurable:true});
    }
    const container=construct(EventTargetCtor,[],ServiceWorkerContainer), state=record("ServiceWorkerContainer");
    state.controller=null;state.ready=null;state.readyInfo=null;state.readyResolve=null;state.messagesEnabled=false;
    state.transport=transport;state.convert=registration;
    state.convertMany=values=>{const result=[];for(let i=0;i<values.length;i++)result[i]=registration(values[i]);return freeze(result);};
    state.makeReady=()=>{
        state.ready=new PromiseCtor(resolve=>{state.readyResolve=resolve;});
        if(state.readyInfo!==null)adapter.queue(()=>{if(state.readyResolve){state.readyResolve(registration(state.readyInfo));state.readyResolve=null;}});
    };
    // The client message queue is initially disabled; packets stay in the
    // native queue until taken here, one trusted MessageEvent per task.
    function enableMessages(){
        if(state.messagesEnabled)return;
        state.messagesEnabled=true;
        native("enableMessages",create(null));
        adapter.queue(drainMessage);
    }
    function drainMessage(){
        if(!state.messagesEnabled)return;
        const taken=native("takeMessage",create(null));
        if(!taken)return;
        const source=worker(taken.source);
        let packet;
        try {packet=adapter.deserializeMessage(taken.packet);}
        catch(_){adapter.fireMessage(container,"messageerror",{origin:taken.origin,source});adapter.queue(drainMessage);return;}
        adapter.fireMessage(container,"message",{data:packet.data,origin:taken.origin,source,ports:freeze(packet.ports)});
        adapter.queue(drainMessage);
    }
    remember(container,state);adapter.navigatorAttribute("serviceWorker",container);
    const initial=create(null);initial.url=adapter.creationURL;initial.key=adapter.storageKey();initial.secure=adapter.secure;
    initial.kind=adapter.kind;initial.window=adapter.window?adapter.clientInfo():null;
    const snapshot=native("init",initial);
    state.controller=worker(snapshot.controller);state.readyInfo=snapshot.ready;
    state.messagesEnabled=!!snapshot.messagesEnabled;
    adapter.bind({
        source(desc){
            if(!desc)return null;
            if(desc.worker)return worker(desc.worker);
            const value=client(desc.client);return value===undefined?null:value;
        },
        clientInfo(){return stringify(adapter.clientInfo());},
        clientChanged(){if(adapter.window)native("clientState",adapter.clientInfo());},
        resident(){
            for(const id in pending)return true;
            if(state.readyResolve!==null)return true;
            if(adapter.listens(container,"controllerchange")||adapter.listens(container,"message")||adapter.listens(container,"messageerror"))return true;
            for(const id in workers) {
                const value=workers[id],r=own(value);
                if(r.state!=="redundant"&&(adapter.listens(value,"statechange")||adapter.listens(value,"error")))return true;
            }
            for(const id in registrations) {
                const value=registrations[id];
                if(own(value).registered&&adapter.listens(value,"updatefound"))return true;
            }
            return false;
        },
        result(id,text){
            const completion=pending[id];if(!completion)return;delete pending[id];
            const result=parse(text);
            if(result.error){completion.no(exception(result.error));return;}
            try {completion.yes(completion.convert(result.value));}catch(error){completion.no(error);}
        },
        notice(text){
            const notice=parse(text);
            if(notice.type==="worker") {
                const info=notice.worker, value=workers[info.id];
                if(!value)return;
                const r=own(value);
                if(info.revision>r.revision){r.state=info.state;r.revision=info.revision;adapter.fire(value,"statechange");}
            } else if(notice.type==="registration") {
                if(!registrations[notice.registration.id])return;
                const value=registration(notice.registration);if(notice.updateFound)adapter.fire(value,"updatefound");
            } else if(notice.type==="controller") {
                state.controller=worker(notice.worker);adapter.fire(container,"controllerchange");
            } else if(notice.type==="ready") {
                state.readyInfo=notice.registration;
                if(state.readyResolve){state.readyResolve(registration(notice.registration));state.readyResolve=null;}
            } else if(notice.type==="messages") {
                drainMessage();
            }
        },
    });
    // Queue enable points: DOMContentLoaded for Windows (delivery is queued,
    // so it still runs after that event's task) and worker start for
    // dedicated/shared workers. onmessage/startMessages enable above.
    if(adapter.documentLoaded)adapter.documentLoaded(enableMessages);
    if(!adapter.window&&!adapter.service)enableMessages();
    if(state.messagesEnabled)adapter.queue(drainMessage);
    if(adapter.service) {
        // HTML/Web IDL exposure sets omit these interfaces in a service
        // worker; the shared worker bootstrap runs before this final layer.
        for(const name of ["Worker","SharedWorker","XMLHttpRequest","XMLHttpRequestEventTarget","XMLHttpRequestUpload","FileReaderSync"])
            delete g[name];
        delete g.URL.createObjectURL;
        delete g.URL.revokeObjectURL;
        const scope=native("scope",create(null));
        const proto=Object.getPrototypeOf(g);
        const registrationObject=registration(scope.registration), serviceObject=worker(scope.worker);
        const clients=create(Clients.prototype);remember(clients,record("Clients"));
        define(proto,"clients",{get(){if(this!=null&&this!==g)throw new TypeErrorCtor("Illegal invocation");return clients;},enumerable:true,configurable:true});
        define(proto,"registration",{get(){if(this!=null&&this!==g)throw new TypeErrorCtor("Illegal invocation");return registrationObject;},enumerable:true,configurable:true});
        define(proto,"serviceWorker",{get(){if(this!=null&&this!==g)throw new TypeErrorCtor("Illegal invocation");return serviceObject;},enumerable:true,configurable:true});
        define(proto,"skipWaiting",{value:{skipWaiting(){
            if(this!=null&&this!==g)return rejected(new TypeErrorCtor("Illegal invocation"));
            return request("skipWaiting",create(null),()=>undefined);
        }}.skipWaiting,writable:true,enumerable:true,configurable:true});
    }
})(globalThis);
