(function () {
    var g = globalThis;
    var portAPI;
    const bitmapTasks = [];
    // Internal slots of worker platform objects (shared with the platform
    // blocks): never author-visible properties of the objects themselves.
    const internalSlotMap = __platform_slots("internals", new WeakMap());
    function internalsFor(object) {
        let record = Reflect.apply(WeakMap.prototype.get, internalSlotMap, [object]);
        if (record === undefined) {
            record = Object.create(null);
            if ((typeof object === "object" && object !== null) || typeof object === "function")
                Reflect.apply(WeakMap.prototype.set, internalSlotMap, [object, record]);
        }
        return record;
    }
    // Tasks queued by the shared platform features (permissions, storage,
    // notifications): one FIFO selected by the worker event loop.
    const platformTasks = [];
    var cfg = g.__worker_cfg || { id: 0, name: "", type: "classic", url: "about:blank", language: "en-US", languages: ["en-US", "en"], hwc: 8 };
    function errStr(where, e) {
        try { return where + ": " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : ""); }
        catch (_) { return where + ": exception could not be formatted"; }
    }

    // --- the real-time event-loop core (driven by the Rust worker thread) ---
    var WK = {
        timers: [], ids: new Set(), nextId: 1, nowMs: 0, activeNesting: 0, errors: [],
        now: function () { return this.nowMs; },
        advanceClock: function (now) { this.nowMs = Math.max(this.nowMs, now); },
        nextDeadline: function () {
            var min = null;
            for (var i = 0; i < this.timers.length; i++) { var a = this.timers[i].at; if (min === null || a < min) min = a; }
            return min;
        },
        tick: function (realNow) {
            this.nowMs = realNow;
            // HTML's event loop runs one timer task and then performs a
            // microtask checkpoint before selecting the next task. Returning
            // after the oldest due timer lets the Rust loop preserve that
            // boundary instead of batching every due timer into one task.
            var due = null;
            for (var i = 0; i < this.timers.length; i++) {
                var candidate = this.timers[i];
                if (candidate.at <= this.nowMs && (!due || candidate.at < due.at)) due = candidate;
            }
            if (!due) return false;
            this.timers.splice(this.timers.indexOf(due), 1);
            var previousNesting = this.activeNesting;
            this.activeNesting = due.nesting;
            try { due.fn.apply(g, due.args); } catch (e) { this.errors.push(errStr("Uncaught", e)); }
            finally { this.activeNesting = previousNesting; }
            if (due.interval && this.ids.has(due.id)) {
                addTimer(due.fn, due.timeout, due.args, true, due.id, due.nesting);
            } else {
                this.ids.delete(due.id);
            }
            return true;
        },
        message: function (s) {
            var packet;
            try { packet = portAPI.deserialize(JSON.parse(s)); }
            catch (e) {
                if (!cfg.shared) fireScope("messageerror", trustedScopeEvent(MessageEvent, "messageerror", {}));
                return;
            }
            if (cfg.shared) {
                // HTML #dom-sharedworker step "enqueue to the shared worker
                // manager" and #run-a-worker: each SharedWorker connection
                // delivers its entangled inside port in a connect event.
                var port = packet.ports[0];
                if (!port) return;
                fireScope("connect", trustedScopeEvent(MessageEvent, "connect", {
                    data: "", origin: "", ports: Object.freeze([port]), source: port
                }));
                return;
            }
            fireScope("message", trustedScopeEvent(MessageEvent, "message", {
                data: packet.data, origin: "", ports: Object.freeze(packet.ports)
            }));
        },
        installPorts: function (api) { portAPI = api; },
        hasPlatformTask: function () { return platformTasks.length > 0; },
        runPlatformTask: function () {
            if (!platformTasks.length) return false;
            try { platformTasks.shift()(); } catch (e) { this.errors.push(errStr("Uncaught", e)); }
            return true;
        },
        hasBitmapTask: function () { return bitmapTasks.length > 0; },
        runBitmapTask: function () { if (!bitmapTasks.length) return false; bitmapTasks.shift()(); return true; },
        hasPortTask: function () { return portAPI.hasTask(); },
        runPortTask: function () { return portAPI.runTask(); },
        takeErrors: function () { var e = this.errors; this.errors = []; return e.join("\u001e"); }
    };
    // The worker loop's control object stays private: the native host and the
    // shared platform blocks reach it through the host-rooted WeakMap keyed by
    // this WorkerGlobalScope, never through a global property.
    Reflect.apply(WeakMap.prototype.set, __platform_slots("controls", new WeakMap()), [g, WK]);
    // WorkerGlobalScope has no print(); remove the engine's Test262 hook.
    delete g.print;
    // HTML Timers "timer initialization steps" apply to both Window and
    // WorkerGlobalScope: TimerHandler accepts a Function or a DOMString. Convert
    // string handlers when scheduled, then compile their classic script in the
    // worker realm when the timer task runs. Web IDL ToString rejects Symbols.
    function prepareTimerHandler(handler) {
        if (typeof handler === "function") return handler;
        if (typeof handler === "symbol") throw new TypeError("Cannot convert a Symbol value to a string");
        var source = String(handler);
        return function () { return (0, eval)(source); };
    }
    function timerTimeout(value) {
        var number = Number(value);
        if (!Number.isFinite(number) || number === 0) return 0;
        number = Math.trunc(number);
        number = ((number % 4294967296) + 4294967296) % 4294967296;
        if (number >= 2147483648) number -= 4294967296;
        return Math.max(0, number);
    }
    function timerDelay(timeout, parentNesting) {
        return parentNesting > 5 && timeout < 4 ? 4 : timeout;
    }
    function addTimer(fn, timeout, args, interval, previousId, parentNesting) {
        fn = prepareTimerHandler(fn);
        timeout = timerTimeout(timeout);
        parentNesting = parentNesting === undefined ? WK.activeNesting : parentNesting;
        var id = previousId === undefined ? WK.nextId++ : previousId;
        WK.ids.add(id);
        WK.timers.push({ id: id, at: WK.nowMs + timerDelay(timeout, parentNesting), timeout: timeout,
                         fn: fn, args: args || [], interval: !!interval, nesting: parentNesting + 1 });
        return id;
    }
    function removeTimer(id) {
        id = Number(id) | 0;
        WK.ids.delete(id);
        WK.timers = WK.timers.filter(function (timer) { return timer.id !== id; });
    }

    // DOM #concept-event-dispatch / #concept-event-listener-inner-invoke:
    // worker targets have no parent, but still invoke capture listeners before
    // non-capture listeners, both AT_TARGET, and clean up dispatch state.
    var LS = new Map();
    var targetListeners = new WeakMap();
    const scopeEventSlots = new WeakMap();
    const scopeEventGet = WeakMap.prototype.get.bind(scopeEventSlots);
    const scopeEventSet = WeakMap.prototype.set.bind(scopeEventSlots);
    const scopeDefine = Object.defineProperty;
    function scopeEventState(event) {
        const state = scopeEventGet(event);
        if (!state) throw new TypeError('Illegal Event invocation');
        return state;
    }
    function lsFor(type, target) {
        var map = LS;
        if (target && target !== g) {
            map = targetListeners.get(target);
            if (!map) throw new TypeError('Illegal EventTarget invocation');
        }
        var l = map.get(type); if (!l) { l = []; map.set(type, l); } return l;
    }
    // (fn, capture) lookup via NATIVE indexOf over the parallel `l.fns`/`l.caps`
    // arrays — same perf invariant as the page realm's `lsFind`: an interpreted
    // per-entry scan goes quadratic under a listener-flooding script.
    function lsFind(l, fn, capture) {
        if (!l.fns) return -1;
        var i = l.fns.indexOf(fn);
        while (i >= 0 && l.caps[i] !== capture) i = l.fns.indexOf(fn, i + 1);
        return i;
    }
    g.addEventListener = function (type, fn, options) {
        // Web IDL callback interfaces retain the object without reading
        // handleEvent until invocation; its getter may change or throw.
        var target = this || g, t = `${type}`, l = lsFor(t, target);
        if (fn != null && typeof fn !== "function" && typeof fn !== "object") throw new TypeError('Expected EventListener');
        var o = options && (typeof options === "object" || typeof options === "function") ? options : {capture:!!options};
        var capture = !!o.capture, once = !!o.once, passive = !!o.passive, signal = o.signal;
        if (fn == null || (signal && signal.aborted)) return;
        if (lsFind(l, fn, capture) >= 0) return;
        var entry = { fn: fn, capture: capture, once: once, passive:passive, removed: false };
        if (!l.fns) { l.fns = []; l.caps = []; }
        l.push(entry); l.fns.push(fn); l.caps.push(entry.capture);
        if (signal && typeof signal.addEventListener === "function") {
            signal.addEventListener("abort", function () { g.removeEventListener.call(target, t, fn, { capture: entry.capture }); }, { once: true });
        }
    };
    g.removeEventListener = function (type, fn, options) {
        var l = lsFor(`${type}`, this);
        if (fn != null && typeof fn !== "function" && typeof fn !== "object") throw new TypeError('Expected EventListener');
        var capture = options && (typeof options === "object" || typeof options === "function") ? !!options.capture : !!options;
        var i = lsFind(l, fn, capture);
        if (i < 0) return;
        l[i].removed = true;
        l.splice(i, 1); l.fns.splice(i, 1); l.caps.splice(i, 1);
    };
    function dispatchScopeEvent(ev, preserveTrusted, target) {
        target = target || g;
        var state = scopeEventState(ev), l = lsFor(state.type, target);
        if (state.dispatching) throw new g.DOMException('Event is already being dispatched','InvalidStateError');
        if (!preserveTrusted) state.isTrusted = false;
        state.dispatching = true; state.target = target; state.currentTarget = target; state.eventPhase = 2;
        try {
            for (var phase = 0; phase < 2 && !state.stopped; phase++) {
                // Clone separately for each invocation, as DOM requires. A
                // listener added during capture may run during the next phase.
                var snap = l.slice();
                for (var i = 0; i < snap.length; i++) {
                    var entry = snap[i];
                    if (entry.removed || entry.capture !== (phase === 0)) continue;
                    if (entry.once) removeListener.call(target, state.type, entry.fn, {capture:entry.capture});
                    state.passive = entry.passive;
                    try { (typeof entry.fn === "function") ? entry.fn.call(target, ev) : entry.fn.handleEvent(ev); }
                    catch (e) { WK.errors.push(errStr(state.type + " handler", e)); }
                    finally { state.passive = false; }
                    if (state.immediate) break;
                }
            }
        } finally {
            state.currentTarget = null; state.eventPhase = 0;
            state.dispatching = false; state.stopped = false; state.immediate = false;
        }
        return !state.defaultPrevented;
    }
    g.dispatchEvent = function (ev) { return dispatchScopeEvent(ev, false, this); };
    function fireScope(type, ev) {
        dispatchScopeEvent(ev, true);
    }

    // --- Event / MessageEvent / ErrorEvent ---
    function initializeScopeEvent(event, type, init, count) {
        if (!count) throw new TypeError('Missing event type');
        type = `${type}`;
        if (init != null && typeof init !== 'object' && typeof init !== 'function') throw new TypeError('Expected EventInit dictionary');
        init = init || {};
        const bubbles = !!init.bubbles, cancelable = !!init.cancelable, composed = !!init.composed;
        scopeEventSet(event, {type,bubbles,cancelable,composed,defaultPrevented:false,
            target:null,currentTarget:null,eventPhase:0,isTrusted:false,
            timeStamp:perfFloor(perfClock()*10)/10-perfOrigin,
            dispatching:false,stopped:false,immediate:false,passive:false});
        scopeDefine(event, "isTrusted", {
            configurable: false, enumerable: true,
            get: function () { return scopeEventState(this).isTrusted; }
        });
    }
    function Event(type, init) {
        if (!new.target) throw new TypeError('Event requires construction');
        initializeScopeEvent(this,type,init,arguments.length);
    }
    for (const name of ['type','target','currentTarget','eventPhase','bubbles','cancelable','defaultPrevented','composed','timeStamp']) {
        const get = {get() { return scopeEventState(this)[name]; }}.get;
        scopeDefine(get,'name',{value:'get '+name,configurable:true});
        scopeDefine(Event.prototype,name,{get,enumerable:true,configurable:true});
    }
    Object.assign(Event.prototype, {
        preventDefault() { const state=scopeEventState(this); if(state.cancelable && !state.passive)state.defaultPrevented=true; },
        stopPropagation() { scopeEventState(this).stopped=true; },
        stopImmediatePropagation() { const state=scopeEventState(this); state.stopped=true;state.immediate=true; },
        composedPath() { const state=scopeEventState(this); return state.dispatching ? [state.target] : []; },
        initEvent(type, bubbles=false, cancelable=false) {
            const state=scopeEventState(this);
            if(!arguments.length)throw new TypeError('Missing event type');
            type=`${type}`; bubbles=!!bubbles;cancelable=!!cancelable;
            if(state.dispatching)return;
            state.type=type;state.bubbles=bubbles;state.cancelable=cancelable;state.target=null;
            state.stopped=false;state.immediate=false;state.defaultPrevented=false;state.isTrusted=false;
        }
    });
    Object.defineProperties(Event.prototype, {
        srcElement:{get() {return scopeEventState(this).target;},enumerable:true,configurable:true},
        cancelBubble:{get() {return scopeEventState(this).stopped;},set(value) {const state=scopeEventState(this);if(value)state.stopped=true;},enumerable:true,configurable:true},
        returnValue:{get() {return !scopeEventState(this).defaultPrevented;},set(value) {const state=scopeEventState(this);if(!value && state.cancelable && !state.passive)state.defaultPrevented=true;},enumerable:true,configurable:true}
    });
    for (const [name,value] of [['NONE',0],['CAPTURING_PHASE',1],['AT_TARGET',2],['BUBBLING_PHASE',3]]) {
        scopeDefine(Event,name,{value,enumerable:true}); scopeDefine(Event.prototype,name,{value,enumerable:true});
    }
    function MessageEvent(type, init) { if(!new.target)throw new TypeError('MessageEvent requires construction'); initializeScopeEvent(this,type,init,arguments.length); init = init || {}; this.data = init.data; this.origin = init.origin || ""; this.lastEventId = init.lastEventId || ""; this.source = init.source || null; this.ports = init.ports || []; }
    MessageEvent.prototype = Object.create(Event.prototype);
    function ErrorEvent(type, init) { if(!new.target)throw new TypeError('ErrorEvent requires construction'); initializeScopeEvent(this,type,init,arguments.length); init = init || {}; this.message = init.message || ""; this.filename = init.filename || ""; this.lineno = init.lineno || 0; this.colno = init.colno || 0; this.error = init.error || null; }
    ErrorEvent.prototype = Object.create(Event.prototype);
    g.Event = Event; g.MessageEvent = MessageEvent; g.ErrorEvent = ErrorEvent;
    g.CustomEvent = function CustomEvent(type, init) { if(!new.target)throw new TypeError('CustomEvent requires construction'); initializeScopeEvent(this,type,init,arguments.length); this.detail = (init && init.detail !== undefined) ? init.detail : null; };
    g.CustomEvent.prototype = Object.create(Event.prototype);
    for (const C of [Event,MessageEvent,ErrorEvent,g.CustomEvent]) {
        scopeDefine(C,'length',{value:1,configurable:true});
        scopeDefine(C.prototype,'constructor',{value:C,writable:true,configurable:true});
        scopeDefine(C.prototype,Symbol.toStringTag,{value:C.name,configurable:true});
    }

    function trustedScopeEvent(C, type, init) {
        var ev = new C(type, init);
        scopeEventState(ev).isTrusted = true;
        return ev;
    }

    // Event-handler IDL attributes participate in the same listener list, at
    // the point where a non-null callback is assigned. This preserves the DOM
    // event listener registration order relative to addEventListener().
    // SharedWorkerGlobalScope has onconnect instead of DedicatedWorkerGlobalScope's
    // onmessage/onmessageerror (HTML #shared-workers-and-the-sharedworkerglobalscope-interface).
    (cfg.shared ? ["connect", "error"] : ["message", "messageerror", "error"]).forEach(function (type) {
        var callback = null, wrapper = null;
        Object.defineProperty(g, "on" + type, {
            configurable: true, enumerable: true,
            get: function () { return callback; },
            set: function (value) {
                if (wrapper) g.removeEventListener(type, wrapper);
                callback = (typeof value === "function" || (value && typeof value.handleEvent === "function")) ? value : null;
                wrapper = callback && function (event) {
                    return typeof callback === "function" ? callback.call(g, event) : callback.handleEvent(event);
                };
                if (wrapper) g.addEventListener(type, wrapper);
            }
        });
    });

    if (!g.DOMException) { g.DOMException = function (message, name) { var e = new Error(message || ""); e.name = name || "Error"; return e; }; }

    const addListener = g.addEventListener, removeListener = g.removeEventListener;
    g.EventTarget = class EventTarget {
        constructor() { targetListeners.set(this,new Map()); }
        addEventListener(type, callback, options) { addListener.call(this, type, callback, options); }
        removeEventListener(type, callback, options) { removeListener.call(this, type, callback, options); }
        dispatchEvent(event) { return dispatchScopeEvent(event, false, this); }
    };
    scopeDefine(g.EventTarget.prototype,Symbol.toStringTag,{value:'EventTarget',configurable:true});
    for (const name of ['addEventListener','removeEventListener','dispatchEvent']) {
        const descriptor=Object.getOwnPropertyDescriptor(g.EventTarget.prototype,name);
        descriptor.enumerable=true;scopeDefine(g.EventTarget.prototype,name,descriptor);
        scopeDefine(descriptor.value,'length',{value:name==='dispatchEvent' ? 1 : 2,configurable:true});
    }
    // Resource Timing fires at the worker's Performance EventTarget. The
    // shared implementation receives only the private flat-target adapter,
    // never a Window or a fabricated Document.
    g.__performance_adapter = {
        config: cfg,
        add(target, type, callback, options) { addListener.call(target,type,callback,options); },
        remove(target, type, callback, options) { removeListener.call(target,type,callback,options); },
        bufferFull(target) {
            dispatchScopeEvent(trustedScopeEvent(Event,'resourcetimingbufferfull',{}),true,target);
        }
    };
    g.__bitmap_adapter = { queue(fn) { bitmapTasks.push(fn); }, source() { return undefined; } };
    g.__port_adapter = {
        slots: new WeakMap(), EventTarget: g.EventTarget,
        add(target, type, callback) { addListener.call(target, type, callback); },
        remove(target, type, callback) { removeListener.call(target, type, callback); },
        deliver(target, type, init) { dispatchScopeEvent(trustedScopeEvent(MessageEvent, type, init), true, target); }
    };

    // --- self / postMessage / close / on* (DedicatedWorkerGlobalScope) ---
    g.self = g;
    g.name = cfg.name || "";
    if (!cfg.shared) g.postMessage = function (message, options) {
        if (arguments.length === 0) throw new TypeError("postMessage requires a message");
        __worker_self_post(JSON.stringify(portAPI.serialize(message, portAPI.optionsTransfer(options))));
    };
    g.close = function () { __worker_self_close(); };
    if (cfg.shared) {
        // A shared worker's global is a SharedWorkerGlobalScope, which scripts
        // use to tell the two worker kinds apart. Its members stay own
        // properties of the global; only the interface chain is installed.
        const illegal = function () { throw new TypeError("Illegal constructor"); };
        const WorkerGlobalScope = function WorkerGlobalScope() { illegal(); };
        const SharedWorkerGlobalScope = function SharedWorkerGlobalScope() { illegal(); };
        Object.setPrototypeOf(WorkerGlobalScope, g.EventTarget);
        Object.setPrototypeOf(WorkerGlobalScope.prototype, g.EventTarget.prototype);
        Object.setPrototypeOf(SharedWorkerGlobalScope, WorkerGlobalScope);
        Object.setPrototypeOf(SharedWorkerGlobalScope.prototype, WorkerGlobalScope.prototype);
        for (const [C, tag] of [[WorkerGlobalScope, "WorkerGlobalScope"], [SharedWorkerGlobalScope, "SharedWorkerGlobalScope"]]) {
            Object.defineProperty(C, "prototype", {writable: false});
            Object.defineProperty(C.prototype, Symbol.toStringTag, {value: tag, configurable: true});
            Object.defineProperty(g, tag, {value: C, writable: true, configurable: true});
        }
        Object.setPrototypeOf(g, SharedWorkerGlobalScope.prototype);
    }

    // --- timers / microtasks / performance ---
    g.setTimeout = function (fn, delay) { return addTimer(fn, delay, Array.prototype.slice.call(arguments, 2), false); };
    g.setInterval = function (fn, delay) { return addTimer(fn, delay, Array.prototype.slice.call(arguments, 2), true); };
    g.clearTimeout = function (id) { removeTimer(id); };
    g.clearInterval = function (id) { removeTimer(id); };
    g.queueMicrotask = function (fn) { Promise.resolve().then(function () { try { fn(); } catch (e) { WK.errors.push(errStr("queueMicrotask", e)); } }); };
    // HR-Time #now-method / #dfn-coarsen-time: use the shared native monotonic
    // clock, independently of Date's integral milliseconds or author overrides.
    // HTML #run-a-worker records the origin before the worker prelude runs.
    var perfClock = g.__clock_now, perfFloor = Math.floor;
    delete g.__clock_now;
    var perfOrigin = perfFloor((cfg.timeOrigin === undefined ? perfClock() : cfg.timeOrigin) * 10) / 10;
    g.performance = Object.assign(new g.EventTarget(), {
        now: function () { return perfFloor(perfClock() * 10) / 10 - perfOrigin; },
        timeOrigin: perfOrigin
    });

    // --- console: a worker's console isn't surfaced; no-op (never throws) ---
    var noop = function () {};
    // Console §clear: no visible action when the environment has no clearable console.
    g.console = { log: noop, info: noop, warn: noop, error: noop, debug: noop, trace: noop, dir: noop, clear() {}, assert: noop, group: noop, groupCollapsed: noop, groupEnd: noop, table: noop, count: noop, time: noop, timeEnd: noop };

    // --- location (WorkerLocation) ---
    var lp = __url_parse(cfg.url, null) || [cfg.url, "", "", "", "", "/", "", "", "", "", ""];
    g.location = { href: lp[0], protocol: lp[1], host: lp[2], hostname: lp[3], port: lp[4], pathname: lp[5], search: lp[6], hash: lp[7], origin: lp[8], toString: function () { return lp[0]; } };
    // Secure Contexts §1.3: a dedicated worker inherits the owner's secure
    // context when its script URL is potentially trustworthy. Rust computes
    // that relationship, including Blob URL inheritance, at construction.
    Object.defineProperty(g, "isSecureContext", {
        configurable: true, enumerable: true, value: !!cfg.secureContext, writable: false,
    });
    // HTML #dom-crossoriginisolated: the settings object's cross-origin
    // isolated capability. TRust does not yet isolate agent clusters with
    // COOP/COEP, so the host never sets cfg.crossOriginIsolated outside
    // tests. HTML's realm creation steps delete SharedArrayBuffer from the
    // globals of an agent cluster that is not cross-origin isolated.
    const crossOriginIsolatedCapability = cfg.crossOriginIsolated === true;
    Object.defineProperty(g, "crossOriginIsolated", {
        configurable: true, enumerable: true,
        get: Object.getOwnPropertyDescriptor({
            get crossOriginIsolated() { return crossOriginIsolatedCapability; },
        }, "crossOriginIsolated").get,
    });
    if (!crossOriginIsolatedCapability) delete g.SharedArrayBuffer;

    // --- navigator (WorkerNavigator), the same honest values as the page ---
    // WHATWG HTML §NavigatorLanguage: languages is a stable FrozenArray and
    // language is its first (most-preferred) entry.
    var navigatorLanguages = Object.freeze((cfg.languages || ["en-US", "en"]).slice());
    // GPC §3.2–§3.4: WorkerNavigator exposes the same top-level preference.
    var navigatorGpc = cfg.globalPrivacyControl !== false;
    g.navigator = {
        userAgent: cfg.ua || "TRust/0.1", appName: "Netscape", appCodeName: "Mozilla", product: "Gecko", productSub: "20100101",
        platform: "Linux", vendor: "", vendorSub: "", language: cfg.language || navigatorLanguages[0], languages: navigatorLanguages, onLine: true,
        hardwareConcurrency: cfg.hwc || 8, maxTouchPoints: 0
    };
    Object.defineProperty(g.navigator, "globalPrivacyControl", {
        configurable: true, enumerable: true,
        get: function () { return navigatorGpc; }
    });
    // The shared Permissions block and the features built on it run in this
    // WorkerGlobalScope through this bootstrap-only adapter (see the Window's).
    g.__feature_adapter = {
        EventTarget: g.EventTarget, window: false,
        add(target, type, fn) { addListener.call(target, type, fn); },
        remove(target, type, fn) { removeListener.call(target, type, fn); },
        fire(target, type) { dispatchScopeEvent(trustedScopeEvent(Event, type, {}), true, target); },
        queue(fn) { platformTasks.push(fn); },
        report(error) { WK.errors.push(errStr("callback", error)); },
        origin() { return lp[8] || "null"; },
        baseURL() { return lp[0]; },
        secure: !!cfg.secureContext,
        context: 0,
    };

    // --- atob / btoa ---
    var B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    g.btoa = function (s) {
        s = String(s); var out = "", i = 0;
        while (i < s.length) {
            var c1 = s.charCodeAt(i++), c2 = s.charCodeAt(i++), c3 = s.charCodeAt(i++);
            var e1 = c1 >> 2, e2 = ((c1 & 3) << 4) | (c2 >> 4), e3 = ((c2 & 15) << 2) | (c3 >> 6), e4 = c3 & 63;
            if (isNaN(c2)) { e3 = 64; e4 = 64; } else if (isNaN(c3)) { e4 = 64; }
            out += B64.charAt(e1) + B64.charAt(e2) + (e3 === 64 ? "=" : B64.charAt(e3)) + (e4 === 64 ? "=" : B64.charAt(e4));
        }
        return out;
    };
    g.atob = function (s) {
        // Strict forgiving-base64 (Infra §4.5), matching the page realm.
        s = String(s).replace(/[\t\n\f\r ]+/g, "");
        if (s.length % 4 === 0) s = s.replace(/={1,2}$/, "");
        if (s.length % 4 === 1) throw new g.DOMException("Failed to execute 'atob': The string to be decoded is not correctly encoded.", "InvalidCharacterError");
        var out = "", i = 0, bits = 0, acc = 0;
        while (i < s.length) {
            var idx = B64.indexOf(s.charAt(i++));
            if (idx < 0) throw new g.DOMException("Failed to execute 'atob': The string to be decoded is not correctly encoded.", "InvalidCharacterError");
            acc = (acc << 6) | idx; bits += 6;
            if (bits >= 8) { bits -= 8; out += String.fromCharCode((acc >> bits) & 0xFF); }
        }
        return out;
    };

    // --- TextEncoder / TextDecoder (UTF-8) ---
    function TextEncoder() {}
    TextEncoder.prototype.encoding = "utf-8";
    TextEncoder.prototype.encode = function (s) {
        return __text_encode(String(s === undefined ? "" : s));
    };
    TextEncoder.prototype.encodeInto = function (s, destination) {
        s = String(s === undefined ? "" : s);
        if (!(destination instanceof Uint8Array)) throw new TypeError("TextEncoder.encodeInto destination must be a Uint8Array");
        var read = 0, written = 0;
        while (read < s.length) {
            var first = s.charCodeAt(read), cp = first, units = 1;
            if (first >= 0xd800 && first <= 0xdbff) {
                var second = read + 1 < s.length ? s.charCodeAt(read + 1) : 0;
                if (second >= 0xdc00 && second <= 0xdfff) {
                    cp = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                    units = 2;
                } else cp = 0xfffd;
            } else if (first >= 0xdc00 && first <= 0xdfff) cp = 0xfffd;
            var needed = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
            if (written + needed > destination.byteLength) break;
            if (needed === 1) destination[written++] = cp;
            else if (needed === 2) {
                destination[written++] = 0xc0 | (cp >> 6);
                destination[written++] = 0x80 | (cp & 0x3f);
            } else if (needed === 3) {
                destination[written++] = 0xe0 | (cp >> 12);
                destination[written++] = 0x80 | ((cp >> 6) & 0x3f);
                destination[written++] = 0x80 | (cp & 0x3f);
            } else {
                destination[written++] = 0xf0 | (cp >> 18);
                destination[written++] = 0x80 | ((cp >> 12) & 0x3f);
                destination[written++] = 0x80 | ((cp >> 6) & 0x3f);
                destination[written++] = 0x80 | (cp & 0x3f);
            }
            read += units;
        }
        return { read: read, written: written };
    };
    function TextDecoder(label) { this.encoding = (label || "utf-8").toLowerCase(); this.fatal = false; }
    TextDecoder.prototype.decode = function (buf) {
        if (!buf) return "";
        var bytes = (buf instanceof Uint8Array) ? buf : (buf.buffer ? new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength) : new Uint8Array(buf));
        var out = "", i = 0;
        while (i < bytes.length) {
            var c = bytes[i++];
            if (c < 0x80) out += String.fromCharCode(c);
            else if (c < 0xE0) out += String.fromCharCode(((c & 0x1F) << 6) | (bytes[i++] & 0x3F));
            else if (c < 0xF0) out += String.fromCharCode(((c & 0x0F) << 12) | ((bytes[i++] & 0x3F) << 6) | (bytes[i++] & 0x3F));
            else { var cp = ((c & 0x07) << 18) | ((bytes[i++] & 0x3F) << 12) | ((bytes[i++] & 0x3F) << 6) | (bytes[i++] & 0x3F); cp -= 0x10000; out += String.fromCharCode(0xD800 + (cp >> 10), 0xDC00 + (cp & 0x3FF)); }
        }
        return out;
    };
    g.TextEncoder = TextEncoder; g.TextDecoder = TextDecoder;

    // --- crypto ---
    g.crypto = {
        getRandomValues: function (a) { for (var i = 0; i < a.length; i++) a[i] = Math.floor(Math.random() * 4294967296); return a; },
        randomUUID: function () { var h = ""; for (var i = 0; i < 36; i++) { if (i === 8 || i === 13 || i === 18 || i === 23) h += "-"; else if (i === 14) h += "4"; else if (i === 19) h += (8 + Math.floor(Math.random() * 4)).toString(16); else h += Math.floor(Math.random() * 16).toString(16); } return h; }
    };

    // --- Blob / File (string-backed, like the page engine's) ---
    function Blob(parts, opts) {
        internalsFor(this).parts = Array.isArray(parts) ? parts.slice() : (parts != null ? [parts] : []);
        opts = opts || {}; this.type = opts.type || "";
        var size = 0; for (var i = 0; i < internalsFor(this).parts.length; i++) { var p = internalsFor(this).parts[i]; size += (typeof p === "string") ? p.length : ((p && p.byteLength) || 0); }
        this.size = size;
    }
    // Byte-faithful reads via __blobBytes/__blobText (hoisted, defined with the
    // blob-URL store below) — a structured-clone-delivered Blob arrives with
    // Uint8Array parts, which the old string-parts-only text()/arrayBuffer()
    // read as empty.
    Blob.prototype.text = function () { return Promise.resolve(__blobText(__blobBytes(this))); };
    Blob.prototype.slice = function (start, end, contentType) {
        var bytes = __blobBytes(this), size = bytes.length;
        var s = start === undefined ? 0 : Math.trunc(+start) || 0;
        var e = end === undefined ? size : Math.trunc(+end) || 0;
        s = s < 0 ? Math.max(size + s, 0) : Math.min(s, size);
        e = e < 0 ? Math.max(size + e, 0) : Math.min(e, size);
        var span = Math.max(e - s, 0), part = bytes.slice(s, s + span);
        var u = new Uint8Array(part.length);
        for (var i = 0; i < part.length; i++) u[i] = part.charCodeAt(i) & 0xFF;
        return new Blob([u], { type: contentType === undefined ? "" : String(contentType).toLowerCase() });
    };
    Blob.prototype.arrayBuffer = function () { var t = __blobBytes(this), b = new Uint8Array(t.length); for (var i = 0; i < t.length; i++) b[i] = t.charCodeAt(i) & 0xFF; return Promise.resolve(b.buffer); };
    function File(parts, name, opts) { Blob.call(this, parts, opts); this.name = String(name); this.lastModified = (opts && opts.lastModified) || Date.now(); }
    File.prototype = Object.create(Blob.prototype);
    g.Blob = Blob; g.File = File;

    // --- structuredClone (in-realm, via the shared codec) ---
    g.structuredClone = function (v, options) { return portAPI.deserialize(portAPI.serialize(v, options && options.transfer)).data; };

    // --- URLSearchParams / URL (over the __url_parse syscall) ---
    function URLSearchParams(init) {
        internalsFor(this).l = [];
        if (typeof init === "string") { var s = init.charAt(0) === "?" ? init.slice(1) : init; if (s) s.split("&").forEach(function (pair) { var eq = pair.indexOf("="); var k = eq < 0 ? pair : pair.slice(0, eq); var v = eq < 0 ? "" : pair.slice(eq + 1); internalsFor(this).l.push([decodeURIComponent(k.replace(/\+/g, " ")), decodeURIComponent(v.replace(/\+/g, " "))]); }, this); }
        else if (init && typeof init.forEach === "function") { init.forEach(function (v, k) { internalsFor(this).l.push([String(k), String(v)]); }, this); }
        else if (init && typeof init === "object") { for (var key in init) if (Object.prototype.hasOwnProperty.call(init, key)) internalsFor(this).l.push([key, String(init[key])]); }
    }
    URLSearchParams.prototype.get = function (k) { for (var i = 0; i < internalsFor(this).l.length; i++) if (internalsFor(this).l[i][0] === k) return internalsFor(this).l[i][1]; return null; };
    URLSearchParams.prototype.getAll = function (k) { var r = []; for (var i = 0; i < internalsFor(this).l.length; i++) if (internalsFor(this).l[i][0] === k) r.push(internalsFor(this).l[i][1]); return r; };
    URLSearchParams.prototype.has = function (k) { return this.get(k) !== null; };
    URLSearchParams.prototype.set = function (k, v) { var done = false; for (var i = internalsFor(this).l.length - 1; i >= 0; i--) if (internalsFor(this).l[i][0] === k) { if (done) internalsFor(this).l.splice(i, 1); else { internalsFor(this).l[i][1] = String(v); done = true; } } if (!done) internalsFor(this).l.push([k, String(v)]); searchParamsNotify(this); };
    URLSearchParams.prototype.append = function (k, v) { internalsFor(this).l.push([String(k), String(v)]); searchParamsNotify(this); };
    URLSearchParams.prototype["delete"] = function (k) { for (var i = internalsFor(this).l.length - 1; i >= 0; i--) if (internalsFor(this).l[i][0] === k) internalsFor(this).l.splice(i, 1); searchParamsNotify(this); };
    URLSearchParams.prototype.forEach = function (cb, t) { for (var i = 0; i < internalsFor(this).l.length; i++) cb.call(t, internalsFor(this).l[i][1], internalsFor(this).l[i][0], this); };
    // application/x-www-form-urlencoded byte serializer (URL Standard): space→"+",
    // percent-encode `! ' ( ) ~` that encodeURIComponent leaves bare. Mirrors the
    // page realm's `fenc`.
    function __fenc(s) { return encodeURIComponent(String(s)).replace(/[!'()~]/g, function (c) { return "%" + c.charCodeAt(0).toString(16).toUpperCase(); }).replace(/%20/g, "+"); }
    URLSearchParams.prototype.toString = function () { return internalsFor(this).l.map(function (p) { return __fenc(p[0]) + "=" + __fenc(p[1]); }).join("&"); };
    // Live binding to an owning URL, mirroring the page realm (see its URL/USP).
    function searchParamsNotify(params) { if (internalsFor(params).url) urlSetSearchFromParams(internalsFor(params).url, params.toString()); }
    function searchParamsSetList(params, query) { internalsFor(params).l = []; var s = String(query).charAt(0) === "?" ? String(query).slice(1) : String(query); if (s) s.split("&").forEach(function (pair) { var eq = pair.indexOf("="); var k = eq < 0 ? pair : pair.slice(0, eq); var v = eq < 0 ? "" : pair.slice(eq + 1); internalsFor(this).l.push([decodeURIComponent(k.replace(/\+/g, " ")), decodeURIComponent(v.replace(/\+/g, " "))]); }, params); }
    // A live URL: assigning a component re-serializes href via __url_set (the
    // url crate's WHATWG setters), exactly like the page realm's class version.
    function URL(url, base) {
        var p = __url_parse(String(url), base != null ? String(base) : null);
        if (!p) throw new TypeError("Invalid URL: " + url);
        internalsFor(this).p = p; internalsFor(this).sp = null;
    }
    // URL Standard #dom-url-parse / #dom-url-canparse, exposed in workers too.
    const urlWellFormed = Function.prototype.call.bind(String.prototype.toWellFormed);
    URL.parse = function parse(url, base = undefined) {
        if (!arguments.length) throw new TypeError("URL.parse requires a URL");
        var parts = __url_parse(urlWellFormed(`${url}`), base === undefined ? null : urlWellFormed(`${base}`));
        if (!parts) return null;
        var result = Object.create(URL.prototype);
        internalsFor(result).p = parts; internalsFor(result).sp = null;
        return result;
    };
    URL.canParse = function canParse(url, base = undefined) {
        if (!arguments.length) throw new TypeError("URL.canParse requires a URL");
        return __url_parse(urlWellFormed(`${url}`), base === undefined ? null : urlWellFormed(`${base}`)) !== null;
    };
    function urlAccessor(i, which) {
        return which
            ? { get: function () { return internalsFor(this).p[i]; }, set: function (v) { var r = __url_set(internalsFor(this).p[0], which, String(v)); if (r) internalsFor(this).p = r; } }
            : { get: function () { return internalsFor(this).p[i]; } };
    }
    Object.defineProperties(URL.prototype, {
        href: { get: function () { return internalsFor(this).p[0]; }, set: function (v) { var r = __url_parse(String(v), null); if (!r) throw new TypeError("Invalid URL: " + v); internalsFor(this).p = r; if (internalsFor(this).sp) searchParamsSetList(internalsFor(this).sp, internalsFor(this).p[6]); } },
        protocol: urlAccessor(1, "protocol"),
        host: urlAccessor(2, "host"),
        hostname: urlAccessor(3, "hostname"),
        port: urlAccessor(4, "port"),
        pathname: urlAccessor(5, "pathname"),
        search: { get: function () { return internalsFor(this).p[6]; }, set: function (v) { var r = __url_set(internalsFor(this).p[0], "search", String(v)); if (r) internalsFor(this).p = r; if (internalsFor(this).sp) searchParamsSetList(internalsFor(this).sp, internalsFor(this).p[6]); } },
        hash: urlAccessor(7, "hash"),
        origin: urlAccessor(8),
        username: urlAccessor(9, "username"),
        password: urlAccessor(10, "password"),
        searchParams: { get: function () { if (!internalsFor(this).sp) { internalsFor(this).sp = new URLSearchParams(internalsFor(this).p[6]); internalsFor(internalsFor(this).sp).url = this; } return internalsFor(this).sp; } },
    });
    function urlSetSearchFromParams(url, qs) { var r = __url_set(internalsFor(url).p[0], "search", qs); if (r) internalsFor(url).p = r; }
    URL.prototype.toString = function () { return internalsFor(this).p[0]; };
    URL.prototype.toJSON = function () { return internalsFor(this).p[0]; };
    g.URL = URL; g.URLSearchParams = URLSearchParams;

    // --- Blob URL store (worker realm) — RAM-only, mirrors the page realm ---
    var __blobURLStore = Object.create(null);
    function __blobBytes(b) {
        if (!b || !Array.isArray(internalsFor(b).parts)) return "";
        var enc = new g.TextEncoder(), out = "";
        for (var i = 0; i < internalsFor(b).parts.length; i++) {
            var p = internalsFor(b).parts[i], v, j;
            if (typeof p === "string") { v = enc.encode(p); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
            else if (p instanceof ArrayBuffer) { v = new Uint8Array(p); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
            else if (p && typeof p.byteLength === "number" && p.buffer) { v = new Uint8Array(p.buffer, p.byteOffset || 0, p.byteLength); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
            else if (p && Array.isArray(internalsFor(p).parts)) out += __blobBytes(p);
            else if (p != null) { v = enc.encode(String(p)); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
        }
        return out;
    }
    function __blobText(bytes) { var u = new Uint8Array(bytes.length); for (var i = 0; i < bytes.length; i++) u[i] = bytes.charCodeAt(i) & 0xFF; return new g.TextDecoder().decode(u); }
    function __resolveBlobURL(u) {
        var h = u.indexOf("#"), key = h >= 0 ? u.slice(0, h) : u, obj = __blobURLStore[key];
        if (!obj) return null;
        if (Array.isArray(internalsFor(obj).parts)) return { bytes: __blobBytes(obj), type: obj.type || "" };
        return { bytes: "", type: "" };
    }
    URL.createObjectURL = function (obj) {
        if (obj === null || typeof obj !== "object") throw new TypeError("Failed to execute 'createObjectURL' on 'URL': Overload resolution failed.");
        var origin = (g.location && g.location.origin) || "null";
        var u = "blob:" + (origin || "null") + "/" + g.crypto.randomUUID();
        __blobURLStore[u] = obj; return u;
    };
    URL.revokeObjectURL = function (u) {
        u = String(u); var h = u.indexOf("#"); if (h >= 0) u = u.slice(0, h);
        if (u.slice(0, 5) === "blob:") delete __blobURLStore[u];
    };

    // HTML "fetch a classic worker-imported script" uses Fetch's script
    // destination response check. Keep the legacy JavaScript MIME types from
    // MIME Sniffing; with `nosniff`, anything else is a network error.
    var __jsMimes = Object.create(null);
    for (var __jm of ["application/ecmascript", "application/javascript", "application/x-ecmascript", "application/x-javascript", "text/ecmascript", "text/javascript", "text/javascript1.0", "text/javascript1.1", "text/javascript1.2", "text/javascript1.3", "text/javascript1.4", "text/javascript1.5", "text/jscript", "text/livescript", "text/x-ecmascript", "text/x-javascript"]) __jsMimes[__jm] = true;
    function __classicScriptResponseOK(r) {
        if (!r || r[0] < 200 || r[0] >= 300) return false;
        var lines = String(r[4] || "").split("\n"), nosniff = false;
        for (var i = 0; i + 1 < lines.length; i += 2) {
            if (lines[i].toLowerCase() === "x-content-type-options") {
                nosniff = lines[i + 1].split(",", 1)[0].trim().toLowerCase() === "nosniff";
                break;
            }
        }
        var essence = String(r[1] || "").split(";", 1)[0].trim().toLowerCase();
        return !nosniff || !!__jsMimes[essence];
    }

    // --- importScripts (classic, synchronous fetch + global eval) ---
    g.importScripts = function () {
        // HTML §10.2.1.1 exposes the method in both worker kinds, but the
        // imported-classic-script algorithm throws in a module worker.
        if (cfg.type === "module") throw new TypeError("importScripts() is unavailable in a module worker");
        for (var i = 0; i < arguments.length; i++) {
            var u = String(arguments[i]);
            if (u.slice(0, 5) === "blob:") {
                var be = __resolveBlobURL(u);
                if (!be) throw new Error("importScripts failed: " + u + " (no blob URL entry)");
                (0, eval)(__blobText(be.bytes));
                continue;
            }
            var rp = __url_parse(u, g.location.href); if (rp) u = rp[0];
            var r = __http_fetch(u, "GET", null, null, "");
            if (!__classicScriptResponseOK(r)) throw new Error("importScripts failed: " + u + " (" + (r ? r[0] : "network error") + ")");
            (0, eval)(r[2]);
        }
    };

    // Fetch #fetch-method returns before network I/O. The host queues response
    // processing on this worker's event loop; importScripts remains synchronous.
    // Fetch #concept-body-consume-body / #dom-body-arraybuffer. The native
    // response tuple stores bytes independently of its optional decoded text.
    // Binary responses intentionally have no text field; never reconstruct
    // binary content from that field or UTF-8-decode it before arrayBuffer().
    const copyBodyBuffer = __body_buffer;
    function makeResponse(status, ctype, body, url, headers) {
        let buffer;
        if (typeof body === "string") {
            const bytes = new Uint8Array(body.length);
            for (let i = 0; i < body.length; i++) bytes[i] = body.charCodeAt(i) & 0xff;
            buffer = bytes.buffer;
        } else {
            buffer = body;
        }
        let used = false;
        function consume(convert) {
            if (used) return Promise.reject(new TypeError("Body is unusable"));
            used = true;
            return Promise.resolve().then(() => convert(copyBodyBuffer(buffer)));
        }
        return {
            ok: status >= 200 && status < 300, status, statusText: "", url, redirected: false, type: "basic",
            get bodyUsed() { return used; },
            headers: new g.Headers(headers || (ctype ? {'content-type':ctype} : {})),
            text() { return consume(bytes => new g.TextDecoder().decode(bytes)); },
            json() { return consume(bytes => JSON.parse(new g.TextDecoder().decode(bytes))); },
            arrayBuffer() { return consume(bytes => bytes); },
            bytes() { return consume(bytes => new Uint8Array(bytes)); },
            blob() { return consume(bytes => new Blob([bytes], {type:ctype || ""})); },
            clone() {
                if (used) throw new TypeError("Body is unusable");
                return makeResponse(status, ctype, buffer, url, headers);
            }
        };
    }
    g.fetch = function (input, init) {
        try {
            init = init || {};
            var url = (input && input.url) ? input.url : String(input);
            if (url.slice(0, 5) === "blob:") {
                var be = __resolveBlobURL(url);
                if (!be) return Promise.reject(new TypeError("Failed to fetch: " + url));
                return Promise.resolve(makeResponse(200, be.type || null, be.bytes, url));
            }
            var rp = __url_parse(url, g.location.href);
            if (!rp) throw new TypeError("Invalid fetch URL: " + url);
            url = rp[0];
            var method = String(init.method || (input && input.method) || "GET").toUpperCase();
            // Use the same header and native BufferSource boundary as Window fetch.
            const headers = new g.Headers(init.headers !== undefined ? init.headers : input && input.headers);
            const value = init.body !== undefined ? init.body : input && input.body;
            let body = null, ctype = null;
            if (value != null) {
                if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) body = copyBodyBuffer(value);
                else if (value instanceof Blob) { body = __blobBytes(value); ctype = value.type || null; }
                else {
                    body = new g.TextEncoder().encode(String(value));
                    ctype = value instanceof URLSearchParams ? 'application/x-www-form-urlencoded;charset=UTF-8' : 'text/plain;charset=UTF-8';
                }
            }
            if (headers.has('content-type')) ctype = headers.get('content-type');
            let headerWire = '';
            headers.forEach((value, key) => { headerWire += (headerWire ? '\n' : '') + key + '\n' + value; });
            const mode = init.mode || (input && input.mode) || 'cors';
            const credentials = init.credentials || (input && input.credentials) || 'same-origin';
            if (!['cors','no-cors','same-origin'].includes(mode) || !['omit','same-origin','include'].includes(credentials))
                throw new TypeError('Invalid fetch mode or credentials');
            return __http_fetch_async(url, method, body, ctype, headerWire, mode, credentials, 'fetch').then(r => {
                if (!r) throw new TypeError("Failed to fetch: " + url);
                const responseHeaders = new g.Headers();
                const lines = String(r[4] || '').split('\n');
                for (let i = 0; i + 1 < lines.length; i += 2) responseHeaders.append(lines[i], lines[i + 1]);
                return makeResponse(r[0], r[1], r[3], url, responseHeaders);
            });
        } catch (error) { return Promise.reject(error); }
    };
})();
