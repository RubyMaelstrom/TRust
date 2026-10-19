// Service Workers 92aba3be #extendableevent-interface / #fetchevent-interface.
// Shared event algorithms; the worker adapter supplies trusted DOM dispatch
// and native completion/response transport. No author property carries state.
(function (g) {
    "use strict";
    const adapter = g.__service_event_adapter;
    delete g.__service_event_adapter;
    if (!adapter) return;
    const apply = Reflect.apply, define = Object.defineProperty, create = Object.create;
    const get = WeakMap.prototype.get, set = WeakMap.prototype.set;
    const hasOwn = Object.prototype.hasOwnProperty;
    const slots = __platform_slots("internals", new WeakMap());
    const EventCtor = adapter.Event, DOMExceptionCtor = g.DOMException, TypeErrorCtor = TypeError;
    const PromiseCtor = Promise, resolve = __promise_resolved_with, react = __promise_react;
    const freeze=Object.freeze, usv=String.prototype.toWellFormed, iterator=Symbol.iterator;
    const stop = EventCtor.prototype.stopImmediatePropagation;
    const canceled = Object.getOwnPropertyDescriptor(EventCtor.prototype,"defaultPrevented").get;
    const fetchAPI = adapter.fetch;
    const live = create(null);
    const object = value => value !== null && (typeof value === "object" || typeof value === "function");
    const string = value => `${value}`;
    function dictionary(value) {
        if (value == null) return create(null);
        if (!object(value)) throw new TypeErrorCtor("Expected an event-init dictionary");
        return value;
    }
    function baseInit(dict) {
        return {bubbles:!!dict.bubbles, cancelable:!!dict.cancelable, composed:!!dict.composed};
    }
    function state(event, fetch = false) {
        const shared = object(event) && apply(get, slots, [event]);
        const state = shared && apply(hasOwn,shared,["serviceEvent"]) && shared.serviceEvent;
        if (!state || fetch && !state.fetch) throw new TypeErrorCtor("Illegal service-worker event invocation");
        return state;
    }
    function remember(event) {
        let shared = apply(get, slots, [event]);
        if (!shared) { shared = create(null); apply(set, slots, [event, shared]); }
        const own={pending:0, failed:false, timedOut:false, finished:false,
            id:null, complete:null, fetch:null, cookies:null, message:null};
        define(shared,"serviceEvent",{value:own,writable:true,enumerable:true,configurable:true});
        return own;
    }
    function finish(event, own) {
        if (own.finished || adapter.dispatching(event) || own.pending !== 0) return;
        own.finished = true;
        if (own.complete !== null) own.complete(!own.failed);
        if (own.id !== null && own.fetch === null) delete live[own.id];
    }
    function addLifetime(event, promise) {
        const own = state(event);
        if (!adapter.trusted(event) || own.timedOut || (own.pending === 0 && !adapter.dispatching(event)))
            throw new DOMExceptionCtor("The event cannot be extended", "InvalidStateError");
        own.pending++;
        const settled = failed => {
            if (failed) own.failed = true;
            // The count decrement is a further microtask, even when the
            // given promise was already settled (SW #add-lifetime-promise).
            react(resolve(undefined), () => { own.pending--; finish(event, own); });
        };
        react(promise, () => settled(false), () => settled(true));
    }
    class ExtendableEvent extends EventCtor {
        constructor(type, eventInitDict = undefined) {
            if (arguments.length < 1) throw new TypeErrorCtor("ExtendableEvent requires a type");
            type = string(type);
            const dict = dictionary(eventInitDict), base = baseInit(dict);
            super(type, base);
            remember(this);
        }
        waitUntil(f) {
            state(this);
            if (arguments.length < 1) throw new TypeErrorCtor("waitUntil requires a promise");
            addLifetime(this, resolve(f));
        }
    }
    class FetchEvent extends ExtendableEvent {
        constructor(type, eventInitDict) {
            if (arguments.length < 2) throw new TypeErrorCtor("FetchEvent requires a type and init");
            type = string(type);
            const dict = dictionary(eventInitDict), base = baseInit(dict);
            // Inherited EventInit members, then FetchEventInit's members in
            // lexicographic order (Web IDL dictionary conversion).
            const clientValue = dict.clientId;
            const clientId = clientValue === undefined ? "" : string(clientValue);
            const handledValue = dict.handled;
            const handled = handledValue === undefined ? null : resolve(handledValue);
            const preloadValue = dict.preloadResponse;
            const preloadResponse = resolve(preloadValue);
            const replacesValue = dict.replacesClientId;
            const replacesClientId = replacesValue === undefined ? "" : string(replacesValue);
            const request = dict.request;
            if (!fetchAPI.isRequest(request)) throw new TypeErrorCtor("FetchEventInit.request must be a Request");
            const resultingValue = dict.resultingClientId;
            const resultingClientId = resultingValue === undefined ? "" : string(resultingValue);
            super(type, base);
            const own = state(this);
            let handledResolve, handledReject;
            const defaultHandled = handled === null ? new PromiseCtor((yes,no)=>{handledResolve=yes;handledReject=no;}) : handled;
            own.fetch = {request,clientId,replacesClientId,resultingClientId,preloadResponse,
                handled:defaultHandled,handledResolve,handledReject,responded:false};
        }
        get request() { return state(this, true).fetch.request; }
        get clientId() { return state(this, true).fetch.clientId; }
        get resultingClientId() { return state(this, true).fetch.resultingClientId; }
        get replacesClientId() { return state(this, true).fetch.replacesClientId; }
        get preloadResponse() { return state(this, true).fetch.preloadResponse; }
        get handled() { return state(this, true).fetch.handled; }
        respondWith(r) {
            const own = state(this, true), fetch = own.fetch;
            if (arguments.length < 1) throw new TypeErrorCtor("respondWith requires a response promise");
            const promise = resolve(r);
            if (!adapter.dispatching(this) || fetch.responded)
                throw new DOMExceptionCtor("respondWith must be called once during dispatch", "InvalidStateError");
            addLifetime(this, promise);
            apply(stop, this, []);
            fetch.responded = true;
            react(promise, response => {
                if (!fetchAPI.isResponse(response) || fetchAPI.bodyUnusable(fetchAPI.response(response))) {
                    adapter.response(own.id, null);
                    return;
                }
                adapter.response(own.id, response);
            }, () => adapter.response(own.id, null));
        }
    }
    function messagePorts(value) {
        if(value===undefined)return freeze([]);
        if(!object(value))throw new TypeErrorCtor("ports must be a sequence");
        const method=value[iterator];
        if(typeof method!=="function")throw new TypeErrorCtor("ports must be iterable");
        const iter=apply(method,value,[]);
        if(!object(iter))throw new TypeErrorCtor("Invalid ports iterator");
        const next=iter.next, result=[];
        for(;;){
            const step=apply(next,iter,[]);
            if(!object(step))throw new TypeErrorCtor("Invalid ports iterator result");
            if(step.done)return freeze(result);
            const port=step.value;
            if(!adapter.isMessagePort(port))throw new TypeErrorCtor("ports must contain MessagePorts");
            define(result,result.length,{value:port,writable:true,enumerable:true,configurable:true});
        }
    }
    function messageState(event) {
        const own=state(event);
        if(!own.message)throw new TypeErrorCtor("Illegal ExtendableMessageEvent invocation");
        return own.message;
    }
    class ExtendableMessageEvent extends ExtendableEvent {
        constructor(type,eventInitDict=undefined){
            if(arguments.length<1)throw new TypeErrorCtor("Event type is required");
            type=string(type);
            const dict=dictionary(eventInitDict),base=baseInit(dict);
            const dataValue=dict.data,data=dataValue===undefined?null:dataValue;
            const lastValue=dict.lastEventId,lastEventId=lastValue===undefined?"":string(lastValue);
            const originValue=dict.origin,origin=originValue===undefined?"":apply(usv,string(originValue),[]);
            const ports=messagePorts(dict.ports);
            const sourceValue=dict.source,source=sourceValue==null?null:sourceValue;
            if(source!==null&&!adapter.isMessageSource(source))throw new TypeErrorCtor("Invalid message source");
            super(type,base);
            state(this).message={data,lastEventId,origin,ports,source};
        }
        get data(){return messageState(this).data;}
        get lastEventId(){return messageState(this).lastEventId;}
        get origin(){return messageState(this).origin;}
        get ports(){return messageState(this).ports;}
        get source(){return messageState(this).source;}
    }
    function cookieList(value) {
        if(value===undefined)return freeze([]);
        if(!object(value))throw new TypeErrorCtor("CookieList must be a sequence");
        const method=value[iterator];
        if(typeof method!=="function")throw new TypeErrorCtor("CookieList must be iterable");
        const iter=apply(method,value,[]);
        if(!object(iter))throw new TypeErrorCtor("Invalid iterator");
        const next=iter.next, result=[];
        for(;;){
            const step=apply(next,iter,[]);
            if(!object(step))throw new TypeErrorCtor("Invalid iterator result");
            if(step.done)return freeze(result);
            const dict=dictionary(step.value), item={};
            const name=dict.name;if(name!==undefined)define(item,"name",{value:apply(usv,string(name),[]),writable:true,enumerable:true,configurable:true});
            const value=dict.value;if(value!==undefined)define(item,"value",{value:apply(usv,string(value),[]),writable:true,enumerable:true,configurable:true});
            define(result,result.length,{value:item,writable:true,enumerable:true,configurable:true});
        }
    }
    class ExtendableCookieChangeEvent extends ExtendableEvent {
        constructor(type,eventInitDict=undefined){
            if(arguments.length<1)throw new TypeErrorCtor("Event type is required");
            type=string(type);
            const dict=dictionary(eventInitDict),base=baseInit(dict);
            const changed=cookieList(dict.changed),deleted=cookieList(dict.deleted);
            super(type,base);
            state(this).cookies={changed,deleted};
        }
        get changed(){const own=state(this);if(!own.cookies)throw new TypeErrorCtor("Illegal cookie event invocation");return own.cookies.changed;}
        get deleted(){const own=state(this);if(!own.cookies)throw new TypeErrorCtor("Illegal cookie event invocation");return own.cookies.deleted;}
    }
    for (const C of [ExtendableEvent, FetchEvent, ExtendableCookieChangeEvent, ExtendableMessageEvent]) {
        const names = Object.getOwnPropertyNames(C.prototype);
        for (let i = 0; i < names.length; i++) if (names[i] !== "constructor") define(C.prototype,names[i],{enumerable:true});
        define(C.prototype,Symbol.toStringTag,{value:C.name,configurable:true});
        define(g,C.name,{value:C,writable:true,configurable:true});
    }
    adapter.bind({
        dispatch(id, type, init) {
            const C = type === "fetch" ? FetchEvent : type === "cookiechange" ? ExtendableCookieChangeEvent : type === "message" || type === "messageerror" ? ExtendableMessageEvent : ExtendableEvent;
            const event = adapter.createTrusted(C, type, init);
            const own = state(event);
            own.id = id;
            own.complete = ok => adapter.complete(id, ok);
            live[id] = event;
            adapter.dispatch(event);
            if (own.fetch !== null && !own.fetch.responded) adapter.fallback(id,apply(canceled,event,[]));
            finish(event, own);
        },
        handled(id, ok) {
            const event = live[id];
            if (!event) return;
            const own = state(event), fetch = own.fetch;
            if (fetch && fetch.handledResolve) {
                if (ok) fetch.handledResolve(undefined);
                else fetch.handledReject(new DOMExceptionCtor("The fetch could not be handled", "NetworkError"));
                fetch.handledResolve = fetch.handledReject = null;
            }
            // waitUntil promises independently retain their event through
            // their reactions, even after the fetch's handled promise settles.
            delete live[id];
        },
        timeout(id) {
            const event = live[id];
            if (!event) return;
            const own = state(event);
            own.timedOut = true;
            own.failed = true;
            if (!own.finished && own.complete !== null) own.complete(false);
            own.finished = true;
            delete live[id];
        },
    });
})(globalThis);
