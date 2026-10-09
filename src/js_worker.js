// The engine's own (ECMAScript) globals, before any platform interface.
globalThis.__engine_global_names = Object.getOwnPropertyNames(globalThis);
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
    let indexedDBTasks = null, databaseTurn = false;
    var cfg = g.__worker_cfg || { id: 0, name: "", type: "classic", url: "about:blank", language: "en-US", languages: ["en-US", "en"], hwc: 8 };
    function errStr(where, e) {
        try { return where + ": " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : ""); }
        catch (_) { return where + ": exception could not be formatted"; }
    }

    // --- the real-time event-loop core (driven by the Rust worker thread) ---
    var WK = {
        timers: [], ids: new Set(), nextId: 1, nowMs: 0, clockAt: null, activeNesting: 0, errors: [],
        now: function () { return this.nowMs; },
        // The loop sets the clock when a task starts; currentTime() adds the
        // time the task has run since, so a timer started mid-task waits its
        // whole timeout (HTML #timer-initialisation-steps "startTime").
        advanceClock: function (now) { this.nowMs = Math.max(this.nowMs, now); this.clockAt = perfClock(); },
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
            try { due.fn.apply(g, due.args); } catch (e) { reportException(e); }
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
        // The shared platform features' FIFO and, alternating with it, the
        // database tasks the host's Indexed Database coordinator queued here.
        hasPlatformTask: function () {
            return platformTasks.length > 0 || (indexedDBTasks !== null && indexedDBTasks.has());
        },
        runPlatformTask: function () {
            databaseTurn = !databaseTurn;
            if (databaseTurn && indexedDBTasks !== null && indexedDBTasks.has()) {
                try { indexedDBTasks.run(); } catch (e) { this.errors.push(errStr("Uncaught", e)); }
                return true;
            }
            if (!platformTasks.length) {
                if (indexedDBTasks === null || !indexedDBTasks.has()) return false;
                try { indexedDBTasks.run(); } catch (e) { this.errors.push(errStr("Uncaught", e)); }
                return true;
            }
            try { platformTasks.shift()(); } catch (e) { this.errors.push(errStr("Uncaught", e)); }
            return true;
        },
        hasBitmapTask: function () { return bitmapTasks.length > 0; },
        runBitmapTask: function () { if (!bitmapTasks.length) return false; bitmapTasks.shift()(); return true; },
        hasPortTask: function () { return portAPI.hasTask(); },
        runPortTask: function () { return portAPI.runTask(); },
        takeErrors: function () { var e = this.errors; this.errors = []; return e.join("\u001e"); },
        // Console lines for the owner's developer console, drained after each task.
        consoleLines: [],
        takeConsole: function () { var lines = this.consoleLines; this.consoleLines = []; return lines.join("\u001e"); },
        // HTML #notify-about-rejected-promises (the shared rejection block).
        notifyRejectedPromises: function (list) { if (rejectionAPI) rejectionAPI.notify(list); }
    };
    var rejectionAPI = null;
    var workerClosing = false;
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
        // HTML #timer-initialisation-steps: create and run a classic script
        // (a Script, not indirect eval code), with the API base URL.
        return function () { __classic_script_run(source, lp[0]); };
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
        WK.timers.push({ id: id, at: currentTime() + timerDelay(timeout, parentNesting), timeout: timeout,
                         fn: fn, args: args || [], interval: !!interval, nesting: parentNesting + 1 });
        return id;
    }
    // The worker's current time on the loop's clock.
    function currentTime() {
        return WK.clockAt === null ? WK.nowMs : WK.nowMs + Math.max(0, perfClock() - WK.clockAt);
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
    // The platform's listener operations, captured before author code runs.
    const scopeAddListener = g.addEventListener, scopeRemoveListener = g.removeEventListener;
    // DOM #concept-event-dispatch with EventTarget's "get the parent": worker
    // targets have no parent, except that Indexed Database defines request ->
    // transaction -> connection (IndexedDB 3 §2.7.1 and §2.8). Kept internal so
    // author properties cannot rewrite an event path.
    const scopeEventParents = new WeakMap();
    // DOM's legacy listener-exception output, used only by Indexed Database.
    const listenerExceptionEvents = new WeakSet();
    // #concept-event-listener-invoke for one path entry and phase: 0 runs
    // capture listeners, 1 the others (at target both, in that order).
    function invokeScopeListeners(state, ev, current, capture) {
        // Clone separately for each invocation, as DOM requires. A listener
        // added during capture may run during the next phase.
        var snap = lsFor(state.type, current).slice();
        for (var i = 0; i < snap.length; i++) {
            var entry = snap[i];
            if (entry.removed || entry.capture !== capture) continue;
            if (entry.once) removeListener.call(current, state.type, entry.fn, {capture:entry.capture});
            state.passive = entry.passive;
            try { (typeof entry.fn === "function") ? entry.fn.call(current, ev) : entry.fn.handleEvent(ev); }
            // DOM #concept-event-listener-inner-invoke reports the exception.
            catch (e) { listenerExceptionEvents.add(ev); reportException(e, state.type + " handler"); }
            finally { state.passive = false; }
            if (state.immediate) break;
        }
    }
    function dispatchScopeEvent(ev, preserveTrusted, target) {
        target = target || g;
        var state = scopeEventState(ev);
        lsFor(state.type, target);
        if (state.dispatching) throw new g.DOMException('Event is already being dispatched','InvalidStateError');
        if (!preserveTrusted) state.isTrusted = false;
        listenerExceptionEvents.delete(ev);
        var path = [target], parent = scopeEventParents.get(target);
        while (parent) { path.push(parent); parent = scopeEventParents.get(parent); }
        state.dispatching = true; state.target = target; state.path = path;
        try {
            // Capturing: from the root down to the target's parent.
            state.eventPhase = 1;
            for (var c = path.length - 1; c >= 1 && !state.stopped; c--) {
                state.currentTarget = path[c];
                invokeScopeListeners(state, ev, path[c], true);
            }
            // At target: capture listeners, then the others.
            state.eventPhase = 2; state.currentTarget = target;
            for (var phase = 0; phase < 2 && !state.stopped; phase++)
                invokeScopeListeners(state, ev, target, phase === 0);
            // Bubbling: up the path for a bubbling event.
            if (state.bubbles) {
                state.eventPhase = 3;
                for (var b = 1; b < path.length && !state.stopped; b++) {
                    state.currentTarget = path[b];
                    invokeScopeListeners(state, ev, path[b], false);
                }
            }
        } finally {
            state.currentTarget = null; state.eventPhase = 0; state.path = null;
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
        composedPath() { const state=scopeEventState(this); return state.dispatching && state.path ? state.path.slice() : []; },
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
    g.Event = Event;
    g.CustomEvent = function CustomEvent(type, init) { if(!new.target)throw new TypeError('CustomEvent requires construction'); initializeScopeEvent(this,type,init,arguments.length); this.detail = (init && init.detail !== undefined) ? init.detail : null; };
    g.CustomEvent.prototype = Object.create(Event.prototype);
    // MessageEvent, ErrorEvent and CloseEvent: the shared HTML_EVENTS block.
    // A worker has no WindowProxy; its MessageEventSource is a MessagePort.
    let messagePortBrand = null;
    g.__html_events_ports = function (brand) { messagePortBrand = brand; };
    g.__html_events_adapter = {
        Event,
        isMessageEventSource(value) { return !!(messagePortBrand && messagePortBrand(value)); },
        isMessagePort(value) { return !!(messagePortBrand && messagePortBrand(value)); },
        dispatching(event) { return scopeEventState(event).dispatching; },
    };
    /*__HTML_EVENTS_SHARED__*/
    const htmlEventsAPI = g.__html_events_api;
    delete g.__html_events_api;
    const MessageEvent = htmlEventsAPI.MessageEvent, ErrorEvent = htmlEventsAPI.ErrorEvent;
    const CloseEvent = htmlEventsAPI.CloseEvent;
    for (const C of [Event,g.CustomEvent]) {
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
    // WorkerGlobalScope adds onerror, onlanguagechange, onoffline, ononline,
    // onrejectionhandled and onunhandledrejection (HTML #the-workerglobalscope-common-interface).
    (cfg.shared ? ["connect"] : ["message", "messageerror"]).concat(["error", "languagechange", "offline", "online",
        "rejectionhandled", "unhandledrejection"]).forEach(function (type) {
        // HTML #event-handler-attributes: [LegacyTreatNonObjectAsNull]
        // EventHandler; activation appends one listener that keeps its place.
        // The handler's listener is added and removed with the platform's
        // own operations: author code that replaces addEventListener never
        // observes or intercepts an event handler's activation.
        var callback = null, listener = null;
        Object.defineProperty(g, "on" + type, {
            configurable: true, enumerable: true,
            get: function () { return callback; },
            set: function (value) {
                callback = (typeof value === "object" && value !== null) || typeof value === "function" ? value : null;
                if (callback === null) {
                    if (listener) scopeRemoveListener.call(g, type, listener);
                    listener = null;
                } else if (!listener) {
                    listener = function (event) { processEventHandler(callback, event); };
                    scopeAddListener.call(g, type, listener);
                }
            }
        });
    });
    // HTML #the-event-handler-processing-algorithm: an ErrorEvent "error" at
    // the global calls onerror with (message, source, lineno, colno, error),
    // and true cancels it; any other handler cancels by returning false.
    function processEventHandler(callback, event) {
        if (typeof callback !== "function") return;
        var special = event.type === "error" && htmlEventsAPI.isErrorEvent(event);
        var returnValue = special
            ? callback.call(g, event.message, event.filename, event.lineno, event.colno, event.error)
            : callback.call(g, event);
        if (special ? returnValue === true : returnValue === false) event.preventDefault();
    }
    // HTML #report-an-exception for a worker global. An unhandled report of a
    // dedicated worker goes on to the Worker object in its owner (the host
    // forwards the record); a shared worker keeps it (#run-a-worker).
    var errorReportingMode = false;
    function reportException(exception, label) {
        reportErrorInformation(htmlEventsAPI.errorInformation(exception), label, exception);
    }
    // #report-an-exception from its extracted information. A nested worker's
    // unhandled report arrives with omitError set: info.error is null.
    function reportErrorInformation(info, label, exception) {
        var notHandled = true;
        if (!errorReportingMode) {
            errorReportingMode = true;
            try {
                notHandled = dispatchScopeEvent(trustedScopeEvent(ErrorEvent, "error", {cancelable: true,
                    message: info.message, filename: info.filename, lineno: info.lineno, colno: info.colno,
                    error: info.error}), true);
            } finally { errorReportingMode = false; }
        }
        if (!notHandled) return;
        if (cfg.shared) {
            WK.consoleLines.push(label ? errStr(label, exception)
                : info.message + (info.filename ? " at " + info.filename + ":" + info.lineno + ":" + info.colno : ""));
            return;
        }
        // The owner's console also receives this worker's diagnostic line.
        if (label) WK.consoleLines.push(errStr(label, exception));
        WK.errors.push("\u0001error " + JSON.stringify({message: info.message, filename: info.filename,
            lineno: info.lineno, colno: info.colno}));
    }
    WK.reportException = reportException;
    // HTML #dom-reporterror.
    Object.defineProperty(g, "reportError", {writable: true, enumerable: true, configurable: true,
        value: {reportError(e) {
            if (arguments.length < 1) throw new TypeError("reportError requires 1 argument");
            reportException(e);
        }}.reportError});

    // DOMException is the shared Web IDL interface (worker_prelude() runs its block first).

    const addListener = g.addEventListener, removeListener = g.removeEventListener;
    // HTML #event-handler-idl-attributes on platform objects of this worker
    // (Worker, IndexedDB requests, transactions and connections): an
    // activated handler appends one listener that keeps its place, and a
    // false return value cancels the event (#the-event-handler-processing-algorithm).
    const targetHandlerSlots = new WeakMap();
    function installTargetHandlers(proto, types) {
        types.forEach(function (type) {
            var name = "on" + type;
            var accessors = {
                get [name]() {
                    var handlers = targetHandlerSlots.get(this), handler = handlers && handlers.get(type);
                    return handler ? handler.value : null;
                },
                set [name](value) {
                    if (!targetListeners.get(this)) throw new TypeError("Illegal EventTarget invocation");
                    var handlers = targetHandlerSlots.get(this);
                    if (!handlers) { handlers = new Map(); targetHandlerSlots.set(this, handlers); }
                    value = (typeof value === "object" && value !== null) || typeof value === "function" ? value : null;
                    var handler = handlers.get(type), target = this;
                    if (value === null) {
                        if (handler) { removeListener.call(target, type, handler.listener); handlers.delete(type); }
                    } else if (handler) handler.value = value;
                    else {
                        handler = { value: value, listener: function (event) {
                            var callback = handler.value;
                            if (typeof callback === "function" && callback.call(target, event) === false) event.preventDefault();
                        } };
                        handlers.set(type, handler);
                        addListener.call(target, type, handler.listener);
                    }
                }
            };
            var descriptor = Object.getOwnPropertyDescriptor(accessors, name);
            Object.defineProperty(proto, name, { get: descriptor.get, set: descriptor.set, enumerable: true, configurable: true });
        });
    }
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
    // The shared AbortSignal block's view of this worker's events.
    g.__abort_adapter = {
        EventTarget: g.EventTarget,
        add(target, type, callback) { addListener.call(target, type, callback); },
        remove(target, type, callback) { removeListener.call(target, type, callback); },
        fire(target, type) { dispatchScopeEvent(trustedScopeEvent(Event, type, {}), true, target); },
        report(error) { WK.errors.push(errStr("abort algorithm", error)); },
    };
    // createImageBitmap() sources of a worker: the canvas block below
    // recognizes OffscreenCanvas; there are no element sources.
    let canvasBitmapSource = null;
    g.__bitmap_adapter = {
        queue(fn) { bitmapTasks.push(fn); },
        source(image, snapshot) { return canvasBitmapSource ? canvasBitmapSource(image, snapshot) : undefined; }
    };
    // The shared canvas block's view of this worker: OffscreenCanvas events,
    // its blob serialization tasks and, in a dedicated worker, the
    // AnimationFrameProvider whose rendering updates the Rust loop drives.
    g.__canvas_adapter = {
        EventTarget: g.EventTarget, control: WK, shared: !!cfg.shared,
        add(target, type, callback) { addListener.call(target, type, callback); },
        remove(target, type, callback) { removeListener.call(target, type, callback); },
        fire(target, type, cancelable, C, init) {
            const event = trustedScopeEvent(C || Event, type, {...init, cancelable: !!cancelable});
            dispatchScopeEvent(event, true, target);
            return !event.defaultPrevented;
        },
        queue(fn) { bitmapTasks.push(fn); },
        report(error) { reportException(error); },
        origin() { return lp[8] || "null"; },
        now() { return perfFloor(perfClock() * 10) / 10 - perfOrigin; },
        loopTime() { return currentTime(); },
        setBitmapSource(fn) { canvasBitmapSource = fn; }
    };
    g.__port_adapter = {
        slots: new WeakMap(), EventTarget: g.EventTarget,
        add(target, type, callback) { addListener.call(target, type, callback); },
        remove(target, type, callback) { removeListener.call(target, type, callback); },
        origin() { return lp[8] || "null"; },
        // #eligible-for-messaging: a WorkerGlobalScope whose closing flag is false.
        eligible() { return !workerClosing; },
        deliver(target, type, init) { dispatchScopeEvent(trustedScopeEvent(MessageEvent, type, init), true, target); }
    };

    // The shared Worker block's view of this worker (HTML: Worker is
    // [Exposed=(Window,DedicatedWorker,SharedWorker)]): nested dedicated
    // workers are owned by this global; their events are tasks of this loop.
    g.__worker_adapter = {
        control: WK, EventTarget: g.EventTarget, Event, MessageEvent, ErrorEvent,
        fire(target, C, type, init) { return dispatchScopeEvent(trustedScopeEvent(C, type, init), true, target); },
        handlers(proto, types) { installTargetHandlers(proto, types); },
        parseURL(url) { var parsed = __url_parse(url, lp[0]); return parsed ? parsed[0] : null; },
        serialize(message, transfer) { return portAPI.serialize(message, transfer); },
        deserialize(packet) { return portAPI.deserialize(packet); },
        optionsTransfer(options) { return portAPI.optionsTransfer(options); },
        // #report-an-exception for this global with omitError set.
        reportUnhandled(info) {
            reportErrorInformation({message: info.message, filename: info.filename, lineno: info.lineno,
                colno: info.colno, error: null});
        },
        failed(message) { if (WK.consoleLines.length < 100) WK.consoleLines.push("worker: " + message); },
        console(line) { if (WK.consoleLines.length < 100) WK.consoleLines.push("worker: " + line); },
    };

    // --- self / postMessage / close / on* (DedicatedWorkerGlobalScope) ---
    g.self = g;
    g.name = cfg.name || "";
    if (!cfg.shared) g.postMessage = function (message, options) {
        if (arguments.length === 0) throw new TypeError("postMessage requires a message");
        __worker_self_post(JSON.stringify(portAPI.serialize(message, portAPI.optionsTransfer(options))));
    };
    g.close = function () { workerClosing = true; __worker_self_close(); };
    {
        // HTML #the-workerglobalscope-common-interface, #dedicated-workers-and-the-
        // dedicatedworkerglobalscope-interface and #shared-workers-and-the-
        // sharedworkerglobalscope-interface: a worker's global is a
        // DedicatedWorkerGlobalScope or a SharedWorkerGlobalScope, both
        // inheriting WorkerGlobalScope. Scripts (and testharness.js) use these
        // interfaces to tell the worker kinds apart. Their members stay own
        // properties of the global; only the interface chain is installed.
        const illegal = function () { throw new TypeError("Illegal constructor"); };
        const WorkerGlobalScope = function WorkerGlobalScope() { illegal(); };
        const kind = cfg.shared ? "SharedWorkerGlobalScope" : "DedicatedWorkerGlobalScope";
        const KindGlobalScope = { [kind]: function () { illegal(); } }[kind];
        Object.setPrototypeOf(WorkerGlobalScope, g.EventTarget);
        Object.setPrototypeOf(WorkerGlobalScope.prototype, g.EventTarget.prototype);
        Object.setPrototypeOf(KindGlobalScope, WorkerGlobalScope);
        Object.setPrototypeOf(KindGlobalScope.prototype, WorkerGlobalScope.prototype);
        for (const [C, tag] of [[WorkerGlobalScope, "WorkerGlobalScope"], [KindGlobalScope, kind]]) {
            Object.defineProperty(C, "prototype", {writable: false});
            Object.defineProperty(C.prototype, Symbol.toStringTag, {value: tag, configurable: true});
            Object.defineProperty(g, tag, {value: C, writable: true, configurable: true});
        }
        Object.setPrototypeOf(g, KindGlobalScope.prototype);
    }

    // --- timers / microtasks / performance ---
    g.setTimeout = function (fn, delay) { return addTimer(fn, delay, Array.prototype.slice.call(arguments, 2), false); };
    g.setInterval = function (fn, delay) { return addTimer(fn, delay, Array.prototype.slice.call(arguments, 2), true); };
    g.clearTimeout = function (id) { removeTimer(id); };
    g.clearInterval = function (id) { removeTimer(id); };
    g.queueMicrotask = function (fn) { Promise.resolve().then(function () { try { fn(); } catch (e) { reportException(e); } }); };
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

    // --- console: lines reach the owner's developer console after each task ---
    // WHATWG Console #logger: each line is the level and the stringified data;
    // groups indent, counters and timers are per console namespace object.
    var consoleGroupDepth = 0, consoleCounts = new Map(), consoleTimers = new Map();
    function consoleLog(level) {
        return function () {
            var parts = [];
            for (var i = 0; i < arguments.length; i++) {
                try { parts.push(String(arguments[i])); } catch (_) { parts.push("?"); }
            }
            if (WK.consoleLines.length < 100)
                WK.consoleLines.push(level + ": " + "  ".repeat(Math.min(consoleGroupDepth, 64)) + parts.join(" "));
        };
    }
    function consoleLabel(label) { return label === undefined ? "default" : `${label}`; }
    g.console = {
        log: consoleLog("log"), info: consoleLog("info"), warn: consoleLog("warn"), error: consoleLog("error"),
        debug: consoleLog("debug"), trace: consoleLog("trace"), dir: consoleLog("dir"), dirxml: consoleLog("dirxml"),
        table: consoleLog("table"),
        // Console §clear: no visible action without a clearable console; empty the group stack.
        clear() { consoleGroupDepth = 0; },
        assert(condition, ...data) {
            if (condition) return;
            if (data.length === 0 || typeof data[0] !== "string") data.unshift("Assertion failed");
            else data[0] = "Assertion failed: " + data[0];
            consoleLog("error").apply(null, data);
        },
        group(...data) { consoleLog("group").apply(null, data.length ? data : ["console group"]); consoleGroupDepth++; },
        groupCollapsed(...data) { consoleLog("groupCollapsed").apply(null, data.length ? data : ["console group"]); consoleGroupDepth++; },
        groupEnd() { consoleGroupDepth = Math.max(0, consoleGroupDepth - 1); },
        count(label = "default") {
            label = consoleLabel(label);
            var count = (consoleCounts.get(label) || 0) + 1;
            consoleCounts.set(label, count);
            consoleLog("count")(label + ": " + count);
        },
        countReset(label = "default") {
            label = consoleLabel(label);
            if (!consoleCounts.has(label)) { consoleLog("warn")("Count for '" + label + "' does not exist"); return; }
            consoleCounts.set(label, 0);
        },
        time(label = "default") {
            label = consoleLabel(label);
            if (consoleTimers.has(label)) { consoleLog("warn")("Timer '" + label + "' already exists"); return; }
            consoleTimers.set(label, g.performance.now());
        },
        timeLog(label = "default", ...data) {
            label = consoleLabel(label);
            if (!consoleTimers.has(label)) { consoleLog("warn")("Timer '" + label + "' does not exist"); return; }
            consoleLog("timeLog").apply(null, [label + ": " + (g.performance.now() - consoleTimers.get(label)).toFixed(3) + " ms"].concat(data));
        },
        timeEnd(label = "default") {
            label = consoleLabel(label);
            if (!consoleTimers.has(label)) { consoleLog("warn")("Timer '" + label + "' does not exist"); return; }
            consoleLog("timeEnd")(label + ": " + (g.performance.now() - consoleTimers.get(label)).toFixed(3) + " ms");
            consoleTimers.delete(label);
        },
    };
    Object.defineProperty(g, "console", { value: g.console, writable: true, enumerable: false, configurable: true });
    Object.defineProperty(g.console, Symbol.toStringTag, { value: "console", configurable: true });

    // --- location (WorkerLocation) ---
    var lp = __url_parse(cfg.url, null) || [cfg.url, "", "", "", "", "/", "", "", "", "", ""];
    // HTML #worker-locations: WorkerGlobalScope's [SameObject] location is a
    // WorkerLocation whose members reflect the worker's URL; none is writable.
    const workerLocationToken = {};
    class WorkerLocation {
        constructor(token) { if (token !== workerLocationToken) throw new TypeError("Illegal constructor"); }
    }
    const workerLocation = Reflect.construct(WorkerLocation, [workerLocationToken]);
    const workerLocationFields = [["href", 0], ["origin", 8], ["protocol", 1], ["host", 2], ["hostname", 3],
        ["port", 4], ["pathname", 5], ["search", 6], ["hash", 7]];
    workerLocationFields.forEach(function (field) {
        var get = {[field[0]]() {
            if (this !== workerLocation) throw new TypeError("Illegal WorkerLocation invocation");
            return lp[field[1]];
        }}[field[0]];
        Object.defineProperty(get, "name", {value: "get " + field[0], configurable: true});
        Object.defineProperty(WorkerLocation.prototype, field[0], {get: get, enumerable: true, configurable: true});
    });
    // The href stringifier.
    Object.defineProperty(WorkerLocation.prototype, "toString", {writable: true, enumerable: true, configurable: true,
        value: {toString() {
            if (this !== workerLocation) throw new TypeError("Illegal WorkerLocation invocation");
            return lp[0];
        }}.toString});
    Object.defineProperty(WorkerLocation.prototype, Symbol.toStringTag, {value: "WorkerLocation", configurable: true});
    Object.defineProperty(g, "WorkerLocation", {value: WorkerLocation, writable: true, configurable: true});
    Object.defineProperty(g, "location", {get: {location() { return workerLocation; }}.location, enumerable: true, configurable: true});
    // The shared Indexed Database and Cache Storage blocks' view of this
    // worker: its storage key (the origin, opaque for a data: URL), its events
    // with IndexedDB's event parents and its storage codec; its database
    // tasks alternate with the platform task queue. The databases and caches are the origin's, shared with Windows.
    g.__indexeddb_adapter = {
        origin: lp[8] || "null", EventTarget: g.EventTarget, Event,
        setParent(target, parent) { scopeEventParents.set(target, parent); },
        fire(target, C, type, init) {
            var event = trustedScopeEvent(C, type, init);
            dispatchScopeEvent(event, true, target);
            return event;
        },
        listenerThrew(event) { return listenerExceptionEvents.has(event); },
        handlers(proto, types) { installTargetHandlers(proto, types); },
        frame() { return null; },
        runIn(frame, fn) { fn(); },
        serialize(value) { return WK.messageCodec.serialize(value, true); },
        deserialize(wire) { return WK.messageCodec.deserialize(wire); },
        register(api) { indexedDBTasks = api; },
    };
    g.__cache_storage_adapter = { origin: lp[8] || "null", secure: !!cfg.secureContext };
    // Secure Contexts §1.3: a dedicated worker inherits the owner's secure
    // context when its script URL is potentially trustworthy. Rust computes
    // that relationship, including Blob URL inheritance, at construction.
    Object.defineProperty(g, "isSecureContext", {
        configurable: true, enumerable: true, value: !!cfg.secureContext, writable: false,
    });
    // HTML #dom-origin and Web IDL #Replaceable. A worker's origin is opaque
    // for a data: URL and otherwise its owner's (HTML #run-a-worker), which a
    // same-origin or blob: script URL carries.
    (function () {
        var origin = lp[8] || "null";
        var descriptor = {
            configurable: true, enumerable: true,
            get: function () { return origin; },
            set: function (value) {
                Object.defineProperty(g, "origin", { value: value, writable: true, enumerable: true, configurable: true });
            },
        };
        Object.defineProperty(descriptor.get, "name", { value: "get origin", configurable: true });
        Object.defineProperty(descriptor.set, "name", { value: "set origin", configurable: true });
        Object.defineProperty(g, "origin", descriptor);
    })();
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
        report(error) { reportException(error); },
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

    // --- TextEncoder / TextDecoder ---
    // worker_prelude() replaces the marker below with the Window prelude's
    // shared text codec block (js_platform.js, __TEXT_CODEC_BEGIN__).
    /*__TEXT_CODEC_SHARED__*/

    // Crypto, Blob, File, FileReader, FileReaderSync, FormData and the Fetch
    // interfaces come from the shared platform blocks worker_prelude() appends.

    // --- structuredClone (in-realm, via the shared codec) ---
    g.structuredClone = function (v, options) { return portAPI.deserialize(portAPI.serialize(v, options && options.transfer)).data; };

    // --- URLSearchParams / URL ---
    // worker_prelude() replaces the marker below with the Window prelude's
    // shared URL block (js_platform.js, __URL_BEGIN__), which installs both
    // interfaces on this global.
    /*__URL_SHARED__*/
    const URLSearchParams = g.URLSearchParams, URL = g.URL;

    // --- Blob URL store: the page's, in the host (File API #url-model) ---
    // URL.createObjectURL and URL.revokeObjectURL are the shared BLOB_URL
    // block's; this worker's adapter supplies the origin a minted URL
    // serializes and receives the block's resolver.
    let blobURLAPI = null;
    g.__blob_url_adapter = {
        origin() { return lp[8] || "null"; },
        register(api) { blobURLAPI = api; },
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

    // --- importScripts (HTML #import-scripts-into-worker-global-scope) ---
    // Parse every URL first, then fetch each classic worker-imported script
    // synchronously and run it as a classic script with rethrow errors set:
    // its declarations join the global scope and an exception propagates.
    g.importScripts = function () {
        if (cfg.type === "module") throw new TypeError("importScripts() is unavailable in a module worker");
        var urls = [], i;
        for (i = 0; i < arguments.length; i++) urls.push(`${arguments[i]}`.toWellFormed());
        var records = [];
        for (i = 0; i < urls.length; i++) {
            var parsed = __url_parse(urls[i], lp[0]);
            if (!parsed) throw new g.DOMException("importScripts: invalid URL " + urls[i], "SyntaxError");
            records.push(parsed[0]);
        }
        for (i = 0; i < records.length; i++) {
            var url = records[i], source;
            if (url.slice(0, 5) === "blob:") {
                // Fetch #scheme-fetch "blob" for this worker's environment.
                var entry = blobURLAPI.read(url);
                if (!entry) throw new g.DOMException("importScripts failed to load " + url, "NetworkError");
                source = new g.TextDecoder().decode(entry.bytes);
            } else {
                var r = __http_fetch(url, "GET", null, null, "");
                if (!__classicScriptResponseOK(r)) throw new g.DOMException("importScripts failed to load " + url, "NetworkError");
                // #fetch-a-classic-worker-imported-script: UTF-8 decode the body.
                source = r[3] instanceof ArrayBuffer ? new g.TextDecoder().decode(new Uint8Array(r[3])) : String(r[2] || "");
            }
            __classic_script_run(source, url);
        }
    };

    // The shared rejection block's view of this worker: its events, its
    // platform task queue and its developer console.
    g.__rejection_adapter = {
        Event,
        fire(target, C, type, init) { return dispatchScopeEvent(trustedScopeEvent(C, type, init), true, target); },
        queue(fn) { platformTasks.push(fn); },
        report(reason) {
            var text;
            try { text = reason instanceof Error ? reason.name + ": " + reason.message : String(reason); }
            catch (_) { text = "?"; }
            if (WK.consoleLines.length < 100) WK.consoleLines.push("unhandled rejection: " + text);
        },
    };
    g.__rejection_bind = function (api) { rejectionAPI = api; };
    // The shared File API and Fetch blocks' view of this worker: its events
    // and task queue, its API base URL and origin, and its blob URL store. A
    // worker has no forms, so FormData(form) never matches HTMLFormElement.
    g.__file_adapter = {
        Event, EventTarget: g.EventTarget, worker: true,
        add(target, type, callback) { addListener.call(target, type, callback); },
        remove(target, type, callback) { removeListener.call(target, type, callback); },
        fire(target, C, type, init) { dispatchScopeEvent(trustedScopeEvent(C, type, init), true, target); },
        queue(fn) { platformTasks.push(fn); },
        now: Date.now,
        // A selected file's range reaches a worker through the page's blob
        // URL store: the host registered the file for this agent, which reads
        // it lazily, like a Window, failing once its snapshot state changed.
        readHostFile(range) {
            if (range.end <= range.start) return "";
            var bytes = __file_read(range.id, range.start, range.end);
            if (typeof bytes === "string") return bytes;
            throw new g.DOMException(String(bytes[1]), String(bytes[0]));
        },
    };
    g.__fetch_adapter = {
        baseURL() { return lp[0]; },
        origin() { return lp[8] || "null"; },
        resolveBlobURL(url) {
            var entry = blobURLAPI.resolve(url);
            return entry ? entry.object : null;
        },
        queue(fn) { platformTasks.push(fn); },
        formEntries() { throw new TypeError("FormData constructor: argument 1 is not an HTMLFormElement"); },
    };
    // The shared CRYPTO block settles SubtleCrypto promises in tasks on the
    // crypto task source, which this worker's event loop runs.
    g.__crypto_adapter = { queue(fn) { platformTasks.push(fn); } };
    // The shared XHR block's view of this worker: its events, its platform
    // task queue and its timer list. A worker has no Document request bodies
    // and its responseType never becomes "document".
    // The shared WEBSOCKET block's view of this worker: connection events
    // arrive as host tasks through the control object's `wsEvent`.
    g.__websocket_adapter = {
        EventTarget: g.EventTarget, MessageEvent, CloseEvent,
        add(target, type, callback) { addListener.call(target, type, callback); },
        remove(target, type, callback) { removeListener.call(target, type, callback); },
        fire(target, C, type, init) { dispatchScopeEvent(trustedScopeEvent(C || Event, type, init), true, target); },
        baseURL() { return lp[0]; },
        open(url, protocols) { return __ws_open(url, protocols); },
        send(id, data, binary) { return __ws_send(id, data, binary); },
        close(id, code, reason) { __ws_close(id, code, reason); },
        taskQueue() { return function (fn) { platformTasks.push(fn); }; },
        bind(event) { WK.wsEvent = event; },
    };
    const xhrTimerId = {}, XHRURL = g.URL;
    g.__xhr_adapter = {
        EventTarget: g.EventTarget, isWindow: false,
        add(target, type, callback) { addListener.call(target, type, callback); },
        remove(target, type, callback) { removeListener.call(target, type, callback); },
        fire(target, C, type, init) { dispatchScopeEvent(trustedScopeEvent(C || Event, type, init), true, target); },
        hasListeners(target) {
            const listeners = targetListeners.get(target);
            if (!listeners) return false;
            let found = false;
            listeners.forEach(function (list) { if (list.length) found = true; });
            return found;
        },
        taskQueue() { return function (fn) { platformTasks.push(fn); }; },
        // A worker's query encoding is UTF-8; the href, or null.
        parseURL(url) {
            try { return new XHRURL(url, lp[0]).href; } catch (_) { return null; }
        },
        fullyActive() { return true; },
        now() { return currentTime(); },
        wallClock() { return perfClock(); },
        // send()'s timeout wait, on the timer list but outside the
        // setTimeout() identifier space.
        delay(ms, fn) {
            const entry = { id: xhrTimerId, at: currentTime() + ms, timeout: ms, fn: fn, args: [], interval: false, nesting: 0 };
            WK.timers.push(entry);
            return entry;
        },
        cancelDelay(entry) {
            const index = WK.timers.indexOf(entry);
            if (index >= 0) WK.timers.splice(index, 1);
        },
        documentBody() { return null; },
        documentResponse() { return null; },
    };
    // The shared EVENTSOURCE block's view of this worker: its events, its
    // platform task queue (remote event and networking tasks) and its timer
    // list for the reconnection delay.
    g.__eventsource_adapter = {
        EventTarget: g.EventTarget, MessageEvent,
        add(target, type, callback) { addListener.call(target, type, callback); },
        remove(target, type, callback) { removeListener.call(target, type, callback); },
        create(C, type, init) { return trustedScopeEvent(C || Event, type, init); },
        dispatch(target, event) { dispatchScopeEvent(event, true, target); },
        // A worker's query encoding is UTF-8; the href, or null.
        parseURL(url) {
            try { return new XHRURL(url, lp[0]).href; } catch (_) { return null; }
        },
        remoteEventQueue() { return function (fn) { platformTasks.push(fn); }; },
        networkQueue() { return function (fn) { platformTasks.push(fn); }; },
        delay(ms, fn) {
            const entry = { id: xhrTimerId, at: currentTime() + ms, timeout: ms, fn: fn, args: [], interval: false, nesting: 0 };
            WK.timers.push(entry);
            return entry;
        },
        cancelDelay(entry) {
            const index = WK.timers.indexOf(entry);
            if (index >= 0) WK.timers.splice(index, 1);
        },
    };
})();
