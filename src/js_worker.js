(function () {
    var g = globalThis;
    var cfg = g.__worker_cfg || { id: 0, name: "", type: "classic", url: "about:blank", language: "en-US", languages: ["en-US", "en"], hwc: 8 };
    function errStr(where, e) { return where + ": " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : ""); }

    // --- the real-time event-loop core (driven by the Rust worker thread) ---
    var WK = g.__wkr = {
        timers: [], ids: new Set(), nextId: 1, nowMs: 0, activeNesting: 0, errors: [],
        now: function () { return this.nowMs; },
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
            var data;
            try { data = g.__sc_deserialize(s); }
            catch (e) { fireScope("messageerror", trustedScopeEvent(MessageEvent, "messageerror", {})); return; }
            fireScope("message", trustedScopeEvent(MessageEvent, "message", { data: data, origin: "" }));
        },
        takeErrors: function () { var e = this.errors; this.errors = []; return e.join("\u001e"); }
    };
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

    // --- EventTarget on the worker global (options-aware: `once`/`signal`
    // behave per spec, `capture` is stored for removal matching — the worker
    // global is a flat target, so there is no capture PHASE) ---
    var LS = new Map();
    function lsFor(type) { var l = LS.get(type); if (!l) { l = []; LS.set(type, l); } return l; }
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
        if (!(typeof fn === "function" || (fn && typeof fn.handleEvent === "function"))) return;
        var o = options === true ? { capture: true } : (options && typeof options === "object" ? options : {});
        if (o.signal && o.signal.aborted) return;
        var t = String(type), l = lsFor(t);
        if (lsFind(l, fn, !!o.capture) >= 0) return;
        var entry = { fn: fn, capture: !!o.capture, once: !!o.once, removed: false };
        if (!l.fns) { l.fns = []; l.caps = []; }
        l.push(entry); l.fns.push(fn); l.caps.push(entry.capture);
        if (o.signal && typeof o.signal.addEventListener === "function") {
            o.signal.addEventListener("abort", function () { g.removeEventListener(t, fn, { capture: entry.capture }); }, { once: true });
        }
    };
    g.removeEventListener = function (type, fn, options) {
        var capture = options === true || !!(options && options.capture);
        var l = lsFor(String(type));
        var i = lsFind(l, fn, capture);
        if (i < 0) return;
        l[i].removed = true;
        l.splice(i, 1); l.fns.splice(i, 1); l.caps.splice(i, 1);
    };
    var trustedScopeEvents = new WeakSet();
    function dispatchScopeEvent(ev, preserveTrusted) {
        if (!preserveTrusted) trustedScopeEvents.delete(ev);
        ev.target = g; ev.currentTarget = g;
        var l = lsFor(ev.type), snap = l.slice();
        for (var i = 0; i < snap.length; i++) {
            var entry = snap[i];
            if (entry.removed) continue;
            // `once`: remove through removeEventListener so the parallel
            // fns/caps arrays stay aligned with the entry list.
            if (entry.once) g.removeEventListener(ev.type, entry.fn, { capture: entry.capture });
            try { (typeof entry.fn === "function") ? entry.fn.call(g, ev) : entry.fn.handleEvent(ev); }
            catch (e) { WK.errors.push(errStr(ev.type + " handler", e)); }
        }
        return !ev.defaultPrevented;
    }
    g.dispatchEvent = function (ev) { return dispatchScopeEvent(ev, false); };
    function fireScope(type, ev) {
        dispatchScopeEvent(ev, true);
    }

    // --- Event / MessageEvent / ErrorEvent ---
    function Event(type, init) {
        init = init || {}; this.type = String(type); this.bubbles = !!init.bubbles;
        this.cancelable = !!init.cancelable; this.defaultPrevented = false;
        this.target = null; this.currentTarget = null; this.timeStamp = Date.now();
        Object.defineProperty(this, "isTrusted", {
            configurable: false, enumerable: true,
            get: function () { return trustedScopeEvents.has(this); }
        });
    }
    Event.prototype.preventDefault = function () { if (this.cancelable) this.defaultPrevented = true; };
    Event.prototype.stopPropagation = function () {}; Event.prototype.stopImmediatePropagation = function () {};
    function MessageEvent(type, init) { Event.call(this, type, init); init = init || {}; this.data = init.data; this.origin = init.origin || ""; this.lastEventId = init.lastEventId || ""; this.source = init.source || null; this.ports = init.ports || []; }
    MessageEvent.prototype = Object.create(Event.prototype);
    function ErrorEvent(type, init) { Event.call(this, type, init); init = init || {}; this.message = init.message || ""; this.filename = init.filename || ""; this.lineno = init.lineno || 0; this.colno = init.colno || 0; this.error = init.error || null; }
    ErrorEvent.prototype = Object.create(Event.prototype);
    g.Event = Event; g.MessageEvent = MessageEvent; g.ErrorEvent = ErrorEvent;
    g.CustomEvent = function CustomEvent(type, init) { MessageEvent.call(this, type, init); this.detail = (init && init.detail !== undefined) ? init.detail : null; };
    g.CustomEvent.prototype = Object.create(MessageEvent.prototype);

    function trustedScopeEvent(C, type, init) {
        var ev = new C(type, init);
        trustedScopeEvents.add(ev);
        return ev;
    }

    // Event-handler IDL attributes participate in the same listener list, at
    // the point where a non-null callback is assigned. This preserves the DOM
    // event listener registration order relative to addEventListener().
    ["message", "messageerror", "error"].forEach(function (type) {
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

    // --- self / postMessage / close / on* (DedicatedWorkerGlobalScope) ---
    g.self = g;
    g.name = cfg.name || "";
    g.postMessage = function (message, transfer) { __worker_self_post(g.__sc_serialize(message)); };
    g.close = function () { __worker_self_close(); };

    // --- timers / microtasks / performance ---
    g.setTimeout = function (fn, delay) { return addTimer(fn, delay, Array.prototype.slice.call(arguments, 2), false); };
    g.setInterval = function (fn, delay) { return addTimer(fn, delay, Array.prototype.slice.call(arguments, 2), true); };
    g.clearTimeout = function (id) { removeTimer(id); };
    g.clearInterval = function (id) { removeTimer(id); };
    g.queueMicrotask = function (fn) { Promise.resolve().then(function () { try { fn(); } catch (e) { WK.errors.push(errStr("queueMicrotask", e)); } }); };
    var perfOrigin = Date.now();
    g.performance = { now: function () { return Date.now() - perfOrigin; }, timeOrigin: perfOrigin };

    // --- console: a worker's console isn't surfaced; no-op (never throws) ---
    var noop = function () {};
    g.console = { log: noop, info: noop, warn: noop, error: noop, debug: noop, trace: noop, dir: noop, assert: noop, group: noop, groupCollapsed: noop, groupEnd: noop, table: noop, count: noop, time: noop, timeEnd: noop };

    // --- location (WorkerLocation) ---
    var lp = __url_parse(cfg.url, null) || [cfg.url, "", "", "", "", "/", "", "", "", "", ""];
    g.location = { href: lp[0], protocol: lp[1], host: lp[2], hostname: lp[3], port: lp[4], pathname: lp[5], search: lp[6], hash: lp[7], origin: lp[8], toString: function () { return lp[0]; } };
    // Secure Contexts §1.3: a dedicated worker inherits the owner's secure
    // context when its script URL is potentially trustworthy. Rust computes
    // that relationship, including Blob URL inheritance, at construction.
    Object.defineProperty(g, "isSecureContext", {
        configurable: true, enumerable: true, value: !!cfg.secureContext, writable: false,
    });

    // --- navigator (WorkerNavigator), the same honest values as the page ---
    // WHATWG HTML §NavigatorLanguage: languages is a stable FrozenArray and
    // language is its first (most-preferred) entry.
    var navigatorLanguages = Object.freeze((cfg.languages || ["en-US", "en"]).slice());
    // GPC §3.2–§3.4: WorkerNavigator exposes the same top-level preference.
    var navigatorGpc = cfg.globalPrivacyControl !== false;
    g.navigator = {
        userAgent: "TRust/0.1", appName: "Netscape", appCodeName: "Mozilla", product: "Gecko", productSub: "20100101",
        platform: "Linux", vendor: "", vendorSub: "", language: cfg.language || navigatorLanguages[0], languages: navigatorLanguages, onLine: true,
        hardwareConcurrency: cfg.hwc || 8, maxTouchPoints: 0
    };
    Object.defineProperty(g.navigator, "globalPrivacyControl", {
        configurable: true, enumerable: true,
        get: function () { return navigatorGpc; }
    });

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
        this.__parts = Array.isArray(parts) ? parts.slice() : (parts != null ? [parts] : []);
        opts = opts || {}; this.type = opts.type || "";
        var size = 0; for (var i = 0; i < this.__parts.length; i++) { var p = this.__parts[i]; size += (typeof p === "string") ? p.length : ((p && p.byteLength) || 0); }
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
    g.structuredClone = function (v) { return g.__sc_deserialize(g.__sc_serialize(v)); };

    // --- URLSearchParams / URL (over the __url_parse syscall) ---
    function URLSearchParams(init) {
        this.__l = [];
        if (typeof init === "string") { var s = init.charAt(0) === "?" ? init.slice(1) : init; if (s) s.split("&").forEach(function (pair) { var eq = pair.indexOf("="); var k = eq < 0 ? pair : pair.slice(0, eq); var v = eq < 0 ? "" : pair.slice(eq + 1); this.__l.push([decodeURIComponent(k.replace(/\+/g, " ")), decodeURIComponent(v.replace(/\+/g, " "))]); }, this); }
        else if (init && typeof init.forEach === "function") { init.forEach(function (v, k) { this.__l.push([String(k), String(v)]); }, this); }
        else if (init && typeof init === "object") { for (var key in init) if (Object.prototype.hasOwnProperty.call(init, key)) this.__l.push([key, String(init[key])]); }
    }
    URLSearchParams.prototype.get = function (k) { for (var i = 0; i < this.__l.length; i++) if (this.__l[i][0] === k) return this.__l[i][1]; return null; };
    URLSearchParams.prototype.getAll = function (k) { var r = []; for (var i = 0; i < this.__l.length; i++) if (this.__l[i][0] === k) r.push(this.__l[i][1]); return r; };
    URLSearchParams.prototype.has = function (k) { return this.get(k) !== null; };
    URLSearchParams.prototype.set = function (k, v) { var done = false; for (var i = this.__l.length - 1; i >= 0; i--) if (this.__l[i][0] === k) { if (done) this.__l.splice(i, 1); else { this.__l[i][1] = String(v); done = true; } } if (!done) this.__l.push([k, String(v)]); this.__notify(); };
    URLSearchParams.prototype.append = function (k, v) { this.__l.push([String(k), String(v)]); this.__notify(); };
    URLSearchParams.prototype["delete"] = function (k) { for (var i = this.__l.length - 1; i >= 0; i--) if (this.__l[i][0] === k) this.__l.splice(i, 1); this.__notify(); };
    URLSearchParams.prototype.forEach = function (cb, t) { for (var i = 0; i < this.__l.length; i++) cb.call(t, this.__l[i][1], this.__l[i][0], this); };
    // application/x-www-form-urlencoded byte serializer (URL Standard): space→"+",
    // percent-encode `! ' ( ) ~` that encodeURIComponent leaves bare. Mirrors the
    // page realm's `fenc`.
    function __fenc(s) { return encodeURIComponent(String(s)).replace(/[!'()~]/g, function (c) { return "%" + c.charCodeAt(0).toString(16).toUpperCase(); }).replace(/%20/g, "+"); }
    URLSearchParams.prototype.toString = function () { return this.__l.map(function (p) { return __fenc(p[0]) + "=" + __fenc(p[1]); }).join("&"); };
    // Live binding to an owning URL, mirroring the page realm (see its URL/USP).
    URLSearchParams.prototype.__notify = function () { if (this.__url) this.__url.__setSearchFromParams(this.toString()); };
    URLSearchParams.prototype.__setList = function (query) { this.__l = []; var s = String(query).charAt(0) === "?" ? String(query).slice(1) : String(query); if (s) s.split("&").forEach(function (pair) { var eq = pair.indexOf("="); var k = eq < 0 ? pair : pair.slice(0, eq); var v = eq < 0 ? "" : pair.slice(eq + 1); this.__l.push([decodeURIComponent(k.replace(/\+/g, " ")), decodeURIComponent(v.replace(/\+/g, " "))]); }, this); };
    // A live URL: assigning a component re-serializes href via __url_set (the
    // url crate's WHATWG setters), exactly like the page realm's class version.
    function URL(url, base) {
        var p = __url_parse(String(url), base != null ? String(base) : null);
        if (!p) throw new TypeError("Invalid URL: " + url);
        this.__p = p; this.__sp = null;
    }
    function urlAccessor(i, which) {
        return which
            ? { get: function () { return this.__p[i]; }, set: function (v) { var r = __url_set(this.__p[0], which, String(v)); if (r) this.__p = r; } }
            : { get: function () { return this.__p[i]; } };
    }
    Object.defineProperties(URL.prototype, {
        href: { get: function () { return this.__p[0]; }, set: function (v) { var r = __url_parse(String(v), null); if (!r) throw new TypeError("Invalid URL: " + v); this.__p = r; if (this.__sp) this.__sp.__setList(this.__p[6]); } },
        protocol: urlAccessor(1, "protocol"),
        host: urlAccessor(2, "host"),
        hostname: urlAccessor(3, "hostname"),
        port: urlAccessor(4, "port"),
        pathname: urlAccessor(5, "pathname"),
        search: { get: function () { return this.__p[6]; }, set: function (v) { var r = __url_set(this.__p[0], "search", String(v)); if (r) this.__p = r; if (this.__sp) this.__sp.__setList(this.__p[6]); } },
        hash: urlAccessor(7, "hash"),
        origin: urlAccessor(8),
        username: urlAccessor(9, "username"),
        password: urlAccessor(10, "password"),
        searchParams: { get: function () { if (!this.__sp) { this.__sp = new URLSearchParams(this.__p[6]); this.__sp.__url = this; } return this.__sp; } },
    });
    URL.prototype.__setSearchFromParams = function (qs) { var r = __url_set(this.__p[0], "search", qs); if (r) this.__p = r; };
    URL.prototype.toString = function () { return this.__p[0]; };
    URL.prototype.toJSON = function () { return this.__p[0]; };
    g.URL = URL; g.URLSearchParams = URLSearchParams;

    // --- Blob URL store (worker realm) — RAM-only, mirrors the page realm ---
    var __blobURLStore = Object.create(null);
    function __blobBytes(b) {
        if (!b || !Array.isArray(b.__parts)) return "";
        var enc = new g.TextEncoder(), out = "";
        for (var i = 0; i < b.__parts.length; i++) {
            var p = b.__parts[i], v, j;
            if (typeof p === "string") { v = enc.encode(p); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
            else if (p instanceof ArrayBuffer) { v = new Uint8Array(p); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
            else if (p && typeof p.byteLength === "number" && p.buffer) { v = new Uint8Array(p.buffer, p.byteOffset || 0, p.byteLength); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
            else if (p && Array.isArray(p.__parts)) out += __blobBytes(p);
            else if (p != null) { v = enc.encode(String(p)); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
        }
        return out;
    }
    function __blobText(bytes) { var u = new Uint8Array(bytes.length); for (var i = 0; i < bytes.length; i++) u[i] = bytes.charCodeAt(i) & 0xFF; return new g.TextDecoder().decode(u); }
    function __resolveBlobURL(u) {
        var h = u.indexOf("#"), key = h >= 0 ? u.slice(0, h) : u, obj = __blobURLStore[key];
        if (!obj) return null;
        if (Array.isArray(obj.__parts)) return { bytes: __blobBytes(obj), type: obj.type || "" };
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

    // --- fetch (sync-backed in v1: blocks the worker thread, never the page) ---
    function makeResponse(status, ctype, text, url) {
        return {
            ok: status >= 200 && status < 300, status: status, statusText: "", url: url, redirected: false, type: "basic", bodyUsed: false,
            headers: { get: function (n) { return String(n).toLowerCase() === "content-type" ? ctype : null; }, has: function (n) { return String(n).toLowerCase() === "content-type"; }, forEach: function () {} },
            text: function () { return Promise.resolve(text); },
            json: function () { return Promise.resolve(JSON.parse(text)); },
            arrayBuffer: function () { var b = new Uint8Array(text.length); for (var i = 0; i < text.length; i++) b[i] = text.charCodeAt(i) & 0xFF; return Promise.resolve(b.buffer); },
            blob: function () { return Promise.resolve(new Blob([text], { type: ctype || "" })); },
            clone: function () { return makeResponse(status, ctype, text, url); }
        };
    }
    g.fetch = function (input, init) {
        init = init || {};
        var url = (input && input.url) ? input.url : String(input);
        if (url.slice(0, 5) === "blob:") {
            var be = __resolveBlobURL(url);
            if (!be) return Promise.reject(new TypeError("Failed to fetch: " + url));
            return Promise.resolve(makeResponse(200, be.type || null, __blobText(be.bytes), url));
        }
        var rp = __url_parse(url, g.location.href); if (rp) url = rp[0];
        var method = String(init.method || (input && input.method) || "GET").toUpperCase();
        var body = (init.body != null) ? String(init.body) : null;
        var r = __http_fetch(url, method, body, null, "");
        if (!r) return Promise.reject(new TypeError("Failed to fetch: " + url));
        return Promise.resolve(makeResponse(r[0], r[1], r[2], url));
    };
})();
