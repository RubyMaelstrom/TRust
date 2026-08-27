(function () {
    "use strict";
    const g = globalThis;
    const cfg = g.__trust_cfg || { url: "about:blank", ua: "TRust/0.1", language: "en-US", languages: ["en-US", "en"], width: 640, height: 384 };
    const trust = { errors: [], logs: [], readyState: "loading" };
    g.__trust = trust;

    // --- the virtual clock ---
    // The engine's timer clock (ms since page start). Explicit timer advances
    // update `timers.now`; between them the Rust Date clock keeps counting host
    // monotonic time. `currentTime()` joins those sources so performance.now(),
    // event timestamps, and newly scheduled timer deadlines never freeze while
    // synchronous JavaScript is executing (High Resolution Time §§2.1, 7.1).
    // Declared up here because the Event class, defined early, reads it.
    // HTML §8.7 keeps timer IDs independently from queued tasks. `activeNesting`
    // is the currently-running timer task's nesting level, or zero for any
    // other task; it drives the normative four-millisecond nested-timer clamp.
    const timers = { q: [], ids: new Set(), now: 0, seq: 1, activeNesting: 0 };
    // HTML §8.10: animation-frame callbacks belong to a per-target callback
    // map, not the timer task source. `q` preserves insertion order; `deadline`
    // represents the next rendering opportunity selected by the host.
    const animationFrames = { q: [], deadline: null, seq: 0 };
    // The absolute epoch the virtual clock is anchored to — the REAL host
    // time at prelude boot (the Rust clock answers real time until the first
    // __clockSync). Every `timers.now` advance re-anchors the Rust clock.
    const __epoch0 = Date.now();
    const __clockSync = typeof __clock_set === "function"
        ? function () { __clock_set(__epoch0 + timers.now); }
        : function () {};
    __clockSync();
    function currentTime() {
        const elapsed = Date.now() - __epoch0;
        return Math.max(timers.now, Number.isFinite(elapsed) ? elapsed : timers.now);
    }

    // --- node wrappers, identity-cached so wrap(id) === wrap(id) ---
    const W = new Map();
    function wrap(id) {
        if (id === null || id === undefined) return null;
        let w = W.get(id);
        if (w) return w;
        const t = __dom_node_type(id);
        if (t === 1) {
            // Dispatch to the element's interface class by tag (HTMLSelectElement,
            // HTMLVideoElement, …); seed the lazily-cached localName since we
            // already read it, keeping this ~syscall-neutral vs the old wrap.
            const tag = __dom_tag(id) || "";
            w = new (classFor(tag))(id);
            w.__trustLN = tag;
        } else {
            w = t === 9 ? new Document(id)
                : t === 3 ? new Text(id)
                : t === 8 ? new Comment(id)
                : t === 11 ? new DocumentFragment(id)
                : new Node(id);
        }
        W.set(id, w);
        return w;
    }

    // A <script> element runs when it is FIRST inserted into the document
    // (HTML "prepare a script") — the universal SDK-loader idiom
    // `document.body.appendChild(scriptEl)` (reCAPTCHA, lazy analytics, embeds).
    // Tracked so re-insertion never re-runs it (the spec's "already started"
    // flag). Classic and module scripts run; a non-JS `type` is left inert. Scripts
    // parsed from innerHTML do NOT execute (spec), so this only fires for
    // genuine element-node insertion through appendChild/insertBefore.
    const SCRIPTS_STARTED = new Set();
    function maybeRunScript(node) {
        if (!node || node.localName !== "script" || SCRIPTS_STARTED.has(node.__id)) return;
        const ty = (node.getAttribute("type") || "").trim().toLowerCase();
        if (ty && ty !== "text/javascript" && ty !== "application/javascript" && ty !== "text/ecmascript" && ty !== "module") return;
        // A classic script carrying `nomodule` is skipped: it exists only for
        // user agents WITHOUT module support, and we run module scripts (HTML
        // §"prepare the script element"). Letting it run loads the legacy
        // polyfill bundle a real browser never executes.
        if (node.hasAttribute("nomodule")) return;
        // Only a script connected to the document runs (not one built up inside
        // a detached fragment, which executes when ITS root is later inserted).
        let n = node, connected = false;
        while (n) { if (n.nodeType === 9) { connected = true; break; } n = n.parentNode; }
        if (!connected) return;
        SCRIPTS_STARTED.add(node.__id);
        __dom_run_injected_script(node.__id);
    }
    // A `<link rel=stylesheet href>` inserted into the live document is fetched
    // and fires `load`/`error` (HTML "obtain a resource" for a stylesheet link).
    // Was a no-op, so a loader waiting on `link.onload` hung — notably webpack's
    // mini-css-extract chunk loader, which keeps `__webpack_require__.e(chunk)`
    // pending until the CSS link loads, stalling every `React.lazy` route whose
    // `Promise.all` of chunks includes a CSS chunk (Twitch's whole page body).
    function maybeLoadStylesheet(node) {
        if (!node || node.localName !== "link" || node.__cssStarted) return;
        // Loaders set `link.rel`/`link.href` as PROPERTIES (webpack's mini-css
        // loader: `c.rel="stylesheet"; c.href=url`), so read the property first
        // (the `rel` IDL attribute doesn't always reflect to getAttribute).
        const rel = (node.rel || node.getAttribute("rel") || "").toLowerCase();
        const rels = rel.split(/\s+/);
        let n = node, connected = false;
        while (n) { if (n.nodeType === 9) { connected = true; break; } n = n.parentNode; }
        if (!connected) return;
        if (rels.indexOf("stylesheet") >= 0) {
            if (!(node.href || node.getAttribute("href"))) return;
            node.__cssStarted = true;
            __dom_load_injected_stylesheet(node.__id);
            return;
        }
        // Resource-hint links (`preload`/`prefetch`/`modulepreload`/`preconnect`/
        // `dns-prefetch`): we don't speculatively fetch (hints are optional), but
        // the element MUST still fire `load` — a loader that `await`s the hint
        // otherwise hangs forever. Astro's ClientRouter preloads the destination
        // page's stylesheets as `<link rel="preload" as="style">` and awaits
        // `Promise.all` of their load/error events BEFORE the view-transition
        // swap; without this the swap never runs and every routed link goes dead.
        if (rels.some((r) => r === "preload" || r === "prefetch" || r === "modulepreload"
                || r === "preconnect" || r === "dns-prefetch")) {
            node.__cssStarted = true;
            // Async, like a real hint resolving — settles inside the same job drain.
            Promise.resolve().then(() => { try { node.dispatchEvent(new Event("load")); } catch (e) {} });
        }
    }

    // A freshly inserted <iframe>/<frame> connected to the document begins
    // navigation (HTML "process the iframe attributes" runs on insertion). A frame
    // built up inside a detached fragment waits until its root is connected —
    // the next load/settle sweep (or a contentDocument read) realizes it then.
    // (Forward-referenced by the Node insert methods; `queueFrameNavigation`
    // is hoisted alongside the other iframe helpers below.)
    function maybeProcessInsertedFrame(frame, parent) {
        if (!parent.isConnected) return;
        queueFrameNavigation(frame);
    }

    // The document base URL: <base href> when present (archive.org sets
    // one; SPA routers resolve '.' against it), the page URL otherwise.
    // CACHED — a full querySelector("base[href]") on every .href/.src read was
    // ~5% of Steam's settle profile. The resolved base only changes on
    // navigation (location) or a runtime <base href> mutation/insertion, each of
    // which resets the cache (setLocParts; the `<base>` guards in setAttribute/
    // removeAttribute and the child-mutation methods). `null` = (re)compute; any
    // real resolved base is a non-null string (so "" stays a valid cache hit).
    // Bulk HTML insertion invalidates the cache as a whole because parsing can
    // insert a <base> below an arbitrary target, and the HTML attribute setter
    // already invalidates for case variants.
    let baseHrefCache = null;
    // HTML §7.2.2: Window.frameElement is a readonly IDL attribute whose
    // getter derives the value from the current browsing context. Keep that
    // value outside the page-visible object: challenge scripts are allowed to
    // define or freeze an own `frameElement` property, and frame bookkeeping
    // must not try to overwrite page-owned descriptors while restoring state.
    let frameElementState = null;
    // Lazily-minted, then cached `document.all` (`[[IsHTMLDDA]]`) — see the
    // `Document` class `get all()`. Per-page (fresh per realm), so its identity is
    // stable within a page but never shared across pages.
    let documentAllValue = null;
    // HTML §6.6 focus model. `null` denotes the document viewport as the
    // focused area; DocumentOrShadowRoot.activeElement maps that viewport's
    // Document anchor to body, then documentElement, exactly as the specified
    // getter does. Keep the focused DOM anchor itself when an element owns
    // focus so modal/focus-restoration code never observes `undefined`.
    let focusedArea = null;
    function viewportFocusAnchor(doc) {
        return (doc && (doc.body || doc.documentElement)) || null;
    }
    function activeElementFor(root) {
        if (focusedArea) {
            // Focus-fixup: once the focused element leaves its document, the
            // viewport is the surviving focusable area. The activeElement
            // getter must not return a detached stale wrapper.
            if (!focusedArea.isConnected) focusedArea = null;
        }
        if (!focusedArea) return root.nodeType === 9 ? viewportFocusAnchor(root) : null;
        // DocumentOrShadowRoot.activeElement retargets a focused shadow-tree
        // descendant to each intervening host. `retarget` already implements
        // DOM's normative retargeting algorithm for the event system.
        const candidate = retarget(focusedArea, root);
        return candidate && rootOfNode(candidate) === root ? candidate : null;
    }
    function focusEvent(target, type, related, bubbles) {
        const ev = createTrustedEvent(FocusEvent, type, {
            bubbles: !!bubbles, cancelable: false, composed: true,
            view: g, detail: 0, relatedTarget: related || null,
        });
        dispatch(target, ev, false);
    }
    function parsedTabIndex(el) {
        const raw = el.getAttribute("tabindex");
        if (raw === null) return null;
        // HTML's integer parser accepts leading ASCII whitespace and a sign,
        // then the longest initial digit run. The IDL attribute is a Web IDL
        // `long`, so values outside that range use the historical default.
        const match = /^[\t\n\f\r ]*([+-]?\d+)/.exec(raw);
        if (!match) return null;
        const value = Number(match[1]);
        return Number.isFinite(value) && value >= -2147483648 && value <= 2147483647
            ? value : null;
    }
    function defaultTabIndex(el) {
        const tag = el.localName;
        if (tag === "summary") {
            const parent = el.parentElement;
            return parent && parent.localName === "details"
                && parent.children.find((child) => child.localName === "summary") === el ? 0 : -1;
        }
        return tag === "a" || tag === "area" || tag === "button" || tag === "frame"
            || tag === "iframe" || tag === "input" || tag === "object"
            || tag === "select" || tag === "textarea" ? 0 : -1;
    }
    function isActuallyDisabled(el) {
        const tag = el && el.localName;
        const formControl = tag === "button" || tag === "fieldset" || tag === "input"
            || tag === "select" || tag === "textarea";
        if (!formControl) return false;
        if (el.hasAttribute("disabled")) return true;
        // HTML's "actually disabled" state propagates from every disabled
        // fieldset to descendant form controls, except those inside that
        // fieldset's first legend child. A meaningless disabled attribute on a
        // generic tabindex element does not make it unfocusable.
        for (let parent = el.parentElement; parent; parent = parent.parentElement) {
            if (parent.localName !== "fieldset" || !parent.hasAttribute("disabled")) continue;
            const firstLegend = parent.children.find((child) => child.localName === "legend");
            if (!firstLegend || !firstLegend.contains(el)) return true;
        }
        return false;
    }
    function elementCanFocus(el) {
        if (!el || !el.isConnected || isActuallyDisabled(el)) return false;
        for (let p = el; p && p.nodeType === 1; p = p.parentElement) {
            if (p.hasAttribute("inert")) return false;
        }
        if (el === g.document.documentElement) return true;
        if (parsedTabIndex(el) !== null) return true;
        const tag = el.localName;
        if (tag === "a" || tag === "area") return el.hasAttribute("href");
        if (tag === "input") return String(el.type || "").toLowerCase() !== "hidden";
        if (tag === "audio" || tag === "video") return el.hasAttribute("controls");
        return tag === "button" || tag === "select" || tag === "textarea"
            || tag === "iframe" || tag === "summary"
            || el.hasAttribute("contenteditable");
    }
    function focusElement(el, options) {
        if (!elementCanFocus(el) || focusedArea === el) return;
        const old = focusedArea;
        // HTML §6.6.4's focus update steps remove focus before firing blur;
        // UI Events §3.3.2 orders blur, focusout, focus, then focusin. A handler
        // reading activeElement during blur therefore sees the viewport
        // fallback, not the future target.
        focusedArea = null;
        if (old && old.isConnected) {
            focusEvent(old, "blur", el, false);
            focusEvent(old, "focusout", el, true);
        }
        focusedArea = el;
        focusEvent(el, "focus", old, false);
        focusEvent(el, "focusin", old, true);
        if (!(options && options.preventScroll)) {
            try { el.scrollIntoView({ block: "center", inline: "center" }); } catch (e) {}
        }
    }
    function blurElement(el) {
        if (!el || focusedArea !== el) return;
        focusedArea = null;
        focusEvent(el, "blur", null, false);
        focusEvent(el, "focusout", null, true);
    }
    function baseHref() {
        if (baseHrefCache !== null) return baseHrefCache;
        const b = g.document.querySelector("base[href]");
        if (!b) return (baseHrefCache = g.location.href);
        const u = __url_parse(b.getAttribute("href") || "", g.location.href);
        return (baseHrefCache = u ? u[0] : g.location.href);
    }
    // Resolve a (possibly relative) request URL against the document base URL —
    // the base for fetch()/XHR per Fetch §"Request" and XHR `open()` (the API
    // base URL of the relevant settings object IS the document base URL, i.e.
    // <base href>, not the document URL). An already-absolute URL — incl.
    // blob:/data:/about: — ignores the base. Falls back to the raw string on a
    // parse miss (the Rust syscall then joins against the page URL as before).
    function resolveURL(u) { const p = __url_parse(u, baseHref()); return p ? p[0] : u; }

    // --- events: listener registry + synchronous capture/target/bubble dispatch ---
    // Each list entry is { fn, capture, once, removed } (DOM §"event listener").
    // `removed` is the spec's removed flag: a listener removed mid-dispatch (or
    // consumed by `once`) is skipped by the in-flight snapshot. `captureCount`
    // tracks whether ANY capture listener exists, so the common no-capture page
    // pays nothing for the capture phase (no ancestor walk on non-bubbling
    // events, no extra pass).
    const LS = new Map();
    let captureCount = 0;
    function lsFor(target, type) {
        let m = LS.get(target);
        if (!m) { m = new Map(); LS.set(target, m); }
        let l = m.get(type);
        if (!l) { l = []; m.set(type, l); }
        return l;
    }
    // Flatten the WebIDL options argument (boolean useCapture, or the
    // AddEventListenerOptions dict) — capture/once/signal honored, `passive`
    // accepted and ignored (nothing here has a scroll-blocking default).
    function lsOpts(options) {
        if (options === true) return { capture: true, once: false, signal: null };
        if (!options || typeof options !== "object") return { capture: false, once: false, signal: null };
        return { capture: !!options.capture, once: !!options.once, signal: options.signal || null };
    }
    // The one "add an event listener" implementation (DOM §2.7), shared by
    // EventTarget.prototype and the window's bound wrappers. Dedup is by
    // (callback, capture) — a re-add with different once/passive is ignored,
    // per spec. An already-aborted signal means never add; a live signal
    // removes the listener when it aborts.
    // Find the (callback, capture) entry index in list `l` — the DOM §2.7.3
    // dedup/removal key — via NATIVE `indexOf` over the parallel raw-callback
    // array `l.fns` (with `l.caps` aligned to it). PERF INVARIANT (don't
    // regress): this scan MUST stay native. An interpreted per-entry loop here
    // (`l[i].fn === fn && l[i].capture === …`) turned a listener-flooding page
    // — Twitch's player registers tens of thousands of listeners while it
    // retries its walled token — into MINUTES of settle: the scan is O(list)
    // per add either way, but property-reading bytecode pays ~100× the
    // constant of the Rust builtin loop, and the flood made it quadratic.
    // The same-fn-other-capture hop below is interpreted but vanishingly rare.
    function lsFind(l, fn, capture) {
        const fns = l.fns;
        if (!fns) return -1;
        let i = fns.indexOf(fn);
        while (i >= 0 && l.caps[i] !== capture) i = fns.indexOf(fn, i + 1);
        return i;
    }
    function addL(target, type, fn, options) {
        if (!(typeof fn === "function" || (fn && typeof fn.handleEvent === "function"))) return;
        const o = lsOpts(options);
        if (o.signal && o.signal.aborted) return;
        const t = String(type);
        const l = lsFor(target, t);
        if (lsFind(l, fn, o.capture) >= 0) return;
        // A single Boa realm hosts the page and its inline-rendered nested
        // documents. Remember the document whose global was current when the
        // listener was registered; dispatch restores that document before
        // invoking the callback.
        const entry = { fn: fn, capture: o.capture, once: o.once, removed: false,
                        frame: trust.__activeFrame || null };
        // Entries are pushed HERE only, so `l`/`l.fns`/`l.caps` stay aligned.
        if (!l.fns) { l.fns = []; l.caps = []; l.capN = 0; }
        l.push(entry); l.fns.push(fn); l.caps.push(o.capture);
        if (o.capture) { captureCount++; l.capN++; }
        if (o.signal && typeof o.signal.addEventListener === "function") {
            o.signal.addEventListener("abort", function () { removeL(target, t, fn, { capture: o.capture }); }, { once: true });
        }
    }
    function removeL(target, type, fn, options) {
        const capture = lsOpts(options).capture;
        const l = lsFor(target, String(type));
        const i = lsFind(l, fn, capture);
        if (i < 0) return;
        l[i].removed = true; // in-flight dispatch snapshots skip it (spec)
        if (l[i].capture) { captureCount--; l.capN--; }
        l.splice(i, 1); l.fns.splice(i, 1); l.caps.splice(i, 1);
    }
    // DOM §2.2 initializes constructed events as untrusted. Events created by
    // the user-agent activation algorithms use the separate "create an event"
    // hook, which initializes `isTrusted` to true; `dispatchEvent()` resets it
    // to false. Keep that bit in an unexposed WeakSet so page code cannot forge
    // trusted input by passing a non-standard constructor option or assigning
    // to the readonly `isTrusted` attribute.
    const trustedEvents = new WeakSet();
    function createTrustedEvent(C, type, opts) {
        const ev = new C(type, opts);
        trustedEvents.add(ev);
        return ev;
    }
    class Event {
        constructor(type, opts) {
            this.type = String(type);
            this.bubbles = !!(opts && opts.bubbles);
            this.cancelable = !!(opts && opts.cancelable);
            this.composed = !!(opts && opts.composed);
            this.defaultPrevented = false;
            this.target = null;
            this.currentTarget = null;
            this.eventPhase = 0; // NONE; dispatch sets 1/2/3 per phase
            Object.defineProperty(this, "isTrusted", {
                configurable: false,
                enumerable: true,
                get() { return trustedEvents.has(this); },
            });
            // CustomEvent.detail (and UIEvent.detail) default to null, not
            // undefined, when not supplied.
            this.detail = opts && "detail" in opts ? opts.detail : null;
            // DOM §2.2: creation time relative to the time origin. This is the
            // current monotonic clock, not merely the last timer checkpoint.
            this.timeStamp = currentTime();
            // Per-interface EventInit members (MouseEventInit.clientX,
            // KeyboardEventInit.key, MessageEventInit.data, …) become event
            // properties. We don't model each interface's dictionary, so copy
            // any extra init members generically — without clobbering the
            // standard fields set above.
            if (opts) for (const k in opts) if (!(k in this)) this[k] = opts[k];
        }
        // Cancelling only takes effect on a cancelable event (spec): a
        // preventDefault on a non-cancelable event is a no-op. Real code and
        // the platform both read defaultPrevented to decide whether to run
        // the default action.
        preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
        // Legacy alias: returnValue is the inverse of defaultPrevented;
        // assigning false cancels (honoring cancelable), true can't un-cancel.
        get returnValue() { return !this.defaultPrevented; }
        set returnValue(v) { if (!v) this.preventDefault(); }
        stopPropagation() { this.__stop = true; }
        stopImmediatePropagation() { this.__stop = this.__stopNow = true; }
        // Legacy DOM init for events made via document.createEvent(): deprecated
        // but still used by feature-detection (webcomponentsjs probes it) and a
        // lot of older code. initCustomEvent is the CustomEvent variant.
        initEvent(type, bubbles, cancelable) {
            trustedEvents.delete(this);
            this.type = String(type);
            this.bubbles = !!bubbles;
            this.cancelable = !!cancelable;
            this.defaultPrevented = false;
        }
        initCustomEvent(type, bubbles, cancelable, detail) {
            // type is a mandatory WebIDL argument.
            if (arguments.length < 1) throw new TypeError("initCustomEvent requires a type");
            trustedEvents.delete(this);
            this.type = String(type);
            this.bubbles = !!bubbles;
            this.cancelable = !!cancelable;
            this.detail = detail === undefined ? null : detail;
            this.defaultPrevented = false;
        }
        // DOM §2.2 composedPath(): non-empty only DURING dispatch (dispatch()
        // builds the path and empties it when it unwinds, per spec), ordered
        // target-first, and clipped so nodes inside a CLOSED shadow tree are
        // invisible to listeners outside it (each struct carries `c` =
        // root-of-closed-tree; slots aren't in our dispatch path, so the
        // spec's slot-in-closed-tree branches are structurally never taken
        // and are omitted).
        composedPath() {
            const path = this.__path;
            if (!path || !path.length) {
                // The one-struct fast path (a non-bubbling event with no
                // capture listeners registered) skips building a path array;
                // during its at-target invocation the spec's answer is just
                // [currentTarget]. Outside dispatch: empty, per spec.
                return this.currentTarget ? [this.currentTarget] : [];
            }
            const out = [this.currentTarget];
            let cti = 0, hidden = 0;
            for (let i = path.length - 1; i >= 0; i--) {
                if (path[i].c) hidden++;
                if (path[i].n === this.currentTarget) { cti = i; break; }
            }
            let cur = hidden, max = hidden;
            for (let i = cti - 1; i >= 0; i--) {
                if (path[i].c) cur++;
                if (cur <= max) out.unshift(path[i].n);
            }
            cur = hidden; max = hidden;
            for (let i = cti + 1; i < path.length; i++) {
                if (cur <= max) out.push(path[i].n);
                if (path[i].c) { cur--; if (cur < max) max = cur; }
            }
            return out;
        }
        // Legacy positional init for the typed createEvent() interfaces. The
        // type-specific tail (view/detail/coords/keys) is accepted and stored
        // generically; the first three args are the real Event init.
        initUIEvent(type, bubbles, cancelable, view, detail) {
            this.initEvent(type, bubbles, cancelable);
            this.view = view; this.detail = detail;
        }
        initMouseEvent(type, bubbles, cancelable, view, detail, sx, sy, cx, cy, ctrl, alt, shift, meta, button, related) {
            this.initEvent(type, bubbles, cancelable);
            this.view = view; this.detail = detail;
            this.screenX = sx; this.screenY = sy; this.clientX = cx; this.clientY = cy;
            this.ctrlKey = ctrl; this.altKey = alt; this.shiftKey = shift; this.metaKey = meta;
            this.button = button; this.relatedTarget = related;
        }
        initKeyboardEvent(type, bubbles, cancelable, view, key) {
            this.initEvent(type, bubbles, cancelable);
            this.view = view; this.key = key;
        }
    }
    // Event-phase constants (DOM §2.2), on the interface object AND the
    // prototype per WebIDL — code compares `ev.eventPhase === Event.AT_TARGET`.
    Event.NONE = 0; Event.CAPTURING_PHASE = 1; Event.AT_TARGET = 2; Event.BUBBLING_PHASE = 3;
    Event.prototype.NONE = 0; Event.prototype.CAPTURING_PHASE = 1;
    Event.prototype.AT_TARGET = 2; Event.prototype.BUBBLING_PHASE = 3;
    // The standard Event-interface hierarchy. Real browsers expose all of
    // these as constructable globals with distinct prototypes; code (and
    // polyfills like webcomponentsjs) reference `window.MouseEvent`, do
    // `new KeyboardEvent(...)`, and check `e instanceof MouseEvent`. They
    // inherit Event's constructor (which already copies init-dict members),
    // so `new MouseEvent("click", { clientX: 5 })` sets `clientX`.
    // A real subclass (Event's constructor already handles `detail`): with
    // `CustomEvent === Event`, EVERY event was `instanceof CustomEvent`.
    class CustomEvent extends Event {}
    class UIEvent extends Event {}
    class MouseEvent extends UIEvent {}
    class PointerEvent extends MouseEvent {}
    class WheelEvent extends MouseEvent {}
    class DragEvent extends MouseEvent {}
    class KeyboardEvent extends UIEvent {}
    class FocusEvent extends UIEvent {}
    class InputEvent extends UIEvent {}
    class TouchEvent extends UIEvent {}
    class CompositionEvent extends UIEvent {}
    class PopStateEvent extends Event {}
    class HashChangeEvent extends Event {}
    class MessageEvent extends Event {}
    class ErrorEvent extends Event {}
    class PromiseRejectionEvent extends Event {}
    class ProgressEvent extends Event {}
    class SubmitEvent extends Event {}
    class StorageEvent extends Event {}
    class AnimationEvent extends Event {}
    class TransitionEvent extends Event {}
    class ClipboardEvent extends Event {}
    class PageTransitionEvent extends Event {}
    class CloseEvent extends Event {}
    // ToggleEvent (HTML §ToggleEvent): `beforetoggle`/`toggle` for popovers
    // (and <details>/<dialog>). oldState/newState ride the init dict.
    class ToggleEvent extends Event {
        constructor(type, init) {
            super(type, init);
            this.oldState = (init && init.oldState) || "";
            this.newState = (init && init.newState) || "";
        }
    }
    // createEvent("MouseEvent") must yield a MouseEvent, etc. (legacy path).
    const EVENT_INTERFACES = {
        Event, CustomEvent, Events: Event, HTMLEvents: Event,
        UIEvent, UIEvents: UIEvent, MouseEvent, MouseEvents: MouseEvent,
        PointerEvent, WheelEvent, DragEvent, KeyboardEvent, KeyEvents: KeyboardEvent,
        FocusEvent, InputEvent, TouchEvent, CompositionEvent, PopStateEvent,
        HashChangeEvent, MessageEvent, ErrorEvent, ProgressEvent, SubmitEvent,
        StorageEvent, AnimationEvent, TransitionEvent, ClipboardEvent,
        PageTransitionEvent, CloseEvent,
    };
    // on<event> attributes compile lazily at first dispatch and re-only
    // when the attribute text changes (zero cost at page load). Old-web
    // semantics: a handler returning false prevents the default.
    function attrHandler(cur, type) {
        if (!(cur instanceof Element)) return null;
        const src = cur.getAttribute("on" + type);
        if (src === null) return null;
        const cache = cur.__onCache || (cur.__onCache = {});
        const slot = cache[type];
        if (!slot || slot.src !== src) {
            let fn = null;
            try { fn = new Function("event", src); }
            catch (e) { trust.errors.push("on" + type + " compile: " + ((e && e.message) || e)); }
            cache[type] = { src: src, fn: fn,
                            frame: trust.__activeFrame || frameOwnerForNode(cur) || null };
            return fn;
        }
        return slot.fn;
    }
    // "Inner invoke" (DOM §2.9): run `cur`'s listeners for the event's current
    // phase. phase 1 = capturing (capture listeners only), 2 = at-target (all,
    // in registration order), 3 = bubbling (non-capture only). The legacy
    // `on<type>` content-attribute handler is a non-capture listener, so it
    // never runs in the capture phase. A `once` listener is removed BEFORE its
    // callback runs (spec), so a re-dispatch from inside it can't re-fire it.
    function invokeListeners(cur, ev, phase) {
        if (phase !== 1) {
            const af = attrHandler(cur, ev.type);
            if (af) {
                const afSlot = cur.__onCache && cur.__onCache[ev.type];
                try {
                    const result = runInFrame(afSlot && afSlot.frame,
                                              () => af.call(cur, ev));
                    if (result === false) ev.preventDefault();
                }
                catch (e) { trust.errors.push("on" + ev.type + ": " + ((e && e.message) || e)); }
                if (ev.__stopNow) return;
            }
        }
        const list = lsFor(cur, ev.type);
        if (!list.length) return;
        // Skip a phase that can't match anything BEFORE slicing/iterating: the
        // per-list capture count (`capN`, maintained by addL/removeL) makes a
        // capture pass over an all-bubble list — or a bubble pass over an
        // all-capture list — free. PERF INVARIANT: a page that floods one
        // target with listeners (Twitch's walled player re-adds ~20k `load`
        // handlers on window) pays that list's length on EVERY dispatch that
        // walks past it otherwise; the interpreted scan of 20k dead entries
        // per load event was seconds per dispatch.
        const capN = list.capN || 0;
        if (phase === 1 && capN === 0) return;
        if (phase === 3 && capN === list.length) return;
        for (const entry of list.slice()) {
            if (entry.removed) continue;
            // `window` is one Boa object for all scoped navigables, but HTML
            // delivers a MessageEvent only to the Window whose postMessage
            // target was addressed. Without this filter, every child frame's
            // message listener also saw top-window traffic and reCAPTCHA
            // interpreted an unrelated message as its own protocol payload.
            if (cur === g && ev.__windowTargetSet && entry.frame !== ev.__frameTarget) continue;
            if (phase === 1 && !entry.capture) continue;
            if (phase === 3 && entry.capture) continue;
            if (entry.once) removeL(cur, ev.type, entry.fn, { capture: entry.capture });
            try {
                runInFrame(entry.frame, () => {
                    if (typeof entry.fn === "function") entry.fn.call(cur, ev);
                    else entry.fn.handleEvent(ev);
                });
            }
            catch (e) { trust.errors.push(ev.type + " handler: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
            if (ev.__stopNow) break;
        }
    }
    // DOM §2.9 "retargeting": walk A up out of any shadow tree whose root is
    // not a shadow-including inclusive ancestor of B — the object the world
    // OUTSIDE that tree sees for A (a component's internals appear as the
    // component). Used for the event path's shadow-adjusted targets and for
    // relatedTarget adjustment.
    function rootOfNode(a) {
        let r = a;
        while (r.parentNode) r = r.parentNode;
        return r;
    }
    function shadowInclusiveContains(anc, b) {
        let n = b;
        for (;;) {
            if (n === anc) return true;
            n = n.parentNode || n.__host; // shadow-including: cross root→host
            if (!n) return false;
        }
    }
    function retarget(a, b) {
        for (;;) {
            if (!(a instanceof Node)) return a;
            const r = rootOfNode(a);
            if (!r.__host) return a; // A's root is not a shadow root
            if (b instanceof Node && shadowInclusiveContains(r, b)) return a;
            a = r.__host;
        }
    }
    // "Dispatch" (DOM §2.9): capture down the composed path, at-target, then
    // bubble back up when the event bubbles (or the caller forces it — the
    // platform events we fire on the document that window listeners must see).
    // The path is built whenever it can matter: a bubbling event, a forced
    // one, or ANY event while capture listeners exist (the spec runs the
    // capture phase even for non-bubbling events — capture-delegated focus
    // handling depends on it). A non-bubbling event on a page with no capture
    // listeners keeps the old one-element fast path.
    //
    // SHADOW RETARGETING (DOM §2.9 "retargeting"): each path entry carries the
    // shadow-adjusted target — inside the target's own shadow tree that is the
    // real target; at the host and above, the HOST. A listener outside a
    // component sees the component, never its internals (`e.target.closest`
    // on light-tree delegation can't match nodes inside a shadow root, exactly
    // like a browser). `ev.target`/`ev.relatedTarget` are restored to the real
    // values after dispatch (deliberate deviation from the spec's clearTargets
    // nulling; pages read these during dispatch). Only Node targets extend
    // their path past themselves: an XHR/WebSocket/AbortSignal is not in the
    // tree, so its events reach nothing else (they never bubbled before
    // either; the capture phase must not start delivering them to window).
    //
    // COMPOSED FLAG (DOM: a shadow root's "get the parent" returns null when
    // the event's composed flag is unset): a non-composed event stops at the
    // root of its target's shadow tree — the first shadow hop on this walk —
    // so `slotchange`, non-composed customEvents, etc. never leak out of a
    // component. RELATEDTARGET (DOM §2.9 steps 5/9.6): a relatedTarget is
    // retargeted per path entry, the whole dispatch is skipped when target and
    // adjusted relatedTarget collapse to the same object (a mouseover wholly
    // inside a component, seen from outside), and propagation ends at the tree
    // where a hop makes them collapse mid-walk.
    function dispatch(target, ev, forceBubble) {
        // Each browsing context owns a distinct Window/EventTarget. TRust
        // currently multiplexes those Window objects through one engine global,
        // so retain the active nested navigable as part of the logical target.
        // Without this filter a child `load` also invoked the parent window's
        // listeners (and the eventual parent `load` invoked every child
        // listener), violating DOM's per-EventTarget listener-list rule.
        if (target === g && !ev.__windowTargetSet) {
            ev.__windowTargetSet = true;
            ev.__frameTarget = trust.__activeFrame || null;
        }
        ev.target = target;
        const origRelated = ev.relatedTarget;
        const hasRelated = origRelated !== null && origRelated !== undefined;
        let relatedAtTarget = null;
        if (hasRelated) {
            relatedAtTarget = retarget(origRelated, target);
            if (target === relatedAtTarget && target !== origRelated) return !ev.defaultPrevented;
            ev.relatedTarget = relatedAtTarget;
        }
        let path = null; // [{ n, t: shadow-adjusted target, r: adjusted relatedTarget, c: root-of-closed-tree }], target-first
        if (forceBubble || ev.bubbles || captureCount > 0) {
            path = [];
            let n = target, t = target;
            path.push({ n: n, t: t, r: relatedAtTarget, c: false });
            if (n instanceof Node) {
                let clipped = false; // ended at a shadow root / relatedTarget collapse, not the tree top
                for (;;) {
                    const parent = n.parentNode;
                    if (parent) { n = parent; }
                    else if (n.__host) {
                        // Shadow hop. Non-composed events stop AT the root of
                        // the original target's tree — necessarily the first
                        // hop this walk reaches.
                        if (!ev.composed) { clipped = true; break; }
                        t = n.__host; n = t; // retarget: outside sees the host
                        // The hop crossed into the tree where both ends of an
                        // over/out pair look the same → propagation ends
                        // (spec: "if parent is relatedTarget, set parent to
                        // null").
                        if (hasRelated && retarget(origRelated, n) === n) { clipped = true; break; }
                    }
                    else break;
                    path.push({
                        n: n, t: t,
                        r: hasRelated ? retarget(origRelated, n) : null,
                        c: n instanceof ShadowRoot && n.__mode === "closed",
                    });
                }
                // DOM: a document's "get the parent" returns NULL for `load`
                // events — a node-targeted `load` NEVER reaches the Window
                // (that's why window.onload means the document load only).
                // Appending window here for subresource `load`s fed a REAL
                // amplification loop: web-vitals' whenReady helper re-adds a
                // window capture `load` listener from inside its own handler
                // ("complete" !== readyState → re-defer), so every script/img
                // load event DOUBLED the list — Twitch's boot grew it to ~33k
                // entries and minutes of dispatch time a browser never sees.
                // Window is a parent only OF THE DOCUMENT: a walk that ended
                // anywhere else (detached subtree, shadow clip, relatedTarget
                // collapse) has no window in its path (DOM: only a document's
                // "get the parent" returns the global).
                if (!clipped && n.nodeType === 9 && target !== g && ev.type !== "load") {
                    path.push({ n: g, t: t, r: hasRelated ? retarget(origRelated, g) : null, c: false });
                }
            }
            ev.__path = path; // composedPath() reads it; emptied on unwind (spec)
        }
        let stopped = false;
        if (path && captureCount > 0) {
            ev.eventPhase = 1; // CAPTURING_PHASE
            for (let i = path.length - 1; i >= 1; i--) {
                ev.currentTarget = path[i].n;
                ev.target = path[i].t;
                if (hasRelated) ev.relatedTarget = path[i].r;
                invokeListeners(path[i].n, ev, 1);
                if (ev.__stop) { stopped = true; break; }
            }
        }
        if (!stopped) {
            ev.eventPhase = 2; // AT_TARGET
            ev.currentTarget = target;
            ev.target = target;
            if (hasRelated) ev.relatedTarget = relatedAtTarget;
            invokeListeners(target, ev, 2);
            if (ev.__stop) stopped = true;
        }
        if (!stopped && path && (forceBubble || ev.bubbles)) {
            ev.eventPhase = 3; // BUBBLING_PHASE
            for (let i = 1; i < path.length; i++) {
                ev.currentTarget = path[i].n;
                ev.target = path[i].t;
                if (hasRelated) ev.relatedTarget = path[i].r;
                invokeListeners(path[i].n, ev, 3);
                if (ev.__stop) break;
            }
        }
        ev.eventPhase = 0;
        ev.currentTarget = null;
        ev.__path = null; // spec: "set event's path to the empty list"
        ev.target = target;
        if (hasRelated) ev.relatedTarget = origRelated;
        return !ev.defaultPrevented;
    }
    trust.fire = function (target, type, bubble) {
        dispatch(target, new Event(type), bubble);
    };
    // A headless DOM never decodes images, so the `load` event a real browser
    // fires when an image fetch succeeds never happens here. The ubiquitous
    // "reveal on load" idiom — an `<img>` painted at `opacity:0` (or hidden)
    // until a `load` handler reveals it (lightGallery's lightbox, lazy-loaders,
    // masonry, fade-in carousels) — then leaves the image invisible forever,
    // and the layout drops an `opacity:0` image entirely. We DO fetch and show
    // images in the layout/render pipeline, so optimistically firing `load` is
    // the correct default. Only imgs something is actually waiting on (a `load`
    // listener / `onload`) are fired, so an ordinary page pays nothing. The
    // event is deferred to a macrotask: a library inserts the `<img>` and THEN
    // binds its handler, so a browser fires `load` on the next turn, once the
    // handler is registered — we match that. Returns the count newly scheduled
    // so the actor can re-scan for images a load handler itself inserts
    // (lightGallery preloads the adjacent slides).
    trust.__imgLoaded = new Set();
    trust.scanImageLoads = function () {
        let imgs;
        try { imgs = g.document.querySelectorAll("img"); } catch (e) { return 0; }
        const pending = [];
        for (let i = 0; i < imgs.length; i++) {
            const im = imgs[i];
            const id = im.__id;
            if (typeof id !== "number" || trust.__imgLoaded.has(id)) continue;
            if (!im.getAttribute("src")) continue;
            const m = LS.get(im);
            const listening =
                (m && m.get("load") && m.get("load").length) || typeof im.onload === "function";
            if (!listening) continue;
            trust.__imgLoaded.add(id);
            pending.push(im);
        }
        if (pending.length) setTimeout(function () {
            for (const im of pending) { try { dispatch(im, new Event("load"), false); } catch (e) {} }
        }, 0);
        return pending.length;
    };
    // --- iframe processing: HTML "process the iframe attributes" ----------
    // An <iframe>/<frame> renders its nested document INLINE (the serializer
    // rewrites the frame + its realized content into a <div data-trust-frame>;
    // see dom.rs `frame_body`). The content navigable's document is fetched
    // (src) or taken from the markup (srcdoc), parsed as a REAL document, and
    // its relative URLs are resolved against the frame's own base. A frame's
    // parser scripts now execute in the scoped child Window above; its
    // cross-origin parent access is still restricted (`contentDocument` → null
    // from the parent, per the HTML same-origin check).
    function stripFragment(u) { const i = u.indexOf("#"); return i < 0 ? u : u.slice(0, i); }
    // A frame URL is same-origin with the page (about:blank/about:srcdoc
    // inherit the parent origin, so they count as same-origin).
    function frameSameOrigin(url) {
        if (!url || url === "about:srcdoc" || url === "about:blank") return true;
        const u = __url_parse(url, g.location.href);
        return u ? u[8] === g.location.origin : false;
    }
    // Shared attribute processing steps, step 3 — circular-navigation guard: a
    // frame must not load a URL already held by one of its inclusive ancestor
    // navigables (the infinite self-embed the spec forbids). The nested
    // document lives in the same arena, so the parentNode chain walks from the
    // frame up through every ancestor frame element to the top document.
    function frameAncestorHasUrl(frame, url) {
        const target = stripFragment(url);
        if (stripFragment(g.location.href) === target) return true;
        let n = frame.parentNode;
        while (n) {
            const ln = n.localName;
            if ((ln === "iframe" || ln === "frame") && n.__frameUrl &&
                stripFragment(n.__frameUrl) === target) return true;
            n = n.parentNode;
        }
        return false;
    }
    // "iframe load event steps": fire load at the element once its content
    // document has loaded. A macrotask so parent onload / addEventListener
    // handlers attached during the current turn still observe it (same shape
    // as the synthetic image-load pass).
    function fireFrameLoad(frame) {
        // HTML document lifecycle queues the iframe load event steps on the
        // DOM manipulation task source, independently of messaging and timers.
        __queue_dom_task(function () {
            // The nested document's Window also receives its load event. The
            // iframe element's load below is a separate parent-document event;
            // both are observable and child bootstraps commonly wait on the
            // former before creating their interactive surface.
            try { runInFrame(frame, function () { dispatch(g, new Event("load"), false); }); } catch (e) {}
            try { dispatch(frame, new Event("load"), false); } catch (e) {}
        }, 0);
    }
    // Install markup as the frame's content navigable, then process any frames
    // nested inside it (bounded by the circular guard + the page fetch cap).
    function loadFrameMarkup(frame, markup, base, frameUrl) {
        frame.__frameUrl = frameUrl;
        frame.__trustParentWindow = undefined;
        frame.__trustTopWindow = undefined;
        __dom_load_frame(frame.__id, String(markup == null ? "" : markup), base);
        runFrameScripts(frame);
        // Stylesheet links are fetched after parser scripts begin. The
        // nested script may itself create the challenge DOM; delaying this
        // optional resource task keeps a stylesheet failure from aborting the
        // content navigable before its required script runs.
        loadFrameStyles(frame);
        queueFrameNavigationsIn(frame);
    }
    // "Process the iframe attributes". The initialInsertion / re-process cases
    // collapse into one idempotent function: the __loaded* de-dup makes a
    // repeat call for the SAME state a no-op, so the load sweep, the lazy
    // contentDocument getter, and src/srcdoc attribute changes all route here.
    function processIframeAttributes(frame) {
        if (!frame) return;
        const ln = frame.localName;
        if (ln !== "iframe" && ln !== "frame") return;
        // srcdoc takes priority over src (spec).
        const srcdoc = frame.getAttribute("srcdoc");
        if (srcdoc !== null) {
            if (frame.__loadedSrcdoc === srcdoc) return;
            frame.__loadedSrcdoc = srcdoc;
            frame.__loadedSrc = undefined;
            // about:srcdoc: the markup IS the document; base/origin inherit the
            // parent document.
            loadFrameMarkup(frame, srcdoc, g.location.href, "about:srcdoc");
            fireFrameLoad(frame);
            return;
        }
        frame.__loadedSrcdoc = undefined;
        // Shared attribute processing steps → a URL, or null (= about:blank).
        const src = frame.getAttribute("src");
        if (!src || src.trim() === "") { frame.__loadedSrc = undefined; return; }
        const parsed = __url_parse(src, baseHref());
        if (!parsed) return;
        const url = parsed[0];
        if (frame.__loadedSrc === url) return; // already navigated to this src
        // HTML §7.4.2.3.2: a javascript: URL navigates by running its decoded
        // classic-script source in the target navigable. A normal completion
        // whose value is a string replaces the active document with that HTML;
        // other completions leave the active document in place. Navigation
        // initiated by the script itself (for example location.replace(...))
        // is picked up by the ordinary queued src-attribute navigation below.
        if (/^javascript:/i.test(url)) {
            if (frameAncestorHasUrl(frame, url)) return;
            frame.__loadedSrc = url;
            const oldSrc = src;
            let result = null;
            try {
                result = runInFrame(frame, function () {
                    const encoded = String(url).slice("javascript:".length);
                    let source = encoded;
                    try { source = decodeURIComponent(encoded); } catch (e) {}
                    try { return (0, eval)(source); }
                    catch (e) {
                        trust.errors.push("frame javascript URL: " + ((e && e.message) || e));
                        return null;
                    }
                });
            } catch (e) {
                trust.errors.push("frame javascript URL: " + ((e && e.message) || e));
            }
            if (typeof result === "string" && frame.getAttribute("src") === oldSrc) {
                loadFrameMarkup(frame, result, frameBaseURL(frame), frameURLFor(frame));
            }
            fireFrameLoad(frame);
            return;
        }
        // Only http(s) navigables are fetchable here (about:/data:/blob: render
        // nothing for now — a documented deviation).
        if (!/^https?:/i.test(url)) { frame.__loadedSrc = undefined; return; }
        if (frameAncestorHasUrl(frame, url)) return; // circular-navigation guard
        frame.__loadedSrc = url; // set before fetching so a re-sweep won't double-load
        let r;
        try { r = __http_fetch(url, "GET", null, null, null); } catch (e) { r = null; }
        if (!r) { fireFrameLoad(frame); return; }
        const status = r[0] | 0;
        const ctype = String(r[1] || "").toLowerCase();
        const isHtml = ctype === "" || ctype.indexOf("text/html") >= 0 ||
            ctype.indexOf("application/xhtml") >= 0;
        if (status >= 200 && status < 300 && isHtml) {
            loadFrameMarkup(frame, r[2] || "", url, url);
        }
        fireFrameLoad(frame);
    }
    // Process every frame within `root` (the document at load, or a freshly
    // installed frame document for nested frames). Idempotent (the __loaded*
    // de-dup), so re-sweeping is cheap.
    function hydrateFramesIn(root) {
        let frames;
        try { frames = root.querySelectorAll("iframe, frame"); } catch (e) { return 0; }
        for (let i = 0; i < frames.length; i++) {
            try {
                processIframeAttributes(frames[i]);
                loadFrameStyles(frames[i]);
            } catch (e) {}
        }
        return frames.length;
    }
    // Queue nested-navigation work instead of fetching and executing the
    // child document on the DOM-mutating script's stack. The counter remains
    // nonzero until the frame's queued load event has run; nested frames add
    // their own entries before their parent entry retires.
    trust.pendingFrameNavigationTasks = 0;
    trust.hasInitialFramesPending = function () {
        return trust.pendingFrameNavigationTasks > 0;
    };
    function queueFrameNavigation(frame) {
        if (!frame || !frame.isConnected || frame.__trustNavigationQueued) return false;
        if (frame.getAttribute("src") === null && frame.getAttribute("srcdoc") === null)
            return false;
        frame.__trustNavigationQueued = true;
        trust.pendingFrameNavigationTasks++;
        __queue_dom_task(function () {
            frame.__trustNavigationQueued = false;
            try {
                processIframeAttributes(frame);
                loadFrameStyles(frame);
            } catch (e) {}
            // processIframeAttributes queues this frame's load event while
            // it is running. Append retirement afterward so parent load is
            // still delayed through that observable event.
            __queue_dom_task(function () {
                trust.pendingFrameNavigationTasks = Math.max(
                    0, trust.pendingFrameNavigationTasks - 1);
            });
        });
        return true;
    }
    function queueFrameNavigationsIn(root) {
        let frames;
        try { frames = root.querySelectorAll("iframe, frame"); } catch (e) { return 0; }
        for (let i = 0; i < frames.length; i++) {
            queueFrameNavigation(frames[i]);
        }
        return frames.length;
    }
    // Parser-created nested navigables start navigation as parsing encounters
    // them, but the navigation itself proceeds in parallel and completion is
    // delivered through later navigation/DOM-manipulation tasks (HTML
    // "navigate" and "iframe load event steps"). A host whose DOM is already
    // parsed must not collapse that entire tree into the DOMContentLoaded task:
    // a captcha or other cross-origin child would then hide the interactive
    // parent until every descendant script had completed.
    //
    // Resident actors use the pending-navigation counter as the parent
    // document's load-delay condition. One-shot transforms continue to call
    // hydrateFrames() synchronously and settle all queued tasks before
    // serializing.
    trust.queueInitialFrameNavigations = function () {
        queueFrameNavigationsIn(g.document);
    };
    // Lazy realization when a script reads a frame's contentDocument before the
    // load sweep (or for a frame inserted after load). The de-dup guards keep a
    // repeat call cheap; a frame with neither src nor srcdoc stays about:blank.
    function ensureFrameProcessed(frame) {
        if (frame.getAttribute("src") !== null || frame.getAttribute("srcdoc") !== null) {
            try { processIframeAttributes(frame); } catch (e) {}
        }
    }
    trust.hydrateFrames = function () { return hydrateFramesIn(g.document); };
    // The actor's entry points: dispatch a user click; enumerate nodes
    // with click listeners (delegation hosts included — the actor sorts
    // containers from buttons).
    // The submit control at or above `el` (the default action of clicking it
    // is to submit its form). A <button>'s type defaults to "submit";
    // type="button"/"reset" do not submit. <input type=submit|image> too.
    function submitControlFor(el) {
        let n = el;
        while (n && n.nodeType === 1) {
            const tag = n.localName;
            if (tag === "button") return (n.getAttribute("type") || "submit").toLowerCase() === "submit" ? n : null;
            if (tag === "input") { const ty = (n.getAttribute("type") || "").toLowerCase(); return (ty === "submit" || ty === "image") ? n : null; }
            n = n.parentNode;
        }
        return null;
    }
    function resetControlFor(el) {
        let n = el;
        while (n && n.nodeType === 1) {
            const tag = n.localName;
            const type = (n.getAttribute("type") || (tag === "button" ? "submit" : "text")).toLowerCase();
            if ((tag === "button" || tag === "input") && type === "reset") return n;
            n = n.parentNode;
        }
        return null;
    }
    // HTML §4.10.23: fire the cancelable reset event, then restore each
    // resettable element's value/checkedness/selectedness from markup.
    function resetForm(form) {
        const event = new Event("reset", { bubbles: true, cancelable: true });
        dispatch(form, event, false);
        if (event.defaultPrevented) return false;
        const controls = form.querySelectorAll("input, textarea, select");
        for (let i = 0; i < controls.length; i++) {
            const control = controls[i];
            if (control.localName === "select") {
                const options = control.querySelectorAll("option");
                let any = false;
                for (let j = 0; j < options.length; j++) {
                    const selected = control.__trustResetSelected
                        ? !!control.__trustResetSelected[j]
                        : options[j].hasAttribute("selected");
                    options[j].selected = selected;
                    any = any || options[j].selected;
                }
                if (!any && options.length) options[0].selected = true;
            } else if (control.localName === "textarea") {
                control.value = control.__trustResetValue === undefined
                    ? (control.textContent || "")
                    : control.__trustResetValue;
            } else {
                const type = (control.getAttribute("type") || "text").toLowerCase();
                if (type === "checkbox" || type === "radio") {
                    control.checked = control.__trustResetChecked === undefined
                        ? control.hasAttribute("checked")
                        : !!control.__trustResetChecked;
                } else if (! ["button", "submit", "reset", "image", "file"].includes(type)) {
                    control.value = control.__trustResetValue === undefined
                        ? (control.getAttribute("value") || "")
                        : control.__trustResetValue;
                }
            }
        }
        return true;
    }
    // HTML §4.10.22.3: a form whose submitter's method is "dialog" closes
    // its nearest ancestor dialog after the submit event is not canceled.  It
    // is a local dialog result, not a network navigation.  Keep this in the
    // page realm so both user activation and requestSubmit() follow the same
    // default-action algorithm and the dialog's close event/returnValue are
    // observable to page JavaScript.
    function formMethodFor(form, submitter) {
        const attr = submitter && submitter.hasAttribute("formmethod")
            ? submitter.getAttribute("formmethod")
            : form.getAttribute("method");
        const method = String(attr || "get").toLowerCase();
        return method === "post" || method === "dialog" ? method : "get";
    }
    function nearestDialog(form) {
        let p = form && form.parentNode;
        while (p && p.nodeType === 1) {
            if (p.localName === "dialog") return p;
            p = p.parentNode;
        }
        return null;
    }
    function handleDialogSubmission(form, submitter) {
        if (formMethodFor(form, submitter) !== "dialog") return false;
        const subject = nearestDialog(form);
        // The HTML algorithm consumes method=dialog even when no ancestor
        // dialog exists; in that case there is simply no close to perform.
        if (!subject) return true;
        let result = null;
        if (submitter && submitter.localName === "input"
            && String(submitter.type || "").toLowerCase() === "image") {
            result = "0,0";
        } else if (submitter
            && (submitter.localName === "button" || submitter.localName === "input")) {
            result = submitter.value || "";
        }
        subject.close(result);
        return true;
    }
    // Activate an element as a click does: fire a bubbling, cancelable `click`
    // event, then (unless prevented) run the submit-control activation. Shared
    // by the actor's `trust.click` (a real user click, `record` = true so the
    // app learns whether to run the native form submit) and the scripted
    // `Element.prototype.click()` (HTML "fire a synthetic pointer event named
    // click" — `record` = false, it must not clobber the actor's read-once
    // `lastClickSubmit`). The bubbling click is what reaches React's delegated
    // root-container listener, so a programmatic `.click()` finally runs onClick.
    // Popovers currently SHOWING, keyed by node id (the arena set is the
    // render truth; this mirror drives the API logic + auto-closing).
    const POPOVER_OPEN = Object.create(null);
    // A user-interaction click per the specs: Pointer Events makes `click` a
    // PointerEvent; UI Events gives it bubbles + cancelable + COMPOSED (it
    // must escape shadow trees — a listener outside a component hears clicks
    // on its internals, retargeted to the host).
    function syntheticClickEvent(trusted) {
        const init = {
            bubbles: true, cancelable: true, composed: true, view: g,
            detail: 1, button: 0, buttons: 0,
            pointerId: 1, pointerType: "mouse", isPrimary: true,
        };
        return trusted
            ? createTrustedEvent(PointerEvent, "click", init)
            : new PointerEvent("click", init);
    }
    function activateClick(t, record, trusted) {
        if (record) trust.lastClickSubmit = null;
        if (!t) return false;
        // HTML §6.6.2: user activation of a click-focusable area runs the
        // focusing steps. HTMLElement.click() is synthetic and deliberately
        // does not focus; the actor's trusted terminal click does.
        if (trusted && elementCanFocus(t)) focusElement(t, { preventScroll: true });
        const ev = syntheticClickEvent(!!trusted);
        dispatch(t, ev, false);
        if (ev.defaultPrevented) return true;
        // HTML §4.11.2: the first <summary> child of a <details> element has
        // activation behavior that toggles the parent's boolean `open`
        // attribute. This is a default action of the click, so it must run
        // after bubbling listeners (which may cancel it), just like the
        // submit-control actions below.
        if (t.localName === "summary") {
            const parent = t.parentElement;
            if (parent && parent.localName === "details") {
                let firstSummary = null;
                for (const child of parent.children) {
                    if (child.localName === "summary") {
                        firstSummary = child;
                        break;
                    }
                }
                if (firstSummary === t) {
                    if (parent.hasAttribute("open")) parent.removeAttribute("open");
                    else parent.setAttribute("open", "");
                    return true;
                }
            }
        }
        // Popover invoker (HTML §popover target attributes): activating a
        // button with `popovertarget` toggles/shows/hides the target popover
        // — the no-JS popover idiom works in live pages.
        const invoker = (t.localName === "button" || t.localName === "input") ? t
            : (t.closest ? t.closest("button[popovertarget],input[popovertarget]") : null);
        const pt = invoker && invoker.getAttribute && invoker.getAttribute("popovertarget");
        if (pt) {
            const target = document.getElementById(pt);
            if (target && target.popover !== null) {
                const action = String(invoker.getAttribute("popovertargetaction") || "toggle").toLowerCase();
                const open = !!POPOVER_OPEN[target.__id];
                try {
                    if (action === "show") { if (!open) target.showPopover(); }
                    else if (action === "hide") { if (open) target.hidePopover(); }
                    else target.togglePopover();
                } catch (e) {}
                return true;
            }
        }
        const reset = resetControlFor(t);
        if (reset) {
            const form = formOwner(reset);
            if (form) resetForm(form);
            return true;
        }
        // The default action of activating a submit control is to submit its
        // form (HTML). A live <button>/<input type=submit> reaches the app as a
        // JsClick, so without this a click fired only a `click` event and the
        // form's `submit` handler (e.g. React's onSubmit, bound on the <form>)
        // never ran — pixiv's login button did "nothing". Fire a real submit;
        // page JS may preventDefault (then it owns the update) — else the app
        // runs the native GET/POST.
        const btn = submitControlFor(t);
        if (btn) {
            const form = formOwner(btn);
            if (form) {
                const sev = trusted
                    ? createTrustedEvent(Event, "submit", { bubbles: true, cancelable: true })
                    : new Event("submit", { bubbles: true, cancelable: true });
                sev.submitter = btn;
                dispatch(form, sev, false);
                if (record || trust.keyDispatch) trust.lastClickSubmit = { form: form.__id, submitter: btn.__id, prevented: sev.defaultPrevented };
                if (!sev.defaultPrevented) handleDialogSubmission(form, btn);
                return sev.defaultPrevented;
            }
        }
        return false;
    }
    trust.click = function (id) {
        return activateClick(wrap(id), true, true);
    };
    // UI Events §3.5: a native keydown is a cancelable event dispatched at
    // the focused element before the user agent performs its default editing
    // action. Keep a short-lived user-key activation flag so a page handler
    // that calls `sendButton.click()` is still observable by the actor as the
    // submit default of this key, while ordinary script `.click()` calls keep
    // their non-user semantics.
    trust.key = function (id, key, code, repeat, composing, shift, ctrl, alt, meta) {
        const t = wrap(id);
        if (!t) return false;
        trust.lastClickSubmit = null;
        trust.keyDispatch = true;
        let prevented = false;
        try {
            const ev = createTrustedEvent(KeyboardEvent, "keydown", {
                bubbles: true, cancelable: true, composed: true, view: g,
                key: String(key || ""), code: String(code || ""),
                repeat: !!repeat, isComposing: !!composing,
                shiftKey: !!shift, ctrlKey: !!ctrl, altKey: !!alt, metaKey: !!meta,
            });
            dispatch(t, ev, false);
            prevented = ev.defaultPrevented;
        } finally {
            trust.keyDispatch = false;
        }
        return prevented;
    };
    // Fire a load/error event on an injected resource. GlobalEventHandlers
    // backs `onload`/`onerror` with the same listener registry, so dispatch
    // invokes it exactly once; calling the property again here would violate
    // DOM dispatch and double-settle script/style loaders.
    trust.scriptEvent = function (id, type) {
        const t = wrap(id);
        if (!t) return;
        const ev = new Event(type);
        dispatch(t, ev, false);
    };
    trust.clickables = function () {
        const out = [];
        for (const entry of LS) {
            const target = entry[0], m = entry[1];
            if (target instanceof Node && typeof target.__id === "number") {
                const l = m.get("click");
                if (l && l.length) out.push(target.__id);
            }
        }
        return out;
    };
    // ---- hover (Pointer Events spec, which absorbed UI Events' mouse order) ----
    // The terminal's pointer (mouse cursor or the gopherus selection) rests on
    // ONE element at a time; the actor delivers committed target changes via
    // trust.hover. Transition sequence on old→new: pointerout/mouseout on old
    // (bubbling), pointerleave/mouseleave per element bottom-up from old to the
    // exclusive common ancestor (NON-bubbling, non-cancelable), pointerover/
    // mouseover on new (bubbling), pointerenter/mouseenter top-down from below
    // the common ancestor to new, then pointermove/mousemove on new.
    // relatedTarget = the other element of the pair. Every event object is
    // FRESH — dispatch mutates ev.target, and __stop/defaultPrevented persist
    // on the object, so reuse across a chain would corrupt the sequence.
    let hoverTarget = null;
    // The composed ancestor path (target-first), the same parentNode/__host
    // walk dispatch() bubbles along, so shadow boundaries hop identically.
    function hoverPath(t) {
        const path = [];
        let n = t;
        while (n && n !== g) { path.push(n); n = n.parentNode || n.__host; }
        return path;
    }
    // One pointer/mouse compat pair. over/out/move bubble and are cancelable;
    // enter/leave are neither (Pointer Events event tables).
    function fireHoverPair(name, target, related, bubbling, x, y) {
        const init = {
            bubbles: bubbling, cancelable: bubbling, composed: bubbling,
            clientX: x, clientY: y,
            pageX: x + (g.scrollX || 0), pageY: y + (g.scrollY || 0),
            screenX: x, screenY: y, button: 0, buttons: 0,
            relatedTarget: related, view: g, detail: 0,
        };
        const pinit = Object.assign({ pointerId: 1, pointerType: "mouse", isPrimary: true }, init);
        dispatch(target, new PointerEvent("pointer" + name, pinit), false);
        dispatch(target, new MouseEvent("mouse" + name, init), false);
    }
    trust.hover = function (id, x, y) {
        // A stale id (the node was detached since the snapshot the app hit-test
        // ran against) wraps to null — degrade to hover-clear, never an error.
        const t = id === null || id === undefined ? null : wrap(id);
        x = +x || 0; y = +y || 0;
        if (t !== hoverTarget) {
            const old = hoverTarget;
            hoverTarget = t;
            const oldPath = old ? hoverPath(old) : [];
            const newPath = t ? hoverPath(t) : [];
            // Trim the shared root suffix: what remains on each side is the
            // chain strictly BELOW the nearest common ancestor.
            let oi = oldPath.length - 1;
            let ni = newPath.length - 1;
            while (oi >= 0 && ni >= 0 && oldPath[oi] === newPath[ni]) { oi--; ni--; }
            if (old) {
                fireHoverPair("out", old, t, true, x, y);
                for (let i = 0; i <= oi; i++) fireHoverPair("leave", oldPath[i], t, false, x, y);
            }
            if (t) {
                fireHoverPair("over", t, old, true, x, y);
                for (let i = ni; i >= 0; i--) fireHoverPair("enter", newPath[i], old, false, x, y);
            }
        }
        // UI Events §3.4.5.8 and Pointer Events require motion events when the
        // pointing device moves even if hit testing retains the same target.
        // The native lane may coalesce samples, but a target transition is not
        // the condition for `pointermove`/`mousemove` dispatch.
        if (t) fireHoverPair("move", t, null, true, x, y);
        // The CSS half: the cascade's :hover chain follows the same committed
        // target (Phase B syscall; guarded so the JS half stands alone).
        if (typeof __dom_set_hover === "function") __dom_set_hover(t ? t.__id : -1);
        return true;
    };
    // The nodes holding any hover-type listener — the serializer marks them
    // (data-trust-hover) so the app can resolve a hover target back to this
    // arena. Delegation needs no descendant marks: the bubbling over/out pair
    // reaches ancestor listeners from whatever target the app resolves.
    const HOVER_TYPES = [
        "mouseover", "mouseout", "mouseenter", "mouseleave", "mousemove",
        "pointerover", "pointerout", "pointerenter", "pointerleave", "pointermove",
    ];
    trust.hoverables = function () {
        const out = [];
        for (const entry of LS) {
            const target = entry[0], m = entry[1];
            if (target instanceof Node && typeof target.__id === "number") {
                for (let i = 0; i < HOVER_TYPES.length; i++) {
                    const l = m.get(HOVER_TYPES[i]);
                    if (l && l.length) { out.push(target.__id); break; }
                }
            }
        }
        return out;
    };
    function nearestForm(el) {
        let p = el;
        while (p) {
            if (p.localName === "form") return p;
            p = p.parentNode;
        }
        return null;
    }
    // HTML form-owner reset/association rules, including controls explicitly
    // associated through `form=id` rather than nested in the form. Activation,
    // requestSubmit(), and the live controls collection share this helper.
    function formOwner(el) {
        if (!el || el.nodeType !== 1) return null;
        const explicit = el.getAttribute("form");
        if (explicit !== null) {
            const owner = g.document && g.document.getElementById(explicit);
            return owner && owner.localName === "form" ? owner : null;
        }
        return nearestForm(el.parentNode);
    }
    function listedFormControls(form) {
        if (!form || !g.document) return [];
        return g.document
            .querySelectorAll("button,fieldset,input,object,output,select,textarea")
            .filter(function (el) {
                // input[type=image] is form-associated but expressly excluded
                // from HTMLFormElement.elements.
                return !(el.localName === "input" && String(el.type || "").toLowerCase() === "image")
                    && formOwner(el) === form;
            });
    }
    function controlWillValidate(el) {
        const type = el.localName === "input" ? String(el.type || "text").toLowerCase() : "";
        return !(el.hasAttribute("disabled")
            || el.hasAttribute("readonly")
            || (el.localName === "input" && ["hidden", "button", "reset", "submit", "image"].includes(type)));
    }
    function controlValidity(el) {
        const type = el.localName === "input" ? String(el.type || "text").toLowerCase() : "";
        const barred = !controlWillValidate(el);
        let valueMissing = false;
        if (!barred && el.hasAttribute("required")) {
            if (type === "checkbox") {
                valueMissing = !el.checked;
            } else if (type === "radio") {
                const owner = formOwner(el);
                const name = el.getAttribute("name") || "";
                valueMissing = !g.document.querySelectorAll("input").some(function (radio) {
                    return String(radio.type).toLowerCase() === "radio"
                        && (radio.getAttribute("name") || "") === name
                        && formOwner(radio) === owner
                        && radio.checked;
                });
            } else {
                valueMissing = String(el.value || "") === "";
            }
        }
        const customError = !!el.__trustValidationMessage;
        return {
            valueMissing: valueMissing, customError: customError,
            typeMismatch: false, patternMismatch: false, tooLong: false, tooShort: false,
            rangeUnderflow: false, rangeOverflow: false, stepMismatch: false, badInput: false,
            valid: !valueMissing && !customError,
        };
    }
    function installConstraintValidation(C) {
        Object.defineProperties(C.prototype, {
            willValidate: { configurable: true, get() { return controlWillValidate(this); } },
            validity: { configurable: true, get() { return controlValidity(this); } },
            validationMessage: { configurable: true, get() {
                if (this.__trustValidationMessage) return this.__trustValidationMessage;
                return controlValidity(this).valueMissing ? "Please fill out this field." : "";
            }},
        });
        C.prototype.setCustomValidity = function (message) { this.__trustValidationMessage = String(message); };
        C.prototype.checkValidity = function () {
            if (controlValidity(this).valid) return true;
            dispatch(this, new Event("invalid", { cancelable: true }), false);
            return false;
        };
        C.prototype.reportValidity = C.prototype.checkValidity;
    }
    function fireFormEvents(el, withClick) {
        // Toggling a checkbox/radio dispatches a click as part of the user
        // activation, BEFORE input/change. It matters: React detects
        // checkbox/radio changes off the CLICK event (its change plugin's
        // shouldUseClickEvent path), not input/change, so without this a
        // controlled checkbox never fires onChange. The checked value is
        // already set, so listeners read the post-toggle state.
        if (withClick) dispatch(el, syntheticClickEvent(), false);
        // HTML: `input` is composed (it crosses shadow boundaries); `change`
        // is not.
        dispatch(el, new Event("input", { bubbles: true, composed: true }), false);
        dispatch(el, new Event("change", { bubbles: true }), false);
    }
    // Set a control property as a USER edit would, NOT a script write.
    // Frameworks (React, Vue, Preact) install an instance-level "value
    // tracker" — an own getter/setter that shadows the prototype's — and
    // suppress their onChange when the new value matches what the tracker
    // last saw. A plain `el.value = x` goes THROUGH that tracker, so the
    // change looks like a no-op and onChange never fires. Walking to the
    // prototype accessor and invoking its setter bypasses the instance
    // tracker (the same trick React Testing Library / Enzyme use), so the
    // following input/change event registers as a genuine user change.
    // With no tracker installed this is identical to `el[prop] = value`.
    function nativeSet(el, prop, value) {
        let p = Object.getPrototypeOf(el);
        while (p) {
            const d = Object.getOwnPropertyDescriptor(p, prop);
            if (d) {
                if (typeof d.set === "function") { d.set.call(el, value); return; }
                break;
            }
            p = Object.getPrototypeOf(p);
        }
        el[prop] = value;
    }
    // A truthy `contenteditable` attribute marks an editing host (the editor
    // root). Mirrors `Dom::is_contenteditable_host` so both sides agree on which
    // element the edit targets.
    function ceHost(el) {
        if (!el || el.nodeType !== 1 || !el.hasAttribute("contenteditable")) return false;
        const v = (el.getAttribute("contenteditable") || "").trim().toLowerCase();
        return v === "" || v === "true" || v === "plaintext-only";
    }
    trust.formSet = function (id, value, checked) {
        const el = wrap(id);
        if (!el) return false;
        value = value === null || value === undefined ? "" : String(value);
        // A contenteditable host edits like a field but isn't a form control:
        // drive it with the real editing algorithm — a cancelable `beforeinput`,
        // then (unless the editor handled it) replace the content and fire
        // `input`. A rich editor (ProseMirror/TipTap) that preventDefaults owns
        // the change; a plain editable, or one that reconciles from DOM
        // mutations (its MutationObserver), takes our content + input event.
        if (ceHost(el)) {
            const bev = new InputEvent("beforeinput", { bubbles: true, cancelable: true, composed: true, inputType: "insertText", data: value });
            dispatch(el, bev, false);
            if (bev.defaultPrevented) return true;
            if (el.textContent === value) return false;
            el.textContent = value;
            dispatch(el, new InputEvent("input", { bubbles: true, composed: true, inputType: "insertText", data: value }), false);
            return true;
        }
        const tag = el.localName;
        const type = String(el.type || "").toLowerCase();
        const isToggle = tag === "input" && (type === "checkbox" || type === "radio");
        let changed = false;
        if (isToggle) {
            if (el.__trustResetChecked === undefined) el.__trustResetChecked = el.hasAttribute("checked");
            const want = !!checked;
            if (type === "radio" && want && el.name) {
                const scope = nearestForm(el) || g.document;
                for (const r of scope.querySelectorAll("input")) {
                    if (r !== el && String(r.type || "").toLowerCase() === "radio" && r.name === el.name && r.checked) {
                        if (r.__trustResetChecked === undefined) r.__trustResetChecked = r.hasAttribute("checked");
                        nativeSet(r, "checked", false);
                        changed = true;
                    }
                }
            }
            if (el.checked !== want) { nativeSet(el, "checked", want); changed = true; }
        } else if (tag === "select") {
            if (!el.__trustResetSelected) {
                el.__trustResetSelected = el.querySelectorAll("option").map(o => o.hasAttribute("selected"));
            }
            for (const o of el.querySelectorAll("option")) {
                const ov = o.getAttribute("value") === null ? o.textContent : o.getAttribute("value");
                const want = ov === value;
                if (want !== o.hasAttribute("selected")) {
                    if (want) o.setAttribute("selected", "");
                    else o.removeAttribute("selected");
                    changed = true;
                }
            }
        } else if (tag === "textarea") {
            if (el.__trustResetValue === undefined) el.__trustResetValue = el.textContent;
            if (el.textContent !== value) { el.textContent = value; changed = true; }
        } else {
            if (el.__trustResetValue === undefined) el.__trustResetValue = el.getAttribute("value") || "";
            if (el.value !== value) { nativeSet(el, "value", value); changed = true; }
        }
        if (changed) fireFormEvents(el, isToggle);
        return changed;
    };
    trust.formSubmit = function (formId, submitterId) {
        const form = wrap(formId);
        if (!form) return false;
        const ev = new SubmitEvent("submit", {
            bubbles: true,
            cancelable: true,
            submitter: submitterId === null || submitterId === undefined ? null : wrap(submitterId),
        });
        dispatch(form, ev, false);
        if (!ev.defaultPrevented && handleDialogSubmission(form, ev.submitter)) return true;
        return ev.defaultPrevented;
    };
    // requestSubmit() runs synchronously in JS through the submit event, then
    // plans a browsing-context navigation. The Rust page actor drains this
    // read-once signal only after serializing any handler/control mutations.
    trust.queueFormSubmit = function (formId, submitterId) {
        trust.pendingFormSubmit = { form: formId, submitter: submitterId };
    };
    trust.takeFormSubmit = function () {
        const s = trust.pendingFormSubmit;
        trust.pendingFormSubmit = null;
        return s ? (String(s.form) + "," + (s.submitter === null ? "" : String(s.submitter))) : "";
    };
    // Construct the HTML form entry list inside the resident page realm. The
    // render serializer intentionally omits display:none/hidden DOM, so asking
    // the app's painted document to reconstruct this data loses exactly the
    // verification/payment-style forms that are commonly hidden.
    trust.formSubmission = function (formId, submitterId) {
        const form = wrap(formId);
        if (!form || form.localName !== "form") return "";
        const submitter = submitterId === null || submitterId === undefined ? null : wrap(submitterId);
        const params = new URLSearchParams();
        function disabled(control) {
            if (control.hasAttribute("disabled")) return true;
            let parent = control.parentNode;
            while (parent && parent !== form) {
                if (parent.localName === "fieldset" && parent.hasAttribute("disabled")) {
                    let firstLegend = null;
                    for (const child of parent.children) {
                        if (child.localName === "legend") { firstLegend = child; break; }
                    }
                    if (!firstLegend || !firstLegend.contains(control)) return true;
                }
                parent = parent.parentNode;
            }
            return false;
        }
        function optionDisabled(option) {
            return option.hasAttribute("disabled")
                || (option.parentNode && option.parentNode.localName === "optgroup"
                    && option.parentNode.hasAttribute("disabled"));
        }
        for (const control of listedFormControls(form)) {
            if (disabled(control)) continue;
            const tag = control.localName;
            if (tag === "fieldset" || tag === "object" || tag === "output") continue;
            const name = control.getAttribute("name") || "";
            if (!name) continue;
            if (tag === "button") {
                if (control !== submitter) continue;
                params.append(name, control.value || "");
                continue;
            }
            if (tag === "select") {
                let appended = false;
                for (const option of control.options) {
                    if (option.selected && !optionDisabled(option)) {
                        params.append(name, option.value);
                        appended = true;
                    }
                }
                // A non-multiple select with no explicit selectedness has its
                // first option selected by default.
                if (!appended && !control.multiple && control.options.length) {
                    const option = control.options[0];
                    if (!optionDisabled(option)) params.append(name, option.value);
                }
                continue;
            }
            if (tag === "input") {
                const type = String(control.type || "text").toLowerCase();
                if (type === "button" || type === "reset" || type === "file" || type === "image") continue;
                if (type === "submit") {
                    if (control !== submitter) continue;
                } else if ((type === "checkbox" || type === "radio") && !control.checked) {
                    continue;
                }
                params.append(name, control.value || ((type === "checkbox" || type === "radio") ? "on" : ""));
                continue;
            }
            if (tag === "textarea") params.append(name, control.value);
        }
        // input[type=image] is excluded from form.elements but can be an
        // explicit submitter; a programmatic activation has coordinates 0,0.
        if (submitter && submitter.localName === "input" && String(submitter.type).toLowerCase() === "image") {
            const name = submitter.getAttribute("name") || "";
            params.append(name ? name + ".x" : "x", "0");
            params.append(name ? name + ".y" : "y", "0");
        }
        const actionAttr = submitter && submitter.hasAttribute("formaction")
            ? submitter.getAttribute("formaction")
            : form.getAttribute("action");
        const methodAttr = submitter && submitter.hasAttribute("formmethod")
            ? submitter.getAttribute("formmethod")
            : form.getAttribute("method");
        let method = String(methodAttr || "get").toLowerCase();
        if (method !== "post" && method !== "dialog") method = "get";
        let action;
        try { action = new URL(actionAttr || g.location.href, g.location.href).href; }
        catch (e) { action = g.location.href; }
        return JSON.stringify({ action: action, method: method, body: params.toString() });
    };

    // --- the DOM classes over the syscall boundary ---
    // Custom-element upgrades return the element being upgraded from
    // the base constructor (the standard polyfill trick), so
    // `class X extends HTMLElement { constructor(){ super(); ... } }`
    // initializes the EXISTING wrapper.
    const CE = { defs: new Map(), tags: new Map(), waiting: new Map(), upgrading: null };
    // EventTarget is the root of the node + window hierarchy (Node and Window
    // both extend it), so the spec's listener methods live here ONCE and
    // everything inherits them. It must be declared before Node/Window (class
    // bindings aren't hoisted). Polyfills save/augment the "native" OFF this
    // prototype — ShadyDOM does `L(EventTarget.prototype,"addEventListener")`
    // and installs `__shady_*` accessors here — so nodes inherit those too.
    // (`lsFor`/`dispatch` are hoisted function declarations, defined above.)
    class EventTarget {
        addEventListener(type, fn, options) {
            // Functions AND `{ handleEvent }` objects (Lit's EventParts register
            // themselves as listeners). `addL` validates + honors the options
            // dict (capture/once/signal — see lsOpts).
            if (typeof fn === "function" || (fn && typeof fn.handleEvent === "function")) {
                addL(this, type, fn, options);
                // A per-element `scroll` listener (an inner-scroll region's un-pin
                // handler) keeps the page resident so the wheel write-back can
                // fire it (see `trust.hasScrollWork`). Window/document scroll is
                // tracked separately via the listener map.
                if (String(type) === "scroll" && this !== g.document) g.__elScroll = true;
            }
        }
        removeEventListener(type, fn, options) { removeL(this, type, fn, options); }
        dispatchEvent(ev) {
            // DOM §2.7 dispatchEvent() always makes the event untrusted,
            // including when a UA-created event is redispatched by script.
            trustedEvents.delete(ev);
            return dispatch(this, ev, false);
        }
    }
    class Node extends EventTarget {
        constructor(id) {
            super();
            if (CE.upgrading !== null) {
                const target = CE.upgrading;
                CE.upgrading = null;
                return target;
            }
            if (id === undefined && CE.tags.size) {
                // `new MyElement()` on a registered class: the platform
                // creates the element (routers mount pages this way).
                let c = new.target;
                while (c) {
                    const tag = CE.tags.get(c);
                    if (tag) {
                        this.__id = __dom_create_element(tag);
                        this.__ceUpgraded = true;
                        W.set(this.__id, this);
                        return;
                    }
                    c = Object.getPrototypeOf(c);
                }
            }
            this.__id = id;
        }
        get nodeType() { return __dom_node_type(this.__id); }
        get nodeName() {
            const t = __dom_tag(this.__id);
            if (t) return t.toUpperCase();
            const n = this.nodeType;
            return n === 3 ? "#text" : n === 9 ? "#document" : n === 8 ? "#comment" : n === 11 ? "#document-fragment" : "#node";
        }
        // DOM §4.4 Node.baseURI: every node reports the serialized document
        // base URL of its node document. The page scope's baseHref() follows
        // HTML's document-base algorithm, including the first <base href>.
        get baseURI() { return baseHref(); }
        get parentNode() { return wrap(__dom_parent(this.__id)); }
        get parentElement() { const p = this.parentNode; return p && p.nodeType === 1 ? p : null; }
        get childNodes() { return __dom_children(this.__id).map(wrap); }
        get children() { return this.childNodes.filter((n) => n.nodeType === 1); }
        get firstChild() { const c = __dom_children(this.__id); return c.length ? wrap(c[0]) : null; }
        get lastChild() { const c = __dom_children(this.__id); return c.length ? wrap(c[c.length - 1]) : null; }
        get firstElementChild() { return this.children[0] || null; }
        get lastElementChild() { const c = this.children; return c[c.length - 1] || null; }
        get childElementCount() { return this.children.length; }
        get nextSibling() { return wrap(__dom_next(this.__id)); }
        get previousSibling() { return wrap(__dom_prev(this.__id)); }
        get nextElementSibling() { let s = this.nextSibling; while (s && s.nodeType !== 1) s = s.nextSibling; return s; }
        get previousElementSibling() { let s = this.previousSibling; while (s && s.nodeType !== 1) s = s.previousSibling; return s; }
        get textContent() { return __dom_text(this.__id); }
        set textContent(v) {
            v = v === null || v === undefined ? "" : String(v);
            if (!MO.length) { __dom_set_text(this.__id, v); slotQueueCheck(this); return; }
            const t = this.nodeType;
            if (t === 3 || t === 8) { const old = __dom_text(this.__id); __dom_set_text(this.__id, v); moCharData(this, old); return; }
            // On an element, textContent replaces all children with one text node.
            const removed = this.childNodes;
            __dom_set_text(this.__id, v);
            moChildBulk(this, removed, this.childNodes);
            slotQueueCheck(this);
        }
        get nodeValue() { const t = this.nodeType; return t === 3 || t === 8 ? __dom_text(this.__id) : null; }
        set nodeValue(v) {
            const t = this.nodeType;
            if (t !== 3 && t !== 8) return;
            v = String(v);
            if (!MO.length) { __dom_set_text(this.__id, v); return; }
            const old = __dom_text(this.__id);
            __dom_set_text(this.__id, v);
            moCharData(this, old);
        }
        // NOTE: `data` is deliberately NOT here. Per the DOM spec it is a
        // CharacterData-only IDL attribute (Text/Comment/ProcessingInstruction),
        // defined on the CharacterData class below. Putting it on Node leaks
        // `data` onto EVERY element, so `"data" in someDiv` is wrongly true —
        // which fools `prop in element` feature tests. YouTube's polymer_resin
        // builds a reference element and does `if ("data" in refEl)` to decide
        // whether a `data` PROPERTY binding is a known DOM sink (sanitize) or a
        // safe custom-element property (pass through); the leaked `data` made it
        // sanitize ytd-* renderers' `.data` object to its innocuous sentinel, so
        // the whole search-results tree never received its data and stayed empty.
        // Node.ownerDocument is the arena's explicit node document.  This is
        // observable for DOMParser trees before and after adoptNode; returning
        // the live document for every node made cross-document adoption
        // indistinguishable from a no-op.
        get ownerDocument() { return wrap(__dom_owner_document(this.__id)); }
        get isConnected() {
            return !!__dom_is_connected(this.__id);
        }
        getRootNode() { let n = this; while (n.parentNode) n = n.parentNode; return n; }
        // Document.adoptNode is defined on Document, below.  Keeping the
        // operation there preserves the DOM's target-document semantics.
        appendChild(c) {
            if (c && c.nodeType === 11 && !c.__host) { for (const k of c.childNodes) this.appendChild(k); return c; }
            // Pre-insertion validity (WHATWG DOM §4.2.3): the syscall refuses
            // (returns false, unmutated) when `c` is an inclusive ancestor.
            if (!__dom_append(this.__id, c.__id)) throw new DOMException("The new child element contains the parent.", "HierarchyRequestError");
            slotQueueCheck(this);
            if (MO.length) moChildInsert(this, c);
            if (CE.defs.size) ceScan(c);
            maybeRunScript(c);
            maybeLoadStylesheet(c);
            if (c.__trustLN === "base") baseHrefCache = null; // maybeRunScript already read .localName
            else if (c.__trustLN === "iframe" || c.__trustLN === "frame") maybeProcessInsertedFrame(c, this);
            return c;
        }
        insertBefore(c, ref) {
            if (c && c.nodeType === 11 && !c.__host) { for (const k of c.childNodes) this.insertBefore(k, ref); return c; }
            const insertion = __dom_insert_before(this.__id, c.__id, ref ? ref.__id : null);
            if (insertion === -1) throw new DOMException("The reference node is not a child of this node.", "NotFoundError");
            if (!insertion) throw new DOMException("The new child element contains the parent.", "HierarchyRequestError");
            slotQueueCheck(this);
            if (MO.length) moChildInsert(this, c);
            if (CE.defs.size) ceScan(c);
            maybeRunScript(c);
            maybeLoadStylesheet(c);
            if (c.__trustLN === "base") baseHrefCache = null;
            else if (c.__trustLN === "iframe" || c.__trustLN === "frame") maybeProcessInsertedFrame(c, this);
            return c;
        }
        removeChild(c) {
            // DOM §4.2.3 pre-remove: validate before mutation-observer or custom-element side
            // effects. A node belonging to some other parent is not silently detached.
            if (!c || c.parentNode !== this) throw new DOMException("The node to be removed is not a child of this node.", "NotFoundError");
            if (c.__trustLN === "base") baseHrefCache = null;
            if (MO.length) moChildRemove(this, c);
            if (CE.defs.size) ceDisconnect(c);
            __dom_detach(c.__id);
            slotQueueCheck(this);
            return c;
        }
        replaceChild(n, old) {
            const prev = old.previousSibling, next = old.nextSibling;
            // Validity (WHATWG DOM §4.2.3) before any side effect: the insert
            // syscall refuses (unmutated) when `n` is an inclusive ancestor.
            const insertion = __dom_insert_before(this.__id, n.__id, old.__id);
            if (insertion === -1) throw new DOMException("The node to be replaced is not a child of this node.", "NotFoundError");
            if (!insertion) throw new DOMException("The new child element contains the parent.", "HierarchyRequestError");
            if (CE.defs.size) ceDisconnect(old);
            __dom_detach(old.__id);
            slotQueueCheck(this);
            if (MO.length) moNotify({ type: "childList", target: this, addedNodes: [n],
                removedNodes: [old], previousSibling: prev, nextSibling: next });
            if (CE.defs.size) ceScan(n);
            maybeRunScript(n);
            maybeLoadStylesheet(n);
            if (n.__trustLN === "base" || old.__trustLN === "base") baseHrefCache = null;
            else if (n.__trustLN === "iframe" || n.__trustLN === "frame") maybeProcessInsertedFrame(n, this);
            return old;
        }
        remove() { if (this.__trustLN === "base") baseHrefCache = null; const p = this.parentNode; if (p && MO.length) moChildRemove(p, this); if (CE.defs.size) ceDisconnect(this); __dom_detach(this.__id); slotQueueCheck(p); }
        append(...ns) { for (const n of ns) this.appendChild(n && typeof n === "object" ? n : g.document.createTextNode(String(n))); }
        prepend(...ns) { const f = this.firstChild; for (const n of ns) this.insertBefore(n && typeof n === "object" ? n : g.document.createTextNode(String(n)), f); }
        // The ChildNode mixin: lit's svg templates go through
        // replaceWith in the Template constructor.
        before(...ns) { const p = this.parentNode; if (!p) return; for (const n of ns) p.insertBefore(n && typeof n === "object" ? n : g.document.createTextNode(String(n)), this); }
        after(...ns) { const p = this.parentNode; if (!p) return; const r = this.nextSibling; for (const n of ns) p.insertBefore(n && typeof n === "object" ? n : g.document.createTextNode(String(n)), r); }
        replaceWith(...ns) { this.before(...ns); this.remove(); }
        replaceChildren(...ns) { let c; while ((c = this.firstChild)) this.removeChild(c); this.append(...ns); }
        cloneNode(deep) {
            const clone = wrap(__dom_clone(this.__id, !!deep));
            // Cloning creates a fresh script element rather than a
            // parser-inserted one, so its force-async flag starts true.
            if (clone instanceof HTMLScriptElement) clone.__trustForceAsync = true;
            return clone;
        }
        contains(o) { while (o) { if (o === this) return true; o = o.parentNode; } return false; }
        isSameNode(o) { return o === this; }
        // WHATWG DOM §4.4 "equals": same interface (nodeType), the type's own
        // fields equal (element: namespace/local-name/attributes; text/comment:
        // data; doctype: name), and children equal pairwise in order. Attribute
        // order does NOT matter (equal counts + each A-attr present-and-equal in
        // B ⇒ sets match, names being unique). react-helmet dedupes head tags
        // with `newTag.isEqualNode(oldTag)`, from a timer — a missing method
        // aborted that reconciliation on every React-Helmet page.
        isEqualNode(o) {
            if (!o) return false;
            if (o === this) return true;
            const t = this.nodeType;
            if (t !== o.nodeType) return false;
            if (t === 1) {
                if (this.localName !== o.localName) return false;
                if (this.namespaceURI !== o.namespaceURI) return false;
                const an = this.getAttributeNames();
                if (an.length !== o.getAttributeNames().length) return false;
                for (let i = 0; i < an.length; i++)
                    if (this.getAttribute(an[i]) !== o.getAttribute(an[i])) return false;
            } else if (t === 3 || t === 8) {
                if (this.nodeValue !== o.nodeValue) return false;
            } else if (t === 10) {
                if (this.nodeName !== o.nodeName) return false;
            }
            const ac = this.childNodes, bc = o.childNodes;
            if (ac.length !== bc.length) return false;
            for (let i = 0; i < ac.length; i++)
                if (!ac[i].isEqualNode(bc[i])) return false;
            return true;
        }
        hasChildNodes() { return __dom_children(this.__id).length > 0; }
        // DOM §4.4: the position of `other` relative to this node, as a
        // bitmask. Was a stub returning 0 — which means "same node", so any
        // caller branching on the bits (ordered insertion, focus traversal)
        // got nonsense for every pair. Chains use parentNode (the node tree;
        // the spec does not compose shadows here). Disconnected pairs get the
        // spec's implementation-specific-but-CONSISTENT order (arena ids are
        // stable for the page's life).
        compareDocumentPosition(other) {
            if (!(other instanceof Node)) throw new TypeError("Failed to execute 'compareDocumentPosition': parameter 1 is not of type 'Node'");
            if (other === this) return 0;
            const chain = (n) => { const c = [n]; let p = n.parentNode; while (p) { c.push(p); p = p.parentNode; } return c; };
            const a = chain(this), b = chain(other);
            if (a[a.length - 1] !== b[b.length - 1]) {
                return 1 /* DISCONNECTED */ + 32 /* IMPLEMENTATION_SPECIFIC */
                    + (other.__id > this.__id ? 4 /* FOLLOWING */ : 2 /* PRECEDING */);
            }
            if (b.indexOf(this) >= 0) return 16 + 4;  // other is CONTAINED_BY this (and follows)
            if (a.indexOf(other) >= 0) return 8 + 2;  // other CONTAINS this (and precedes)
            // Walk down from the shared root to the deepest common ancestor;
            // the divergent children's sibling order decides.
            let i = a.length - 1, j = b.length - 1;
            while (i > 0 && j > 0 && a[i - 1] === b[j - 1]) { i--; j--; }
            const kids = a[i].childNodes;
            for (let k = 0; k < kids.length; k++) {
                if (kids[k] === a[i - 1]) return 4;   // our branch first → other FOLLOWING
                if (kids[k] === b[j - 1]) return 2;   // their branch first → other PRECEDING
            }
            return 1 + 32 + 4; // unreachable (both branches are children of a[i])
        }
        normalize() {}
        // addEventListener/removeEventListener/dispatchEvent are inherited from
        // EventTarget.prototype now (Node extends EventTarget, per spec). Keeping
        // them solely there means a polyfill that augments EventTarget.prototype
        // (ShadyDOM's `__shady_*` accessors) is visible on every node too.
    }
    Node.ELEMENT_NODE = 1; Node.TEXT_NODE = 3; Node.COMMENT_NODE = 8;
    Node.DOCUMENT_NODE = 9; Node.DOCUMENT_FRAGMENT_NODE = 11;
    Node.DOCUMENT_POSITION_DISCONNECTED = 1; Node.DOCUMENT_POSITION_PRECEDING = 2;
    Node.DOCUMENT_POSITION_FOLLOWING = 4; Node.DOCUMENT_POSITION_CONTAINS = 8;
    Node.DOCUMENT_POSITION_CONTAINED_BY = 16;
    Node.DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC_ORDER = 32;

    function makeStyle() {
        return {
            cssText: "",
            setProperty(k, v) { this[k] = String(v); },
            getPropertyValue(k) { return typeof this[k] === "string" ? this[k] : ""; },
            removeProperty(k) { const v = this[k]; delete this[k]; return typeof v === "string" ? v : ""; },
        };
    }
    const kebab = (s) => s.replace(/[A-Z]/g, (m) => "-" + m.toLowerCase());
    // CSS Values 4 §6: a non-zero <length> requires a unit. CSSOM §6.7.1
    // parses a value against the property's grammar before mutating its
    // declaration block, so `el.style.height = 768` is ignored rather than
    // becoming 768px. This is intentionally the same property family guarded
    // by Rust's declaration parser: script and authored CSS must cascade alike.
    const unitlessNonzeroLengthProperties = new Set([
        "width", "min-width", "max-width", "height", "min-height", "max-height",
        "inline-size", "min-inline-size", "max-inline-size",
        "block-size", "min-block-size", "max-block-size",
        "margin", "margin-top", "margin-right", "margin-bottom", "margin-left",
        "margin-inline", "margin-block", "margin-inline-start", "margin-inline-end",
        "margin-block-start", "margin-block-end",
        "padding", "padding-top", "padding-right", "padding-bottom", "padding-left",
        "padding-inline", "padding-block", "padding-inline-start", "padding-inline-end",
        "padding-block-start", "padding-block-end",
        "inset", "inset-inline", "inset-block", "inset-inline-start", "inset-inline-end",
        "inset-block-start", "inset-block-end", "top", "right", "bottom", "left",
        "gap", "row-gap", "column-gap", "column-width", "flex-basis", "font-size",
        "letter-spacing", "word-spacing", "text-indent", "vertical-align",
        "border-width", "border-top-width", "border-right-width", "border-bottom-width",
        "border-left-width", "border-radius", "border-top-left-radius",
        "border-top-right-radius", "border-bottom-right-radius", "border-bottom-left-radius",
        "outline-width", "outline-offset", "background-position", "background-size",
        "object-position", "transform-origin", "translate", "box-shadow", "text-shadow"
    ]);
    const bareNonzeroCssNumber = (token) => {
        token = token.replace(/^[,/]|[,/]$/g, "").trim();
        return /^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?$/i.test(token)
            && Number(token) !== 0;
    };
    const acceptsStyleValue = (property, value) => {
        property = String(property).toLowerCase();
        value = String(value).trim();
        if (!unitlessNonzeroLengthProperties.has(property)) return true;
        // Parenthesized function tokens are not split, so their internal
        // scalar numbers are handled by the function grammar.
        return !value.split(/\s+/).some(bareNonzeroCssNumber);
    };
    // el.style is backed by the REAL style attribute: writes are DOM
    // mutations (dirty bit, serialized, visibility honored). CSSOM §6.6.1's
    // declaration-block creation/attribute-change steps keep ONE parsed block
    // synchronized with its owner node; reparsing on every property GET is not
    // part of those algorithms (and makes framework style reads quadratic).
    // Keep the parsed block until the exact attribute text changes. All live
    // attribute writes funnel through setAttribute/removeAttribute, and the
    // raw-text comparison also covers writes made outside this proxy.
    function styleFor(el) {
        let parsedRaw;
        let parsedMap;
        const parse = () => {
            const raw = el.getAttribute("style") || "";
            if (parsedMap !== undefined && raw === parsedRaw) return parsedMap;
            const m = Object.create(null);
            for (const part of raw.split(";")) {
                const i = part.indexOf(":");
                if (i > 0) {
                    const key = part.slice(0, i).trim().toLowerCase();
                    const value = part.slice(i + 1).trim();
                    if (acceptsStyleValue(key, value)) m[key] = value;
                }
            }
            parsedRaw = raw;
            parsedMap = m;
            return parsedMap;
        };
        // CSSStyleDeclaration mutation methods change THIS declaration list,
        // then update the style attribute. Keep that list alive across a run of
        // property writes; reparsing after every `style.foo = value` was the
        // remaining hot path on React commits with large style objects.
        const write = (m) => {
            const keys = Object.keys(m);
            if (keys.length) {
                const raw = keys.map((k) => k + ": " + m[k]).join("; ");
                // Publish the complete declaration state before attribute
                // reactions run. A reaction that directly replaces `style`
                // produces different raw text, which parse() detects normally.
                parsedRaw = raw;
                parsedMap = m;
                el.setAttribute("style", raw);
            } else {
                parsedRaw = "";
                parsedMap = m;
                el.removeAttribute("style");
            }
        };
        return new Proxy({}, {
            get(_, p) {
                if (typeof p !== "string") return undefined;
                if (p === "cssText") return el.getAttribute("style") || "";
                if (p === "setProperty") return (k, v) => {
                    const m = parse(), key = String(k).toLowerCase(), value = String(v);
                    if (!value) delete m[key];
                    else if (acceptsStyleValue(key, value)) m[key] = value;
                    write(m);
                };
                if (p === "getPropertyValue") return (k) => parse()[String(k).toLowerCase()] || "";
                if (p === "removeProperty") return (k) => { const m = parse(); const key = String(k).toLowerCase(); const v = m[key] || ""; delete m[key]; write(m); return v; };
                if (p === "length") return Object.keys(parse()).length;
                return parse()[kebab(p)] ?? "";
            },
            set(_, p, v) {
                if (typeof p !== "string") return true;
                if (p === "cssText") {
                    if (String(v).trim()) el.setAttribute("style", String(v));
                    else el.removeAttribute("style");
                    return true;
                }
                const m = parse();
                const key = kebab(p);
                if (v === "" || v === null || v === undefined) delete m[key];
                else if (acceptsStyleValue(key, v)) m[key] = String(v);
                write(m);
                return true;
            },
            has() { return true; },
            deleteProperty(_, p) {
                if (typeof p === "string") { const m = parse(); delete m[kebab(p)]; write(m); }
                return true;
            },
        });
    }

    // Container types mpv routinely plays — what a media element honestly
    // reports it "can play" (we present media via mpv on follow, see layout's
    // `flow_media` + `is_playable_media_url`).
    const MEDIA_MIME = /^(?:video|audio)\/(?:mp4|webm|ogg|mpeg|mp3|aac|x-aac|x-m4a|mp4a-latm|flac|x-flac|wav|x-wav|x-matroska|quicktime|x-msvideo|x-flv|3gpp2?|x-ms-wmv)$/;
    function emptyTimeRanges() { return { length: 0, start() { return 0; }, end() { return 0; } }; }
    function emptyTrackList() { const l = []; l.getTrackById = () => null; l.addEventListener = () => {}; l.removeEventListener = () => {}; return l; }
    // The WebIDL brand for an HTML element, i.e. what
    // `Object.prototype.toString.call(el)` must report ("[object HTMLDivElement]").
    // Spec requires every platform object to carry its interface name as its
    // @@toStringTag; without it elements stringified as "[object Object]", which
    // broke the very common is-an-Element idiom `toString.call(x).includes("Element")`
    // (Tippy.js returns an empty array — and then `.destroy()` throws — when its
    // element check fails). Irregular tags map explicitly; hyphenated/generic tags
    // are the base HTMLElement; everything else is HTML<Cap>Element (which also
    // harmlessly names truly-unknown tags rather than tracking HTMLUnknownElement).
    const HTML_IFACE_IRREGULAR = {
        a: "Anchor", p: "Paragraph", ul: "UList", ol: "OList", li: "LI", dl: "DList",
        br: "BR", hr: "HR", img: "Image", q: "Quote", blockquote: "Quote",
        ins: "Mod", del: "Mod", caption: "TableCaption", col: "TableCol",
        colgroup: "TableCol", table: "Table", tbody: "TableSection",
        thead: "TableSection", tfoot: "TableSection", tr: "TableRow", td: "TableCell",
        th: "TableCell", textarea: "TextArea", iframe: "IFrame", frame: "Frame",
        frameset: "FrameSet", datalist: "DataList", optgroup: "OptGroup",
        fieldset: "FieldSet", h1: "Heading", h2: "Heading", h3: "Heading",
        h4: "Heading", h5: "Heading", h6: "Heading",
    };
    // Known elements with no specific interface (report the base HTMLElement).
    const HTML_IFACE_GENERIC = new Set(["abbr", "address", "article", "aside", "b",
        "bdi", "bdo", "cite", "code", "dd", "dfn", "dt", "em", "figcaption", "figure",
        "footer", "header", "hgroup", "i", "kbd", "main", "mark", "nav", "noscript",
        "rp", "rt", "ruby", "s", "samp", "section", "small", "strong", "sub",
        "summary", "sup", "u", "var", "wbr", "center", "acronym", "big", "nobr",
        "tt", "strike"]);
    function htmlInterfaceName(local) {
        const t = String(local || "").toLowerCase();
        if (!t) return "HTMLUnknownElement";
        if (t.indexOf("-") >= 0 || HTML_IFACE_GENERIC.has(t)) return "HTMLElement";
        const irr = HTML_IFACE_IRREGULAR[t];
        if (irr) return "HTML" + irr + "Element";
        return "HTML" + t.charAt(0).toUpperCase() + t.slice(1) + "Element";
    }
    // A real DOMTokenList (https://dom.spec.whatwg.org/#interface-domtokenlist)
    // backs `element.classList` (and `relList`). It MUST be a class with a
    // shared prototype, not a bare object literal: legacy classList polyfills
    // (W3Schools' common-deps, html5shiv-era shims) feature-detect a method on
    // an instance, then patch `DOMTokenList.prototype` — so the global has to
    // exist AND prototype patches have to reach every instance. The methods read
    // the live `class` attribute through `__el` so the list stays in sync with
    // direct attribute writes. `blocking` supplies its supported-token set;
    // lists such as classList with no supported tokens throw from supports().
    class DOMTokenList {
        constructor(el, attr, supported) {
            this.__el = el;
            this.__attr = attr || "class";
            this.__supported = supported || null;
        }
        __get() { return (this.__el.getAttribute(this.__attr) || "").split(/\s+/).filter(Boolean); }
        __set(l) { this.__el.setAttribute(this.__attr, l.join(" ")); }
        add(...cs) { const l = this.__get(); for (const c of cs) if (!l.includes(String(c))) l.push(String(c)); this.__set(l); }
        remove(...cs) { const ss = cs.map(String); this.__set(this.__get().filter((x) => !ss.includes(x))); }
        toggle(c, force) {
            const has = this.__get().includes(String(c));
            const want = force === undefined ? !has : !!force;
            if (want && !has) this.add(c);
            if (!want && has) this.remove(c);
            return want;
        }
        replace(oldT, newT) {
            const l = this.__get(); const i = l.indexOf(String(oldT));
            if (i < 0) return false;
            if (!l.includes(String(newT))) l[i] = String(newT); else l.splice(i, 1);
            this.__set(l); return true;
        }
        contains(c) { return this.__get().includes(String(c)); }
        item(i) { return this.__get()[i] ?? null; }
        supports(token) {
            if (!this.__supported) throw new TypeError("DOMTokenList has no supported tokens");
            // DOM §interface-DOMTokenList: supported-token matching first
            // ASCII-lowercases the argument. The supported-token lists below
            // are ASCII-lowercase, as are the HTML link-type keywords.
            return this.__supported.includes(String(token).replace(/[A-Z]/g, (c) =>
                String.fromCharCode(c.charCodeAt(0) + 0x20)));
        }
        get length() { return this.__get().length; }
        get value() { return this.__el.getAttribute(this.__attr) || ""; }
        set value(v) { this.__el.setAttribute(this.__attr, String(v)); }
        toString() { return this.__el.getAttribute(this.__attr) || ""; }
        forEach(fn, thisArg) { this.__get().forEach((t, i) => fn.call(thisArg, t, i, this)); }
        keys() { return this.__get().keys(); }
        values() { return this.__get().values(); }
        entries() { return this.__get().entries(); }
        [Symbol.iterator]() { return this.__get()[Symbol.iterator](); }
    }

    // HTML §4.2.4 (the link element) and §4.6.2 (hyperlink elements) define
    // the supported-token sets for relList. Keep these lists limited to
    // processing TRust actually implements: relList.supports() is a feature
    // probe, so reporting an unimplemented link type as supported is as wrong
    // as omitting relList entirely. The arrays are private and shared by all
    // same-kind elements; DOMTokenList still owns the live rel attribute.
    const LINK_REL_SUPPORTED = [
        "dns-prefetch", "modulepreload", "preconnect", "prefetch", "preload", "stylesheet",
    ];
    const HYPERLINK_REL_SUPPORTED = ["noopener", "noreferrer", "opener"];
    function relListFor(element, supported) {
        return element.__trustRelList
            || (element.__trustRelList = new DOMTokenList(element, "rel", supported));
    }

    // CSS Font Loading Module Level 3 §§2–3.  The terminal compositor owns
    // font selection and does not rasterize web-font files, but the platform
    // objects still have to exist: sites use document.fonts as a readiness
    // gate before mounting their real view.  Keep the FontFace/FontFaceSet
    // object model and setlike behavior; URL-backed faces remain unloaded
    // until explicitly loaded, and an explicit load resolves with terminal
    // fallback metrics rather than blocking the page forever on an unusable
    // font resource.
    class FontFace {
        constructor(family, source, descriptors) {
            this.family = String(family);
            this.__source = source;
            this.__status = "unloaded";
            this.__sets = new Set();
            this.__resolveLoaded = null;
            this.__rejectLoaded = null;
            this.__loaded = new Promise((resolve, reject) => {
                this.__resolveLoaded = resolve;
                this.__rejectLoaded = reject;
            });
            descriptors = descriptors || {};
            this.style = descriptors.style === undefined ? "normal" : String(descriptors.style);
            this.weight = descriptors.weight === undefined ? "normal" : String(descriptors.weight);
            this.stretch = descriptors.stretch === undefined
                ? (descriptors.width === undefined ? "normal" : String(descriptors.width))
                : String(descriptors.stretch);
            this.width = this.stretch;
            this.unicodeRange = descriptors.unicodeRange === undefined
                ? "U+0-10FFFF" : String(descriptors.unicodeRange);
            this.variant = descriptors.variant === undefined ? "normal" : String(descriptors.variant);
            this.featureSettings = descriptors.featureSettings === undefined
                ? "normal" : String(descriptors.featureSettings);
            this.variationSettings = descriptors.variationSettings === undefined
                ? "normal" : String(descriptors.variationSettings);
            this.display = descriptors.display === undefined ? "auto" : String(descriptors.display);
            this.ascentOverride = descriptors.ascentOverride === undefined
                ? "normal" : String(descriptors.ascentOverride);
            this.descentOverride = descriptors.descentOverride === undefined
                ? "normal" : String(descriptors.descentOverride);
            this.lineGapOverride = descriptors.lineGapOverride === undefined
                ? "normal" : String(descriptors.lineGapOverride);
        }
        get status() { return this.__status; }
        get loaded() { return this.__loaded; }
        load() {
            if (this.__status === "unloaded") {
                this.__status = "loading";
                // No web-font rasterizer is present in the terminal backend.
                // Complete the API operation with the fallback face so callers
                // waiting on FontFace.loaded do not strand the application.
                this.__status = "loaded";
                for (const set of this.__sets) set.__fontLoaded(this);
                this.__resolveLoaded(this);
            }
            return this.__loaded;
        }
    }
    class FontFaceSetLoadEvent extends Event {
        constructor(type, init) {
            super(type, init);
            this.fontfaces = Object.freeze((init && init.fontfaces || []).slice());
        }
    }
    class FontFaceSet extends EventTarget {
        constructor(initialFaces) {
            super();
            this.__faces = new Set();
            this.onloading = null;
            this.onloadingdone = null;
            this.onloadingerror = null;
            this.__ready = Promise.resolve(this);
            for (const face of initialFaces || []) this.add(face);
        }
        add(font) {
            if (!(font instanceof FontFace)) throw new TypeError("FontFaceSet.add: argument is not a FontFace");
            if (!this.__faces.has(font)) {
                this.__faces.add(font);
                font.__sets.add(this);
                if (font.status === "loading") this.__status = "loading";
            }
            return this;
        }
        delete(font) {
            if (!this.__faces.delete(font)) return false;
            if (font.__sets) font.__sets.delete(this);
            return true;
        }
        clear() {
            for (const font of this.__faces) if (font.__sets) font.__sets.delete(this);
            this.__faces.clear();
        }
        has(font) { return this.__faces.has(font); }
        get size() { return this.__faces.size; }
        entries() { return Array.from(this.__faces, (font) => [font, font])[Symbol.iterator](); }
        keys() { return this.__faces.keys(); }
        values() { return this.__faces.values(); }
        forEach(callback, thisArg) {
            this.__faces.forEach((font) => callback.call(thisArg, font, font, this));
        }
        [Symbol.iterator]() { return this.values(); }
        get status() { return this.__status || "loaded"; }
        get ready() { return this.__ready; }
        load(_font, _text) {
            return Promise.all(Array.from(this.__faces, (font) => font.load()))
                .then(() => Array.from(this.__faces));
        }
        check(_font, _text) {
            return Array.from(this.__faces).every((font) => font.status === "loaded");
        }
        __fontLoaded(_font) {
            this.__status = "loaded";
        }
        get [Symbol.toStringTag]() { return "FontFaceSet"; }
    }

    // `element.dataset` is a DOMStringMap (https://html.spec.whatwg.org/#domstringmap).
    // The interface object MUST exist on the global AND `el.dataset instanceof
    // DOMStringMap` MUST be true: Facebook/Instagram's async-CSS bootloader does
    // `e.dataset instanceof window.DOMStringMap ? e.dataset : null`, so a missing
    // global makes the `instanceof` RHS `undefined` → a TypeError that aborts the
    // bootstrap. Direct construction throws like the platform ("Illegal
    // constructor"); the live map is the proxy in the `dataset` getter, whose
    // target inherits this prototype so the `instanceof` check holds.
    const DOMStringMap = function () { throw new TypeError("Illegal constructor"); };

    // CSSOM View §6 queues element scroll notifications on the event loop; it
    // does not dispatch them synchronously from `scrollLeft`/`scrollBy`. Keep
    // one task per scrolling box so two-axis writes in one operation coalesce,
    // and resolve every scroll Promise after the instant scroll completes.
    // TRust may perform `smooth` instantly (CSSOM View explicitly conditions
    // smooth animation on whether the UA honors the behavior), but completion
    // and event ordering remain the same.
    const PENDING_ELEMENT_SCROLLS = new Map();
    function queueElementScroll(el, resolve) {
        let pending = PENDING_ELEMENT_SCROLLS.get(el.__id);
        if (pending) {
            if (resolve) pending.resolvers.push(resolve);
            return;
        }
        pending = { element: el, resolvers: resolve ? [resolve] : [] };
        PENDING_ELEMENT_SCROLLS.set(el.__id, pending);
        g.setTimeout(function () {
            // Delete first: a scroll handler may initiate a distinct scroll,
            // which must receive a later task rather than being lost here.
            PENDING_ELEMENT_SCROLLS.delete(el.__id);
            trust.fireElementScroll(el.__id);
            try { dispatch(el, new Event("scrollend"), false); }
            catch (e) { trust.errors.push("element scrollend handler: " + ((e && e.message) || e)); }
            for (const done of pending.resolvers) {
                try { done(); } catch (e) {}
            }
        }, 0);
    }

    function normalizedScrollNumber(v) {
        v = +v;
        return isFinite(v) ? v : 0;
    }

    function checkedScrollBehavior(options) {
        const behavior = options && options.behavior !== undefined
            ? String(options.behavior) : "auto";
        if (behavior !== "auto" && behavior !== "instant" && behavior !== "smooth") {
            throw new TypeError("Invalid ScrollBehavior value: " + behavior);
        }
        return behavior;
    }

    class Element extends Node {
        // DOM Standard ParentNode: selector methods are exposed on Element,
        // Document, and DocumentFragment—not on CharacterData/Text nodes.
        // Keeping these off Node is observable (`"querySelectorAll" in text`
        // is false) and prevents code that tests for ParentNode from treating
        // an inserted text node as an element.
        querySelector(s) { const r = __dom_query(this.__id, String(s), true); return r.length ? wrap(r[0]) : null; }
        querySelectorAll(s) { return __dom_query(this.__id, String(s), false).map(wrap); }
        getElementsByTagName(t) { return this.querySelectorAll(String(t)); }
        getElementsByClassName(c) { return this.querySelectorAll(String(c).trim().split(/\s+/).map((x) => "." + x).join("")); }
        // nodeType and the tag are IMMUTABLE for a node: `wrap()` already
        // dispatched this class BY node type, and an element's local name never
        // changes. So return a constant nodeType (no `__dom_node_type` syscall)
        // and lazily cache the tag — killing the per-access syscalls that
        // jQuery's `each`/`data`/`add` pound (profile-directed: `nodeType`/
        // `nodeName`/`tagName` getters were ~8% of Steam's settle phase; a
        // getter-hammering micro-bench runs ~25% faster).
        get nodeType() { return 1; }
        // Cached localName. NAMESPACED `__trustLN` (not the obvious `__ln`)
        // because page code writes its OWN expandos onto our node wrappers and a
        // 2-char name collides: YouTube/Polymer stores a MutationObserver
        // linked-list node in `node.__ln` (`kgv`: `k.__ln = {value,previous,next}`)
        // — sharing `__ln` clobbered our cached tag AND made YT read our string
        // where it expected its object (`"div".next = …` → "cannot set
        // non-writable property"). Keep every per-node internal field `__trust*`.
        get localName() { let t = this.__trustLN; if (t === undefined) t = this.__trustLN = __dom_tag(this.__id) || ""; return t; }
        get tagName() { let t = this.__tn; if (t === undefined) t = this.__tn = this.localName.toUpperCase(); return t; }
        get nodeName() { return this.tagName; }
        // `Element.namespaceURI` — immutable, so cache it (undefined = uncached,
        // null = the null namespace). HTML elements report the XHTML namespace;
        // inline SVG/MathML their own. Vue 3 hydration reads
        // `el.namespaceURI.includes("svg")`, so a missing value threw on every
        // SSR Vue/Nuxt page (joinpeertube).
        get namespaceURI() { let n = this.__ns; if (n === undefined) n = this.__ns = __dom_namespace(this.__id); return n; }
        get [Symbol.toStringTag]() { return htmlInterfaceName(this.localName); }
        // NOTE: type-SPECIFIC IDL surfaces (HTMLMediaElement media state on
        // <video>/<audio>, the <canvas> 2d context, HTMLSelectElement options,
        // HTMLInputElement value/checked/type, anchor URL parts, iframe
        // contentDocument, <template>/<meta> content, <style>/<link> sheet, and
        // the reflected value/type/href/src/name/disabled attributes) live on
        // their OWN interface prototypes below — NOT here. A real browser puts
        // each accessor only on its owning interface, so on every other element
        // the same name is a plain writable expando and `"options" in div` is
        // false. See `class HTMLElement` and the per-interface classes after
        // Element, plus `defineReflected` for the multi-interface reflectors.
        // getAttribute is hammered by every framework's traversal/normalisation
        // (jQuery's .attr/.hasClass, event delegation, and the value/checked/id/
        // class IDL getters below all route here). A per-element read cache
        // (`__ac`) elides the repeat `__dom_get_attr` syscalls. It's a null-proto
        // bag so attribute names like "constructor"/"__proto__" stay plain keys;
        // the syscall only ever returns a string or null, so a cached `undefined`
        // uniquely means "not cached yet".
        // CORRECTNESS: every attribute write on the live page funnels through
        // setAttribute/removeAttribute (style/dataset/classList/className/value/
        // checked all route here; nothing mutates an attr Rust-side mid-page), so
        // those two are the only invalidation points. Because Rust matches
        // attribute names case-INSENSITIVELY, a write DROPS the whole bag instead
        // of patching one raw-cased key (cheap, and immune to mixed-case access).
        getAttribute(n) {
            n = String(n);
            const c = this.__ac || (this.__ac = Object.create(null));
            const v = c[n];
            if (v !== undefined) return v;
            return (c[n] = __dom_get_attr(this.__id, n));
        }
        setAttribute(n, v) {
            n = String(n); v = String(v);
            const lower = n.toLowerCase();
            // HTMLScriptElement's force-async flag is cleared whenever its
            // async content attribute is added. Removing it later must not
            // restore force-async (HTML "prepare the script element").
            if (lower === "async" && this.localName === "script") this.__trustForceAsync = false;
            const old = (this.__ceUpgraded || MO.length) ? this.getAttribute(n) : null;
            __dom_set_attr(this.__id, n, v);
            this.__ac = undefined; // attrs changed: drop the read cache (see getAttribute)
            // DOM §4.9.1: NamedNodeMap is a live collection. Refresh the
            // existing [SameObject] map synchronously so a caller holding
            // `const attrs = el.attributes` observes this write immediately,
            // including while it is iterating the map.
            this.__attrMapStale = true;
            if (this.__attrMap) void this.attributes;
            if (n === "href" && this.localName === "base") baseHrefCache = null;
            ceAttrChanged(this, lower, old, v);
            if (MO.length) moAttr(this, n, old);
            // DOM §4.2.2.4: changing a light child's `slot`, or a slot's
            // `name`, can change the assigned-node lists and must signal the
            // affected slots at the next microtask checkpoint.
            if (lower === "slot" || (lower === "name" && this.localName === "slot")) slotQueueCheck(this.parentNode || this);
            // Changing src/srcdoc re-runs "process the iframe attributes".
            if (n === "src" || n === "srcdoc") { const ln = this.localName; if (ln === "iframe" || ln === "frame") queueFrameNavigation(this); }
        }
        setAttributeNS(_, n, v) { this.setAttribute(n, v); }
        removeAttribute(n) {
            n = String(n);
            const lower = n.toLowerCase();
            const old = (this.__ceUpgraded || MO.length) ? this.getAttribute(n) : null;
            __dom_remove_attr(this.__id, n);
            this.__ac = undefined; // attrs changed: drop the read cache (see getAttribute)
            // DOM §4.9.1 requires the same live-list behavior for removals.
            // FAST's standards-based template compiler removes marker Attrs
            // while walking `element.attributes`, so deferring this refresh
            // leaves the remaining bindings unprocessed.
            this.__attrMapStale = true;
            if (this.__attrMap) void this.attributes;
            if (n === "href" && this.localName === "base") baseHrefCache = null;
            ceAttrChanged(this, lower, old, null);
            if (MO.length) moAttr(this, n, old);
            if (lower === "slot" || (lower === "name" && this.localName === "slot")) slotQueueCheck(this.parentNode || this);
            // Removing src/srcdoc re-runs "process the iframe attributes".
            if (n === "src" || n === "srcdoc") { const ln = this.localName; if (ln === "iframe" || ln === "frame") queueFrameNavigation(this); }
        }
        hasAttribute(n) { return this.getAttribute(n) !== null; }
        getAttributeNames() { return __dom_attr_names(this.__id); }
        hasAttributes() { return __dom_attr_names(this.__id).length > 0; }
        // Attr-node accessors (DOM §4.9.2). React DOM's property commit reads
        // getAttributeNode then removeAttributeNode; without them it threw
        // "undefined is not a callable (reading 'removeAttributeNode')". An Attr
        // here is the SAME plain object the `attributes` NamedNodeMap yields
        // (name/value/ownerElement/…) — we keep no GC-wrapped Attr node.
        getAttributeNode(n) { return this.attributes.getNamedItem(n); }
        getAttributeNodeNS(_ns, n) { return this.attributes.getNamedItem(n); }
        setAttributeNode(attr) {
            const old = this.getAttributeNode(attr.name);
            this.setAttribute(attr.name, attr.value == null ? "" : attr.value);
            attr.ownerElement = this;
            return old;
        }
        setAttributeNodeNS(attr) { return this.setAttributeNode(attr); }
        removeAttributeNode(attr) {
            // Spec returns the removed Attr; be lenient on a stale/foreign node
            // (fail-open, like the rest of the platform surface) and remove by name.
            const removed = this.getAttributeNode(attr.name) || attr;
            this.removeAttribute(attr.name);
            return removed;
        }
        // NamedNodeMap, array-like enough for Array.from/iteration/indexing
        // (Alpine's DOM morph does `Array.from(el.attributes)` — undefined
        // here threw ToObject and aborted danbooru's whole render).
        // [SameObject] per spec: ONE map per element, identity-stable across
        // accesses; its contents refresh in place after every attribute write
        // (`__attrMapStale` rides the same set/removeAttribute funnels that
        // drop the `getAttribute` read cache), preserving live-list behavior
        // for existing references as required by the DOM Standard.
        get attributes() {
            // Plain loop + snapshot values + `this`-based methods: NO
            // closure capturing a block-scoped local invoked from a native
            // callback (Boa trap #6 — `.map`/getters here aborted the page
            // with a define-opcode OOB panic). Values snapshot per rebuild.
            let list = this.__attrMap;
            if (list && !this.__attrMapStale) return list;
            if (!list) {
                list = [];
                list.__owner = this;
                list.item = function (i) { return this[i] || null; };
                list.getNamedItem = function (nm) {
                    for (var j = 0; j < this.length; j++) if (this[j].name === String(nm)) return this[j];
                    return null;
                };
                // setNamedItem/removeNamedItem round out the map (DOM §4.9.1);
                // they route through the owner's set/removeAttribute funnels.
                list.setNamedItem = function (attr) { return this.__owner.setAttributeNode(attr); };
                list.setNamedItemNS = function (attr) { return this.__owner.setAttributeNode(attr); };
                list.removeNamedItem = function (nm) {
                    const old = this.getNamedItem(nm);
                    if (!old) throw new (g.DOMException || TypeError)("No attribute named " + nm, "NotFoundError");
                    this.__owner.removeAttribute(String(nm));
                    return old;
                };
                this.__attrMap = list;
            } else {
                // Rebuild in place (identity must survive): drop the named
                // props of the OLD entries, then the entries themselves.
                for (let j = 0; j < list.length; j++) {
                    const old = list[j].name;
                    if (old !== "length" && old !== "item" && old !== "getNamedItem") delete list[old];
                }
                list.length = 0;
            }
            const names = __dom_attr_names(this.__id) || [];
            for (let i = 0; i < names.length; i++) {
                const n = names[i];
                const v = __dom_get_attr(this.__id, n);
                const attr = {
                    name: n, localName: n, nodeName: n, namespaceURI: null,
                    prefix: null, specified: true, ownerElement: this,
                    value: v, nodeValue: v,
                };
                list.push(attr);
                // NamedNodeMap named-property access: `attributes[name]` returns
                // the Attr (WebIDL named getter). jQuery's event-support probe
                // reads `div.attributes["onsubmit"].expando`; without this it
                // was undefined → ToObject throw that aborted jQuery's boot.
                // Skip names that would clobber the array length / methods.
                if (n !== "length" && n !== "item" && n !== "getNamedItem") list[n] = attr;
            }
            this.__attrMapStale = false;
            return list;
        }
        // Lit's ?attr= boolean bindings commit through this.
        toggleAttribute(name, force) {
            const want = force === undefined ? !this.hasAttribute(name) : !!force;
            if (want) this.setAttribute(name, "");
            else this.removeAttribute(name);
            return want;
        }
        get id() { return this.getAttribute("id") || ""; }
        set id(v) { this.setAttribute("id", v); }
        get className() { return this.getAttribute("class") || ""; }
        set className(v) { this.setAttribute("class", v); }
        // `name`, `value`, `checked`, `selected`, `multiple`, `disabled` and the
        // <select>/<option> surfaces moved to their owning interfaces (see below).
        get hidden() { return this.hasAttribute("hidden"); }
        set hidden(v) { if (v) this.setAttribute("hidden", ""); else this.removeAttribute("hidden"); }
        // Reflected string IDL attributes (HTML spec): the getter returns the
        // content attribute or "" — NOT undefined — so the universal idiom
        // `el.lang.toLowerCase()` / `el.dir === "rtl"` works. pixiv reads
        // `document.documentElement.lang.toLowerCase()` at boot; without this
        // it got `undefined.toLowerCase()` and threw, killing the whole bundle.
        get lang() { return this.getAttribute("lang") || ""; }
        set lang(v) { this.setAttribute("lang", String(v)); }
        get dir() { return this.getAttribute("dir") || ""; }
        set dir(v) { this.setAttribute("dir", String(v)); }
        get title() { return this.getAttribute("title") || ""; }
        set title(v) { this.setAttribute("title", String(v)); }
        get slot() { return this.getAttribute("slot") || ""; }
        set slot(v) { this.setAttribute("slot", String(v)); }
        // `type`/`href`/`src` and the anchor URL components moved to their
        // owning interfaces (HTMLInputElement, HTMLAnchorElement, …) below.
        get innerHTML() { return __dom_inner_html(this.__id); }
        set innerHTML(v) {
            if (!MO.length) {
                __dom_set_inner_html(this.__id, String(v));
                baseHrefCache = null;
                if (CE.defs.size) ceScan(this);
                slotQueueCheck(this);
                // HTML §4.8.6: innerHTML insertion still processes iframe
                // attributes and starts each newly inserted nested navigable.
                // Script elements remain inert under the fragment parser.
                queueFrameNavigationsIn(this);
                return;
            }
            const removed = this.childNodes;
            __dom_set_inner_html(this.__id, String(v));
            baseHrefCache = null;
            moChildBulk(this, removed, this.childNodes);
            if (CE.defs.size) ceScan(this);
            slotQueueCheck(this);
            queueFrameNavigationsIn(this);
        }
        // `content` (<template>/<meta>) and `contentDocument`/`contentWindow`
        // (<iframe>/<frame>) moved to their owning interfaces below. A generic
        // element has no `content` property, so a framework's `.content = …`
        // property binding (lit's PropertyPart) now sets a plain expando here.
        attachShadow(init) {
            const id = __dom_attach_shadow(this.__id);
            let sr = W.get(id);
            if (!(sr instanceof ShadowRoot)) {
                sr = new ShadowRoot(id);
                W.set(id, sr);
            }
            sr.__host = this;
            sr.__mode = init && init.mode === "closed" ? "closed" : "open";
            this.__sr = sr;
            return sr;
        }
        get shadowRoot() { return this.__sr && this.__sr.__mode === "open" ? this.__sr : null; }
        // ElementInternals, minimally: form components construct with
        // this unguarded (archive.org's dropdowns) — always-valid,
        // form-less internals keep them booting.
        attachInternals() {
            return {
                form: null, shadowRoot: this.__sr || null, willValidate: false,
                validity: { valid: true }, validationMessage: "", labels: [],
                states: new Set(), ariaLabel: null,
                setFormValue() {}, setValidity() {},
                checkValidity() { return true; }, reportValidity() { return true; },
            };
        }
        get outerHTML() { return __dom_outer_html(this.__id); }
        get innerText() { return this.textContent; }
        set innerText(v) { this.textContent = v; }
        insertAdjacentHTML(p, h) {
            p = String(p).toLowerCase();
            // DOM §insert adjacent: an unrecognized position is a SyntaxError
            // (the Rust fallback used to silently treat it as beforeend).
            if (p !== "beforebegin" && p !== "afterbegin" && p !== "beforeend" && p !== "afterend")
                throw new DOMException("Failed to execute 'insertAdjacentHTML': The value provided ('" + p + "') is not one of 'beforeBegin', 'afterBegin', 'beforeEnd', or 'afterEnd'.", "SyntaxError");
            const container = (p === "beforebegin" || p === "afterend") ? this.parentNode : this;
            if (!MO.length || !container) {
                __dom_insert_adjacent(this.__id, p, String(h));
                baseHrefCache = null;
                if (CE.defs.size) { const par = this.parentNode; ceScan(par || this); }
                queueFrameNavigationsIn(container || this);
                return;
            }
            const before = new Set(container.childNodes.map((k) => k.__id));
            __dom_insert_adjacent(this.__id, p, String(h));
            baseHrefCache = null;
            const added = container.childNodes.filter((k) => !before.has(k.__id));
            moChildBulk(container, [], added);
            if (CE.defs.size) { const par = this.parentNode; ceScan(par || this); }
            queueFrameNavigationsIn(container);
        }
        insertAdjacentElement(p, el) {
            const pos = String(p).toLowerCase();
            if (pos === "beforeend") this.appendChild(el);
            else if (pos === "afterbegin") this.insertBefore(el, this.firstChild);
            // beforebegin/afterend with no parent: return null, no insertion (spec).
            else if (pos === "beforebegin") { if (!this.parentNode) return null; this.parentNode.insertBefore(el, this); }
            else if (pos === "afterend") { if (!this.parentNode) return null; this.parentNode.insertBefore(el, this.nextSibling); }
            else throw new DOMException("Failed to execute 'insertAdjacentElement': The value provided ('" + pos + "') is not one of 'beforeBegin', 'afterBegin', 'beforeEnd', or 'afterEnd'.", "SyntaxError");
            return el;
        }
        insertAdjacentText(p, text) {
            this.insertAdjacentElement(String(p).toLowerCase(), document.createTextNode(String(text)));
        }
        get style() { if (!this.__style) this.__style = styleFor(this); return this.__style; }
        // `.sheet` (<style>/<link> CSSOM) moved to HTMLStyleElement/HTMLLinkElement.
        // `el.style = "color:red"` — the [PutForwards=cssText] behaviour: assigning
        // a string to .style sets inline cssText (a getter-only .style throws in
        // strict mode, which silently broke YouTube's renderers in attr callbacks).
        set style(v) {
            if (v === null || v === undefined || String(v).trim() === "") this.removeAttribute("style");
            else this.setAttribute("style", String(v));
        }
        get dataset() {
            if (!this.__ds) {
                const el = this;
                this.__ds = new Proxy(Object.create(DOMStringMap.prototype), {
                    get(_, p) { return typeof p === "string" ? (el.getAttribute("data-" + kebab(p)) ?? undefined) : undefined; },
                    set(_, p, v) { if (typeof p === "string") el.setAttribute("data-" + kebab(p), String(v)); return true; },
                    has(_, p) { return typeof p === "string" && el.getAttribute("data-" + kebab(p)) !== null; },
                });
            }
            return this.__ds;
        }
        get classList() {
            if (!this.__cl) this.__cl = new DOMTokenList(this);
            return this.__cl;
        }
        matches(s) { return !!__dom_matches(this.__id, String(s)); }
        webkitMatchesSelector(s) { return this.matches(s); }
        closest(s) { let e = this; while (e && e.nodeType === 1) { if (e.matches(s)) return e; e = e.parentNode; } return null; }
        // HTML `HTMLElement.click()`: fire a synthetic, non-trusted `click`
        // event (bubbles to React's delegated root listener) + run activation.
        // Was a no-op, so any programmatic click (consent "Accept" buttons,
        // framework-driven toggles, auto-clickers) silently did nothing.
        click() { try { activateClick(this, false, false); } catch (e) {} }
        focus(options) { focusElement(this, options || {}); }
        blur() { blurElement(this); }
        get tabIndex() {
            const parsed = parsedTabIndex(this);
            return parsed === null ? defaultTabIndex(this) : parsed;
        }
        set tabIndex(v) { this.setAttribute("tabindex", String(Number(v) | 0)); }
        // --- The Popover API (HTML §the popover attribute) ---------------
        // State truth lives in the ARENA (`__dom_popover` → the UA hide rule
        // + `:popover-open`); POPOVER_OPEN mirrors it for the API logic.
        // Light dismiss (click-outside closes auto popovers) is DEFERRED —
        // the terminal's click dispatch has no pointerdown/up pair to track.
        // `popover` reflects as the enumerated keyword state: missing → null,
        // ""/"auto" → "auto", "hint" → "hint", anything else → "manual".
        get popover() {
            const v = this.getAttribute("popover");
            if (v === null) return null;
            const s = String(v).toLowerCase();
            return (s === "" || s === "auto") ? "auto" : s === "hint" ? "hint" : "manual";
        }
        set popover(v) {
            if (v === null || v === undefined) this.removeAttribute("popover");
            else this.setAttribute("popover", String(v));
        }
        showPopover() {
            const state = this.popover;
            if (state === null) throw new DOMException("Element has no popover attribute", "NotSupportedError");
            // Already showing: "check popover validity" RETURNS FALSE on a
            // state mismatch — show popover then just returns (HTML; only a
            // no-popover attribute or a disconnected element throws). Steam's
            // tooltip re-calls showPopover on every hover tick and a throw
            // here fed its error boundary.
            if (POPOVER_OPEN[this.__id]) return;
            if (!this.isConnected) throw new DOMException("Popover is not connected", "InvalidStateError");
            const bev = new g.ToggleEvent("beforetoggle", { oldState: "closed", newState: "open", cancelable: true });
            dispatch(this, bev, false);
            if (bev.defaultPrevented) return;
            // An auto/hint popover closes the other showing auto/hint
            // popovers (the spec's stack, flattened: last one wins).
            if (state !== "manual") {
                for (const k in POPOVER_OPEN) {
                    const other = POPOVER_OPEN[k];
                    if (other && other !== this && other.popover !== "manual") {
                        try { other.hidePopover(); } catch (e) {}
                    }
                }
            }
            POPOVER_OPEN[this.__id] = this;
            __dom_popover(this.__id, true);
            const self = this;
            g.setTimeout(function () { dispatch(self, new g.ToggleEvent("toggle", { oldState: "closed", newState: "open" }), false); }, 0);
        }
        hidePopover() {
            if (this.popover === null) throw new DOMException("Element has no popover attribute", "NotSupportedError");
            // Already hidden: silent return, same validity rule as show.
            if (!POPOVER_OPEN[this.__id]) return;
            // beforetoggle open→closed is NOT cancelable (spec).
            dispatch(this, new g.ToggleEvent("beforetoggle", { oldState: "open", newState: "closed" }), false);
            delete POPOVER_OPEN[this.__id];
            __dom_popover(this.__id, false);
            const self = this;
            g.setTimeout(function () { dispatch(self, new g.ToggleEvent("toggle", { oldState: "open", newState: "closed" }), false); }, 0);
        }
        togglePopover(force) {
            const open = !!POPOVER_OPEN[this.__id];
            if (open && (force === undefined || !force)) { this.hidePopover(); return false; }
            if (!open && (force === undefined || !!force)) { this.showPopover(); return true; }
            return open;
        }
        // Element scrolling (CSSOM View, Phase 3 inner-scroll regions). A
        // definite-height `overflow-y:auto|scroll` box is a real scroll viewport
        // (the app reserves H rows and windows a retained buffer over them). The
        // page OWNS the scroll position via these members; the app measures the
        // box GEOMETRY and pushes it back, so the conditional pin idiom
        // (`if scrollTop + clientHeight >= scrollHeight`) reads TRUE values and a
        // chat that sets `scrollTop = scrollHeight` actually pins to the bottom.
        // `scroll()`/`scrollTo()` set an absolute position; `scrollBy()` a
        // relative one; each takes either `(x, y)` or a `{left, top}` options
        // dict. `scrollIntoView()` scrolls each ancestor scroll container so this
        // element is visible (the recursive CSSOM scroll). The root element /
        // scrollingElement still mirrors the page scroll (the terminal owns it).
        __snapInlinePosition(natural, direction) {
            // CSS Scroll Snap 1 §6: an inline-axis snap container selects a
            // valid descendant snap area's alignment position after a scroll.
            // The exact selection algorithm is deliberately UA-defined; use
            // the nearest candidate to the intended endpoint, while respecting
            // the intended direction of relative (`scrollBy`) operations.
            let type = "";
            try { type = String(g.getComputedStyle(this).getPropertyValue("scroll-snap-type") || "").toLowerCase(); }
            catch (e) { return natural; }
            const typeParts = type.trim().split(/\s+/);
            if (typeParts[0] !== "x" && typeParts[0] !== "inline" && typeParts[0] !== "both") return natural;

            const current = this.scrollLeft;
            const max = Math.max(0, this.scrollWidth - this.clientWidth);
            const box = this.__rect();
            const candidates = [];
            let descendants;
            try { descendants = this.querySelectorAll("*"); } catch (e) { return natural; }
            // A snap area can be a light-DOM child assigned through a slot in
            // the scroller's shadow tree (Archive's carousel is exactly this
            // standard composed-tree shape). `querySelectorAll` intentionally
            // does not pierce that boundary, so include each slot's flattened
            // assignments when collecting descendant snap areas.
            const areas = descendants.slice();
            for (const node of descendants) {
                if (node.localName !== "slot" || typeof node.assignedElements !== "function") continue;
                try { areas.push.apply(areas, node.assignedElements({ flatten: true })); }
                catch (e) {}
            }
            for (const area of areas) {
                let align = "";
                try { align = String(g.getComputedStyle(area).getPropertyValue("scroll-snap-align") || "").trim().toLowerCase(); }
                catch (e) { continue; }
                const parts = align.split(/\s+/);
                // One value applies to both axes; with two values the second is
                // the inline-axis alignment used by an x/inline container.
                const inline = parts.length > 1 ? parts[1] : parts[0];
                if (inline !== "start" && inline !== "center" && inline !== "end") continue;
                const r = area.__rect();
                let candidate = r.left - box.left;
                if (inline === "center") candidate += r.width / 2 - this.clientWidth / 2;
                else if (inline === "end") candidate += r.width - this.clientWidth;
                candidate = Math.max(0, Math.min(max, candidate));
                if (direction > 0 && candidate + 0.01 < current) continue;
                if (direction < 0 && candidate - 0.01 > current) continue;
                candidates.push(candidate);
            }
            if (!candidates.length) return natural;
            let best = candidates[0], distance = Math.abs(best - natural);
            for (let i = 1; i < candidates.length; i++) {
                const d = Math.abs(candidates[i] - natural);
                if (d < distance) { best = candidates[i]; distance = d; }
            }
            return best;
        }
        __scrollToOptions(options, direction) {
            checkedScrollBehavior(options);
            let left = options.left === undefined ? this.scrollLeft : normalizedScrollNumber(options.left);
            let top = options.top === undefined ? this.scrollTop : normalizedScrollNumber(options.top);
            if (this.localName === "html") {
                g.scrollTo(left, top);
                return Promise.resolve();
            }
            const maxLeft = Math.max(0, this.scrollWidth - this.clientWidth);
            const maxTop = Math.max(0, this.scrollHeight - this.clientHeight);
            left = Math.max(0, Math.min(maxLeft, left));
            top = Math.max(0, Math.min(maxTop, top));
            left = this.__snapInlinePosition(left, direction || 0);
            const changed = __dom_scroll_set(this.__id, top, left);
            if (!changed) return Promise.resolve();
            const self = this;
            return new Promise(function (resolve) { queueElementScroll(self, resolve); });
        }
        scrollTo(x, y) {
            const options = (x !== null && typeof x === "object") ? x : { left: x, top: y };
            return this.__scrollToOptions(options, 0);
        }
        scroll(x, y) { return this.scrollTo(x, y); }
        scrollBy(x, y) {
            const options = (x !== null && typeof x === "object")
                ? { left: x.left, top: x.top, behavior: x.behavior }
                : { left: x, top: y };
            checkedScrollBehavior(options);
            const dx = options.left === undefined ? 0 : normalizedScrollNumber(options.left);
            const dy = options.top === undefined ? 0 : normalizedScrollNumber(options.top);
            options.left = this.scrollLeft + dx;
            options.top = this.scrollTop + dy;
            return this.__scrollToOptions(options, dx > 0 ? 1 : (dx < 0 ? -1 : 0));
        }
        scrollIntoView(arg) {
            // Boolean legacy: true ⇒ align top ("start"), false ⇒ bottom ("end").
            const block = (arg && typeof arg === "object" && arg.block) ? String(arg.block)
                : (arg === false ? "end" : "start");
            const top = this.__rect().top, bottom = this.__rect().bottom;
            let a = this.parentNode;
            while (a && a.nodeType === 1) {
                // A real scroll container (content taller than its viewport): the
                // element's offset within it is its rect minus the container's
                // (both measured at scroll 0 in the inline flow), so that offset
                // IS the scrollTop that brings it to the container's top.
                if (a.scrollHeight > a.clientHeight + 1) {
                    const ar = a.__rect(), ch = a.clientHeight;
                    const offTop = top - ar.top, offBottom = bottom - ar.top;
                    if (block === "end") a.scrollTop = offBottom - ch;
                    else if (block === "center") a.scrollTop = (offTop + offBottom) / 2 - ch / 2;
                    else a.scrollTop = offTop; // "start"/"nearest" default
                }
                a = a.parentNode;
            }
        }
        // Geometry: a layout pass over the live DOM gives each element its REAL
        // box (CSS pixels, quantized to terminal cells — what we actually
        // paint). `__dom_rect` returns [left, top, width, height] for a laid-out
        // element, or null when it has none. Coordinates are document-origin
        // (page scroll is not threaded in yet, so they read viewport-relative at
        // the top of the page, where load-time measurement happens).
        __rect() {
            let r = null;
            try { r = __dom_rect(this.__id); } catch (e) { r = null; }
            if (r) {
                const left = r[0], top = r[1], width = r[2], height = r[3];
                return { x: left, y: top, left, top, width, height,
                         right: left + width, bottom: top + height,
                         toJSON() { return this; } };
            }
            // Phase 3 (CSSOM View §"the getBoundingClientRect() method"): an
            // element with NO associated CSS layout box returns an ALL-ZERO
            // rect. After the opacity:0/visibility:hidden paint-suppression work,
            // every RENDERED element (even an empty infinite-scroll sentinel)
            // gets a real box from the measurement pass, so a null here means the
            // element genuinely has no box — `display:none` (self/ancestor),
            // detached, or hidden — and `getBoundingClientRect`/`offset*`/
            // `client*` must report 0, exactly as a browser does (the old
            // viewport-sized fallback lied: a display:none element measured as
            // the whole window). EXCEPTION: an embedded/replaced element the
            // measurement pass deliberately SKIPs (`<svg>`/`<canvas>`/`<iframe>`/
            // `<object>`/`<math>`/`<embed>`) DOES have a real box in a browser —
            // our layout just can't compute it — so a chart/embed library
            // measuring one must still see a non-zero size; keep the viewport-box
            // hedge for those (only when connected — a detached one is still 0).
            const t = this.localName;
            if (this.isConnected &&
                (t === "svg" || t === "canvas" || t === "iframe" ||
                 t === "object" || t === "math" || t === "embed")) {
                return { x: 0, y: 0, left: 0, top: 0, right: g.innerWidth, bottom: g.innerHeight,
                         width: g.innerWidth, height: g.innerHeight, toJSON() { return this; } };
            }
            return { x: 0, y: 0, left: 0, top: 0, right: 0, bottom: 0,
                     width: 0, height: 0, toJSON() { return this; } };
        }
        // getBoundingClientRect/getClientRects are VIEWPORT-relative (CSSOM
        // View): the document-origin `__rect()` shifted up/left by the page
        // scroll. At load scroll is 0 so this is `__rect()` unchanged; once the
        // terminal threads a scroll position (PageCmd::Scroll → setScroll), a
        // scroll-based lazy-loader reading `getBoundingClientRect().top` sees the
        // box move through the viewport, exactly as in a browser. `offset*` stays
        // document/offsetParent-relative (spec) — only the client rect shifts.
        getBoundingClientRect() {
            const r = this.__rect();
            const sx = g.scrollX || 0, sy = g.scrollY || 0;
            if (!sx && !sy) return r;
            return { x: r.x - sx, y: r.y - sy, left: r.left - sx, top: r.top - sy,
                     right: r.right - sx, bottom: r.bottom - sy,
                     width: r.width, height: r.height, toJSON() { return this; } };
        }
        getClientRects() { return [this.getBoundingClientRect()]; }
        get offsetWidth() { return this.__rect().width; }
        get offsetHeight() { return this.__rect().height; }
        get offsetTop() { return this.__rect().top; }
        get offsetLeft() { return this.__rect().left; }
        get offsetParent() { return g.document.body; }
        // The root element's client area IS the viewport (CSSOM View): a page
        // reading `document.documentElement.clientHeight` to size against the
        // window must get the viewport, not the full document height. Every
        // other element reports its own laid-out box.
        // client*/scroll* read the app-measured box geometry (px) when present
        // (the region geometry round-trip), else fall back to the element rect —
        // the pre-Phase-3 behaviour, so a non-region element is unchanged. The
        // root element's client box IS the viewport (CSSOM View); its
        // scrollHeight is the full document height (its rect, via the fallback).
        get clientWidth() {
            if (this.localName === "html") return g.innerWidth;
            const v = __dom_scroll_get(this.__id, 5);
            return v !== null ? v : this.__rect().width;
        }
        get clientHeight() {
            if (this.localName === "html") return g.innerHeight;
            const v = __dom_scroll_get(this.__id, 4);
            return v !== null ? v : this.__rect().height;
        }
        get clientTop() { return 0; }
        get clientLeft() { return 0; }
        get scrollWidth() {
            const v = __dom_scroll_get(this.__id, 3);
            return v !== null ? v : this.__rect().width;
        }
        get scrollHeight() {
            const v = __dom_scroll_get(this.__id, 2);
            return v !== null ? v : this.__rect().height;
        }
        // The root scroller mirrors the page scroll position (document.scrolling
        // Element === documentElement). Every other element owns a real scroll
        // position (CSSOM View): the getter reads the stored value and the setter
        // clamps to `[0, scrollHeight − clientHeight]` and records the write
        // (`__dom_scroll_set` → the app re-windows the region). The root's setter
        // routes to the window scroll (terminal-owned, so currently inert).
        get scrollTop() {
            if (this.localName === "html") return g.scrollY || 0;
            return __dom_scroll_get(this.__id, 0) || 0;
        }
        set scrollTop(v) {
            v = normalizedScrollNumber(v);
            if (this.localName === "html") { g.scrollTo(g.scrollX || 0, v); return; }
            const max = Math.max(0, this.scrollHeight - this.clientHeight);
            if (v < 0) v = 0; else if (v > max) v = max;
            if (__dom_scroll_set(this.__id, v, this.scrollLeft)) queueElementScroll(this);
        }
        get scrollLeft() {
            if (this.localName === "html") return g.scrollX || 0;
            return __dom_scroll_get(this.__id, 1) || 0;
        }
        set scrollLeft(v) {
            v = normalizedScrollNumber(v);
            if (this.localName === "html") { g.scrollTo(v, g.scrollY || 0); return; }
            const max = Math.max(0, this.scrollWidth - this.clientWidth);
            if (v < 0) v = 0; else if (v > max) v = max;
            const direction = v > this.scrollLeft ? 1 : (v < this.scrollLeft ? -1 : 0);
            v = this.__snapInlinePosition(v, direction);
            if (__dom_scroll_set(this.__id, this.scrollTop, v)) queueElementScroll(this);
        }
    }

    // --- per-interface element prototypes (the DOM IDL hierarchy) -------------
    // Each type-specific IDL accessor (options on <select>, the media state on
    // <video>/<audio>, the <canvas> context, anchor URL parts, iframe
    // contentDocument, …) lives ONLY on its OWNING interface's prototype, exactly
    // as a real browser does — so on every other element the same name is a plain
    // writable expando and `"options" in div === false`. `wrap()` dispatches each
    // element to its interface class by tag (classFor). The read-only members are
    // getter-only, so assigning one on its owning element throws in strict mode
    // (what a browser does); off-type the name is just an own data property.
    //
    // HTMLElement is the base for HTML elements (chain: HTMLDivElement →
    // HTMLElement → Element → Node). A real browser also splits the HTMLElement
    // mixin members (lang/dir/title/hidden/dataset/style/click/focus/innerText/
    // offset*) onto HTMLElement, but TRust keeps those on Element: SVGElement
    // (also `extends Element`) genuinely shares most of them via the
    // HTMLOrSVGElement / ElementCSSInlineStyle mixins, and moving them to a
    // separate HTMLElement would STRIP them from SVG for no real-world gain. The
    // per-INTERFACE accessors below are the ones that fool `"prop" in el` feature
    // tests, so those are what we relocate. HTMLElement still exists as its own
    // constructor: page custom elements `extends HTMLElement`, `div instanceof
    // HTMLElement` is true, and `svg instanceof HTMLElement` is correctly false.
    class HTMLElement extends Element {}

    // HTMLMediaElement (<video>/<audio>). TRust presents media via mpv (a
    // followed link), not inline playback, but a player library (video.js, Plyr,
    // JW Player, …) probes the element; finding it can't play, it shows "No
    // compatible source was found" AND strips <source>, so the layout never sees
    // the media. Reporting honest support for the formats mpv plays, plus benign
    // media state, keeps the <source> in the DOM and the error away.
    class HTMLMediaElement extends HTMLElement {
        canPlayType(type) {
            const t = String(type || "").toLowerCase().split(";")[0].trim();
            return MEDIA_MIME.test(t) || t === "application/x-mpegurl"
                || t === "application/vnd.apple.mpegurl" || t === "application/dash+xml"
                ? "maybe" : "";
        }
        load() {}
        // HTMLMediaElement §"playing the media resource": an element that is
        // "not allowed to play" returns a promise rejected with
        // NotAllowedError — the exact signal a real browser gives for blocked
        // autoplay. We NEVER play inline (media routes to mpv via the follow
        // affordance), so report that honestly instead of a lying resolve:
        // hover-preview sites (Steam's sale-capsule microtrailers, every
        // autoplay-guarded player since Chrome's policy) handle precisely
        // this rejection and fall back to their poster/image UI — which keeps
        // the capsule IMAGE instead of swapping to a video we can't paint.
        // (The lying resolve left Steam's `with_microtrailer` class on
        // forever: image hidden, unpaintable video shown, capsule destroyed.)
        // The rejection is pre-observed so a page that never .catch()es
        // doesn't count as a page error — browsers only console-warn there.
        play() {
            const p = Promise.reject(new DOMException(
                "play() failed: no inline media playback in this rendering",
                "NotAllowedError"
            ));
            p.catch(function () {});
            return p;
        }
        pause() {}
        addTextTrack() { return { mode: "disabled", cues: null, activeCues: null, addCue() {}, removeCue() {}, addEventListener() {}, removeEventListener() {} }; }
        fastSeek(t) { this.__ct = +t || 0; }
        get src() { const r = this.getAttribute("src"); if (r === null) return ""; const u = __url_parse(r, baseHref()); return u ? u[0] : r; }
        set src(v) { this.setAttribute("src", String(v)); }
        get currentSrc() { return this.getAttribute("src") || ""; }
        get readyState() { return 0; }
        get networkState() { return 0; }
        get error() { return null; }
        get ended() { return false; }
        get seeking() { return false; }
        get duration() { return NaN; }
        get buffered() { return emptyTimeRanges(); }
        get played() { return emptyTimeRanges(); }
        get seekable() { return emptyTimeRanges(); }
        get textTracks() { return this.__tt || (this.__tt = emptyTrackList()); }
        get audioTracks() { return this.__at || (this.__at = emptyTrackList()); }
        get videoTracks() { return this.__vt || (this.__vt = emptyTrackList()); }
        get paused() { return this.__paused !== false; }
        set paused(v) { this.__paused = !!v; }
        get currentTime() { return this.__ct || 0; }
        set currentTime(v) { this.__ct = +v || 0; }
        get volume() { return this.__vol === undefined ? 1 : this.__vol; }
        set volume(v) { this.__vol = +v; }
        get muted() { return !!this.__muted; }
        set muted(v) { this.__muted = !!v; }
        get playbackRate() { return this.__pbr === undefined ? 1 : this.__pbr; }
        set playbackRate(v) { this.__pbr = +v; }
        get defaultPlaybackRate() { return 1; }
        set defaultPlaybackRate(_v) {}
    }
    // videoWidth/videoHeight are HTMLVideoElement-only (not on <audio>).
    class HTMLVideoElement extends HTMLMediaElement {
        get videoWidth() { return 0; }
        get videoHeight() { return 0; }
    }
    class HTMLAudioElement extends HTMLMediaElement {}

    // <canvas> 2d context. We paint no raster, but sites use it to normalise CSS
    // colours (Web Animations sets ctx.fillStyle and reads it back) and to
    // measure text. A pass-through stub stores/echoes its properties and no-ops
    // drawing — enough that the code doesn't throw, without pretending to paint.
    class HTMLCanvasElement extends HTMLElement {
        getContext(kind) {
            if (String(kind) !== "2d") return null;
            return this.__ctx2d || (this.__ctx2d = {
                canvas: this,
                fillStyle: "#000000", strokeStyle: "#000000",
                font: "10px sans-serif", globalAlpha: 1, lineWidth: 1,
                lineCap: "butt", lineJoin: "miter", textAlign: "start", textBaseline: "alphabetic",
                save() {}, restore() {}, scale() {}, rotate() {}, translate() {},
                transform() {}, setTransform() {}, resetTransform() {},
                beginPath() {}, closePath() {}, moveTo() {}, lineTo() {},
                bezierCurveTo() {}, quadraticCurveTo() {}, arc() {}, arcTo() {},
                rect() {}, ellipse() {}, fill() {}, stroke() {}, clip() {},
                clearRect() {}, fillRect() {}, strokeRect() {},
                fillText() {}, strokeText() {}, drawImage() {},
                measureText(t) { return { width: String(t).length * 6 }; },
                getImageData() { return { data: new Uint8ClampedArray(0), width: 0, height: 0 }; },
                putImageData() {}, createImageData() { return { data: new Uint8ClampedArray(0), width: 0, height: 0 }; },
                createLinearGradient() { return { addColorStop() {} }; },
                createRadialGradient() { return { addColorStop() {} }; },
                createPattern() { return null; },
                createConicGradient() { return { addColorStop() {} }; },
                setLineDash() {}, getLineDash() { return []; },
                isPointInPath() { return false; }, isPointInStroke() { return false; },
                drawFocusIfNeeded() {},
            });
        }
        toDataURL() { return "data:,"; }
    }

    // HTMLSelectElement: options is the <option> descendants (optgroups included,
    // per spec) as a real Array. options/selectedOptions are read-only (getter-
    // only). value is the first selected option's value; set re-points it.
    class HTMLSelectElement extends HTMLElement {
        __options() { return this.querySelectorAll("option"); }
        __selectedOption() {
            const os = this.__options();
            for (const o of os) if (o.selected) return o;
            // A single (non-multiple) select with nothing explicitly selected
            // defaults to its first option (HTML spec).
            return (!this.multiple && os.length) ? os[0] : null;
        }
        __selectValue(val) {
            const os = this.__options(); let matched = false;
            for (const o of os) {
                const m = !matched && o.value === val;
                o.selected = m;
                if (m) matched = true;
            }
        }
        get options() { return this.__options(); }
        get selectedOptions() { return this.__options().filter((o) => o.selected); }
        get selectedIndex() {
            const os = this.__options();
            for (let i = 0; i < os.length; i++) if (os[i].selected) return i;
            return this.multiple ? -1 : (os.length ? 0 : -1);
        }
        set selectedIndex(i) {
            const os = this.__options(); i = Number(i);
            for (let k = 0; k < os.length; k++) os[k].selected = (k === i);
        }
        get multiple() { return this.hasAttribute("multiple"); }
        set multiple(v) { if (v) this.setAttribute("multiple", ""); else this.removeAttribute("multiple"); }
        get value() { const o = this.__selectedOption(); return o ? o.value : ""; }
        set value(v) { this.__selectValue(String(v)); }
    }
    // HTMLOptionElement.value falls back to text when the attribute is absent
    // (round-trips a valueless <option>). selected/defaultSelected both reflect
    // the `selected` content attribute the layout/form path reads; React's
    // <select> commit reads+writes both, so they're read-write.
    class HTMLOptionElement extends HTMLElement {
        get value() { const v = this.getAttribute("value"); return v === null ? this.textContent : v; }
        set value(v) { this.setAttribute("value", String(v)); }
        get selected() { return this.hasAttribute("selected"); }
        set selected(v) { if (v) this.setAttribute("selected", ""); else this.removeAttribute("selected"); }
        get defaultSelected() { return this.hasAttribute("selected"); }
        set defaultSelected(v) { if (v) this.setAttribute("selected", ""); else this.removeAttribute("selected"); }
    }
    // HTMLInputElement: value reflects the `value` attribute (no dirty-value
    // tracking here); checked reflects `checked`; the `type` IDL attribute
    // defaults to "text" when absent (React's change-event plugin keys off it).
    class HTMLInputElement extends HTMLElement {
        get value() { const v = this.getAttribute("value"); return v === null ? "" : v; }
        set value(v) { this.setAttribute("value", String(v)); }
        get checked() { return this.hasAttribute("checked"); }
        set checked(v) { if (v) this.setAttribute("checked", ""); else this.removeAttribute("checked"); }
        get type() { const t = this.getAttribute("type"); return t === null ? "text" : t.toLowerCase(); }
        set type(v) { this.setAttribute("type", String(v)); }
    }
    // <textarea>.value is its raw text content (no `value` content attribute) —
    // the form-submit path and formSet read/write the same.
    class HTMLTextAreaElement extends HTMLElement {
        get value() { return this.textContent; }
        set value(v) { this.textContent = String(v); }
    }
    installConstraintValidation(HTMLInputElement);
    installConstraintValidation(HTMLSelectElement);
    installConstraintValidation(HTMLTextAreaElement);
    // Interfaces whose simple reflected attributes are installed below.
    class HTMLButtonElement extends HTMLElement {}
    class HTMLSlotElement extends HTMLElement {
        assignedNodes(options) {
            return slotAssignedNodes(this, !!(options && options.flatten));
        }
        assignedElements(options) {
            return this.assignedNodes(options).filter((node) => node.nodeType === 1);
        }
    }
    // HTML §the-script-element. Keep this interface complete rather than
    // scattering script behavior across generic Element reflectors. The
    // force-async state is an internal flag: parser-created scripts begin
    // false, while Document.createElement marks new script elements true.
    class HTMLScriptElement extends HTMLElement {
        get type() { return this.getAttribute("type") || ""; }
        set type(v) { this.setAttribute("type", String(v)); }
        get src() {
            const raw = this.getAttribute("src");
            if (raw === null) return "";
            const parsed = __url_parse(raw, baseHref());
            return parsed ? parsed[0] : raw;
        }
        set src(v) { this.setAttribute("src", String(v)); }
        get noModule() { return this.hasAttribute("nomodule"); }
        set noModule(v) { if (v) this.setAttribute("nomodule", ""); else this.removeAttribute("nomodule"); }
        get async() { return this.__trustForceAsync === true || this.hasAttribute("async"); }
        set async(v) {
            this.__trustForceAsync = false;
            if (v) this.setAttribute("async", ""); else this.removeAttribute("async");
        }
        get defer() { return this.hasAttribute("defer"); }
        set defer(v) { if (v) this.setAttribute("defer", ""); else this.removeAttribute("defer"); }
        get blocking() {
            return this.__trustBlocking
                || (this.__trustBlocking = new DOMTokenList(this, "blocking", ["render"]));
        }
        get crossOrigin() {
            const raw = this.getAttribute("crossorigin");
            if (raw === null) return null;
            return raw.toLowerCase() === "use-credentials" ? "use-credentials" : "anonymous";
        }
        set crossOrigin(v) {
            if (v === null) this.removeAttribute("crossorigin");
            else this.setAttribute("crossorigin", String(v));
        }
        get referrerPolicy() {
            const raw = (this.getAttribute("referrerpolicy") || "").toLowerCase();
            return ["no-referrer", "no-referrer-when-downgrade", "origin", "origin-when-cross-origin", "same-origin", "strict-origin", "strict-origin-when-cross-origin", "unsafe-url"].includes(raw) ? raw : "";
        }
        set referrerPolicy(v) { this.setAttribute("referrerpolicy", String(v)); }
        get integrity() { return this.getAttribute("integrity") || ""; }
        set integrity(v) { this.setAttribute("integrity", String(v)); }
        get fetchPriority() {
            const raw = (this.getAttribute("fetchpriority") || "").toLowerCase();
            return raw === "high" || raw === "low" ? raw : "auto";
        }
        set fetchPriority(v) { this.setAttribute("fetchpriority", String(v)); }
        get text() { return this.textContent; }
        set text(v) { this.textContent = String(v); }
        // Obsolete but required IDL members (HTML §obsolete features).
        get charset() { return this.getAttribute("charset") || ""; }
        set charset(v) { this.setAttribute("charset", String(v)); }
        get event() { return this.getAttribute("event") || ""; }
        set event(v) { this.setAttribute("event", String(v)); }
        get htmlFor() { return this.getAttribute("for") || ""; }
        set htmlFor(v) { this.setAttribute("for", String(v)); }
        static supports(type) {
            type = String(type);
            return type === "classic" || type === "module" || type === "importmap" || type === "speculationrules";
        }
    }
    class HTMLFormElement extends HTMLElement {
        get elements() {
            return this.__trustElements
                || (this.__trustElements = new HTMLFormControlsCollection(this));
        }
        get length() { return this.elements.length; }
        checkValidity() {
            for (const control of listedFormControls(this)) {
                if (typeof control.checkValidity === "function" && !control.checkValidity()) return false;
            }
            return true;
        }
        reportValidity() { return this.checkValidity(); }
        submit() {
            // HTML §4.10.22.3: submit() bypasses constraint validation and
            // does not fire a submit event; it runs the form submission
            // algorithm directly.
            if (!this.isConnected || this.__trustFiringSubmit) return;
            trust.queueFormSubmit(this.__id, null);
        }
        requestSubmit(submitter) {
            const supplied = arguments.length > 0 && submitter !== undefined;
            if (supplied) {
                if (!submitter || submitControlFor(submitter) !== submitter) {
                    throw new TypeError("Failed to execute 'requestSubmit' on 'HTMLFormElement': The specified element is not a submit button.");
                }
                if (formOwner(submitter) !== this) {
                    throw new DOMException("The specified element is not owned by this form element.", "NotFoundError");
                }
            } else {
                submitter = null;
            }
            // The form submission algorithm aborts before validation/event
            // dispatch when the form cannot navigate.
            if (!this.isConnected || this.__trustFiringSubmit) return;
            const skipValidation = this.hasAttribute("novalidate")
                || (submitter && submitter.hasAttribute("formnovalidate"));
            if (!skipValidation && !this.checkValidity()) return;
            const ev = new SubmitEvent("submit", {
                bubbles: true,
                cancelable: true,
                submitter: submitter,
            });
            this.__trustFiringSubmit = true;
            try {
                dispatch(this, ev, false);
            } finally {
                this.__trustFiringSubmit = false;
            }
            if (!ev.defaultPrevented) {
                if (handleDialogSubmission(this, submitter)) return;
                trust.queueFormSubmit(this.__id, submitter ? submitter.__id : null);
            }
        }
    }
    class HTMLImageElement extends HTMLElement {
        get currentSrc() { return __image_current_src(this.__id); }
        get complete() { return __image_complete(this.__id); }
    }
    // HTMLHyperlinkElementUtils (the create-an-<a>-to-parse-URLs trick;
    // router-slot reads m.pathname) lives on <a> and <area>; href + the URL
    // components are installed via installUrlParts below.
    class HTMLAnchorElement extends HTMLElement {
        get relList() { return relListFor(this, HYPERLINK_REL_SUPPORTED); }
        set relList(v) { this.relList.value = String(v); }
    }
    class HTMLAreaElement extends HTMLElement {
        get relList() { return relListFor(this, HYPERLINK_REL_SUPPORTED); }
        set relList(v) { this.relList.value = String(v); }
    }
    // contentDocument/contentWindow (the nested browsing context) on <iframe>/
    // <frame>; installed below so both share one body.
    class HTMLIFrameElement extends HTMLElement {}
    class HTMLFrameElement extends HTMLElement {}
    // <template>.content is the inert fragment its markup parses into (read-only).
    class HTMLTemplateElement extends HTMLElement {
        get content() { return wrap(__dom_template_content(this.__id)); }
    }
    // HTMLMetaElement.content reflects the `content` attribute (pixiv stashes
    // boot config as JSON in <meta content='{…}'> and does JSON.parse(meta.content)).
    class HTMLMetaElement extends HTMLElement {
        get content() { return this.getAttribute("content") || ""; }
        set content(v) { this.setAttribute("content", String(v)); }
    }
    // <style>.sheet — the parsed CSSOM view of this element's CSS text, re-parsed
    // when textContent changes. <link>.sheet stays null (we don't model the
    // per-link loaded stylesheet; document.styleSheets is the page-level view).
    class HTMLStyleElement extends HTMLElement {
        get sheet() {
            const text = this.textContent || "";
            if (!this.__sheet || this.__sheetText !== text) {
                this.__sheet = makeStyleSheet(text, this);
                this.__sheetText = text;
            }
            return this.__sheet;
        }
    }
    class HTMLLinkElement extends HTMLElement {
        // HTML §4.2.4: SameObject DOMTokenList reflecting the rel attribute.
        // MSN uses this for feature detection before initializing its app.
        get relList() { return relListFor(this, LINK_REL_SUPPORTED); }
        set relList(v) { this.relList.value = String(v); }
        get sheet() { return null; }
    }
    // <dialog> (HTML §4.11.4). We implement the observable method/event surface;
    // the modal TOP LAYER + backdrop + inertness are deliberately NOT rendered
    // (her call — a dialog just paints where it flows, overlaps allowed). `open`
    // reflects the boolean attribute (the layout's visibility gate); modal-ness
    // is tracked in `__dlgModal`. show/showModal/close/requestClose follow the
    // spec steps: InvalidStateError on an already-open show(Modal), showModal
    // also requires connection; requestClose fires a cancelable `cancel` and
    // aborts if prevented; "close the dialog" removes `open`, sets returnValue,
    // and fires `close`. beforetoggle/toggle (ToggleEvent) fire like popovers.
    // `closedBy` is intentionally omitted (its enumerated defaults are too new
    // to implement without guessing — feature-detectable as undefined).
    class HTMLDialogElement extends HTMLElement {
        get open() { return this.hasAttribute("open"); }
        set open(v) { if (v) this.setAttribute("open", ""); else this.removeAttribute("open"); }
        get returnValue() { return this.__dlgReturn == null ? "" : this.__dlgReturn; }
        set returnValue(v) { this.__dlgReturn = v == null ? "" : String(v); }
        show() {
            if (this.hasAttribute("open")) {
                if (!this.__dlgModal) return;               // already open (non-modal): no-op
                throw new DOMException("The dialog is already open as a modal dialog", "InvalidStateError");
            }
            const bev = new g.ToggleEvent("beforetoggle", { oldState: "closed", newState: "open", cancelable: true });
            dispatch(this, bev, false);
            if (bev.defaultPrevented || this.hasAttribute("open")) return;
            this.setAttribute("open", "");
            this.__dlgModal = false;
            this.__dlgToggle("closed", "open");
        }
        showModal() {
            if (this.hasAttribute("open")) throw new DOMException("The dialog is already open", "InvalidStateError");
            if (!this.isConnected) throw new DOMException("The dialog is not connected", "InvalidStateError");
            const bev = new g.ToggleEvent("beforetoggle", { oldState: "closed", newState: "open", cancelable: true });
            dispatch(this, bev, false);
            if (bev.defaultPrevented || this.hasAttribute("open")) return;
            this.setAttribute("open", "");
            this.__dlgModal = true;
            this.__dlgToggle("closed", "open");
        }
        close(returnValue) {
            this.__dlgClose(arguments.length ? String(returnValue) : null);
        }
        requestClose(returnValue) {
            if (!this.hasAttribute("open")) return;
            const cev = new Event("cancel", { cancelable: true });
            dispatch(this, cev, false);
            if (cev.defaultPrevented) return;
            this.__dlgClose(arguments.length ? String(returnValue) : null);
        }
        // "Close the dialog": remove `open`, set returnValue, queue toggle + close.
        __dlgClose(result) {
            if (!this.hasAttribute("open")) return;
            dispatch(this, new g.ToggleEvent("beforetoggle", { oldState: "open", newState: "closed" }), false);
            if (!this.hasAttribute("open")) return;
            this.removeAttribute("open");
            this.__dlgModal = false;
            if (result !== null) this.__dlgReturn = result;
            const self = this;
            g.setTimeout(function () {
                dispatch(self, new g.ToggleEvent("toggle", { oldState: "open", newState: "closed" }), false);
                dispatch(self, new Event("close"), false);
            }, 0);
        }
        __dlgToggle(oldState, newState) {
            const self = this;
            g.setTimeout(function () { dispatch(self, new g.ToggleEvent("toggle", { oldState: oldState, newState: newState }), false); }, 0);
        }
    }

    // Obsolete but required by HTML §16.3.1. The rendering layer samples the
    // same page-relative clock from these internal pause markers, so stop()
    // freezes at the current step and start() resumes without counting the
    // paused interval.
    class HTMLMarqueeElement extends HTMLElement {
        get behavior() { return this.getAttribute("behavior") || ""; }
        set behavior(v) { this.setAttribute("behavior", String(v)); }
        get bgColor() { return this.getAttribute("bgcolor") || ""; }
        set bgColor(v) { this.setAttribute("bgcolor", String(v)); }
        get direction() { return this.getAttribute("direction") || ""; }
        set direction(v) { this.setAttribute("direction", String(v)); }
        get height() { return this.getAttribute("height") || ""; }
        set height(v) { this.setAttribute("height", String(v)); }
        get width() { return this.getAttribute("width") || ""; }
        set width(v) { this.setAttribute("width", String(v)); }
        get hspace() { return Math.max(0, Number(this.getAttribute("hspace")) || 0) >>> 0; }
        set hspace(v) { this.setAttribute("hspace", String(Number(v) >>> 0)); }
        get vspace() { return Math.max(0, Number(this.getAttribute("vspace")) || 0) >>> 0; }
        set vspace(v) { this.setAttribute("vspace", String(Number(v) >>> 0)); }
        get scrollAmount() {
            const n = Number(this.getAttribute("scrollamount"));
            return Number.isFinite(n) && n >= 0 ? n >>> 0 : 6;
        }
        set scrollAmount(v) { this.setAttribute("scrollamount", String(Number(v) >>> 0)); }
        get scrollDelay() {
            const n = Number(this.getAttribute("scrolldelay"));
            return Number.isFinite(n) && n >= 0 ? n >>> 0 : 85;
        }
        set scrollDelay(v) { this.setAttribute("scrolldelay", String(Number(v) >>> 0)); }
        get trueSpeed() { return this.hasAttribute("truespeed"); }
        set trueSpeed(v) { if (v) this.setAttribute("truespeed", ""); else this.removeAttribute("truespeed"); }
        get loop() {
            const n = Number(this.getAttribute("loop"));
            return Number.isInteger(n) && n >= 1 ? n : -1;
        }
        set loop(v) {
            const n = Number(v);
            if (Number.isInteger(n) && (n > 0 || n === -1)) this.setAttribute("loop", String(n));
        }
        start() {
            const stopped = Number(this.getAttribute("data-trust-marquee-stopped"));
            if (!Number.isFinite(stopped) || stopped < 0) return;
            const now = currentTime() / 1000;
            const total = Number(this.getAttribute("data-trust-marquee-paused-total")) || 0;
            this.setAttribute("data-trust-marquee-paused-total", String(Math.max(0, total + now - stopped)));
            this.removeAttribute("data-trust-marquee-stopped");
        }
        stop() {
            if (this.hasAttribute("data-trust-marquee-stopped")) return;
            this.setAttribute("data-trust-marquee-stopped", String(currentTime() / 1000));
        }
    }

    // wrap() dispatches a node id (type 1) to its interface class by tag. The map
    // is memoized (localName is immutable, so a class only resolves once); an
    // interface with no specialized class falls back to the generic HTMLElement.
    // Reuses htmlInterfaceName + the interface-constructor globals (set below).
    const ELEM_CLASS = new Map();
    function classFor(local) {
        let C = ELEM_CLASS.get(local);
        if (C !== undefined) return C;
        // The interface class comes from the GLOBAL (so a page that subclasses,
        // say, HTMLElement keeps inheriting our methods). BUT a page may REPLACE
        // `window.HTMLElement` with a custom-element ES5 shim — YouTube/Polymer
        // installs `HTMLElement = function(){ return Reflect.construct(real, [],
        // newTarget) }` so ES5 transpiled classes can `extend` it. That shim
        // constructs with an EMPTY argument list, so `new (g.HTMLElement)(id)`
        // DROPS the node id we pass — every wrapper it builds gets `__id ===
        // undefined`, and the element then reads as detached (`isConnected`
        // false, `parentNode` null), so Polymer never connects/stamps it. Always
        // use our OWN lexical base classes for construction (they faithfully
        // forward the id to the Node constructor); the global is only consulted
        // for the genuinely-specialized interfaces it still owns.
        const name = htmlInterfaceName(local);
        C = name === "HTMLElement" ? HTMLElement : g[name] || HTMLElement;
        ELEM_CLASS.set(local, C);
        return C;
    }

    // The HTML spec reflects name/value/type/href/src/disabled on SEVERAL
    // interfaces; installing each only on its owners (not the shared Element)
    // makes `"name" in div === false` while keeping every value-bearing tag
    // working. `reflectOn` skips an interface that already defines its own
    // specialized accessor (HTMLSelectElement.value, HTMLInputElement.type).
    function reflectOn(names, prop, descFactory) {
        for (let i = 0; i < names.length; i++) {
            const C = g[names[i]];
            if (!C || Object.prototype.hasOwnProperty.call(C.prototype, prop)) continue;
            Object.defineProperty(C.prototype, prop, descFactory(prop));
        }
    }
    const reflectStrDesc = (attr) => ({
        get() { return this.getAttribute(attr) || ""; },
        set(v) { this.setAttribute(attr, String(v)); },
        configurable: true, enumerable: false,
    });
    const reflectBoolDesc = (attr) => ({
        get() { return this.hasAttribute(attr); },
        set(v) { if (v) this.setAttribute(attr, ""); else this.removeAttribute(attr); },
        configurable: true, enumerable: false,
    });
    const reflectUrlDesc = (attr) => ({
        get() { const r = this.getAttribute(attr); if (r === null) return ""; const u = __url_parse(r, baseHref()); return u ? u[0] : r; },
        set(v) { this.setAttribute(attr, String(v)); },
        configurable: true, enumerable: false,
    });
    // HTML §4.6.3: URL component getters reinitialize the hyperlink URL from
    // its href content attribute, and component setters run the corresponding
    // URL state-override parser before updating href. `i` is a function
    // parameter so the getter never captures a block-scoped loop local (trap
    // #6 hygiene). An href-less hyperlink has a null URL: protocol is ":" and
    // the other decomposition members are empty; setters are no-ops.
    function urlPartDesc(i, readOnly) {
        const d = { configurable: true, enumerable: false,
            get() {
                const raw = this.getAttribute("href");
                if (raw === null) return i === 1 ? ":" : "";
                const u = __url_parse(raw, baseHref());
                return u ? u[i] : (i === 1 ? ":" : "");
            } };
        if (!readOnly) {
            d.set = function (value) {
                const raw = this.getAttribute("href");
                if (raw === null) return;
                const u = __url_parse(raw, baseHref());
                if (!u) return;
                const updated = __url_set(u[0], [
                    "", "protocol", "host", "hostname", "port", "pathname", "search", "hash", "origin",
                ][i], String(value));
                if (updated) this.setAttribute("href", updated[0]);
            };
        }
        return d;
    }
    function installUrlParts(Cls) {
        Object.defineProperty(Cls.prototype, "protocol", urlPartDesc(1, false));
        Object.defineProperty(Cls.prototype, "host", urlPartDesc(2, false));
        Object.defineProperty(Cls.prototype, "hostname", urlPartDesc(3, false));
        Object.defineProperty(Cls.prototype, "port", urlPartDesc(4, false));
        Object.defineProperty(Cls.prototype, "pathname", urlPartDesc(5, false));
        Object.defineProperty(Cls.prototype, "search", urlPartDesc(6, false));
        Object.defineProperty(Cls.prototype, "hash", urlPartDesc(7, false));
        Object.defineProperty(Cls.prototype, "origin", urlPartDesc(8, true));
    }
    installUrlParts(HTMLAnchorElement);
    installUrlParts(HTMLAreaElement);
    // CSSOM View §4 requires Window.innerWidth/innerHeight to report THIS
    // browsing context's viewport.  For a nested navigable that viewport is
    // the iframe's content box, not the top-level viewport.  Prefer the layout
    // engine's current border box; a newly inserted iframe can start running
    // its fetched document before the next presentation pass, so retain the
    // HTML replaced-element 300x150 defaults and resolve simple authored
    // px/% sizes against its containing block for that interval.
    function frameStyleValue(el, name) {
        try { return String(g.getComputedStyle(el).getPropertyValue(name) || "").trim(); }
        catch (e) { return ""; }
    }
    function frameEdgeSum(el, axis) {
        const names = axis === "width"
            ? ["padding-left", "padding-right", "border-left-width", "border-right-width"]
            : ["padding-top", "padding-bottom", "border-top-width", "border-bottom-width"];
        let sum = 0;
        for (const name of names) {
            const n = parseFloat(frameStyleValue(el, name));
            if (Number.isFinite(n) && n > 0) sum += n;
        }
        return sum;
    }
    function frameMeasuredContentSize(el, axis) {
        let r = null;
        try { r = __dom_rect(el.__id); } catch (e) {}
        if (!r) return 0;
        const borderBox = +(axis === "width" ? r[2] : r[3]);
        if (!(borderBox > 0)) return 0;
        return Math.max(0, borderBox - frameEdgeSum(el, axis));
    }
    function frameContainingBlockSize(el, axis, depth) {
        const parent = el && el.parentElement;
        if (!parent || parent.localName === "html")
            return axis === "width" ? +g.innerWidth : +g.innerHeight;
        const measured = frameMeasuredContentSize(parent, axis);
        if (measured > 0) return measured;
        if (depth < 4) {
            const specified = frameSpecifiedContentSize(parent, axis, depth + 1);
            if (specified > 0) return specified;
        }
        return axis === "width" ? +g.innerWidth : +g.innerHeight;
    }
    function frameSpecifiedContentSize(el, axis, depth) {
        const raw = frameStyleValue(el, axis);
        let size = 0;
        if (/^-?(?:\d+|\d*\.\d+)px$/i.test(raw)) size = parseFloat(raw);
        else if (/^-?(?:\d+|\d*\.\d+)%$/.test(raw))
            size = frameContainingBlockSize(el, axis, depth) * parseFloat(raw) / 100;
        else if (raw === "0") size = 0;
        if (!(size >= 0) || !Number.isFinite(size)) return 0;
        if (frameStyleValue(el, "box-sizing").toLowerCase() === "border-box")
            size = Math.max(0, size - frameEdgeSum(el, axis));
        return size;
    }
    function frameViewportDimension(frame, axis) {
        const measured = frameMeasuredContentSize(frame, axis);
        if (measured > 0) return measured;
        const specified = frameSpecifiedContentSize(frame, axis, 0);
        if (specified > 0) return specified;
        // HTML's width/height dimension attributes provide CSS-pixel hints for
        // replaced elements. Invalid, negative and absent values use the
        // iframe default dimensions from HTML's rendering rules.
        const attr = frame.getAttribute(axis);
        if (attr !== null && /^\s*\d+\s*$/.test(attr)) return +attr.trim();
        return axis === "width" ? 300 : 150;
    }
    // Last viewport size observed for each nested navigable. CSSOM View §13.1
    // runs resize steps for every Document whose own viewport changed,
    // including when an iframe's dimensions change. TRust multiplexes those
    // Window objects through one realm, so this per-frame state is also what
    // lets a top-level resize target each logical Window exactly once.
    const frameViewportSizes = new WeakMap();
    function rememberFrameViewport(frame, width, height) {
        frameViewportSizes.set(frame, [width, height]);
    }
    function fireChangedFrameViewportResizes() {
        let frames = [];
        try { frames = g.document.querySelectorAll("iframe, frame"); } catch (e) { return; }
        // Document order visits an embedding frame before frames in its child
        // document. A parent handler may resize a nested iframe (SCM Player's
        // outer resize handler does exactly that), so the child's dimensions
        // are sampled only after that parent callback has completed.
        for (let i = 0; i < frames.length; i++) {
            const frame = frames[i];
            if (!frame.__frameUrl) continue;
            const width = frameViewportDimension(frame, "width");
            const height = frameViewportDimension(frame, "height");
            const old = frameViewportSizes.get(frame);
            rememberFrameViewport(frame, width, height);
            if (!old || (old[0] === width && old[1] === height)) continue;
            runInFrame(frame, function () {
                dispatch(g, new Event("resize"), false);
            });
        }
    }
    function mediaQueryListForViewport(query, viewport) {
        const q = String(query);
        return {
            media: q,
            get matches() {
                const size = viewport();
                return !!__match_media(q, size[0], size[1]);
            },
            onchange: null,
            addListener() {}, removeListener() {},
            addEventListener() {}, removeEventListener() {},
            dispatchEvent() { return false; },
        };
    }
    // HTMLIFrameElement.contentDocument / .contentWindow — the nested browsing
    // context's document and WindowProxy. Backed by a real same-arena
    // FrameDocument: the near-universal idiom `iframe.contentDocument ||
    // iframe.contentWindow.document` reads them unconditionally, and srcdoc/
    // document.write content renders inline. A cross-origin nested document
    // renders but isn't script-accessible (contentDocument → null).
    function installFrameSurface(Cls) {
        Object.defineProperty(Cls.prototype, "contentDocument", { configurable: true, enumerable: false,
            get() {
                ensureFrameProcessed(this); // load src/srcdoc if a script reads us early
                if (this.__frameUrl && !frameSameOrigin(this.__frameUrl)) return null;
                return this.__contentDoc || (this.__contentDoc = new FrameDocument(this));
            } });
        Object.defineProperty(Cls.prototype, "contentWindow", { configurable: true, enumerable: false,
            get() {
                if (!this.__contentWin) {
                    const frame = this;
                    this.__contentWin = {
                        get document() { return frame.contentDocument; },
                        get location() {
                            const u = frame.__frameUrl;
                            const href = u && u !== "about:srcdoc" ? u : "about:blank";
                            const parsed = __url_parse(href, g.location.href);
                            return {
                                href: href,
                                origin: parsed ? parsed[8] : "null",
                                replace(v) { try { frame.setAttribute("src", String(v)); } catch (e) {} },
                                assign(v) { try { frame.setAttribute("src", String(v)); } catch (e) {} },
                            };
                        },
                        parent: g, top: g, frames: g, frameElement: this,
                        // Parent code may address a child browsing context
                        // through WindowProxy.postMessage. Route the task to
                        // the child's scoped global rather than dropping it;
                        // reCAPTCHA's anchor protocol depends on this.
                        postMessage(message, targetOrigin, transfer) {
                            postMessageToFrame(frame, message, g,
                                transferPorts(targetOrigin, transfer), g.location.origin,
                                frame.__trustParentWindow || g);
                        },
                        matchMedia(query) {
                            return mediaQueryListForViewport(query, function () {
                                return [frameViewportDimension(frame, "width"),
                                    frameViewportDimension(frame, "height")];
                            });
                        },
                        focus() {}, blur() {},
                        addEventListener() {}, removeEventListener() {},
                    };
                    // A same-origin frame's contentWindow is a real Window with
                    // the standard constructors. We run one realm, so exposing the
                    // realm's globals on it is faithful: the well-known "pristine
                    // constructor" idiom `let {URL}=iframe.contentWindow` (grabbing
                    // an unmonkeypatched URL/Function from a hidden frame — ChatGPT's
                    // auth bundle does this) then finds `URL` et al instead of
                    // undefined → `new (undefined)()` "not a constructor". Own
                    // getters below (document/location/self/window/parent…) still
                    // shadow the global's.
                    Object.setPrototypeOf(this.__contentWin, g);
                    Object.defineProperties(this.__contentWin, {
                        innerWidth: { configurable: true, enumerable: true,
                            get() { return frameViewportDimension(frame, "width"); } },
                        innerHeight: { configurable: true, enumerable: true,
                            get() { return frameViewportDimension(frame, "height"); } },
                    });
                    this.__contentWin.self = this.__contentWin;
                    this.__contentWin.window = this.__contentWin;
                }
                return this.__contentWin;
            } });
    }
    installFrameSurface(HTMLIFrameElement);
    installFrameSurface(HTMLFrameElement);

    // CharacterData: the shared text-bearing interface for Text and Comment.
    // `data` is [LegacyNullToEmptyString] — null becomes "" (but undefined
    // stringifies to "undefined"); `length` is the data's UTF-16 length.
    class CharacterData extends Node {
        get data() { return __dom_text(this.__id) || ""; }
        // The single choke point for text/comment data changes: `data`,
        // `nodeValue`, `appendData`/`insertData`/`deleteData`/`replaceData`, and
        // Node's `textContent` (on a text node) all route here, so the
        // characterData MutationRecord is emitted from this one setter.
        set data(v) {
            v = v === null ? "" : String(v);
            if (!MO.length) { __dom_set_text(this.__id, v); return; }
            const old = __dom_text(this.__id) || "";
            __dom_set_text(this.__id, v);
            moCharData(this, old);
        }
        get nodeValue() { return this.data; }
        set nodeValue(v) { this.data = v; }
        get length() { return this.data.length; }
        // offset/count are WebIDL `unsigned long` — ToUint32 (`>>> 0`) maps a
        // negative like `-2**32 + 2` to 2; `offset > length` is an
        // IndexSizeError; count is clamped to the remaining length.
        substringData(offset, count) {
            if (arguments.length < 2) throw new TypeError("2 arguments required");
            const d = this.data, o = offset >>> 0;
            if (o > d.length) throw new DOMException("offset out of bounds", "IndexSizeError");
            return d.slice(o, o + Math.min(count >>> 0, d.length - o));
        }
        appendData(s) {
            if (arguments.length < 1) throw new TypeError("1 argument required");
            this.data = this.data + String(s);
        }
        insertData(offset, s) { this.replaceData(offset, 0, s); }
        deleteData(offset, count) { this.replaceData(offset, count, ""); }
        replaceData(offset, count, s) {
            if (arguments.length < 3) throw new TypeError("3 arguments required");
            const d = this.data, o = offset >>> 0;
            if (o > d.length) throw new DOMException("offset out of bounds", "IndexSizeError");
            const c = Math.min(count >>> 0, d.length - o);
            this.data = d.slice(0, o) + String(s) + d.slice(o + c);
        }
    }
    class Text extends CharacterData { get nodeType() { return 3; } get nodeName() { return "#text"; } get [Symbol.toStringTag]() { return "Text"; } }

    class Document extends Node {
        get nodeType() { return 9; }
        get nodeName() { return "#document"; }
        get [Symbol.toStringTag]() { return "HTMLDocument"; }
        // `document.all` — the legacy `HTMLAllCollection`, the web's one
        // `[[IsHTMLDDA]]` object (Annex B.3.6): falsy, `typeof "undefined"`, and
        // `== null`/`== undefined`, but a stable distinct object for `===`. Minted
        // by the engine (`__html_dda`) and cached so its identity is stable across
        // reads (polymer_resin and others compare `value === document.all`). We do
        // not implement its named/indexed element access — identity + the falsy
        // semantics are what the platform actually depends on here.
        // NB: cache-test is `=== null`, NOT `||` — the cached value is the falsy
        // `[[IsHTMLDDA]]` object, so a truthiness test would re-mint it every read
        // and break `document.all === document.all` identity.
        get all() { if (documentAllValue === null) documentAllValue = __html_dda(); return documentAllValue; }
        // A document has NO owner document (DOM §`Document` overrides Node's
        // `ownerDocument` to null). Inheriting Node's "return the document" here
        // made `document.ownerDocument === document`, which sent ProseMirror's
        // shadow-root getSelection shim (`() => n.ownerDocument.getSelection()`)
        // into infinite recursion when it patched a missing `document.getSelection`.
        get ownerDocument() { return null; }
        // HTML §6.6.6 DocumentOrShadowRoot.activeElement getter. The viewport
        // focus fallback is body/documentElement, never JavaScript undefined.
        get activeElement() { return activeElementFor(this); }
        hasFocus() { return true; }
        // `document.getSelection()` is the Selection API alias for
        // `window.getSelection()`. Without it ProseMirror monkey-patches the
        // root's prototype to add one — see `ownerDocument` above.
        getSelection() { return g.getSelection(); }
        // The main document (node 0) reads its root element by direct syscall
        // (hot path). A DETACHED document (a `DOMParser` result, `__id !== 0`)
        // scopes to its OWN subtree instead — `__dom_doc_element` only knows the
        // live tree's root.
        get documentElement() { return this.__id === 0 ? wrap(__dom_doc_element()) : (this.querySelector("html") || this.firstElementChild); }
        // The element that scrolls the viewport (CSSOM View). Standards mode ⇒
        // the document element; its scrollTop/scrollHeight/clientHeight mirror
        // the page scroll, so `document.scrollingElement.scrollTop` reads the
        // threaded scroll position. The infinite-scroll idiom
        // `window.pageYOffset || document.scrollingElement.scrollTop` resolves.
        get scrollingElement() { return this.documentElement; }
        get body() { return this.querySelector("body"); }
        get head() { return this.querySelector("head"); }
        get readyState() { return trust.readyState; }
        // CSS Font Loading Module Level 3 §4.2: a document's font source is a
        // stable FontFaceSet.  Its setlike collection is independent per
        // Document, including detached documents created by DOMParser.
        get fonts() { return this.__fonts || (this.__fonts = new FontFaceSet()); }
        get title() { const t = this.querySelector("title"); return t ? t.textContent : ""; }
        // HTML §the title element: setting with no <title> CREATES one in the
        // head (the old setter silently dropped the write); no head → no-op.
        set title(v) {
            let t = this.querySelector("title");
            if (!t) {
                const head = this.querySelector("head");
                if (!head) return;
                t = this.createElement("title");
                head.appendChild(t);
            }
            t.textContent = String(v);
        }
        get cookie() { return __cookie_get(); }
        set cookie(v) { __cookie_set(String(v)); }
        get location() { return g.location; }
        // HTML §2.4.3: the document base URL is used by relative URL APIs,
        // including new URL("_framework/dotnet.js", document.baseURI).
        // document.URL is the document URL and may differ when <base> exists.
        get baseURI() { return baseHref(); }
        // The document's origin domain. Spec returns the origin's effective
        // domain (the host); a terminal browser has no frames, so the host IS
        // the domain. The setter is the legacy same-origin relaxation — store
        // it so a `document.domain = document.domain` round-trips, but it has
        // no cross-origin effect here. Missing this throws on sites that read
        // it (GitHub's behaviors bundle: "Unable to get document domain").
        get domain() { return this.__domain !== undefined ? this.__domain : g.location.hostname; }
        set domain(v) { this.__domain = String(v); }
        get defaultView() { return g; }
        // `document.referrer` (HTML §3.1.5): the address of the page that linked
        // here, or the EMPTY STRING for a direct navigation / when policy strips
        // it. The spec contract is that it is ALWAYS a string — never undefined.
        // TRust navigations don't thread a referrer into page JS, so we report
        // "" (direct navigation — a valid, common value). Missing it entirely
        // (undefined) broke connected-react-router: its reducer seeds initial
        // state from `document.referrer`, so an undefined value made that slice
        // reducer return undefined on INIT → Redux's "reducer returned undefined
        // during initialization" (#12), which aborts the whole store + render.
        get referrer() { return ""; }
        get documentURI() { return g.location.href; }
        get URL() { return g.location.href; }
        get currentScript() { return wrap(trust.currentScript); }
        get implementation() {
            const doc = this;
            return {
                createHTMLDocument() {
                    // A detached mini-document, real enough for jQuery's
                    // support checks and parseHTML: same arena, same API.
                    const html = doc.createElement("html");
                    const head = doc.createElement("head");
                    const body = doc.createElement("body");
                    html.appendChild(head); html.appendChild(body);
                    return {
                        documentElement: html, head: head, body: body,
                        createElement: (t) => doc.createElement(t),
                        createTextNode: (s) => doc.createTextNode(s),
                        createDocumentFragment: () => doc.createDocumentFragment(),
                        getElementsByTagName: (t) => html.getElementsByTagName(t),
                        querySelector: (s) => html.querySelector(s),
                        querySelectorAll: (s) => html.querySelectorAll(s),
                        createRange: () => new Range(),
                        createNodeIterator: (r, w) => new NodeIterator(r, w),
                        createTreeWalker: (r, w, f) => new TreeWalker(r, w, f),
                    };
                },
            };
        }
        // DOM Standard §4.5: adoptNode removes a node from its old parent,
        // changes the node document for its entire shadow-including subtree,
        // and returns the very same node object.  The Rust arena performs the
        // pointer/owner-document transition; this layer exposes the specified
        // exceptions and invokes custom-element adoptedCallback steps.
        adoptNode(node) {
            if (!(this instanceof Document)) {
                throw new TypeError("Illegal invocation");
            }
            if (!(node instanceof Node)) {
                throw new TypeError("Failed to execute 'adoptNode': parameter 1 is not of type 'Node'");
            }
            if (node.nodeType === 9) {
                throw new DOMException("The node is a document", "NotSupportedError");
            }
            if (node instanceof ShadowRoot) {
                throw new DOMException("The node is a shadow root", "HierarchyRequestError");
            }
            const oldId = __dom_adopt(this.__id, node.__id);
            if (oldId === -3) throw new TypeError("Illegal invocation");
            if (oldId === -4) throw new DOMException("The node is a document", "NotSupportedError");
            if (oldId === -5) throw new DOMException("The node is a shadow root", "HierarchyRequestError");
            if (oldId < 0) throw new TypeError("The node is not valid");
            const oldDocument = wrap(oldId);
            if (oldDocument !== this) ceAdopt(node, oldDocument, this);
            return node;
        }
        get forms() { return this.querySelectorAll("form"); }
        get links() { return this.querySelectorAll("a[href]"); }
        get images() { return this.querySelectorAll("img"); }
        // The arena keeps script ELEMENTS (only the render serializer drops
        // them), so this is the real collection — it returned `[]` before,
        // hiding every script from a view-transitions swap that re-executes
        // the new document's scripts.
        get scripts() { return this.querySelectorAll("script"); }
        // CSSOM §document.styleSheets: the document's sheets in tree order.
        // Our cascade folds fetched <link> sheets in Rust-side (link.sheet is
        // null — no CSSOM object exists for them), so the list is the <style>
        // elements' parsed sheets. Didn't exist at all before — code iterating
        // it threw on `undefined`.
        get styleSheets() {
            return new StyleSheetList(this.querySelectorAll("style").map((s) => s.sheet));
        }
        createElement(t) {
            const el = wrap(__dom_create_element(String(t)));
            // HTML's script-element creation steps give dynamically created
            // scripts a true force-async flag. Setting async (as an IDL or
            // content attribute) clears it; parser-created wrappers never get
            // this marker and therefore default to false.
            if (el.localName === "script") el.__trustForceAsync = true;
            const ctor = CE.defs.get(String(t).toLowerCase());
            if (ctor) upgradeElement(el, ctor);
            return el;
        }
        get adoptedStyleSheets() { return this.__adopted || (this.__adopted = []); }
        set adoptedStyleSheets(v) { this.__adopted = v; adoptedSync(this); }
        createElementNS(_, t) { return this.createElement(t); }
        createTextNode(s) { return wrap(__dom_create_text(s === undefined ? "" : String(s))); }
        createComment(s) { return wrap(__dom_create_comment(s === undefined ? "" : String(s))); }
        // A detached Attr (DOM §4.9.2): a plain object matching what the
        // `attributes` NamedNodeMap yields, so setAttributeNode can consume it.
        createAttribute(n) {
            n = String(n).toLowerCase();
            return { name: n, localName: n, nodeName: n, namespaceURI: null, prefix: null, specified: true, ownerElement: null, value: "", nodeValue: "" };
        }
        createAttributeNS(_ns, n) { const a = this.createAttribute(String(n)); return a; }
        // DOM §4.5 `importNode`: clone into THIS document and use its custom
        // element registry as the fallback registry. Template contents belong
        // to an inert template document, so this is observably different from
        // `template.content.cloneNode(true)`: import queues upgrade reactions
        // for defined descendants and invokes them before returning to author
        // script ([CEReactions]). Polymer stamps imported templates, binds
        // object properties to the detached custom elements, then inserts the
        // fragment; postponing upgrade until insertion lets those own properties
        // shadow the component's reactive prototype setters forever.
        importNode(n, options) {
            if (!n || typeof n.__id !== "number")
                throw new TypeError("Failed to execute 'importNode': parameter 1 is not of type 'Node'");
            if (n.nodeType === 9 || n instanceof ShadowRoot)
                throw new DOMException("Documents and shadow roots cannot be imported", "NotSupportedError");
            let subtree = false;
            if (typeof options === "boolean") subtree = options;
            else if (options && typeof options === "object") {
                subtree = !options.selfOnly;
                const registry = options.customElementRegistry;
                if (registry !== undefined && registry !== null && registry !== customElements)
                    throw new DOMException("Unsupported custom element registry", "NotSupportedError");
            }
            const clone = n.cloneNode(subtree);
            // `cloneNode` has completed the whole subtree now, matching the
            // clone algorithm's reaction boundary: constructors run in tree
            // order, against the complete clone, but connectedCallback waits
            // until the still-detached result is inserted.
            if (CE.defs.size) ceScan(clone);
            return clone;
        }
        // DOM Standard ParentNode methods. Document used to inherit these
        // from Node; keep its own implementation now that CharacterData no
        // longer exposes selector methods.
        querySelector(s) { const r = __dom_query(this.__id, String(s), true); return r.length ? wrap(r[0]) : null; }
        querySelectorAll(s) { return __dom_query(this.__id, String(s), false).map(wrap); }
        getElementsByTagName(t) { return this.querySelectorAll(String(t)); }
        getElementsByClassName(c) { return this.querySelectorAll(String(c).trim().split(/\s+/).map((x) => "." + x).join("")); }
        createTreeWalker(root, whatToShow, filter) { return new TreeWalker(root, whatToShow, filter); }
        createNodeIterator(root, whatToShow) { return new NodeIterator(root, whatToShow); }
        createDocumentFragment() { return wrap(__dom_create_fragment()); }
        createRange() { return new Range(); }
        getElementById(i) {
            // `__dom_get_by_id` scans the LIVE tree; a detached parsed document
            // (`__id !== 0`) must scan its own subtree instead.
            if (this.__id !== 0) {
                for (const e of this.querySelectorAll("[id]")) if (e.id === String(i)) return e;
                return null;
            }
            return wrap(__dom_get_by_id(String(i)));
        }
        // Quoted + escaped: an unquoted `[name=X]` broke on any name with
        // selector-special characters (the FrameDocument version already quoted).
        getElementsByName(n) {
            const esc = String(n).replace(/\\/g, "\\\\").replace(/"/g, '\\"');
            return this.querySelectorAll('[name="' + esc + '"]');
        }
        // `document.elementFromPoint(x, y)` (CSSOM View): the topmost element at
        // the viewport coordinate, or null when the point is outside the
        // viewport. A terminal browser does no JS-side pixel hit-testing, so we
        // can't resolve WHICH element sits under the point — but the method must
        // EXIST (Microsoft Clarity and other analytics/heatmap/tooltip libraries
        // call it during boot; missing it is a "not a function" TypeError that
        // aborts their init). Honest deviation, consistent with our other
        // geometry shims (getBoundingClientRect returns the viewport box, not 0):
        // an in-viewport point answers with the body (the element that fills the
        // viewport in a normal document) rather than null, so callers that gate
        // on a returned element proceed instead of treating the page as empty.
        elementFromPoint(x, y) {
            x = +x; y = +y;
            if (!(x >= 0 && y >= 0 && x < g.innerWidth && y < g.innerHeight)) return null;
            return this.body || this.documentElement || null;
        }
        elementsFromPoint(x, y) { const el = this.elementFromPoint(x, y); return el ? [el] : []; }
        createEvent(type) { const C = EVENT_INTERFACES[String(type)] || Event; return new C(""); }
        hasFocus() { return true; }
        // A TRust document is always a visible, focused, non-prerendering
        // foreground page. SPAs routinely DEFER heavy rendering until
        // `visibilityState === "visible"` (or skip work while `prerendering`),
        // so leaving these undefined makes such pages wait forever for a state
        // they never see. (YouTube's kevlar gates feed work on visibility.)
        get visibilityState() { return "visible"; }
        get hidden() { return false; }
        get prerendering() { return false; }
        get wasDiscarded() { return false; }
        get visibilityStates() { return ["visible"]; }
        write(...text) { documentWrite(this, text, false); }
        writeln(...text) { documentWrite(this, text, true); }
        open() {} close() {}
    }

    // HTMLIFrameElement's nested-browsing-context document — the part of the
    // iframe spec a terminal can honor: same-origin scripted/`srcdoc` content.
    // (https://html.spec.whatwg.org/multipage/iframe-embed-object.html). The
    // nested document is built from REAL arena nodes parented under the
    // <iframe> element (<html><head><body>), so `document.open/write/close`
    // and DOM mutations land in the live tree, the CSS cascade sees the frame's
    // own <style>, and the serializer can flow the body inline (it rewrites the
    // <iframe>+content into a block so the re-parse doesn't treat it as the
    // RAWTEXT the HTML parser makes of <iframe> content). A cross-origin `src`
    // frame we never load keeps an empty body and renders nothing — the same
    // graceful degrade as before.
    class FrameDocument {
        constructor(frameEl) {
            this.__frame = frameEl;
            this.nodeType = 9;
        }
        // The content navigable's document element, found live in the arena.
        // `processIframeAttributes` installs real <html> content for src/srcdoc
        // frames; an unscripted about:blank frame gets an empty skeleton on
        // first access so `document.write` has a <body> to write into. (Found,
        // not cached, so it stays correct after a (re)navigation replaces it.)
        get documentElement() {
            const kids = this.__frame.childNodes;
            for (let i = 0; i < kids.length; i++) {
                const c = kids[i];
                if (c.nodeType === 1 && c.localName === "html") return c;
            }
            const rootDocument = wrap(0);
            const html = rootDocument.createElement("html");
            html.appendChild(rootDocument.createElement("head"));
            html.appendChild(rootDocument.createElement("body"));
            this.__frame.appendChild(html);
            return html;
        }
        get head() { return this.documentElement.querySelector("head") || this.documentElement; }
        get body() { return this.documentElement.querySelector("body") || this.documentElement; }
        get defaultView() { return trust.__activeFrame === this.__frame ? g : this.__frame.contentWindow; }
        get readyState() { return "complete"; }
        get cookie() { return ""; }
        set cookie(_v) {}
        get title() { const t = this.querySelector("title"); return t ? t.textContent : ""; }
        set title(v) { let t = this.querySelector("title"); if (!t) { t = this.createElement("title"); this.head.appendChild(t); } t.textContent = String(v); }
        get location() { return trust.__activeFrame === this.__frame ? g.location : this.__frame.contentWindow.location; }
        get URL() { return this.location.href; }
        get documentURI() { return this.location.href; }
        // Parent-side access must use this child document's base rather than
        // the currently active page scope.
        get baseURI() { return frameBaseURL(this.__frame); }
        get implementation() { return wrap(0).implementation; }
        get [Symbol.toStringTag]() { return "HTMLDocument"; }
        open() { const b = this.body; while (b.firstChild) b.removeChild(b.firstChild); return this; }
        get currentScript() {
            const script = typeof trust.currentScript === "number" ? wrap(trust.currentScript) : null;
            return script && frameOwnerForNode(script) === this.__frame ? script : null;
        }
        write(...text) { documentWrite(this, text, false); }
        writeln(...text) { documentWrite(this, text, true); }
        close() {}
        // These constructors must route to the canonical top document. While
        // a child scope is active, the bare `document` binding is THIS
        // FrameDocument; delegating through it would recurse forever on the
        // first `document.createElement()` (reCAPTCHA creates its checkbox
        // subtree this way).
        createElement(t) { return wrap(0).createElement(t); }
        createElementNS(_n, t) { return wrap(0).createElement(t); }
        createTextNode(s) { return wrap(0).createTextNode(s); }
        createComment(s) { return wrap(0).createComment(s); }
        createAttribute(n) { return wrap(0).createAttribute(n); }
        createAttributeNS(ns, n) { return wrap(0).createAttributeNS(ns, n); }
        createDocumentFragment() { return wrap(0).createDocumentFragment(); }
        createEvent(t) { return wrap(0).createEvent(t); }
        createRange() { return new Range(); }
        getElementById(i) { return this.documentElement.querySelector('[id="' + String(i).replace(/"/g, '\\"') + '"]'); }
        getElementsByTagName(t) { return this.documentElement.getElementsByTagName(t); }
        getElementsByClassName(c) { return this.documentElement.querySelectorAll("." + String(c)); }
        getElementsByName(n) { return this.documentElement.querySelectorAll('[name="' + String(n).replace(/"/g, '\\"') + '"]'); }
        querySelector(s) { return this.documentElement.querySelector(s); }
        querySelectorAll(s) { return this.documentElement.querySelectorAll(s); }
        addEventListener(type, fn, options) { addL(this, type, fn, options); }
        removeEventListener(type, fn, options) { removeL(this, type, fn, options); }
        dispatchEvent(ev) {
            trustedEvents.delete(ev);
            return dispatch(this, ev, false);
        }
        hasFocus() { return true; }
    }

    // A nested document has its own browsing-context global in HTML, and a
    // cross-origin child still executes its own scripts. TRust keeps one Boa
    // realm for the page actor, so emulate the per-navigable global with a
    // short-lived execution scope. Parent script access remains protected by
    // the restricted WindowProxy below: it exposes messaging and navigation,
    // but never the parent's document or arbitrary globals.
    let topFrameState = null;
    function frameOwnerForNode(node) {
        let n = node;
        while (n && n.parentNode) {
            const p = n.parentNode;
            if (p.localName === "iframe" || p.localName === "frame") return p;
            n = p;
        }
        return null;
    }
    // WHATWG HTML §8.4.3 `document.write()` inserts its string into the
    // parser's input stream immediately before the insertion point. TRust has
    // already materialized the source tree when it executes parser-created
    // scripts, so retain the equivalent DOM cursor: immediately after the
    // current script and before the source node that originally followed it.
    // Advancing the cursor to the last inserted node preserves input-stream
    // order across repeated writes, including writes that produce text and
    // multiple sibling elements. Dynamically-created scripts have no parser
    // insertion point (`__trustForceAsync` is their creation-time marker), so
    // they retain the existing graceful append behavior until document.open's
    // script-created parser is modeled in full.
    function documentWrite(doc, values, lineFeed) {
        let markup = "";
        for (const value of values) markup += String(value);
        if (lineFeed) markup += "\n";

        const rawCurrent = trust.currentScript;
        const script = typeof rawCurrent === "number" ? wrap(rawCurrent) : null;
        const ownerFrame = doc instanceof FrameDocument ? doc.__frame : null;
        if (script && script.localName === "script" && script.__trustForceAsync !== true &&
            frameOwnerForNode(script) === ownerFrame && script.parentNode) {
            const parent = script.parentNode;
            let cursor = typeof script.__trustWriteCursor === "number"
                ? wrap(script.__trustWriteCursor) : script;
            if (!cursor || cursor.parentNode !== parent) cursor = script;
            const sourceSuccessor = cursor.nextSibling;
            cursor.insertAdjacentHTML("afterend", markup);
            let tail = cursor;
            while (tail.nextSibling && tail.nextSibling !== sourceSuccessor)
                tail = tail.nextSibling;
            script.__trustWriteCursor = tail.__id;
            return;
        }

        const host = doc.body || doc.documentElement;
        if (host) host.insertAdjacentHTML("beforeend", markup);
    }
    function frameURLFor(frame) {
        return frame && frame.__frameUrl ? String(frame.__frameUrl) : "about:blank";
    }
    function frameBaseURL(frame) {
        const parentURL = frameURLFor(frame);
        let base = parentURL;
        try {
            const b = new FrameDocument(frame).querySelector("base[href]");
            if (b) {
                const p = __url_parse(b.getAttribute("href") || "", parentURL);
                if (p) base = p[0];
            }
        } catch (e) {}
        return base;
    }
    function frameResourceURL(node) {
        if (!node) return "";
        const raw = node.getAttribute("src") || node.getAttribute("href") || "";
        if (!String(raw).trim()) return "";
        const owner = frameOwnerForNode(node);
        const base = owner ? frameBaseURL(owner) : baseHref();
        const p = __url_parse(raw, base);
        return p ? p[0] : raw;
    }
    trust.resourceURL = function (nodeId) { return frameResourceURL(wrap(nodeId)); };

    // HTML §7.4.4/§7.4.5: an un-cancelled hyperlink navigates the chosen
    // navigable. The subject node's Document supplies the URL base, and an
    // existing child navigable named by `target` is navigated in place. TRust
    // represents every same-page navigable in one arena, so update the iframe
    // owner and re-run its attribute-processing steps; only a top-level choice
    // is returned to the frontend as a page navigation.
    trust.followAnchorDefault = function (nodeId) {
        let anchor = wrap(nodeId);
        while (anchor && anchor.nodeType === 1 && anchor.localName !== "a")
            anchor = anchor.parentNode;
        if (!anchor || anchor.localName !== "a") return null;
        const raw = anchor.getAttribute("href");
        if (raw === null || !String(raw).trim()) return null;
        const subject = frameOwnerForNode(anchor);
        const base = subject ? frameBaseURL(subject) : baseHref();
        const parsed = __url_parse(String(raw), base);
        if (!parsed) return null;
        const url = String(parsed[0] || "");
        if (!url || /^javascript:/i.test(url)) return null;

        const rawTarget = String(anchor.getAttribute("target") || "");
        const keyword = rawTarget.toLowerCase();
        let destination;
        if (!rawTarget || keyword === "_self") {
            destination = subject;
        } else if (keyword === "_parent") {
            destination = subject ? frameOwnerForNode(subject) : null;
        } else if (keyword === "_top") {
            destination = null;
        } else if (keyword === "_blank") {
            // TRust currently presents one top-level traversable. Opening a
            // fresh auxiliary context therefore degrades to that traversable.
            destination = null;
        } else {
            destination = undefined;
            let frames = [];
            try { frames = document.querySelectorAll("iframe, frame"); } catch (e) {}
            for (let i = 0; i < frames.length; i++) {
                if (frames[i].getAttribute("name") === rawTarget) {
                    destination = frames[i];
                    break;
                }
            }
            // A non-keyword name with no existing familiar navigable requests
            // a new top-level traversable. Use the single available one.
            if (destination === undefined) destination = null;
        }
        if (!destination) return url;
        try {
            destination.removeAttribute("srcdoc");
            destination.setAttribute("src", url);
            return null;
        } catch (e) {
            trust.errors.push("hyperlink navigation: " + ((e && e.message) || e));
            return null;
        }
    };

    function makeFrameLocation(frame, parentLocation) {
        const raw = frameURLFor(frame);
        const inherited = raw === "about:srcdoc" || raw === "about:blank";
        const parsed = __url_parse(inherited ? parentLocation.href : raw, parentLocation.href) ||
            [raw, "", "", "", "", "/", "", "", "", "", ""];
        const state = parsed.slice();
        if (inherited) state[0] = raw;
        return {
            get href() { return state[0]; },
            get protocol() { return state[1]; }, get host() { return state[2]; },
            get hostname() { return state[3]; }, get port() { return state[4]; },
            get pathname() { return state[5]; }, get search() { return state[6]; },
            get hash() { return state[7]; },
            get origin() { return inherited ? parentLocation.origin : state[8]; },
            assign(v) { try { frame.setAttribute("src", String(v)); } catch (e) {} },
            replace(v) { try { frame.setAttribute("src", String(v)); } catch (e) {} },
            reload() { try { frame.__loadedSrc = undefined; queueFrameNavigation(frame); } catch (e) {} },
            toString() { return state[0]; },
        };
    }
    function frameParentFrame(frame) {
        return frame ? frameOwnerForNode(frame) : null;
    }
    function frameLocationObject(frame, fallback) {
        return frame ? frame.contentWindow.location : fallback;
    }
    function frameSameOriginWithParent(frame, parentLocation) {
        const raw = frameURLFor(frame);
        const inherited = raw === "about:srcdoc" || raw === "about:blank";
        const href = parentLocation && parentLocation.href || g.location.href;
        const child = __url_parse(inherited ? href : raw, href);
        const parent = __url_parse(href, href);
        return !!(child && parent && child[8] === parent[8]);
    }
    // A WindowProxy's target is fixed by the browsing context it represents.
    // The same Boa object is used for every scoped Window, so these facades
    // preserve the important distinction for nested frames: `parent` targets
    // the immediate containing frame, while `top` targets the page window.
    function makeParentWindow(parentLocation, child, sameOrigin, targetFrame, topWindow) {
        const parent = {};
        parent.postMessage = function (message, targetOrigin, transfer) {
            // MessageEvent.source is the child's WindowProxy, never the
            // iframe Element. reCAPTCHA inspects that object while validating
            // its handshake; passing the element made its source probe read
            // an absent value and throw on Object/URL conversion.
            postMessageToFrame(targetFrame || null, message, child && child.contentWindow,
                transferPorts(targetOrigin, transfer), g.location.origin);
        };
        parent.location = frameLocationObject(targetFrame, parentLocation);
        parent.closed = false; parent.length = 0; parent.opener = null;
        // Same-origin child code may read the parent's document. The
        // cross-origin case deliberately omits it, matching WindowProxy's
        // restricted property surface.
        if (sameOrigin) parent.document = targetFrame ? new FrameDocument(targetFrame) :
            (topFrameState && topFrameState.document) || wrap(0);
        const nextParent = targetFrame && frameParentFrame(targetFrame);
        parent.parent = nextParent
            ? makeParentWindow(frameLocationObject(nextParent, parentLocation), targetFrame,
                frameSameOriginWithParent(targetFrame, frameLocationObject(nextParent, parentLocation)),
                nextParent, topWindow)
            : (targetFrame ? (topWindow || parent) : parent);
        parent.top = topWindow || parent; parent.window = parent;
        parent.self = parent; parent.frames = parent;
        try { Object.defineProperty(parent, Symbol.toStringTag, { value: "Window" }); } catch (e) {}
        return parent;
    }
    // HTML web messaging transfers MessagePorts in the MessageEvent. The
    // reCAPTCHA bootstrap uses the legacy `postMessage(data, [port])` overload
    // to establish its RPC channel; silently dropping that list leaves
    // `event.ports[0]` undefined and the child aborts while opening the
    // challenge. Preserve the transferable port object (the page actor has
    // one realm, so detaching the sender-side identity would only make the
    // emulation less useful without adding observable value here).
    function transferPorts(second, third) {
        const list = Array.isArray(second) ? second : (Array.isArray(third) ? third : []);
        return list.filter((port) => port && typeof port.postMessage === "function" &&
            typeof port.start === "function");
    }
    function postMessageToFrame(frame, message, source, ports, origin, receiverSource) {
        __queue_message_task(function () {
            runInFrame(frame, function () {
                // A transferred MessagePort is owned by the receiving
                // navigable for subsequent callbacks. The object remains
                // identity-preserving in this one-realm implementation, but
                // its callback scope must move with the transfer.
                for (const port of ports || []) {
                    if (port && typeof port === "object") port.__frame = frame || null;
                }
                const ev = new MessageEvent("message", {
                    data: message, origin: origin || "", source: receiverSource || source || g,
                    ports: ports || [],
                });
                ev.__windowTargetSet = true; ev.__frameTarget = frame || null;
                g.dispatchEvent(ev);
            });
        }, 0);
    }
    function postMessageToTop(message, source, ports, origin) {
        __queue_message_task(function () {
            runInFrame(null, function () {
                const ev = new MessageEvent("message", {
                    data: message, origin: origin || "", source: source || g,
                    ports: ports || [],
                });
                ev.__windowTargetSet = true; ev.__frameTarget = null;
                g.dispatchEvent(ev);
            });
        }, 0);
    }
    function captureFrameState() {
        return {
            document: g.document,
            location: Object.getOwnPropertyDescriptor(g, "location"),
            parent: g.parent, top: g.top, frames: g.frames, frameElement: frameElementState,
            name: g.name, cfgUrl: g.__trust_cfg && g.__trust_cfg.url,
            innerWidth: g.innerWidth, innerHeight: g.innerHeight,
            pageXOffset: g.pageXOffset, pageYOffset: g.pageYOffset,
            scrollX: g.scrollX, scrollY: g.scrollY, base: baseHrefCache,
        };
    }
    function restoreFrameState(state) {
        g.document = state.document;
        if (state.location) Object.defineProperty(g, "location", state.location);
        g.parent = state.parent; g.top = state.top; g.frames = state.frames;
        frameElementState = state.frameElement; g.name = state.name;
        if (g.__trust_cfg) g.__trust_cfg.url = state.cfgUrl;
        g.innerWidth = state.innerWidth; g.innerHeight = state.innerHeight;
        g.pageXOffset = state.pageXOffset; g.pageYOffset = state.pageYOffset;
        g.scrollX = state.scrollX; g.scrollY = state.scrollY;
        baseHrefCache = state.base;
    }
    function enterFrame(frame) {
        const token = { state: captureFrameState(), active: trust.__activeFrame || null };
        if (trust.__activeFrame === frame) return token;
        if (!frame) {
            if (!topFrameState) topFrameState = token.state;
            restoreFrameState(topFrameState);
            trust.__activeFrame = null;
            return token;
        }
        if (!topFrameState && !trust.__activeFrame) topFrameState = token.state;
        const parentFrame = frameParentFrame(frame);
        const parentLocation = frameLocationObject(parentFrame, g.location);
        const topLocation = topFrameState && topFrameState.location
            ? topFrameState.location.get.call(g) : g.location;
        const url = frameURLFor(frame);
        const sameOrigin = frameSameOriginWithParent(frame, parentLocation);
        const frameLocation = makeFrameLocation(frame, parentLocation);
        const frameWidth = frameViewportDimension(frame, "width");
        const frameHeight = frameViewportDimension(frame, "height");
        rememberFrameViewport(frame, frameWidth, frameHeight);
        Object.defineProperty(g, "location", {
            configurable: true, enumerable: true,
            get() { return frameLocation; },
            set(v) { frameLocation.assign(v); },
        });
        g.document = new FrameDocument(frame);
        const topWindow = frame.__trustTopWindow || makeParentWindow(topLocation, frame,
            frameSameOriginWithParent(frame, topLocation), null, null);
        const parent = parentFrame
            ? frame.__trustParentWindow || makeParentWindow(parentLocation, frame, sameOrigin, parentFrame, topWindow)
            : topWindow;
        frame.__trustTopWindow = topWindow;
        frame.__trustParentWindow = parent;
        g.parent = parent; g.top = topWindow; g.frames = sameOrigin ? g : parent;
        frameElementState = frame; g.name = frame.getAttribute("name") || "";
        if (g.__trust_cfg) g.__trust_cfg.url = url;
        g.innerWidth = frameWidth; g.innerHeight = frameHeight;
        baseHrefCache = null;
        trust.__activeFrame = frame;
        return token;
    }
    function leaveFrame(token) {
        if (!token) return;
        restoreFrameState(token.state);
        trust.__activeFrame = token.active;
    }
    function runInFrame(frame, fn) {
        const token = enterFrame(frame || null);
        try { return fn(); } finally { leaveFrame(token); }
    }
    trust.__activeFrame = null;
    trust.bindFrameForNode = function (nodeId) {
        const node = wrap(nodeId);
        const frame = frameOwnerForNode(node);
        // Top-level injected scripts already execute in the canonical page
        // scope. Avoid pushing/restoring a synthetic null-frame token here:
        // the token captures the live Window state and can otherwise restore
        // a stale document after the host async job returns. Only nested
        // navigables need an explicit scope switch.
        if (!frame) return;
        (trust.__frameBindings || (trust.__frameBindings = [])).push(enterFrame(frame));
    };
    trust.restoreFrame = function () {
        const stack = trust.__frameBindings;
        if (stack && stack.length) leaveFrame(stack.pop());
    };

    function loadFrameStyles(frame) {
        runInFrame(frame, function () {
            let links;
            try { links = new FrameDocument(frame).querySelectorAll("link"); } catch (e) { return; }
            for (const link of links) {
                try { maybeLoadStylesheet(link); } catch (e) {}
            }
        });
    }
    // HTML's classic-script fetch checks the response status and, when
    // `nosniff` is present, its JavaScript MIME essence. The page prelude's
    // worker loader has the same rule, but frame parser scripts use this local
    // helper because they execute synchronously while the nested document is
    // being installed.
    function frameClassicScriptResponseOK(response) {
        if (!response || response[0] < 200 || response[0] >= 300) return false;
        const lines = String(response[4] || "").split("\n");
        let nosniff = false;
        for (let i = 0; i + 1 < lines.length; i += 2) {
            if (lines[i].toLowerCase() === "x-content-type-options") {
                nosniff = lines[i + 1].split(",", 1)[0].trim().toLowerCase() === "nosniff";
                break;
            }
        }
        if (!nosniff) return true;
        return /^(application|text)\/(java|ecma)script(?:$|;)/i.test(String(response[1] || "").trim());
    }
    function runFrameScripts(frame) {
        runInFrame(frame, function () {
            let scripts;
            try { scripts = new FrameDocument(frame).querySelectorAll("script"); } catch (e) { return; }
            for (const script of scripts) {
                if (frameOwnerForNode(script) !== frame || SCRIPTS_STARTED.has(script.__id)) continue;
                const ty = (script.getAttribute("type") || "").trim().toLowerCase();
                if (ty && ty !== "text/javascript" && ty !== "application/javascript" &&
                    ty !== "text/ecmascript") continue;
                if (script.hasAttribute("nomodule")) continue;
                SCRIPTS_STARTED.add(script.__id);
                let source = script.textContent || "";
                const src = script.getAttribute("src");
                try {
                    if (src) {
                        const url = frameResourceURL(script);
                        const response = __http_fetch(url, "GET", null, null, null);
                        if (!frameClassicScriptResponseOK(response)) {
                            trust.errors.push("frame script " + url + ": network error");
                            continue;
                        }
                        source = response[2] || "";
                    }
                    const old = trust.currentScript;
                    trust.currentScript = script.__id;
                    try { (0, eval)(source); }
                    catch (e) { trust.errors.push("frame script: " + ((e && e.message) || e)); }
                    trust.currentScript = old;
                } catch (e) {
                    trust.errors.push("frame script: " + ((e && e.message) || e));
                }
            }
            // The parser-created document reaches interactive after its parser
            // scripts. Fire the child document event before the parent iframe's
            // load task, matching HTML's nested-document lifecycle ordering.
            try { dispatch(g.document, new Event("DOMContentLoaded"), false); } catch (e) {}
        });
    }

    class DocumentFragment extends Node {
        get nodeType() { return 11; }
        get nodeName() { return "#document-fragment"; }
        get [Symbol.toStringTag]() { return "DocumentFragment"; }
        querySelector(s) { const r = __dom_query(this.__id, String(s), true); return r.length ? wrap(r[0]) : null; }
        querySelectorAll(s) { return __dom_query(this.__id, String(s), false).map(wrap); }
        getElementsByTagName(t) { return this.querySelectorAll(String(t)); }
        getElementsByClassName(c) { return this.querySelectorAll(String(c).trim().split(/\s+/).map((x) => "." + x).join("")); }
    }
    class Comment extends CharacterData { get nodeType() { return 8; } get nodeName() { return "#comment"; } get [Symbol.toStringTag]() { return "Comment"; } }
    // Lit walks comment markers with one of these.
    // A spec-faithful DOM TreeWalker (https://dom.spec.whatwg.org/#interface-treewalker).
    // The full traversal surface — firstChild/lastChild/next|previousSibling/
    // next|previousNode/parentNode — and the NodeFilter (whatToShow bitmask +
    // the acceptNode callback) are required: Primer React's focus zone builds
    // `createTreeWalker(root, SHOW_ELEMENT, {acceptNode})` and drives it with
    // `firstChild()`/`nextNode()` to find focusable elements. A walker missing
    // those methods threw `… is not a callable (reading 'firstChild')` during
    // React render, which GitHub's top-level boundary turned into "Unable to
    // load page." (FILTER_ACCEPT=1, FILTER_REJECT=2, FILTER_SKIP=3.)
    class TreeWalker {
        constructor(root, whatToShow, filter) {
            this.root = root;
            this.currentNode = root;
            this.whatToShow = (whatToShow === undefined ? 0xFFFFFFFF : whatToShow) >>> 0;
            this.filter = filter || null;
        }
        __filter(n) {
            const t = n.nodeType;
            const bit = (t >= 1 && t <= 32) ? (1 << (t - 1)) : 0;
            if ((this.whatToShow & bit) === 0) return 3; // FILTER_SKIP
            const f = this.filter;
            if (f === null) return 1; // FILTER_ACCEPT
            return typeof f === "function" ? f(n) : f.acceptNode(n);
        }
        // "traverse children", first=true -> firstChild(), false -> lastChild().
        __children(first) {
            let node = first ? this.currentNode.firstChild : this.currentNode.lastChild;
            while (node) {
                const result = this.__filter(node);
                if (result === 1) { this.currentNode = node; return node; }
                if (result === 3) {
                    const child = first ? node.firstChild : node.lastChild;
                    if (child) { node = child; continue; }
                }
                while (node) {
                    const sibling = first ? node.nextSibling : node.previousSibling;
                    if (sibling) { node = sibling; break; }
                    const parent = node.parentNode;
                    if (!parent || parent === this.root || parent === this.currentNode) return null;
                    node = parent;
                }
            }
            return null;
        }
        firstChild() { return this.__children(true); }
        lastChild() { return this.__children(false); }
        // "traverse siblings", next=true -> nextSibling(), false -> previousSibling().
        __siblings(next) {
            let node = this.currentNode;
            if (node === this.root) return null;
            for (;;) {
                let sibling = next ? node.nextSibling : node.previousSibling;
                while (sibling) {
                    node = sibling;
                    const result = this.__filter(node);
                    if (result === 1) { this.currentNode = node; return node; }
                    sibling = next ? node.firstChild : node.lastChild;
                    if (result === 2 || !sibling) sibling = next ? node.nextSibling : node.previousSibling;
                }
                node = node.parentNode;
                if (!node || node === this.root) return null;
                if (this.__filter(node) === 1) return null;
            }
        }
        nextSibling() { return this.__siblings(true); }
        previousSibling() { return this.__siblings(false); }
        parentNode() {
            let node = this.currentNode;
            while (node && node !== this.root) {
                node = node.parentNode;
                if (node && this.__filter(node) === 1) { this.currentNode = node; return node; }
            }
            return null;
        }
        nextNode() {
            let node = this.currentNode;
            let result = 1; // FILTER_ACCEPT
            for (;;) {
                while (result !== 2 && node.firstChild) {
                    node = node.firstChild;
                    result = this.__filter(node);
                    if (result === 1) { this.currentNode = node; return node; }
                }
                let temporary = node;
                let broke = false;
                while (temporary) {
                    if (temporary === this.root) return null;
                    const sibling = temporary.nextSibling;
                    if (sibling) { node = sibling; broke = true; break; }
                    temporary = temporary.parentNode;
                }
                if (!broke) return null;
                result = this.__filter(node);
                if (result === 1) { this.currentNode = node; return node; }
            }
        }
        previousNode() {
            let node = this.currentNode;
            while (node !== this.root) {
                let sibling = node.previousSibling;
                while (sibling) {
                    node = sibling;
                    let result = this.__filter(node);
                    while (result !== 2 && node.lastChild) {
                        node = node.lastChild;
                        result = this.__filter(node);
                    }
                    if (result === 1) { this.currentNode = node; return node; }
                    sibling = node.previousSibling;
                }
                if (node === this.root || !node.parentNode) return null;
                node = node.parentNode;
                if (this.__filter(node) === 1) { this.currentNode = node; return node; }
            }
            return null;
        }
    }
    // NodeIterator: a flat document-order walk over a subtree (root first,
    // then descendants). DOMPurify drives sanitization with one of these
    // (`ownerDocument.createNodeIterator(body, …)`). Live, not a snapshot —
    // a sanitizer that removes the current node detaches it, so iteration
    // would stop at that subtree; benign for the content we run it on.
    class NodeIterator {
        constructor(root, whatToShow) {
            this.root = root;
            this.referenceNode = root;
            this.pointerBeforeReferenceNode = true;
            this.whatToShow = (whatToShow === undefined ? 0xFFFFFFFF : whatToShow) >>> 0;
        }
        __shows(n) {
            const bit = n.nodeType === 1 ? 1 : n.nodeType === 3 ? 4 : n.nodeType === 8 ? 128 : 0;
            return (this.whatToShow & bit) !== 0;
        }
        // The document-order successor of `n` within `root`.
        __after(n) {
            let next = n.firstChild;
            if (next) return next;
            let cur = n;
            while (cur && cur !== this.root) {
                if (cur.nextSibling) return cur.nextSibling;
                cur = cur.parentNode;
            }
            return null;
        }
        nextNode() {
            let node = this.referenceNode;
            let before = this.pointerBeforeReferenceNode;
            for (;;) {
                if (before) { before = false; }
                else {
                    const nx = this.__after(node);
                    if (!nx) return null;
                    node = nx;
                }
                if (this.__shows(node)) {
                    this.referenceNode = node;
                    this.pointerBeforeReferenceNode = false;
                    return node;
                }
            }
        }
        previousNode() { return null; }
        detach() {}
    }
    // DOMParser: parse a markup string into a detached document. `text/html`
    // yields a REAL `Document` (nodeType 9) backed by a detached arena node with
    // the parser's genuine `<html>`/`<head>`/`<body>` split — so it inherits the
    // full Document read surface (`scripts`/`forms`/`links`/`images`/`title`/
    // `querySelector*`/`getElementById*`…). This matters for a view-transitions
    // swap (Astro's ClientRouter) that reads `newDocument.scripts` and swaps
    // `newDocument.head`/`.body` separately: the old body-fragment parse crammed
    // the whole document into one `<body>` and had no `.scripts`, so the swap
    // threw and every client-routed link went dead. DOMPurify/jQuery.parseHTML
    // (body-level fragments) still work — the document parser wraps a stray
    // fragment into `<html><head></head><body>…`.
    class DOMParser {
        parseFromString(str, type) {
            const s = String(str === undefined ? "" : str);
            const t = String(type || "text/html").toLowerCase();
            // We have no XML parser; treat anything non-XML as HTML (the common
            // case). An XML mediaType falls back to the HTML document parser too
            // — best-effort, same as before, but now a well-formed document.
            void t;
            return wrap(__dom_parse_document(s));
        }
    }
    // `new XMLSerializer().serializeToString(node)` — the inverse of DOMParser.
    // Delegates to our HTML serializer (outerHTML); documents serialize their root.
    class XMLSerializer {
        serializeToString(node) {
            if (!node) return "";
            if (node.outerHTML !== undefined && node.outerHTML !== null) return node.outerHTML;
            if (node.documentElement) return node.documentElement.outerHTML || "";
            if (node.nodeType === 3 || node.nodeType === 8) return String(node.nodeValue || "");
            return node.innerHTML !== undefined ? node.innerHTML : "";
        }
    }
    class ShadowRoot extends Node {
        get nodeType() { return 11; }
        get nodeName() { return "#document-fragment"; }
        get [Symbol.toStringTag]() { return "ShadowRoot"; }
        get host() { return this.__host || null; }
        get mode() { return this.__mode || "open"; }
        get activeElement() { return activeElementFor(this); }
        get innerHTML() { return __dom_inner_html(this.__id); }
        set innerHTML(v) {
            __dom_set_inner_html(this.__id, String(v));
            if (CE.defs.size) ceScan(this);
            slotQueueCheck(this);
        }
        get adoptedStyleSheets() { return this.__adopted || (this.__adopted = []); }
        set adoptedStyleSheets(v) { this.__adopted = v; adoptedSync(this); }
        getElementById(i) { const r = this.querySelectorAll("[id]"); for (const e of r) if (e.id === String(i)) return e; return null; }
        querySelector(s) { const r = __dom_query(this.__id, String(s), true); return r.length ? wrap(r[0]) : null; }
        querySelectorAll(s) { return __dom_query(this.__id, String(s), false).map(wrap); }
        getElementsByTagName(t) { return this.querySelectorAll(String(t)); }
        getElementsByClassName(c) { return this.querySelectorAll(String(c).trim().split(/\s+/).map((x) => "." + x).join("")); }
    }

    // WHATWG DOM §4.2.2.3 "Finding slots and slottables". Direct assignment is
    // computed by the arena so it follows tree order and the host/shadow-root
    // relationship exactly. The optional flattening step recursively substitutes
    // nested slots and uses fallback children only when a slot has no assignment.
    function slotAssignedNodes(slot, flatten) {
        let nodes = __dom_slot_assigned(slot.__id).map(wrap);
        if (!flatten) return nodes;
        if (!nodes.length) nodes = slot.childNodes;
        const result = [];
        for (const node of nodes) {
            if (node && node.localName === "slot" && rootOfNode(node) instanceof ShadowRoot) {
                result.push(...slotAssignedNodes(node, true));
            } else {
                result.push(node);
            }
        }
        return result;
    }

    let slotCheckQueued = false;
    const slotCheckRoots = [];
    function slotAffectedRoot(node) {
        if (!node) return null;
        if (node instanceof ShadowRoot) return node;
        const root = rootOfNode(node);
        if (root instanceof ShadowRoot) return root;
        // A light-tree mutation affects the shadow root attached to its host.
        return node.__sr || null;
    }
    function slotQueueCheck(node) {
        const root = slotAffectedRoot(node);
        if (!root) return;
        if (!slotCheckRoots.includes(root)) slotCheckRoots.push(root);
        if (slotCheckQueued) return;
        slotCheckQueued = true;
        Promise.resolve().then(slotFlushChecks);
    }
    function slotFlushChecks() {
        slotCheckQueued = false;
        const roots = slotCheckRoots.splice(0);
        const changed = [];
        for (const root of roots) {
            for (const slot of root.querySelectorAll("slot")) {
                const signature = __dom_slot_assigned(slot.__id).join(",");
                const previous = slot.__trustSlotSignature === undefined ? "" : slot.__trustSlotSignature;
                slot.__trustSlotSignature = signature;
                if (signature !== previous) changed.push(slot);
            }
        }
        // `slotchange` bubbles but is not composed (DOM §4.2.2.4).
        for (const slot of changed) slot.dispatchEvent(new Event("slotchange", { bubbles: true }));
    }

    // --- the custom elements registry ---
    function upgradeElement(el, ctor) {
        if (el.__ceUpgraded) return;
        el.__ceUpgraded = true;
        // Read observedAttributes BEFORE constructing — the platform
        // contract define() relies on. Lit's static getter runs its
        // finalize() here, creating reactive accessors; construct
        // first and instance fields shadow them forever.
        let observed = [];
        try { observed = ctor.observedAttributes || []; } catch (e) { observed = []; }
        Object.setPrototypeOf(el, ctor.prototype);
        CE.upgrading = el;
        try { new ctor(); }
        catch (e) { trust.errors.push("custom element ctor: " + ((e && e.message) || e)); }
        finally { CE.upgrading = null; }
        for (const a of observed) {
            const v = el.getAttribute(a);
            if (v !== null && typeof el.attributeChangedCallback === "function") {
                try { el.attributeChangedCallback(a, null, v); }
                catch (e) { trust.errors.push("attributeChangedCallback: " + ((e && e.message) || e)); }
            }
        }
        maybeConnect(el);
    }
    function maybeConnect(el) {
        if (el.__ceUpgraded && !el.__ceConnected && el.isConnected
            && typeof el.connectedCallback === "function") {
            el.__ceConnected = true;
            try { el.connectedCallback(); }
            catch (e) { trust.errors.push("connectedCallback: " + ((e && e.message) || e)); }
        }
    }
    function ceScan(node) {
        if (!node || typeof node !== "object" || node.__id === undefined) return;
        // Rust returns just the custom-element candidates (hyphenated tags) in
        // the inserted subtree, shadow roots included and the root itself — so
        // we wrap/visit only those, never the non-custom bulk of the subtree
        // (the old per-node JS recursion wrapped every node it walked).
        const ids = __dom_ce_candidates(node.__id);
        for (let i = 0; i < ids.length; i++) {
            const el = wrap(ids[i]);
            const ctor = CE.defs.get(el.localName);
            if (ctor) { upgradeElement(el, ctor); maybeConnect(el); }
        }
    }
    function ceAdopt(node, oldDocument, newDocument) {
        if (!node || oldDocument === newDocument || !CE.defs.size) return;
        // `__dom_ce_candidates` follows the shadow-including tree and returns
        // only custom-element candidates, matching the adoption algorithm's
        // callback walk without wrapping every ordinary descendant.
        const ids = __dom_ce_candidates(node.__id);
        for (let i = 0; i < ids.length; i++) {
            const el = wrap(ids[i]);
            if (el.__ceUpgraded && typeof el.adoptedCallback === "function") {
                try { el.adoptedCallback(oldDocument, newDocument); }
                catch (e) { trust.errors.push("adoptedCallback: " + ((e && e.message) || e)); }
            }
        }
    }
    // define()'s catch-up upgrade, but shadow-piercing: an element
    // rendered into a shadow root BEFORE its definition (archive.org's
    // router does this for the late-loaded page component) is invisible
    // to document.querySelectorAll, so without crossing __sr it would
    // never upgrade — constructed never, rendered never, empty forever.
    function ceUpgradeName(name, ctor) {
        // The candidate set — every composed-tree element with this tag, shadow
        // roots included — is computed in Rust in a single pointer walk (see
        // __dom_upgrade_candidates) instead of recursing the whole document in
        // JS on every define(). Only the matching elements are wrapped and
        // upgraded; the old walk materialized a wrapper + a childNodes syscall
        // for ALL ~16.8k nodes per define on a big page.
        const ids = __dom_upgrade_candidates(g.document.__id, name);
        for (let i = 0; i < ids.length; i++) upgradeElement(wrap(ids[i]), ctor);
    }
    // A definition can arrive while the parser/module task is still attaching
    // the document subtree.  The upgrade itself is synchronous, but the
    // connected reaction is delivered at the custom-element reactions
    // microtask checkpoint; retry the already-upgraded candidates there so a
    // transiently-disconnected instance is not left with an empty shadow root.
    function ceConnectName(name) {
        const ids = __dom_upgrade_candidates(g.document.__id, name);
        for (let i = 0; i < ids.length; i++) {
            const el = wrap(ids[i]);
            if (el.__ceUpgraded) maybeConnect(el);
        }
    }
    function ceDisconnect(node) {
        if (!node || typeof node !== "object") return;
        if (node.__ceConnected && typeof node.disconnectedCallback === "function") {
            node.__ceConnected = false;
            try { node.disconnectedCallback(); }
            catch (e) { trust.errors.push("disconnectedCallback: " + ((e && e.message) || e)); }
        }
        if (node.childNodes) for (const c of node.childNodes) ceDisconnect(c);
    }
    function ceAttrChanged(el, name, old, val) {
        if (!el.__ceUpgraded || old === val) return;
        const observed = (el.constructor && el.constructor.observedAttributes) || [];
        if (observed.includes(name) && typeof el.attributeChangedCallback === "function") {
            try { el.attributeChangedCallback(name, old, val); }
            catch (e) { trust.errors.push("attributeChangedCallback: " + ((e && e.message) || e)); }
        }
    }
    const customElements = {
        define(name, ctor) {
            name = String(name).toLowerCase();
            if (CE.defs.has(name)) return;
            // The registration-time observedAttributes read (see above).
            try { void (ctor.observedAttributes || []); } catch (e) { /* page's problem */ }
            CE.defs.set(name, ctor);
            CE.tags.set(ctor, name);
            ceUpgradeName(name, ctor);
            Promise.resolve().then(() => ceConnectName(name));
            const w = CE.waiting.get(name);
            if (w) { CE.waiting.delete(name); w.resolve(ctor); }
        },
        get(name) { return CE.defs.get(String(name).toLowerCase()); },
        getName(ctor) { return CE.tags.get(ctor) || null; },
        whenDefined(name) {
            name = String(name).toLowerCase();
            if (CE.defs.has(name)) return Promise.resolve(CE.defs.get(name));
            let w = CE.waiting.get(name);
            if (!w) {
                w = {};
                w.promise = new Promise((resolve) => { w.resolve = resolve; });
                CE.waiting.set(name, w);
            }
            return w.promise;
        },
        upgrade(root) { if (CE.defs.size) ceScan(root); },
    };
    // Scopes (document / shadow roots) that ever adopted sheets, so a
    // later replaceSync() re-pushes their joined text to the cascade.
    const adoptedScopes = [];
    const adoptedSync = (scope) => {
        if (!adoptedScopes.includes(scope)) adoptedScopes.push(scope);
        let text = "";
        for (const s of scope.__adopted || []) text += ((s && s.__text) || "") + "\n";
        __dom_adopt_styles(scope.__id, text);
    };
    const sheetSync = (sheet) => {
        for (const scope of adoptedScopes) {
            if ((scope.__adopted || []).includes(sheet)) adoptedSync(scope);
        }
    };
    // ---- CSSOM: <style>.sheet.cssRules and the CSSRule hierarchy ----
    // __css_parse(text) → a JSON rule tree (dom.rs parse_cssom_json); we
    // wrap it as the standard CSSRule subclasses so stylesheet-introspection
    // and feature-detection code (css3test's Supports.atrule/descriptorvalue,
    // CSS-in-JS libraries) read real rules. Distinct classes so
    // `constructor.name`/`instanceof` answer correctly.
    function parseCss(text) {
        try { return JSON.parse(__css_parse(String(text || ""))); } catch (e) { return []; }
    }
    // Split stylesheet text into its top-level rules (string/comment/brace
    // aware), so a CSSStyleSheet can model insertRule/deleteRule by index and
    // join them back to text the Rust cascade reads. @media/@supports/@keyframes
    // count as ONE top-level rule (their inner braces don't split); top-level
    // `;` at-rules (@import/@charset) are their own rule.
    function splitCssRules(text) {
        const s = String(text || "");
        const out = [];
        let depth = 0, start = 0, i = 0;
        const n = s.length;
        while (i < n) {
            const c = s[i];
            if (c === "/" && s[i + 1] === "*") { const e = s.indexOf("*/", i + 2); i = e < 0 ? n : e + 2; continue; }
            if (c === '"' || c === "'") { const q = c; i++; while (i < n && s[i] !== q) { if (s[i] === "\\") i++; i++; } i++; continue; }
            if (c === "{") { depth++; i++; continue; }
            if (c === "}") { i++; if (--depth <= 0) { const r = s.slice(start, i).trim(); if (r) out.push(r); start = i; depth = 0; } continue; }
            if (c === ";" && depth === 0) { const r = s.slice(start, i + 1).trim(); if (r) out.push(r); i++; start = i; continue; }
            i++;
        }
        const tail = s.slice(start).trim();
        if (tail) out.push(tail);
        return out;
    }
    // A CSSStyleDeclaration over a rule's [name,value] pairs. Read-mostly
    // (rule edits don't flow back into our cascade); covers the surface
    // introspection code reads: length/item/getPropertyValue/cssText and
    // camelCase-or-kebab property access.
    function ruleStyle(pairs) {
        const order = [];
        const map = new Map();
        for (const pv of pairs || []) {
            if (!map.has(pv[0])) order.push(pv[0]);
            map.set(pv[0], pv[1]);
        }
        const base = {
            get length() { return order.length; },
            item(i) { return order[Number(i)] || ""; },
            getPropertyValue(k) { const v = map.get(String(k).toLowerCase()); return v == null ? "" : v; },
            getPropertyPriority() { return ""; },
            setProperty(k, v) { k = String(k).toLowerCase(); if (!map.has(k)) order.push(k); map.set(k, String(v)); },
            removeProperty(k) { k = String(k).toLowerCase(); const v = map.get(k) || ""; if (map.delete(k)) { const i = order.indexOf(k); if (i >= 0) order.splice(i, 1); } return v; },
            get cssText() { return order.map((k) => k + ": " + map.get(k) + ";").join(" "); },
            set cssText(_) {},
        };
        return new Proxy(base, {
            get(t, p) {
                if (p in t) return t[p];
                if (typeof p === "string") {
                    if (/^\d+$/.test(p)) return order[Number(p)] || "";
                    const v = map.get(kebab(p));
                    return v == null ? "" : v;
                }
                return undefined;
            },
            set(t, p, v) {
                if (p in t) { t[p] = v; return true; }
                if (typeof p === "string") base.setProperty(kebab(p), v);
                return true;
            },
            has(t, p) { return (p in t) || (typeof p === "string" && map.has(kebab(p))); },
        });
    }
    function mediaList(q) {
        q = String(q || "");
        const parts = q.split(",").map((s) => s.trim()).filter(Boolean);
        const ml = {
            get mediaText() { return q; },
            set mediaText(v) { q = String(v); },
            get length() { return parts.length; },
            item(i) { return parts[Number(i)] || null; },
            toString() { return q; },
        };
        parts.forEach((p, i) => { ml[i] = p; });
        return ml;
    }
    // Array subclassing is finicky across engines; a plain array-like with
    // copied indices is safe and gives length/[i]/item/iteration + an
    // honest `constructor.name`.
    class CSSRuleList {
        constructor(items) { this.length = items.length; for (let i = 0; i < items.length; i++) this[i] = items[i]; }
        item(i) { return this[Number(i)] ?? null; }
        [Symbol.iterator]() { return Array.prototype[Symbol.iterator].call(this); }
    }
    function ruleList(items) { return new CSSRuleList(items); }
    // CSSOM §StyleSheetList — the type behind `document.styleSheets` (which
    // simply didn't exist; iterating it threw on `undefined`). Same
    // array-like shape as CSSRuleList.
    class StyleSheetList {
        constructor(items) { this.length = items.length; for (let i = 0; i < items.length; i++) this[i] = items[i]; }
        item(i) { return this[Number(i)] ?? null; }
        [Symbol.iterator]() { return Array.prototype[Symbol.iterator].call(this); }
        get [Symbol.toStringTag]() { return "StyleSheetList"; }
    }

    class CSSRule { get cssText() { return ""; } get parentStyleSheet() { return null; } }
    class CSSStyleRule extends CSSRule {
        constructor(j) { super(); this.selectorText = j.sel || ""; this.style = ruleStyle(j.d); }
        get type() { return 1; }
        get cssText() { return this.selectorText + " { " + this.style.cssText + " }"; }
    }
    class CSSGroupingRule extends CSSRule {
        constructor(j) { super(); this.cssRules = buildRules(j.r); }
        insertRule(_r, i) { return i || 0; }
        deleteRule() {}
    }
    class CSSMediaRule extends CSSGroupingRule {
        constructor(j) { super(j); this.media = mediaList(j.q); this.conditionText = j.q || ""; }
        get type() { return 4; }
    }
    class CSSSupportsRule extends CSSGroupingRule {
        constructor(j) { super(j); this.conditionText = j.q || ""; }
        get type() { return 12; }
    }
    class CSSContainerRule extends CSSGroupingRule {
        constructor(j) { super(j); this.conditionText = j.q || ""; this.containerName = ""; }
    }
    class CSSLayerBlockRule extends CSSGroupingRule {
        constructor(j) { super(j); this.name = j.q || ""; }
    }
    class CSSFontFaceRule extends CSSRule {
        constructor(j) { super(); this.style = ruleStyle(j.d); }
        get type() { return 5; }
    }
    class CSSPageRule extends CSSRule {
        constructor(j) { super(); this.selectorText = j.sel || ""; this.style = ruleStyle(j.d); }
        get type() { return 6; }
    }
    class CSSKeyframeRule extends CSSRule {
        constructor(j) { super(); this.keyText = j.key || ""; this.style = ruleStyle(j.d); }
        get type() { return 8; }
    }
    class CSSKeyframesRule extends CSSRule {
        constructor(j) { super(); this.name = j.name || ""; this.cssRules = buildRules(j.r); }
        get type() { return 7; }
    }
    class CSSImportRule extends CSSRule {
        constructor(j) { super(); this.href = j.q || ""; this.media = mediaList(""); }
        get type() { return 3; }
    }
    class CSSNamespaceRule extends CSSRule { get type() { return 10; } }
    class CSSCounterStyleRule extends CSSRule {
        constructor(j) { super(); this.name = j.name || ""; this.style = ruleStyle(j.d); }
    }
    class CSSPropertyRule extends CSSRule {
        constructor(j) { super(); this.name = j.name || ""; }
    }
    const RULE_CTORS = {
        style: CSSStyleRule, media: CSSMediaRule, supports: CSSSupportsRule,
        container: CSSContainerRule, layer: CSSLayerBlockRule, scope: CSSGroupingRule,
        document: CSSGroupingRule, "font-face": CSSFontFaceRule, page: CSSPageRule,
        keyframes: CSSKeyframesRule, keyframe: CSSKeyframeRule, import: CSSImportRule,
        namespace: CSSNamespaceRule, "counter-style": CSSCounterStyleRule,
        property: CSSPropertyRule, "font-feature-values": CSSRule,
    };
    function buildRules(arr) {
        const out = [];
        for (const j of arr || []) { const C = RULE_CTORS[j.t]; if (C) out.push(new C(j)); }
        return ruleList(out);
    }

    // styled-components & other CSS-in-JS inject ALL their CSS through
    // sheet.insertRule() in CSSOM "speedy" mode, leaving the owning <style>
    // element's text node EMPTY (the browser cascades from `cssRules`, not the
    // text). Our Rust cascade reads `<style>` text, so a <style>-owned sheet
    // mirrors its joined rules back into the element via __dom_set_text — else
    // every styled-components flex/grid rule is invisible and the page collapses
    // to one block column. Rules are kept as an array so insert/delete honour the
    // CSSOM index (default 0; cascade order is load-bearing). cssRules is lazy.
    class CSSStyleSheet {
        constructor() { this.__ruleTexts = []; this.__rules = null; this.ownerNode = null; this.media = mediaList(""); }
        get __text() { return this.__ruleTexts.join("\n"); }
        get cssRules() { return this.__rules || (this.__rules = buildRules(parseCss(this.__text))); }
        get rules() { return this.cssRules; }
        replace(t) { this.replaceSync(t); return Promise.resolve(this); }
        replaceSync(t) { this.__ruleTexts = splitCssRules(t); this.__rules = null; this.__changed(); }
        insertRule(r, i) {
            const idx = (i === undefined) ? 0 : (i | 0);
            if (idx < 0 || idx > this.__ruleTexts.length) throw new DOMException("insertRule index out of range", "IndexSizeError");
            this.__ruleTexts.splice(idx, 0, String(r));
            this.__rules = null; this.__changed();
            return idx;
        }
        deleteRule(i) {
            const idx = i | 0;
            if (idx < 0 || idx >= this.__ruleTexts.length) throw new DOMException("deleteRule index out of range", "IndexSizeError");
            this.__ruleTexts.splice(idx, 1);
            this.__rules = null; this.__changed();
        }
        __changed() {
            const o = this.ownerNode;
            if (o && o.__id !== undefined && o.localName === "style") {
                __dom_set_text(o.__id, this.__text);
                o.__sheetText = this.__text;   // keep <style>.sheet's getter cache coherent
            } else {
                sheetSync(this);
            }
        }
    }
    function makeStyleSheet(text, owner) {
        const s = new CSSStyleSheet();
        s.__ruleTexts = splitCssRules(text);
        s.ownerNode = owner || null;
        return s;
    }

    // Distinct subclasses so `instanceof` answers honestly (false for
    // our wrappers): Vue picks SVG namespaces by SVGElement checks.
    class SVGElement extends Element { get [Symbol.toStringTag]() { return "SVGElement"; } }
    // (HTMLInputElement/HTMLSelectElement/… are defined with their real per-
    // interface bodies right after Element, above — not empty stubs anymore.)

    // Standard DOM node interfaces real browsers expose as global
    // constructors. Code and polyfills (webcomponentsjs walks
    // `["Text","Comment","CDATASection","ProcessingInstruction"]` and reads
    // `window[name].prototype`) reference them and check `instanceof`. We
    // model the common node types on `Node`/`Text`/`Element`; expose the rest
    // with a roughly-correct chain so the constructors and prototypes exist.
    // The global is a `Window` in real browsers: code references the bare
    // `Window` interface (a ReferenceError without it — webcomponentsjs does
    // this) and checks `window instanceof Window`. Window IS an EventTarget.
    class Window extends EventTarget {}
    // Web IDL attributes are accessor properties on the interface prototype;
    // readonly attributes have a getter but no setter. This is the Window
    // `frameElement` binding from HTML §7.2.2, not a writable expando used as
    // scratch storage by the frame scheduler.
    Object.defineProperty(Window.prototype, "frameElement", {
        configurable: true, enumerable: false,
        get() { return frameElementState; },
    });
    class CDATASection extends Text {}
    class ProcessingInstruction extends CharacterData {}
    class DocumentType extends Node {}
    class Attr extends Node {}
    // WHATWG DOM puts the element-traversal accessors on the ParentNode mixin
    // (Document/DocumentFragment/Element/ShadowRoot) and NonDocumentTypeChildNode
    // (Element/CharacterData) — NOT on Node. We author them once on `class Node`
    // above for brevity, then relocate to the spec interfaces here. This is not
    // cosmetic: libraries feature-detect and CAPTURE the native accessors as OWN
    // properties of those exact prototypes. ShadyDOM (loaded by YouTube/Polymer
    // in shady mode) wires `__shady_native_firstElementChild` via
    // `Object.getOwnPropertyDescriptor(Element.prototype, "firstElementChild")`;
    // with the getter only on Node.prototype that descriptor is undefined, the
    // capture silently no-ops, and a non-shadow element's shady `firstElementChild`
    // returns undefined. YouTube's renderer-stamper reuses existing children by
    // scanning `firstElementChild`, so it then re-creates instead of reusing and
    // the masthead end buttons (any stamped list) render doubled. Relocating to
    // the spec interfaces restores the capture, hence the reuse path.
    {
        const PARENT_NODE = ["children", "firstElementChild", "lastElementChild", "childElementCount"];
        const CHILD_NODE = ["nextElementSibling", "previousElementSibling"];
        const move = (proto, names) => {
            for (const n of names) {
                const d = Object.getOwnPropertyDescriptor(Node.prototype, n);
                if (d) Object.defineProperty(proto, n, d);
            }
        };
        for (const p of [Element.prototype, Document.prototype, DocumentFragment.prototype, ShadowRoot.prototype]) move(p, PARENT_NODE);
        for (const p of [Element.prototype, CharacterData.prototype]) move(p, CHILD_NODE);
        for (const n of [...PARENT_NODE, ...CHILD_NODE]) delete Node.prototype[n];
    }
    // querySelectorAll/getElementsBy* return real Arrays (so .map/.forEach/
    // spread all work); these constructors exist for the `'NodeList' in window`
    // / `instanceof` feature checks code performs. NamedNodeMap is the type of
    // Element.attributes.
    class NodeList {}
    class HTMLCollection {}
    // WHATWG DOM/HTML collections are live, array-indexed legacy platform
    // objects. Recompute membership at each observable operation so DOM moves,
    // removals, and `form=id` reassociation are visible through an already-held
    // collection object.
    function collectionProxy(target) {
        return new Proxy(target, {
            get(t, p, r) {
                if (typeof p === "string" && /^(0|[1-9][0-9]*)$/.test(p))
                    return t.item(Number(p));
                if (Reflect.has(t, p)) return Reflect.get(t, p, r);
                if (typeof p === "string") {
                    const named = t.namedItem(p);
                    return named === null ? undefined : named;
                }
                return undefined;
            },
        });
    }
    class RadioNodeList extends NodeList {
        constructor(resolve) {
            super();
            this.__resolve = resolve;
            return collectionProxy(this);
        }
        __list() { return this.__resolve(); }
        get length() { return this.__list().length; }
        item(index) {
            index = Number(index);
            return Number.isInteger(index) && index >= 0 ? (this.__list()[index] || null) : null;
        }
        get value() {
            for (const el of this.__list()) {
                if (el.localName === "input" && String(el.type).toLowerCase() === "radio" && el.checked)
                    return el.value;
            }
            return "";
        }
        set value(value) {
            value = String(value);
            for (const el of this.__list()) {
                if (el.localName === "input" && String(el.type).toLowerCase() === "radio" && el.value === value) {
                    el.checked = true;
                    return;
                }
            }
        }
        forEach(fn, thisArg) { return this.__list().forEach(fn, thisArg); }
        [Symbol.iterator]() { return this.__list()[Symbol.iterator](); }
        get [Symbol.toStringTag]() { return "RadioNodeList"; }
    }
    class HTMLFormControlsCollection extends HTMLCollection {
        constructor(form) {
            super();
            this.__form = form;
            return collectionProxy(this);
        }
        __list() { return listedFormControls(this.__form); }
        get length() { return this.__list().length; }
        item(index) {
            index = Number(index);
            return Number.isInteger(index) && index >= 0 ? (this.__list()[index] || null) : null;
        }
        namedItem(name) {
            name = String(name);
            if (name === "") return null;
            const form = this.__form;
            const matches = function () {
                return listedFormControls(form).filter(function (el) {
                    return el.id === name || el.getAttribute("name") === name;
                });
            };
            const list = matches();
            if (!list.length) return null;
            if (list.length === 1) return list[0];
            return new RadioNodeList(matches);
        }
        forEach(fn, thisArg) { return this.__list().forEach(fn, thisArg); }
        [Symbol.iterator]() { return this.__list()[Symbol.iterator](); }
        get [Symbol.toStringTag]() { return "HTMLFormControlsCollection"; }
    }
    class NamedNodeMap {}
    g.NodeList = NodeList; g.HTMLCollection = HTMLCollection;
    g.RadioNodeList = RadioNodeList;
    g.HTMLFormControlsCollection = HTMLFormControlsCollection;
    g.NamedNodeMap = NamedNodeMap;
    g.EventTarget = EventTarget; g.Window = Window; g.CharacterData = CharacterData;
    g.CDATASection = CDATASection; g.ProcessingInstruction = ProcessingInstruction;
    g.DocumentType = DocumentType; g.Attr = Attr;
    g.Node = Node; g.Element = Element; g.HTMLElement = HTMLElement;
    g.Text = Text; g.Document = Document; g.HTMLDocument = Document;
    g.DocumentFragment = DocumentFragment; g.Comment = Comment;
    g.Event = Event; g.CustomEvent = CustomEvent;
    g.UIEvent = UIEvent; g.MouseEvent = MouseEvent; g.PointerEvent = PointerEvent;
    g.WheelEvent = WheelEvent; g.DragEvent = DragEvent; g.KeyboardEvent = KeyboardEvent;
    g.FocusEvent = FocusEvent; g.InputEvent = InputEvent; g.TouchEvent = TouchEvent;
    g.CompositionEvent = CompositionEvent; g.PopStateEvent = PopStateEvent;
    g.HashChangeEvent = HashChangeEvent; g.MessageEvent = MessageEvent;
    g.ErrorEvent = ErrorEvent; g.PromiseRejectionEvent = PromiseRejectionEvent;
    g.ProgressEvent = ProgressEvent; g.SubmitEvent = SubmitEvent;
    g.StorageEvent = StorageEvent; g.AnimationEvent = AnimationEvent;
    g.TransitionEvent = TransitionEvent; g.ClipboardEvent = ClipboardEvent;
    g.PageTransitionEvent = PageTransitionEvent; g.CloseEvent = CloseEvent;
    g.ToggleEvent = ToggleEvent;
    g.FontFace = FontFace; g.FontFaceSet = FontFaceSet;
    g.FontFaceSetLoadEvent = FontFaceSetLoadEvent;
    g.ShadowRoot = ShadowRoot;
    g.TreeWalker = TreeWalker;
    g.NodeIterator = NodeIterator;
    g.DOMParser = DOMParser;
    g.XMLSerializer = XMLSerializer;
    g.NodeFilter = {
        SHOW_ALL: 0xFFFFFFFF, SHOW_ELEMENT: 1, SHOW_TEXT: 4, SHOW_COMMENT: 128,
        FILTER_ACCEPT: 1, FILTER_REJECT: 2, FILTER_SKIP: 3,
    };
    g.CSSStyleSheet = CSSStyleSheet;
    g.CSSRule = CSSRule; g.CSSStyleRule = CSSStyleRule;
    g.CSSGroupingRule = CSSGroupingRule; g.CSSMediaRule = CSSMediaRule;
    g.CSSSupportsRule = CSSSupportsRule; g.CSSContainerRule = CSSContainerRule;
    g.CSSLayerBlockRule = CSSLayerBlockRule; g.CSSFontFaceRule = CSSFontFaceRule;
    g.CSSPageRule = CSSPageRule; g.CSSKeyframeRule = CSSKeyframeRule;
    g.CSSKeyframesRule = CSSKeyframesRule; g.CSSImportRule = CSSImportRule;
    g.CSSNamespaceRule = CSSNamespaceRule; g.CSSCounterStyleRule = CSSCounterStyleRule;
    g.CSSPropertyRule = CSSPropertyRule; g.CSSRuleList = CSSRuleList;
    g.StyleSheetList = StyleSheetList;
    g.customElements = customElements;
    g.SVGElement = SVGElement;
    g.HTMLInputElement = HTMLInputElement; g.HTMLSelectElement = HTMLSelectElement;
    g.HTMLTextAreaElement = HTMLTextAreaElement; g.HTMLFormElement = HTMLFormElement;
    g.HTMLAnchorElement = HTMLAnchorElement; g.HTMLImageElement = HTMLImageElement;
    g.HTMLScriptElement = HTMLScriptElement; g.HTMLButtonElement = HTMLButtonElement;
    g.HTMLSlotElement = HTMLSlotElement;
    // The per-interface classes with specialized bodies (defined after Element).
    // These must be registered BEFORE the generic interface-zoo loop below so its
    // `if (!g[__cn])` guard keeps them (rather than overwriting with empty ones).
    g.HTMLOptionElement = HTMLOptionElement; g.HTMLCanvasElement = HTMLCanvasElement;
    g.HTMLMediaElement = HTMLMediaElement; g.HTMLVideoElement = HTMLVideoElement;
    g.HTMLAudioElement = HTMLAudioElement; g.HTMLAreaElement = HTMLAreaElement;
    g.HTMLIFrameElement = HTMLIFrameElement; g.HTMLFrameElement = HTMLFrameElement;
    g.HTMLTemplateElement = HTMLTemplateElement; g.HTMLMetaElement = HTMLMetaElement;
    g.HTMLStyleElement = HTMLStyleElement; g.HTMLLinkElement = HTMLLinkElement;
    g.HTMLDialogElement = HTMLDialogElement;
    g.HTMLMarqueeElement = HTMLMarqueeElement;
    // The rest of the standard HTML element interface zoo. Browsers expose a
    // constructor for every element kind; boot code patches their prototypes
    // and feature-detects them (YouTube's kevlar reads bare `HTMLTemplateElement`,
    // `HTMLDivElement`, … — a ReferenceError on the first missing one). Each is
    // a distinct HTMLElement subclass so prototypes and `instanceof` behave; the
    // guard skips the interfaces with specialized bodies defined above. wrap()
    // now dispatches each element to its interface class (classFor), so e.g.
    // `document.createElement('div') instanceof HTMLDivElement` is true.
    for (const __n of ["Area","Audio","BR","Base","Body","Canvas","Data","DataList",
        "Details","Dialog","Div","DList","Embed","FieldSet","Heading","Head","HR",
        "Html","IFrame","Label","Legend","LI","Link","Map","Media","Menu","Meta",
        "Marquee","Meter","Mod","Object","OList","OptGroup","Option","Output","Paragraph",
        "Param","Picture","Pre","Progress","Quote","Slot","Source","Span","Style",
        "TableCaption","TableCell","TableCol","Table","TableRow","TableSection",
        "Template","Time","Title","Track","UList","Unknown","Video"]) {
        const __cn = "HTML" + __n + "Element";
        if (!g[__cn]) {
            const __C = class extends HTMLElement {};
            try { Object.defineProperty(__C, "name", { value: __cn }); } catch (e) {}
            g[__cn] = __C;
        }
    }
    // Now that every HTML interface constructor exists, install the multi-
    // interface reflected IDL attributes on their owning interfaces only (HTML
    // spec attribute→element mapping; obsolete reflectors like a.name / frame.src
    // included so nothing that worked before regresses). reflectOn skips an
    // interface that defines its own specialized accessor (e.g. select.value,
    // input.type, media.src). After this, `"name" in div` / `"href" in div` etc.
    // are false — the names exist only where the spec puts them.
    reflectOn(["HTMLButtonElement", "HTMLFieldSetElement", "HTMLFormElement",
        "HTMLInputElement", "HTMLSelectElement", "HTMLTextAreaElement",
        "HTMLIFrameElement", "HTMLMapElement", "HTMLOutputElement",
        "HTMLObjectElement", "HTMLMetaElement", "HTMLParamElement", "HTMLSlotElement",
        "HTMLAnchorElement", "HTMLImageElement", "HTMLEmbedElement",
        "HTMLFrameElement", "HTMLDetailsElement"], "name", reflectStrDesc);
    reflectOn(["HTMLButtonElement", "HTMLLIElement", "HTMLDataElement",
        "HTMLMeterElement", "HTMLProgressElement", "HTMLOutputElement",
        "HTMLParamElement"], "value", reflectStrDesc);
    reflectOn(["HTMLAnchorElement", "HTMLButtonElement", "HTMLLinkElement",
        "HTMLEmbedElement", "HTMLObjectElement", "HTMLOListElement",
        "HTMLScriptElement", "HTMLSourceElement", "HTMLStyleElement"], "type", reflectStrDesc);
    reflectOn(["HTMLButtonElement", "HTMLFieldSetElement", "HTMLInputElement",
        "HTMLOptGroupElement", "HTMLOptionElement", "HTMLSelectElement",
        "HTMLTextAreaElement", "HTMLLinkElement"], "disabled", reflectBoolDesc);
    reflectOn(["HTMLAnchorElement", "HTMLAreaElement", "HTMLLinkElement",
        "HTMLBaseElement"], "href", reflectUrlDesc);
    reflectOn(["HTMLDetailsElement"], "open", reflectBoolDesc);
    // `rel` is a reflected DOMString IDL attribute; without it `link.rel =
    // "stylesheet"` (set as a property, as webpack's mini-css loader does) never
    // reached `getAttribute("rel")`, so the sheet wasn't recognized.
    reflectOn(["HTMLAnchorElement", "HTMLAreaElement", "HTMLLinkElement",
        "HTMLFormElement"], "rel", reflectStrDesc);
    reflectOn(["HTMLImageElement", "HTMLScriptElement", "HTMLIFrameElement",
        "HTMLEmbedElement", "HTMLSourceElement", "HTMLTrackElement",
        "HTMLInputElement", "HTMLFrameElement"], "src", reflectUrlDesc);
    reflectOn(["HTMLIFrameElement"], "srcdoc", reflectStrDesc);
    reflectOn(["HTMLImageElement", "HTMLSourceElement"], "srcset", reflectStrDesc);
    reflectOn(["HTMLImageElement", "HTMLSourceElement"], "sizes", reflectStrDesc);
    reflectOn(["HTMLSourceElement"], "media", reflectStrDesc);
    reflectOn(["HTMLImageElement"], "loading", reflectStrDesc);
    // SVG element interface zoo (all extend SVGElement). SvelteKit's link
    // handler branches on `e instanceof SVGAElement` to read `href.baseVal`
    // vs `href` — a bare `SVGAElement` was a ReferenceError that broke its
    // link interception/preloading; libraries also feature-detect these.
    for (const __n of ["A", "SVG", "G", "Defs", "Desc", "Title", "Symbol", "Use",
        "Image", "Switch", "Style", "Script", "Path", "Rect", "Circle", "Ellipse",
        "Line", "Polyline", "Polygon", "Text", "TSpan", "TextPath", "Marker",
        "ClipPath", "Mask", "Pattern", "LinearGradient", "RadialGradient", "Stop",
        "Filter", "ForeignObject", "Graphics", "Geometry", "View", "GradientStop"]) {
        const __cn = "SVG" + __n + "Element";
        if (!g[__cn]) {
            const __C = class extends SVGElement {};
            try { Object.defineProperty(__C, "name", { value: __cn }); } catch (e) {}
            g[__cn] = __C;
        }
    }
    // WebGL interface objects. We have no GPU, so canvas.getContext('webgl'|
    // 'webgl2') returns null (HTMLCanvasElement above) — EXACTLY a browser with
    // WebGL blocklisted / hardware acceleration off. But the interface objects
    // themselves still exist on a modern browser regardless of whether a context
    // can be obtained, and feature-detection reads `'WebGLRenderingContext' in
    // window`; omitting them reads as "a browser too old to know WebGL" instead
    // of the truthful "modern browser, WebGL unavailable on this machine". They
    // are non-constructible interface objects, so call/`new` throws a TypeError
    // ("Illegal constructor"), matching the platform. We expose NO context and
    // NO renderer string — that fabricated fingerprint surface stays absent.
    for (const __n of ["WebGLRenderingContext", "WebGL2RenderingContext"]) {
        const __C = function () { throw new TypeError("Illegal constructor"); };
        try { Object.defineProperty(__C, "name", { value: __n }); } catch (e) {}
        g[__n] = __C;
    }
    g.Image = class { constructor() { return g.document.createElement("img"); } };
    // `new Audio(src)` — the legacy HTMLAudioElement constructor (parallel to
    // Image). Returns an <audio> element with no-op media methods: TRust never
    // plays audio (the video→mpv / no-media ethos), but sites construct one for
    // sound-effect preloading and feature detection — a bare `Audio` reference
    // (ReferenceError when absent) silently broke YouTube's whole renderer family.
    g.Audio = class {
        constructor(src) {
            const el = g.document.createElement("audio");
            if (src !== undefined && src !== null) el.setAttribute("src", String(src));
            // createElement('audio') wraps as HTMLAudioElement, so play/pause/
            // load/canPlayType come from the HTMLMediaElement prototype
            // (canPlayType reports honest support for mpv-playable formats).
            return el;
        }
    };
    // Path2D — the Canvas geometry container (a declared <canvas> path). We
    // paint no raster, so it records nothing and no-ops its building methods,
    // but it MUST exist and be constructable: code that does `new Path2D()` /
    // `new Path2D(svgPath)` and feeds it to ctx.fill(path)/stroke(path)/clip(path)
    // (which already ignore the arg) otherwise hits a ReferenceError on the bare
    // global (Twitch's polyfills reference it unguarded).
    g.Path2D = class Path2D {
        constructor(_path) {}
        addPath() {} closePath() {} moveTo() {} lineTo() {}
        bezierCurveTo() {} quadraticCurveTo() {} arc() {} arcTo() {}
        ellipse() {} rect() {} roundRect() {}
    };
    // Blob/File — a standard data container. Sites construct Blobs (object
    // URLs, upload chunking, sanitizer/worker plumbing) and a bare `Blob`
    // reference (ReferenceError when absent) silently broke YouTube renderers.
    // The read surface is BYTE-faithful: `__blobBytes` (hoisted, defined with
    // the blob-URL store below) flattens the parts to the true underlying
    // bytes — string parts UTF-8, BufferSource parts raw — so slice()/text()/
    // arrayBuffer()/stream() round-trip binary instead of losing it (slice()
    // used to return an EMPTY blob, so upload chunkers sent nothing).
    g.Blob = class Blob {
        constructor(parts, opts) {
            this.__parts = Array.isArray(parts) ? parts.slice() : (parts ? [parts] : []);
            let size = 0;
            for (const p of this.__parts) {
                if (typeof p === "string") size += p.length;
                else if (p && typeof p.byteLength === "number") size += p.byteLength;
                else if (p && typeof p.size === "number") size += p.size;
                else size += String(p).length;
            }
            this.size = size;
            this.type = (opts && opts.type) ? String(opts.type).toLowerCase() : "";
        }
        // File API §slice: negative offsets are size-relative, the range is
        // clamped, and the slice carries the given content type (or "").
        slice(start, end, contentType) {
            const bytes = __blobBytes(this);
            const size = bytes.length;
            let s = start === undefined ? 0 : Math.trunc(+start) || 0;
            let e = end === undefined ? size : Math.trunc(+end) || 0;
            if (s < 0) s = Math.max(size + s, 0); else s = Math.min(s, size);
            if (e < 0) e = Math.max(size + e, 0); else e = Math.min(e, size);
            const span = Math.max(e - s, 0);
            return new g.Blob([__latin1ToBytes(bytes.slice(s, s + span))],
                { type: contentType === undefined ? "" : String(contentType).toLowerCase() });
        }
        text() { return Promise.resolve(new g.TextDecoder().decode(__latin1ToBytes(__blobBytes(this)))); }
        arrayBuffer() { return Promise.resolve(__latin1ToBytes(__blobBytes(this)).buffer); }
        stream() {
            const bytes = __latin1ToBytes(__blobBytes(this));
            return new g.ReadableStream({
                start(c) { if (bytes.length) c.enqueue(bytes); c.close(); },
            });
        }
    };
    g.File = class File extends g.Blob {
        constructor(parts, name, opts) { super(parts, opts); this.name = String(name); this.lastModified = (opts && opts.lastModified) || Date.now(); }
    };
    // Text -> its UTF-8 bytes as a binary (latin1) string, so btoa() and
    // ArrayBuffer views see real bytes (not surrogate-pair chars).
    const utf8Binary = (s) => {
        const bytes = new g.TextEncoder().encode(s);
        let out = "";
        for (let i = 0; i < bytes.length; i++) out += String.fromCharCode(bytes[i]);
        return out;
    };
    // FormData (XHR standard §5, https://xhr.spec.whatwg.org/#interface-formdata):
    // an ordered list of (name, value) entries where a value is a string or a
    // File. `new FormData(form)` collects the form's submittable fields (HTML
    // §"constructing the entry list" — the common cases). Used as a fetch/XHR
    // body it encodes as multipart/form-data (`__formDataWire` below). Steam's
    // store main.js news-up a FormData in a timer — a ReferenceError without it.
    g.FormData = class FormData {
        constructor(form) {
            this.__entries = [];
            if (form === undefined || form === null) return;
            if (typeof form.querySelectorAll !== "function" || form.localName !== "form") {
                throw new TypeError("FormData constructor: argument 1 is not a form element");
            }
            const els = form.querySelectorAll("input, select, textarea");
            for (let i = 0; i < els.length; i++) {
                const el = els[i];
                const name = el.getAttribute("name");
                if (!name) continue;
                if (el.disabled || el.hasAttribute("disabled")) continue;
                const tag = el.localName;
                if (tag === "select") {
                    const opts = el.querySelectorAll("option");
                    for (let j = 0; j < opts.length; j++) {
                        if (opts[j].selected && !opts[j].disabled) {
                            this.__entries.push({ name: name, value: String(opts[j].value) });
                        }
                    }
                } else if (tag === "textarea") {
                    this.__entries.push({ name: name, value: String(el.value == null ? "" : el.value) });
                } else {
                    const type = (el.getAttribute("type") || "text").toLowerCase();
                    if (type === "checkbox" || type === "radio") {
                        if (!el.checked) continue;
                        this.__entries.push({ name: name, value: el.hasAttribute("value") ? String(el.getAttribute("value")) : "on" });
                    } else if (type === "file") {
                        // No real file selection in this engine — the spec's
                        // "no files selected" entry: a single empty File.
                        this.__entries.push({ name: name, value: new g.File([], "", { type: "application/octet-stream" }) });
                    } else if (type === "submit" || type === "button" || type === "reset" || type === "image") {
                        continue; // buttons enter only as the submitter (we pass none)
                    } else if (type === "hidden" && name === "_charset_") {
                        this.__entries.push({ name: name, value: "UTF-8" });
                    } else {
                        this.__entries.push({ name: name, value: String(el.value == null ? "" : el.value) });
                    }
                }
            }
        }
        // Blob → File conversion on append/set (spec: a Blob value becomes a
        // File named "blob"; an explicit filename renames either).
        __val(value, filename) {
            if (value && Array.isArray(value.__parts)) {
                if (!(value instanceof g.File)) {
                    return new g.File(value.__parts.slice(), filename === undefined ? "blob" : String(filename), { type: value.type });
                }
                if (filename !== undefined) {
                    return new g.File(value.__parts.slice(), String(filename), { type: value.type, lastModified: value.lastModified });
                }
                return value;
            }
            return String(value);
        }
        append(name, value, filename) { this.__entries.push({ name: String(name), value: this.__val(value, filename) }); }
        set(name, value, filename) {
            const n = String(name);
            const v = this.__val(value, filename);
            const es = this.__entries;
            let placed = false;
            for (let i = 0; i < es.length; i++) {
                if (es[i].name !== n) continue;
                if (placed) { es.splice(i, 1); i--; continue; }
                es[i] = { name: n, value: v };
                placed = true;
            }
            if (!placed) es.push({ name: n, value: v });
        }
        delete(name) {
            const n = String(name);
            const es = this.__entries;
            for (let i = 0; i < es.length; i++) {
                if (es[i].name === n) { es.splice(i, 1); i--; }
            }
        }
        get(name) {
            const n = String(name);
            for (let i = 0; i < this.__entries.length; i++) if (this.__entries[i].name === n) return this.__entries[i].value;
            return null;
        }
        getAll(name) {
            const n = String(name);
            const out = [];
            for (let i = 0; i < this.__entries.length; i++) if (this.__entries[i].name === n) out.push(this.__entries[i].value);
            return out;
        }
        has(name) {
            const n = String(name);
            for (let i = 0; i < this.__entries.length; i++) if (this.__entries[i].name === n) return true;
            return false;
        }
        entries() { return this.__entries.map((e) => [e.name, e.value])[Symbol.iterator](); }
        keys() { return this.__entries.map((e) => e.name)[Symbol.iterator](); }
        values() { return this.__entries.map((e) => e.value)[Symbol.iterator](); }
        forEach(fn, thisArg) {
            const es = this.__entries.slice();
            for (let i = 0; i < es.length; i++) fn.call(thisArg, es[i].value, es[i].name, this);
        }
        get [Symbol.toStringTag]() { return "FormData"; }
    };
    g.FormData.prototype[Symbol.iterator] = g.FormData.prototype.entries;

    // FileReader: async reads off a Blob/File. We already hold the blob's
    // bytes in JS, so the read is local; we still settle on a macrotask
    // (setTimeout 0) like the platform, firing loadstart -> load -> loadend
    // (or error) and the matching on* handlers.
    g.FileReader = class FileReader extends EventTarget {
        constructor() {
            super();
            this.readyState = 0; // EMPTY
            this.result = null;
            this.error = null;
            this.onloadstart = null; this.onprogress = null; this.onload = null;
            this.onabort = null; this.onerror = null; this.onloadend = null;
        }
        get EMPTY() { return 0; }
        get LOADING() { return 1; }
        get DONE() { return 2; }
        __fire(t) {
            const ev = new Event(t); ev.target = this; ev.currentTarget = this;
            const on = this["on" + t];
            if (typeof on === "function") { try { on.call(this, ev); } catch (e) { trust.errors.push("filereader on" + t + ": " + ((e && e.message) || e)); } }
            try { dispatch(this, ev, false); } catch (e) {}
        }
        __read(blob, makeResult) {
            this.readyState = 1; // LOADING
            this.result = null; this.error = null;
            this.__fire("loadstart");
            const self = this;
            g.setTimeout(() => {
                try {
                    const r = makeResult(blob);
                    self.result = r; self.readyState = 2;
                    self.__fire("progress"); self.__fire("load"); self.__fire("loadend");
                } catch (e) {
                    self.error = g.DOMException ? new g.DOMException(String((e && e.message) || e), "NotReadableError") : e;
                    self.readyState = 2;
                    self.__fire("error"); self.__fire("loadend");
                }
            }, 0);
        }
        // Byte-faithful reads via `__blobBytes` (string-only blobs read
        // identically to the old text-part join; binary parts now survive).
        readAsText(blob) { this.__read(blob, (b) => new g.TextDecoder().decode(__latin1ToBytes(__blobBytes(b)))); }
        readAsBinaryString(blob) { this.__read(blob, (b) => __blobBytes(b)); }
        readAsDataURL(blob) {
            this.__read(blob, (b) => {
                const type = (b && b.type) || "application/octet-stream";
                return "data:" + type + ";base64," + g.btoa(__blobBytes(b));
            });
        }
        readAsArrayBuffer(blob) {
            this.__read(blob, (b) => __latin1ToBytes(__blobBytes(b)).buffer);
        }
        abort() {
            if (this.readyState !== 1) return;
            this.readyState = 2; this.result = null;
            this.__fire("abort"); this.__fire("loadend");
        }
    };
    g.FileReader.EMPTY = 0; g.FileReader.LOADING = 1; g.FileReader.DONE = 2;

    g.window = g; g.self = g; g.top = g; g.parent = g;
    // `window.frames` is the WindowProxy itself in a browser (an array-like of
    // child browsing contexts). With no child frames it's just `window`, so
    // `window.frames[name]` is a plain undefined lookup rather than a throw.
    // Consent-management bootstraps (IAB TCF `__tcfapiLocator` probes —
    // FastCMP, Quantcast, every CMP stub) read `window.frames[locatorName]`
    // unguarded; a missing `frames` made that a "convert undefined to object".
    g.frames = g;
    g.DOMTokenList = DOMTokenList;
    g.DOMStringMap = DOMStringMap;
    g.document = wrap(0);

    // --- environment ---
    const L = __url_parse(cfg.url, null) || [cfg.url, "", "", "", "", "", "", "", ""];
    const locState = {
        href: L[0], protocol: L[1], host: L[2], hostname: L[3], port: L[4],
        pathname: L[5], search: L[6], hash: L[7], origin: L[8],
    };
    const setLocParts = (p) => {
        locState.href = p[0]; locState.protocol = p[1]; locState.host = p[2];
        locState.hostname = p[3]; locState.port = p[4]; locState.pathname = p[5];
        locState.search = p[6]; locState.hash = p[7]; locState.origin = p[8];
        baseHrefCache = null; // the base resolves against location.href
    };
    const withoutHash = (u) => {
        const i = String(u).indexOf("#");
        return i < 0 ? String(u) : String(u).slice(0, i);
    };
    const fireHashChange = (oldURL, newURL) => {
        const ev = new Event("hashchange");
        ev.oldURL = oldURL; ev.newURL = newURL;
        dispatch(g, ev, false);
    };
    const navigateLoc = (u, hashOnly, replace) => {
        if (u === undefined || u === null) return;
        const p = __url_parse(String(u), locState.href);
        if (!p) return;
        const old = locState.href;
        setLocParts(p);
        if (withoutHash(old) === withoutHash(p[0])) {
            if (old !== p[0]) fireHashChange(old, p[0]);
            // Same document, only the fragment moved (or was re-set): HTML's
            // "navigate to a fragment" scrolls the indicated element into view.
            // Signal the app the new target (`""` = the top, for a bare URL /
            // `#top`); the live engine has no scroll model of its own. This is
            // how Astro's ClientRouter #anchor links (`location.href = "#x"`)
            // reach the app to scroll — see PageEvt::ScrollToFragment.
            trust.scrollFragment = locState.hash ? locState.hash.slice(1) : "";
        } else if (!hashOnly) {
            trust.navigation = p[0];
            trust.navigationReplace = !!replace;
        }
    };
    const updateLoc = (u) => {
        if (u === undefined || u === null) return;
        const p = __url_parse(String(u), locState.href);
        if (p) setLocParts(p);
    };
    // HTML §Location component setters: copy the URL, apply the component with
    // the URL parser's state-override semantics (`__url_set` = the same WHATWG
    // setter the URL class uses), then Location-object navigate to the result.
    // A value the parser refuses leaves the URL unchanged, and `navigateLoc`
    // treats an unchanged href as a no-op (deliberate deviation: the spec
    // re-navigates — a reload — even on a no-change set; a terminal browser
    // has nothing to gain from that).
    const setLocPart = (which, v) => {
        const r = __url_set(locState.href, which, String(v));
        if (r) navigateLoc(r[0], false);
    };
    const loc = {
        get href() { return locState.href; }, set href(v) { navigateLoc(v, false); },
        get protocol() { return locState.protocol; },
        set protocol(v) {
            // Basic-parse `v + ":"` with scheme start state: the scheme is
            // whatever precedes the first ":" (so "https:" == "https::::" ==
            // "https", and trailing junk after the colon is ignored); a
            // non-scheme token is a SyntaxError. Location (unlike URL) then
            // only navigates when the result stays in the HTTP(S) family.
            v = String(v);
            const colon = v.indexOf(":");
            const scheme = colon < 0 ? v : v.slice(0, colon);
            if (!/^[A-Za-z][A-Za-z0-9+.-]*$/.test(scheme))
                throw new DOMException("Failed to set the 'protocol' property on 'Location': '" + v + "' is not a valid protocol.", "SyntaxError");
            const r = __url_set(locState.href, "protocol", scheme);
            if (r && (r[1] === "http:" || r[1] === "https:")) navigateLoc(r[0], false);
        },
        get host() { return locState.host; }, set host(v) { setLocPart("host", v); },
        get hostname() { return locState.hostname; }, set hostname(v) { setLocPart("hostname", v); },
        get port() { return locState.port; }, set port(v) { setLocPart("port", v); },
        get pathname() { return locState.pathname; }, set pathname(v) { navigateLoc(locState.origin + String(v) + locState.search + locState.hash, false); },
        get search() { return locState.search; }, set search(v) { const q = String(v); navigateLoc(locState.origin + locState.pathname + (q && q[0] === "?" ? q : (q ? "?" + q : "")) + locState.hash, false); },
        get hash() { return locState.hash; }, set hash(v) { const h = String(v); navigateLoc(withoutHash(locState.href) + (h && h[0] === "#" ? h : (h ? "#" + h : "")), true); },
        get origin() { return locState.origin; },
        assign(u) { navigateLoc(u, false); },
        replace(u) { navigateLoc(u, false, true); },
        reload() { trust.navigation = locState.href; trust.navigationReplace = false; },
        toString() { return locState.href; },
    };
    Object.defineProperty(g, "location", {
        configurable: true, enumerable: true,
        get() { return loc; },
        set(v) { navigateLoc(v, false); },
    });
    // Secure Contexts §3.1–§3.2: the Rust loader supplies the result for
    // network documents; this fallback keeps hand-built test contexts honest
    // for HTTPS, file, and loopback HTTP URLs. Blob URLs inherit the owner's
    // result in the Worker configuration below.
    const secureContext = cfg.secureContext !== undefined ? !!cfg.secureContext :
        (locState.protocol === "https:" || locState.protocol === "wss:" || locState.protocol === "file:" ||
         (locState.protocol === "http:" &&
          (locState.hostname.toLowerCase() === "localhost" ||
           locState.hostname.toLowerCase().endsWith(".localhost") ||
           /^127(?:\.\d{1,3}){3}$/.test(locState.hostname) || locState.hostname === "::1")));
    Object.defineProperty(g, "isSecureContext", {
        configurable: true, enumerable: true, value: secureContext, writable: false,
    });
    trust.navigationReplaces = function () { return !!trust.navigationReplace; };
    trust.takeNavigation = function () {
        const n = trust.navigation || null;
        trust.navigation = null; trust.navigationReplace = false;
        return n;
    };
    // HTML §7.2.5 URL and history update steps are synchronous inside the
    // realm, but browser chrome/session history live across the Rust actor
    // boundary. Preserve every push/replace in task order; keeping only the
    // last URL would collapse distinct session-history entries created by one
    // script task.
    trust.historyUpdates = [];
    trust.takeHistoryUpdates = function () {
        const updates = trust.historyUpdates;
        trust.historyUpdates = [];
        return JSON.stringify(updates);
    };
    // The pending same-document fragment scroll (see `navigateLoc`). `undefined`
    // (no signal) → null; a string (possibly `""` for the top) → that target.
    trust.takeScrollFragment = function () { const f = trust.scrollFragment; trust.scrollFragment = undefined; return f === undefined ? null : f; };
    // Host objects must NOT look like plain objects. Real browsers tag
    // them, so `Object.prototype.toString.call(window)` is "[object
    // Window]". Without this they read as "[object Object]", and a
    // library that deep-merges/clones (jQuery UI's widget.extend via
    // isPlainObject) follows window.window / document.defaultView in an
    // infinite cycle until the recursion limit trips (broke danbooru).
    try { g[Symbol.toStringTag] = "Window"; } catch (e) { /* frozen global */ }
    // ...and put the global on Window.prototype so `window instanceof Window`
    // holds and `Window.prototype` reads resolve. The own properties set
    // above are unaffected by the reparent; guard in case the global is frozen.
    try { Object.setPrototypeOf(g, Window.prototype); } catch (e) { /* frozen global */ }
    // WHATWG HTML §NavigatorLanguage: languages is a stable FrozenArray and
    // language is its first (most-preferred) entry.
    const navigatorLanguages = Object.freeze((cfg.languages || ["en-US", "en"]).slice());
    // GPC §3.2–§3.4: cache the user preference for this top-level navigation;
    // the read-only DOM property must agree with the Sec-GPC header sent by
    // the user agent for that navigation.
    const navigatorGpc = cfg.globalPrivacyControl !== false;
    g.navigator = {
        // Real browsers report a region-qualified BCP-47 tag (Chrome/Firefox
        // default to "en-US"), not a bare "en". Language detectors key off
        // this: Open WebUI's i18n loads exactly the detected tag, and its
        // bundle ships "en-US" (no bare "en"), so a bare "en" missed the map
        // and rejected the translation load.
        userAgent: cfg.ua, language: cfg.language || navigatorLanguages[0], languages: navigatorLanguages,
        platform: "Linux", cookieEnabled: true, onLine: true,
        plugins: [], mimeTypes: [], webdriver: false,
        // Spec-mandated Navigator members (HTML §"Client identification" /
        // NavigatorConcurrentHardware; maxTouchPoints from Pointer Events).
        // appCodeName/appName/product/vendorSub are FROZEN compatibility
        // constants every conformant browser returns verbatim regardless of
        // engine; vendor/productSub take the non-Chrome/non-WebKit (Gecko-mode)
        // values — the honest residual for a client that is neither. appVersion
        // is the UA with a leading "Mozilla/" stripped (we carry none → the UA
        // itself). hardwareConcurrency is the real host core count; we are not a
        // touch device so maxTouchPoints is 0. These are honest values for our
        // environment that legit feature-detection reads, NOT browser-spoofing
        // (returning `undefined` makes us look subtly broken to standard code).
        appCodeName: "Mozilla", appName: "Netscape", product: "Gecko",
        productSub: "20100101", vendor: "", vendorSub: "",
        appVersion: String(cfg.ua).replace(/^Mozilla\//, ""),
        doNotTrack: null,
        hardwareConcurrency: cfg.hardwareConcurrency || 8, maxTouchPoints: 0,
        // Beacon API §sendBeacon: a fire-and-forget POST. We really send it
        // (there is no unload window to defer past, so queue-and-send
        // collapses to send-now); `true` is the spec's accepted-for-
        // transmission signal, `false` only if the arguments are unusable.
        // Analytics/telemetry call this unguarded from visibility handlers —
        // a missing function was an uncaught TypeError there.
        sendBeacon(url, data) {
            try {
                g.fetch(String(url), { method: "POST", body: data === undefined ? null : data, keepalive: true })
                    .catch(() => {});
                return true;
            } catch (e) { return false; }
        },
        // Permissions API (navigator.permissions.query) — feature-detected
        // widely. Returns a Promise<PermissionStatus>. We grant no device/UA
        // capability, so every query resolves to the neutral pre-decision
        // "prompt" state (the spec default before any user choice — honest:
        // we have made no grant, and the state never changes so onchange never
        // fires). A missing/non-string descriptor name rejects with TypeError
        // per spec; we accept any other name (permissive over the long tail of
        // vendor-specific names, rather than the spec's reject-unknown).
        permissions: {
            query(desc) {
                if (desc == null || typeof desc.name !== "string") {
                    return Promise.reject(new TypeError(
                        "Failed to execute 'query' on 'Permissions': required member name is undefined."));
                }
                return Promise.resolve({
                    name: desc.name, state: "prompt", onchange: null,
                    addEventListener() {}, removeEventListener() {},
                    dispatchEvent() { return false; },
                });
            },
        },
    };
    Object.defineProperty(g.navigator, "globalPrivacyControl", {
        configurable: true, enumerable: true,
        get() { return navigatorGpc; },
    });
    g.screen = { width: cfg.width, height: cfg.height, availWidth: cfg.width, availHeight: cfg.height, colorDepth: 24, pixelDepth: 24 };
    g.innerWidth = cfg.width; g.innerHeight = cfg.height;
    g.outerWidth = cfg.width; g.outerHeight = cfg.height;
    g.devicePixelRatio = cfg.devicePixelRatio; g.pageXOffset = 0; g.pageYOffset = 0;
    g.scrollX = 0; g.scrollY = 0;
    // WHATWG HTML §7.2.5 "The History interface", shared history
    // push/replace state steps and "can have its URL rewritten". These are
    // same-document updates: clone the state, validate/resolve the optional
    // URL, update Location without hashchange/popstate, then tell the browser
    // controller to apply the corresponding history handling behavior.
    const canRewriteHistoryURL = (current, target) => {
        if (!current || !target) return false;
        // scheme, username, password, host, and port must all be identical.
        if (current[1] !== target[1] || current[9] !== target[9] ||
            current[10] !== target[10] || current[3] !== target[3] ||
            current[4] !== target[4]) return false;
        if (target[1] === "http:" || target[1] === "https:") return true;
        if (target[1] === "file:") return current[5] === target[5];
        return current[5] === target[5] && current[6] === target[6];
    };
    let historyObject;
    const updateHistoryState = (s, u, replace) => {
            // StructuredSerializeForStorage/deserialize gives history.state a
            // detached value and propagates DataCloneError for unsupported
            // input. `structuredClone` is installed by the time author code
            // can invoke this method.
            const state = g.structuredClone(s);
            let parsed = __url_parse(locState.href, null);
            if (u !== undefined && u !== null && String(u) !== "") {
                parsed = __url_parse(String(u), locState.href);
                const current = __url_parse(locState.href, null);
                if (!parsed || !canRewriteHistoryURL(current, parsed)) {
                    throw new DOMException(
                        "History state URL cannot be created in a document with origin '" + locState.origin + "'.",
                        "SecurityError");
                }
            }
            // The current URL is already valid; this is only a defensive guard
            // for synthetic test realms whose configured URL failed parsing.
            if (!parsed) throw new DOMException("Invalid history state URL.", "SecurityError");
            historyObject.state = state;
            if (!replace) historyObject.length += 1;
            setLocParts(parsed);
            trust.historyUpdates.push({ url: parsed[0], replace: !!replace });
    };
    historyObject = {
        length: 1, state: null, scrollRestoration: "auto",
        pushState(s, _t, u) { updateHistoryState(s, u, false); },
        replaceState(s, _t, u) { updateHistoryState(s, u, true); },
        back() {}, forward() {}, go() {},
    };
    g.history = historyObject;
    // getComputedStyle is now cascade-backed (read-only): __dom_computed
    // returns the inherited / UA-defaulted value for tracked properties and
    // the inline value for the rest, falling back to the element's own inline
    // style on a miss. Was inline-only (it just handed back el.style).
    function computedStyleFor(el) {
        const lookup = (k) => {
            k = kebab(String(k));
            let v = null;
            try { v = __dom_computed(el.__id, k); } catch (e) { v = null; }
            if (v !== null && v !== undefined) return v;
            return el.style.getPropertyValue(k) || "";
        };
        return new Proxy({}, {
            get(_, p) {
                if (typeof p !== "string") return undefined;
                if (p === "getPropertyValue") return (k) => lookup(k);
                if (p === "cssText") return el.getAttribute("style") || "";
                return lookup(p);
            },
            set() { return true; }, // computed style is read-only
            has() { return true; },
        });
    }
    g.getComputedStyle = (el) => (el instanceof Element ? computedStyleFor(el) : makeStyle());
    // matchMedia evaluates the query against the real viewport through the same
    // Rust `@media` evaluator the cascade uses (width/height/orientation etc.);
    // `.matches` is a live getter so a later read reflects the current viewport.
    // Listener plumbing stays inert — TRust re-evaluates media only on reload
    // (a breakpoint-crossing resize reloads), so there is no change event to fire.
    g.matchMedia = (m) => mediaQueryListForViewport(m, function () {
        return [g.innerWidth, g.innerHeight];
    });
    // window.CSS — feature detection (used across the web, not just
    // css3test). `supports("selector(…)")` runs the real selector engine
    // (honest); the property/value form leans on the style declaration's
    // own acceptance (permissive, like the rest of our CSS surface — we
    // recognize broadly, we don't validate values). `escape` is the CSSOM
    // serialization algorithm.
    function cssEscape(value) {
        value = String(value);
        const len = value.length;
        let out = "";
        for (let i = 0; i < len; i++) {
            const c = value.charCodeAt(i);
            if (c === 0) { out += "�"; continue; }
            if ((c >= 0x1 && c <= 0x1f) || c === 0x7f ||
                (i === 0 && c >= 0x30 && c <= 0x39) ||
                (i === 1 && c >= 0x30 && c <= 0x39 && value.charCodeAt(0) === 0x2d)) {
                out += "\\" + c.toString(16) + " "; continue;
            }
            if (i === 0 && len === 1 && c === 0x2d) { out += "\\" + value.charAt(i); continue; }
            if (c >= 0x80 || c === 0x2d || c === 0x5f ||
                (c >= 0x30 && c <= 0x39) || (c >= 0x41 && c <= 0x5a) || (c >= 0x61 && c <= 0x7a)) {
                out += value.charAt(i); continue;
            }
            out += "\\" + value.charAt(i);
        }
        return out;
    }
    const CSS = {
        escape: cssEscape,
        supports(prop, value) {
            if (value === undefined) {
                let cond = String(prop).trim();
                const m = /^selector\(([\s\S]*)\)$/.exec(cond);
                if (m) return __css_supports_selector(m[1].trim());
                if (cond[0] === "(" && cond[cond.length - 1] === ")") cond = cond.slice(1, -1);
                const c = cond.indexOf(":");
                if (c < 0) return false;
                return CSS.supports(cond.slice(0, c).trim(), cond.slice(c + 1).trim());
            }
            try {
                const d = document.createElement("_").style;
                d.setProperty(String(prop), String(value));
                return d.getPropertyValue(String(prop)) !== "";
            } catch (e) { return false; }
        },
    };
    g.CSS = CSS;
    g.alert = () => {}; g.confirm = () => false; g.prompt = () => null;
    g.scroll = g.scrollTo = g.scrollBy = () => {};
    // window.open: open a new browsing context. A single-view TUI has none, so
    // this is a no-op that returns a minimal stub window (NEVER null — page
    // code routinely chains `window.open(...).focus()`) and never throws. A
    // missing `window.open` was an UNCAUGHT TypeError that aborted click
    // handlers mid-flow (erome's age gate calls `window.open(url)` then
    // `location.href = ...`; the throw killed the navigation). We deliberately
    // don't navigate the current view for a programmatic popup — ad/popunder
    // scripts abuse it — so flows that fall back to `location.href` proceed.
    g.open = function (url) {
        const u = url === undefined ? "" : String(url);
        return {
            closed: false, name: "",
            focus() {}, blur() {}, print() {}, close() { this.closed = true; },
            postMessage() {}, moveTo() {}, resizeTo() {}, scroll() {}, scrollTo() {},
            location: { href: u, assign() {}, replace() {}, reload() {}, toString() { return u; } },
            document: g.document, opener: g,
        };
    };
    // DOM Range: feature-detected/instanceof'd at boot, and used for
    // measurement + HTML-string parsing (createContextualFragment, jQuery's
    // `$.parseHTML` fallback). We hold endpoints honestly but approximate
    // geometry with the viewport box like the element rect stubs.
    class Range {
        constructor() {
            this.startContainer = g.document; this.endContainer = g.document;
            this.startOffset = 0; this.endOffset = 0; this.collapsed = true;
            this.commonAncestorContainer = g.document;
        }
        __upd() { this.collapsed = this.startContainer === this.endContainer && this.startOffset === this.endOffset; this.commonAncestorContainer = this.startContainer; }
        setStart(node, off) { this.startContainer = node; this.startOffset = off | 0; this.__upd(); }
        setEnd(node, off) { this.endContainer = node; this.endOffset = off | 0; this.__upd(); }
        setStartBefore(node) { if (node && node.parentNode) this.setStart(node.parentNode, 0); }
        setStartAfter(node) { if (node && node.parentNode) this.setStart(node.parentNode, 0); }
        setEndBefore(node) { if (node && node.parentNode) this.setEnd(node.parentNode, 0); }
        setEndAfter(node) { if (node && node.parentNode) this.setEnd(node.parentNode, 0); }
        selectNode(node) { this.startContainer = this.endContainer = this.commonAncestorContainer = node; this.collapsed = false; }
        selectNodeContents(node) { this.selectNode(node); }
        collapse(toStart) {
            if (toStart) { this.endContainer = this.startContainer; this.endOffset = this.startOffset; }
            else { this.startContainer = this.endContainer; this.startOffset = this.endOffset; }
            this.collapsed = true;
        }
        cloneRange() { const r = new Range(); r.startContainer = this.startContainer; r.endContainer = this.endContainer; r.startOffset = this.startOffset; r.endOffset = this.endOffset; r.collapsed = this.collapsed; r.commonAncestorContainer = this.commonAncestorContainer; return r; }
        cloneContents() { return g.document.createDocumentFragment(); }
        extractContents() { return g.document.createDocumentFragment(); }
        deleteContents() {}
        insertNode(node) { const c = this.startContainer; if (c && c.insertBefore) c.insertBefore(node, (c.childNodes && c.childNodes[this.startOffset]) || null); }
        surroundContents(node) { this.insertNode(node); }
        createContextualFragment(html) { const tpl = g.document.createElement("template"); tpl.innerHTML = String(html); return tpl.content; }
        getBoundingClientRect() { return { x: 0, y: 0, top: 0, left: 0, right: g.innerWidth, bottom: g.innerHeight, width: g.innerWidth, height: g.innerHeight }; }
        getClientRects() { return [this.getBoundingClientRect()]; }
        detach() {}
        toString() { return ""; }
    }
    g.Range = Range;
    class Selection {
        constructor() { this.rangeCount = 0; this.isCollapsed = true; this.type = "None"; this.anchorNode = null; this.focusNode = null; }
        toString() { return ""; }
        getRangeAt() { return new Range(); }
        addRange() {} removeAllRanges() {} removeRange() {} empty() {}
        collapse() {} collapseToStart() {} collapseToEnd() {} selectAllChildren() {}
        setBaseAndExtent() {} extend() {} containsNode() { return false; }
    }
    g.Selection = Selection;
    g.getSelection = () => new Selection();
    // --- MutationObserver (real) ---------------------------------------
    // A pure-JS DOM-mutation observer, delivered as a microtask exactly like
    // the spec's "mutation observer microtask". Records are emitted ONLY by the
    // mutation wrappers below (appendChild/setAttribute/…), so MO costs nothing
    // until something actually mutates the DOM — during load/settle, a click or
    // form dispatch, OR an at-rest timer/animation (the engine now runs at rest,
    // so a slideshow or poller that mutates the DOM fires its observers in real
    // time too). A page with no observers AND no scheduled work simply parks
    // (zero idle CPU): nothing mutates, so nothing fires.
    //
    // Records are recorded against the live observer list `MO`. The hot path
    // (zero observers) is a single `MO.length` check at each mutation site;
    // with observers present, an unrelated mutation costs a `target ===` identity
    // test per registration (no ancestor walk unless that registration is
    // `subtree`). The subtree match is the one Rust syscall `__dom_contains`
    // (not a JS parent walk — trap #9).
    //
    // MO and each observer's registration list are PLAIN ARRAYS, deliberately
    // NOT Boa Set/Map: a Map/Set `for…of` holds a `MapLock` whose GC finalizer
    // re-borrows the backing map, and under the heavy allocation this hot loop
    // does (a record object per mutation) a GC mid-iteration trips
    // "Object already borrowed". Arrays have no such finalizer.
    const MO = [];               // live observers (each with a per-observer record queue)
    const MO_EMPTY = Object.freeze([]); // shared empty addedNodes/removedNodes (frozen ⇒ safe to share)
    let moHasChildList = false;
    let moHasAttributes = false;
    let moHasCharacterData = false;
    let moQueued = false;        // a delivery microtask is already scheduled
    let moChain = 0;             // consecutive delivery turns without the queue going quiet
    let moDisabled = false;      // tripped if an observer-feeds-observer loop runs away
    const MO_CHAIN_CAP = 1000;   // microtask-checkpoint lid (the spec has none; we need one)

    // Reset the loop guard at the start of each fresh compute window so a
    // pathological burst in one dispatch can't permanently mute a later one.
    function moResetGuard() { moChain = 0; moDisabled = false; }
    g.__trust.moResetGuard = moResetGuard;

    function moEnqueue() {
        if (moQueued || moDisabled) return;
        moQueued = true;
        Promise.resolve().then(moDeliver);
    }
    function moRecomputeKinds() {
        moHasChildList = moHasAttributes = moHasCharacterData = false;
        for (let i = 0; i < MO.length; i++) {
            const regs = MO[i].__targets;
            for (let j = 0; j < regs.length; j++) {
                const r = regs[j];
                moHasChildList = moHasChildList || r.childList;
                moHasAttributes = moHasAttributes || r.attributes;
                moHasCharacterData = moHasCharacterData || r.characterData;
                if (moHasChildList && moHasAttributes && moHasCharacterData) return;
            }
        }
    }
    function moDeliver() {
        moQueued = false;
        if (moDisabled) return;
        if (++moChain > MO_CHAIN_CAP) {
            moDisabled = true;
            for (let i = 0; i < MO.length; i++) MO[i].__records = [];
            trust.errors.push("MutationObserver: delivery exceeded " + MO_CHAIN_CAP +
                " microtask turns (observer loop?) — disabled for this page");
            return;
        }
        // Snapshot the observer list: a callback may observe/disconnect mid-loop.
        const obs = MO.slice();
        for (let i = 0; i < obs.length; i++) {
            const o = obs[i];
            if (!o.__records.length) continue;
            const recs = o.__records;
            o.__records = [];
            try { o.__cb(recs, o); }
            catch (e) { trust.errors.push("MutationObserver callback: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        }
        // The chain has ended iff no callback re-queued during this turn; only
        // then is it safe to clear the loop counter.
        if (!moQueued) moChain = 0;
    }

    function moIsAncestor(anc, node) {
        // anc strictly contains node? Direct-target matches are handled by the
        // `t === target` test; this is only consulted for subtree observers.
        return !!(node && __dom_contains(anc.__id, node.__id));
    }

    // Queue `rec` to every interested observer. `rec.type` is one of
    // "childList" | "attributes" | "characterData"; oldValue is nulled per
    // observer unless one of its matching registrations asked for it (spec).
    function moNotify(rec) {
        // 1-entry cache for the subtree ancestor test within THIS mutation:
        // multiple subtree observers commonly share a root (Steam registers 3
        // separate `#document subtree` observers), and they are scanned
        // consecutively, so the same `__dom_contains(root, target)` was run
        // once per observer. Caching the last (rootId -> result) collapses
        // those identical syscalls to one — no allocation, no semantic change.
        let cAid = null, cRes = false;
        // Deferred sibling capture: an insert/remove passes `__sib` (the node)
        // instead of pre-read `previousSibling`/`nextSibling`. We resolve them
        // ONCE, lazily, at the first matching observer — so a childList mutation
        // on a DETACHED subtree (jQuery builds offscreen; ~80% of Steam's) that
        // matches nobody pays NO sibling syscalls/wraps. Computed here it is
        // still synchronous with the mutation (insert: after `__dom_append`;
        // remove: before `__dom_detach`, since moNotify runs inside moChildRemove
        // before the detach), so the snapshot is spec-correct.
        let sibDone = false, prevSib = null, nextSib = null;
        // Resolve the record's type to booleans ONCE — moNotify runs per mutation
        // and the inner loop is per (observer × registration), so re-comparing
        // the `rec.type` string each iteration was the bulk of its cost.
        const isCL = rec.type === "childList", isAttr = rec.type === "attributes";
        let matchedAny = false;
        for (let i = 0; i < MO.length; i++) {
            const o = MO[i], regs = o.__targets;
            let matched = false, wantOld = false;
            for (let j = 0; j < regs.length; j++) {
                const opts = regs[j];
                if (isCL ? !opts.childList : isAttr ? !opts.attributes : !opts.characterData) continue;
                let hit = opts.target === rec.target;
                if (!hit && opts.subtree) {
                    const aid = opts.target.__id;
                    if (aid === cAid) hit = cRes;
                    else { cRes = moIsAncestor(opts.target, rec.target); cAid = aid; hit = cRes; }
                }
                if (!hit) continue;
                if (isAttr && opts.attributeFilter &&
                    opts.attributeFilter.indexOf(rec.attributeName) < 0) continue;
                matched = true;
                if ((isAttr && opts.attributeOldValue) ||
                    (!isCL && !isAttr && opts.characterDataOldValue)) { wantOld = true; break; }
            }
            if (!matched) continue;
            matchedAny = true;
            if (rec.__sib !== undefined && !sibDone) {
                sibDone = true;
                const s = rec.__sib;
                prevSib = s ? wrap(__dom_prev(s.__id)) : null;
                nextSib = s ? wrap(__dom_next(s.__id)) : null;
            }
            o.__records.push({
                type: rec.type,
                target: rec.target,
                addedNodes: rec.addedNodes || MO_EMPTY,
                removedNodes: rec.removedNodes || MO_EMPTY,
                previousSibling: rec.__sib !== undefined ? prevSib : (rec.previousSibling || null),
                nextSibling: rec.__sib !== undefined ? nextSib : (rec.nextSibling || null),
                attributeName: rec.attributeName || null,
                attributeNamespace: null,
                oldValue: wantOld ? (rec.oldValue === undefined ? null : rec.oldValue) : null,
            });
        }
        // DOM Standard §4.3.2 queues the mutation-observer microtask when a
        // record is actually queued. An observer registry can be non-empty
        // while every registration filters out this mutation; avoid creating
        // a needless Promise reaction in that case.
        if (matchedAny) moEnqueue();
    }

    // Emission helpers used by the mutation wrappers. Each bails on the
    // zero-observer fast path before touching the DOM for siblings/oldValue.
    function moChildInsert(parent, node) {        // call AFTER the insert
        if (!MO.length || !moHasChildList) return;
        // `__sib: node` defers prev/next-sibling capture into moNotify (resolved
        // only if some observer matches — see there). Was: eager
        // `previousSibling: node.previousSibling, …`, 2 syscalls + 2 wraps on
        // EVERY insert including the detached ones nobody observes.
        moNotify({ type: "childList", target: parent, addedNodes: [node], __sib: node });
    }
    function moChildRemove(parent, node) {        // call BEFORE the detach
        if (!MO.length || !moHasChildList) return;
        moNotify({ type: "childList", target: parent, removedNodes: [node], __sib: node });
    }
    function moChildBulk(target, removed, added) { // innerHTML / textContent / insertAdjacentHTML
        if (!MO.length || !moHasChildList) return;
        moNotify({ type: "childList", target, addedNodes: added, removedNodes: removed });
    }
    function moAttr(target, name, oldValue) {
        if (!MO.length || !moHasAttributes) return;
        moNotify({ type: "attributes", target, attributeName: name, oldValue });
    }
    function moCharData(target, oldValue) {
        if (!MO.length || !moHasCharacterData) return;
        moNotify({ type: "characterData", target, oldValue });
    }

    g.MutationObserver = class MutationObserver {
        constructor(cb) {
            if (typeof cb !== "function")
                throw new TypeError("Failed to construct 'MutationObserver': parameter 1 is not a function");
            this.__cb = cb;
            this.__records = [];
            this.__targets = []; // array of registrations: { target, childList, … }
        }
        observe(target, options) {
            if (!target || typeof target.__id !== "number")
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': parameter 1 is not of type 'Node'");
            options = options || {};
            let attributes = options.attributes;
            let characterData = options.characterData;
            const childList = !!options.childList;
            const subtree = !!options.subtree;
            const attributeOldValue = !!options.attributeOldValue;
            const characterDataOldValue = !!options.characterDataOldValue;
            const attributeFilter = options.attributeFilter
                ? Array.prototype.map.call(options.attributeFilter, String) : null;
            // Spec defaults: an *OldValue/Filter flag implies its category.
            if (attributes === undefined) attributes = !!(attributeOldValue || attributeFilter);
            if (characterData === undefined) characterData = !!characterDataOldValue;
            if (!childList && !attributes && !characterData)
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object must set at least one of 'attributes', 'characterData', or 'childList' to true.");
            if (attributeOldValue && !attributes)
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object may only set 'attributeOldValue' to true when 'attributes' is true or not present.");
            if (attributeFilter && !attributes)
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object may only set 'attributeFilter' when 'attributes' is true or not present.");
            if (characterDataOldValue && !characterData)
                throw new TypeError("Failed to execute 'observe' on 'MutationObserver': The options object may only set 'characterDataOldValue' to true when 'characterData' is true or not present.");
            // Re-observing the same node REPLACES its options (spec). Records
            // already queued for this observer survive (not the registration).
            const reg = { target, childList, attributes, characterData, subtree,
                attributeOldValue, characterDataOldValue, attributeFilter };
            let replaced = false;
            for (let i = 0; i < this.__targets.length; i++) {
                if (this.__targets[i].target === target) { this.__targets[i] = reg; replaced = true; break; }
            }
            if (!replaced) this.__targets.push(reg);
            if (MO.indexOf(this) < 0) MO.push(this);
            moRecomputeKinds();
        }
        disconnect() {
            this.__targets = [];
            this.__records = [];
            const i = MO.indexOf(this);
            if (i >= 0) MO.splice(i, 1);
            moRecomputeKinds();
        }
        takeRecords() { const r = this.__records; this.__records = []; return r; }
    };
    g.__viewportRect = () => {
        const vw = g.innerWidth, vh = g.innerHeight;
        return { x: 0, y: 0, left: 0, top: 0, right: vw, bottom: vh, width: vw, height: vh };
    };
    // IntersectionObserver — HONEST viewport intersection (W3C Intersection
    // Observer + CSSOM View). The terminal now threads the real scroll position
    // into the engine (PageCmd::Scroll → trust.setScroll updates scrollY and
    // re-runs the update step), so a below-the-fold target reports NOT
    // intersecting until the user scrolls it into the viewport ± the observer's
    // rootMargin. This is what makes a sentinel-driven infinite scroller
    // demand-driven (archive.org) instead of trying to reveal the whole document
    // at load — the old "whole document is the viewport" stub fired every target
    // fully-visible-once, which made an infinite scroller request endless tiles.
    // The trade (her call): below-fold lazy images load on scroll, not at load
    // (browser behaviour; a rootMargin still pre-buffers). Registry `IO` is a
    // PLAIN ARRAY, never a Boa Set/Map — same MapLock GC trap MO documents.
    const IO = [];
    // ResizeObserver registry — a PLAIN ARRAY (never a Boa Set/Map, the MapLock
    // GC trap MO documents). ResizeObserver is EDGE-TRIGGERED like IO: a target's
    // callback fires whenever its observed (border-box) size changes across the
    // page's active life, delivered in the "update the rendering" step
    // (`trust.updateResizes`, driven by `run_layout_observers`). An active engine
    // MUST re-deliver — a responsive grid (Twitch's shelves) reads its
    // container's width the moment it mounts, when the layout is still partial,
    // and needs the corrected width delivered once the DOM finishes building or
    // the terminal resizes; without re-delivery it stays stuck at the mount-time
    // measurement and renders too few cards.
    const RO = [];
    // Parse a rootMargin string into 4 {v, pct} offsets in CSS-margin order
    // (top, right, bottom, left), each px or %. Percentages resolve per-axis
    // against the root rect (top/bottom vs height, left/right vs width); the px
    // form is the overwhelming real-world case (Steam/archive use px or none).
    function ioParseRootMargin(s) {
        const parts = String(s == null ? "0px" : s).trim().split(/\s+/);
        const one = (p) => {
            const m = /^(-?\d*\.?\d+)(px|%)?$/.exec(p || "");
            return m ? { v: parseFloat(m[1]), pct: m[2] === "%" } : { v: 0, pct: false };
        };
        const t = one(parts[0]);
        const r = parts.length > 1 ? one(parts[1]) : t;
        const b = parts.length > 2 ? one(parts[2]) : t;
        const l = parts.length > 3 ? one(parts[3]) : r;
        return [t, r, b, l];
    }
    function ioNormThresholds(t) {
        let arr;
        if (t === undefined || t === null) arr = [0];
        else if (Array.isArray(t)) arr = t.slice();
        else arr = [t];
        arr = arr.map(Number).filter((n) => n >= 0 && n <= 1);
        if (!arr.length) arr = [0];
        arr.sort((a, b) => a - b);
        return arr;
    }
    g.IntersectionObserver = class {
        constructor(cb, opts) {
            this.__cb = cb;
            opts = opts || {};
            // An element root (scroll container) isn't modelled — the terminal
            // has one scroll, the document's — so any root is treated as the
            // viewport. rootMargin/threshold are honoured.
            this.root = opts.root || null;
            this.rootMargin = opts.rootMargin == null ? "0px 0px 0px 0px" : String(opts.rootMargin);
            this.__rm = ioParseRootMargin(opts.rootMargin);
            this.thresholds = ioNormThresholds(opts.threshold);
            // Each registration: {el, lastIndex, lastIx}. Start lastIndex at -1
            // so the FIRST update always queues an entry (the spec's
            // previousThresholdIndex = -1) — i.e. observe() yields one initial
            // callback with the current state, isIntersecting possibly false.
            this.__targets = [];
        }
        observe(el) {
            if (!el) return;
            for (let i = 0; i < this.__targets.length; i++) if (this.__targets[i].el === el) return;
            this.__targets.push({ el: el, lastIndex: -1, lastIx: false });
            if (IO.indexOf(this) < 0) IO.push(this);
            // Report the initial state on a macrotask (the existing settle drain
            // runs it): a target observed at load still gets one callback.
            g.setTimeout(() => trust.updateIntersections(), 0);
        }
        unobserve(el) {
            for (let i = 0; i < this.__targets.length; i++) {
                if (this.__targets[i].el === el) { this.__targets.splice(i, 1); break; }
            }
            if (!this.__targets.length) { const k = IO.indexOf(this); if (k >= 0) IO.splice(k, 1); }
        }
        disconnect() { this.__targets = []; const k = IO.indexOf(this); if (k >= 0) IO.splice(k, 1); }
        takeRecords() { return []; }
    };
    // W3C Intersection Observer §2.3 exposes every entry attribute on
    // IntersectionObserverEntry.prototype. Libraries use those Web IDL members
    // for feature detection; advertising only `isIntersecting` makes a complete
    // native observer look partial and causes them to install polling polyfills.
    // Keep the geometry values immutable, as DOMRectReadOnly values are.
    function ioEntryRect(init) {
        init = init || {};
        const x = init.x === undefined ? 0 : Number(init.x);
        const y = init.y === undefined ? 0 : Number(init.y);
        const width = init.width === undefined ? 0 : Number(init.width);
        const height = init.height === undefined ? 0 : Number(init.height);
        return Object.freeze({
            x, y, width, height,
            top: Math.min(y, y + height),
            right: Math.max(x, x + width),
            bottom: Math.max(y, y + height),
            left: Math.min(x, x + width),
        });
    }
    g.IntersectionObserverEntry = class IntersectionObserverEntry {
        constructor(init) {
            if (init === null || init === undefined)
                throw new TypeError("Failed to construct 'IntersectionObserverEntry': 1 argument required");
            init = Object(init);
            const required = ["time", "rootBounds", "boundingClientRect",
                "intersectionRect", "isIntersecting", "intersectionRatio", "target"];
            for (let i = 0; i < required.length; i++) {
                if (!(required[i] in init))
                    throw new TypeError("Failed to construct 'IntersectionObserverEntry': required member '" + required[i] + "' is undefined");
            }
            if (!init.target || typeof init.target.__id !== "number")
                throw new TypeError("Failed to construct 'IntersectionObserverEntry': target is not an Element");
            Object.defineProperty(this, "__entry", { value: Object.freeze({
                time: Number(init.time),
                rootBounds: init.rootBounds === null ? null : ioEntryRect(init.rootBounds),
                boundingClientRect: ioEntryRect(init.boundingClientRect),
                intersectionRect: ioEntryRect(init.intersectionRect),
                isIntersecting: Boolean(init.isIntersecting),
                intersectionRatio: Number(init.intersectionRatio),
                target: init.target,
            }) });
        }
    };
    Object.defineProperties(g.IntersectionObserverEntry.prototype, {
        time: { get() { return this.__entry.time; }, enumerable: true, configurable: true },
        rootBounds: { get() { return this.__entry.rootBounds; }, enumerable: true, configurable: true },
        boundingClientRect: { get() { return this.__entry.boundingClientRect; }, enumerable: true, configurable: true },
        intersectionRect: { get() { return this.__entry.intersectionRect; }, enumerable: true, configurable: true },
        isIntersecting: { get() { return this.__entry.isIntersecting; }, enumerable: true, configurable: true },
        intersectionRatio: { get() { return this.__entry.intersectionRatio; }, enumerable: true, configurable: true },
        target: { get() { return this.__entry.target; }, enumerable: true, configurable: true },
    });

    // The spec's "update intersection observations" step: for each observer ×
    // target, intersect the target's DOCUMENT-space box with the viewport
    // expanded by rootMargin, then queue an entry ONLY when the threshold index
    // or isIntersecting changed (edge-triggered, per spec — not a flood). Run at
    // every settle (an observe() self-schedules it) and on every scroll.
    trust.updateIntersections = function () {
        if (!IO.length) return 0;
        let delivered = 0;
        const sx = g.scrollX || 0, sy = g.scrollY || 0;
        const vw = g.innerWidth, vh = g.innerHeight;
        const observers = IO.slice();
        for (let oi = 0; oi < observers.length; oi++) {
            const o = observers[oi];
            const rm = o.__rm;
            const mT = rm[0].pct ? (rm[0].v / 100) * vh : rm[0].v;
            const mR = rm[1].pct ? (rm[1].v / 100) * vw : rm[1].v;
            const mB = rm[2].pct ? (rm[2].v / 100) * vh : rm[2].v;
            const mL = rm[3].pct ? (rm[3].v / 100) * vw : rm[3].v;
            // Root intersection rectangle in DOCUMENT coords (the viewport window
            // at the current scroll, dilated by rootMargin).
            const rL = sx - mL, rT = sy - mT, rR = sx + vw + mR, rB = sy + vh + mB;
            const rootBounds = {
                x: -mL, y: -mT, left: -mL, top: -mT, right: vw + mR, bottom: vh + mB,
                width: vw + mR + mL, height: vh + mT + mB,
            };
            const ths = o.thresholds;
            const entries = [];
            const targets = o.__targets.slice();
            for (let ti = 0; ti < targets.length; ti++) {
                const rec = targets[ti];
                let dr = null;
                try { dr = __dom_rect(rec.el.__id); } catch (e) { dr = null; }
                // dr = [left, top, width, height] (document coords), or null only
                // when the target has NO laid-out box (display:none / detached) ⇒
                // honestly NOT intersecting. Every real element — including an
                // empty infinite-scroll sentinel — now gets a real (zero-height)
                // box from the layout measurement pass, so honest intersection
                // works for the IntersectionObserver standard without guesswork.
                const tL = dr ? dr[0] : 0, tT = dr ? dr[1] : 0, tW = dr ? dr[2] : 0, tH = dr ? dr[3] : 0;
                const tR = tL + tW, tB = tT + tH;
                const iL = Math.max(tL, rL), iT = Math.max(tT, rT);
                const iR = Math.min(tR, rR), iB = Math.min(tB, rB);
                // isIntersecting: rects intersect or are edge-adjacent (zero area
                // still counts, per spec).
                const isIx = !!dr && iR >= iL && iB >= iT;
                const iW = Math.max(0, iR - iL), iH = Math.max(0, iB - iT);
                const targetArea = tW * tH;
                let ratio = 0;
                if (isIx) ratio = targetArea > 0 ? Math.min(1, (iW * iH) / targetArea) : 1;
                // thresholdIndex = index of first threshold strictly greater than
                // ratio, else thresholds.length.
                let idx = ths.length;
                for (let k = 0; k < ths.length; k++) { if (ths[k] > ratio) { idx = k; break; } }
                if (idx === rec.lastIndex && isIx === rec.lastIx) continue;
                rec.lastIndex = idx; rec.lastIx = isIx;
                const bcr = {
                    x: tL - sx, y: tT - sy, left: tL - sx, top: tT - sy,
                    right: tR - sx, bottom: tB - sy, width: tW, height: tH,
                };
                const ir = isIx
                    ? { x: iL - sx, y: iT - sy, left: iL - sx, top: iT - sy, right: iR - sx, bottom: iB - sy, width: iW, height: iH }
                    : { x: 0, y: 0, left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0 };
                entries.push(new g.IntersectionObserverEntry({
                    target: rec.el, isIntersecting: isIx, intersectionRatio: ratio,
                    boundingClientRect: bcr, intersectionRect: ir, rootBounds: rootBounds, time: g.performance.now(),
                }));
            }
            if (entries.length) {
                delivered += entries.length;
                try { o.__cb(entries, o); }
                catch (e) { trust.errors.push("IntersectionObserver: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
            }
        }
        return delivered;
    };

    // Apply a viewport scroll (CSS px, document origin): update the scroll
    // position properties, fire the viewport `scroll` event, and re-run the
    // intersection observations. Driven by the actor's PageCmd::Scroll as the
    // user scrolls the laid-out document. A no-op when the position is unchanged.
    trust.setScroll = function (x, y) {
        x = +x || 0; y = +y || 0;
        if (x < 0) x = 0; if (y < 0) y = 0;
        // Clamp to the scrollable range using the engine's OWN measured document
        // height (documentElement.scrollHeight − innerHeight). The app may carry
        // a slightly different viewport/row count than the measure pass, so it
        // anchors a "scrolled to the bottom" to the document height; clamping it
        // here lands the viewport on the true bottom — where an infinite-scroll
        // sentinel sits — instead of a few rows short of it.
        const de = g.document.documentElement;
        if (de) {
            const maxY = Math.max(0, de.scrollHeight - (g.innerHeight || 0));
            if (y > maxY) y = maxY;
            const maxX = Math.max(0, de.scrollWidth - (g.innerWidth || 0));
            if (x > maxX) x = maxX;
        }
        if (x === (g.scrollX || 0) && y === (g.scrollY || 0)) return;
        g.scrollX = x; g.scrollY = y; g.pageXOffset = x; g.pageYOffset = y;
        // Fire `scroll` at the document with forceBubble so window scroll
        // listeners run too (dispatch pushes window onto the path). scroll itself
        // doesn't bubble, but both document and window listeners must fire — the
        // classic `window.addEventListener('scroll', ...)` infinite-scroll idiom
        // (Steam) depends on it.
        try { dispatch(g.document, new Event("scroll"), true); }
        catch (e) { trust.errors.push("scroll handler: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        trust.updateIntersections();
    };

    // The app's browser viewport changed (first displayed after the load-time
    // shell, or the terminal resized): adopt the REAL size so innerWidth/
    // innerHeight — and everything derived from them (documentElement
    // clientWidth/clientHeight, the IntersectionObserver root rectangle, the
    // setScroll clamp) — report the viewport the reader actually has, then
    // fire `resize` at the Window (CSSOM View §4.1: when the viewport is
    // resized, fire resize at the Window) and re-run the intersection
    // observations against the new root. Before this, the engine kept the
    // fetch-time size for the page's whole life — a few rows taller than the
    // browser view (the startup screen's layout), so "the app's bottom" sat
    // short of the engine's and geometry-driven reveals aimed past the reader.
    trust.setViewport = function (w, h) {
        w = +w || 0; h = +h || 0;
        if (w <= 0 || h <= 0) return;
        if (w === g.innerWidth && h === g.innerHeight) return;
        g.innerWidth = w; g.innerHeight = h;
        try { dispatch(g, new Event("resize"), false); }
        catch (e) { trust.errors.push("resize handler: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        // CSSOM View §13.1 applies independently to every Document viewport.
        // Dispatch in frame scope so the shared realm restores that frame's
        // document/inner dimensions and filters Window listeners by their
        // registration browsing context.
        try { fireChangedFrameViewportResizes(); }
        catch (e) { trust.errors.push("frame resize handler: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        // The viewport changed, so every element's box may have — deliver
        // ResizeObserver (a responsive grid re-columns) before intersections.
        trust.updateResizes();
        trust.updateIntersections();
    };

    // The terminal wheel scrolled an inner-scroll region: the actor has already
    // written the element's new scrollTop (PageCmd::SetScroll → set_scroll_pos),
    // so fire the element's `scroll` event (CSSOM View — it does NOT bubble) and
    // re-run intersections. A page that conditionally pins to the bottom learns
    // here that the user scrolled up and stops following.
    trust.fireElementScroll = function (node) {
        const el = wrap(node);
        if (!el) return;
        try { dispatch(el, new Event("scroll"), false); }
        catch (e) { trust.errors.push("element scroll handler: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        trust.updateIntersections();
    };

    // Does the page have scroll-driven work (so the actor keeps it live at rest
    // to receive PageCmd::Scroll / SetScroll)? An IntersectionObserver, a
    // window/document `scroll` listener, or a per-element `scroll` listener (an
    // inner-scroll region's un-pin handler). Peeks without creating entries.
    trust.hasScrollWork = function () {
        if (IO.length) return true;
        if (g.__elScroll) return true;
        const wm = LS.get(g);
        if (wm) { const l = wm.get("scroll"); if (l && l.length) return true; }
        const dm = LS.get(g.document);
        if (dm) { const l = dm.get("scroll"); if (l && l.length) return true; }
        return typeof g.onscroll === "function";
    };
    g.ResizeObserver = class {
        constructor(cb) { this.__cb = cb; this.__targets = []; }
        observe(el) {
            if (!el) return;
            for (let i = 0; i < this.__targets.length; i++) if (this.__targets[i].el === el) return;
            // lastW/lastH = -1 so the FIRST delivery always fires (spec: observe
            // queues an initial observation with the current size).
            this.__targets.push({ el: el, lastW: -1, lastH: -1 });
            if (RO.indexOf(this) < 0) RO.push(this);
            // Report the initial size on a later task. The live event loop runs
            // that task after the current lifecycle/author task; one-shot
            // snapshots drain it while producing their eventual state.
            g.setTimeout(() => trust.updateResizes(), 0);
        }
        unobserve(el) {
            for (let i = 0; i < this.__targets.length; i++) {
                if (this.__targets[i].el === el) { this.__targets.splice(i, 1); break; }
            }
            if (!this.__targets.length) { const k = RO.indexOf(this); if (k >= 0) RO.splice(k, 1); }
        }
        disconnect() { this.__targets = []; const k = RO.indexOf(this); if (k >= 0) RO.splice(k, 1); }
    };
    // The spec's ResizeObserver delivery ("gather active observations" → "broadcast"):
    // for each observer × target, measure the target's CURRENT border box and fire
    // the callback ONLY when its size changed since the last delivery (edge-
    // triggered — not a flood). Run in the "update the rendering" step at every
    // settle/dispatch/viewport-resize (`run_layout_observers`), so a component that
    // sizes itself off its container gets the corrected size as the layout evolves.
    trust.updateResizes = function () {
        if (!RO.length) return 0;
        let delivered = 0;
        const observers = RO.slice();
        for (let oi = 0; oi < observers.length; oi++) {
            const o = observers[oi];
            const entries = [];
            const targets = o.__targets.slice();
            for (let ti = 0; ti < targets.length; ti++) {
                const rec = targets[ti];
                let r = null;
                try { r = (rec.el && rec.el.getBoundingClientRect) ? rec.el.getBoundingClientRect() : null; } catch (e) { r = null; }
                const w = r ? r.width : 0, h = r ? r.height : 0;
                if (w === rec.lastW && h === rec.lastH) continue;
                rec.lastW = w; rec.lastH = h;
                const cr = r || { x: 0, y: 0, left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0 };
                const box = [{ inlineSize: w, blockSize: h }];
                entries.push({ target: rec.el, contentRect: cr, borderBoxSize: box, contentBoxSize: box, devicePixelContentBoxSize: box });
            }
            if (entries.length) {
                delivered += entries.length;
                try { o.__cb(entries, o); }
                catch (e) { trust.errors.push("ResizeObserver: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
            }
        }
        return delivered;
    };
    // W3C Cooperative Scheduling of Background Tasks. Idle work has its own
    // task source and two callback lists; it is not a zero-delay timer. The
    // host starts an idle period only when no ordinary task is runnable and
    // supplies its real end time (at most 50 ms, or the next scheduled task).
    const idleCallbacks = { pending: [], runnable: [], seq: 0 };
    const idleTasks = [];
    const idleDeadlineToken = {};
    const idleDeadlineState = new WeakMap();
    class IdleDeadline {
        constructor(token, deadline, didTimeout) {
            if (token !== idleDeadlineToken) throw new TypeError("Illegal constructor");
            idleDeadlineState.set(this, { deadline, didTimeout });
        }
        timeRemaining() {
            const state = idleDeadlineState.get(this);
            if (!state) throw new TypeError("Illegal invocation");
            return Math.max(0, state.deadline - currentTime());
        }
        get didTimeout() {
            const state = idleDeadlineState.get(this);
            if (!state) throw new TypeError("Illegal invocation");
            return state.didTimeout;
        }
        get [Symbol.toStringTag]() { return "IdleDeadline"; }
    }
    g.IdleDeadline = IdleDeadline;
    function takeIdleCallback(handle) {
        for (const list of [idleCallbacks.pending, idleCallbacks.runnable]) {
            const index = list.findIndex((entry) => entry.handle === handle);
            if (index >= 0) return list.splice(index, 1)[0];
        }
        return null;
    }
    g.requestIdleCallback = function (callback, options) {
        if (typeof callback !== "function") throw new TypeError("requestIdleCallback callback must be callable");
        const handle = ++idleCallbacks.seq;
        const entry = {
            handle,
            callback,
            frame: trust.__activeFrame || null,
            timeoutTimer: null,
        };
        idleCallbacks.pending.push(entry);
        if (options !== undefined && options !== null && "timeout" in Object(options)) {
            const number = Number(options.timeout);
            const timeout = Number.isFinite(number) ? (Math.trunc(number) >>> 0) : 0;
            if (timeout > 0) {
                entry.timeoutTimer = g.setTimeout(() => {
                    entry.timeoutTimer = null;
                    idleTasks.push({ kind: "timeout", handle });
                }, timeout);
            }
        }
        return handle;
    };
    g.cancelIdleCallback = function (handle) {
        handle = Number(handle) >>> 0;
        const entry = takeIdleCallback(handle);
        if (entry && entry.timeoutTimer !== null) g.clearTimeout(entry.timeoutTimer);
    };
    trust.hasIdleRequest = function () {
        return idleCallbacks.pending.length > 0 || idleCallbacks.runnable.length > 0;
    };
    trust.startIdlePeriod = function (deadline) {
        deadline = Math.min(currentTime() + 50, Math.max(currentTime(), Number(deadline)));
        idleCallbacks.runnable.push(...idleCallbacks.pending);
        idleCallbacks.pending.length = 0;
        if (idleCallbacks.runnable.length) idleTasks.push({ kind: "period", deadline });
        return idleCallbacks.runnable.length > 0;
    };
    trust.runIdleTask = function () {
        if (!idleTasks.length) return false;
        const task = idleTasks.shift();
        let entry = null;
        let deadline = currentTime();
        let didTimeout = false;
        if (task.kind === "timeout") {
            entry = takeIdleCallback(task.handle);
            didTimeout = true;
        } else if (currentTime() < task.deadline && idleCallbacks.runnable.length) {
            entry = idleCallbacks.runnable.shift();
            deadline = task.deadline;
        }
        if (entry) {
            if (entry.timeoutTimer !== null) g.clearTimeout(entry.timeoutTimer);
            const deadlineArg = new IdleDeadline(idleDeadlineToken, deadline, didTimeout);
            try { runInFrame(entry.frame, () => entry.callback(deadlineArg)); }
            catch (e) { trust.errors.push("idle callback: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        }
        if (task.kind === "period" && currentTime() < task.deadline && idleCallbacks.runnable.length) {
            idleTasks.push({ kind: "period", deadline: task.deadline });
        }
        return true;
    };

    /*__CRYPTO_BEGIN__*/
    // --- crypto: getRandomValues + randomUUID + subtle.digest ---
    // No CSPRNG here (text browser, no entropy source): random values
    // are Math.random-derived — fine for request ids / cache keys, NOT
    // real cryptography. subtle.digest IS a true SHA so libraries that
    // hash before they fetch work (archive.org's collection search gates
    // its tile fetch on a SHA-1 request-uid — without this the grid
    // stays empty). Only digest is implemented; the rest of SubtleCrypto
    // stays an honest remainder.
    const __cryptoBytes = (d) => {
        if (d instanceof ArrayBuffer) return new Uint8Array(d.slice(0));
        if (ArrayBuffer.isView(d)) return new Uint8Array(d.buffer.slice(d.byteOffset, d.byteOffset + d.byteLength));
        return new Uint8Array(0);
    };
    // Keep the WebCrypto BufferSource as an ArrayBuffer across the host seam.
    // The interpreted fallback algorithms still operate on Uint8Array values,
    // but SHA-256 is the hot path for proof-of-work and must not encode/decode
    // every input and digest through a JS string.
    const __nativeSha256 = (data) => __crypto_sha256_digest(data);
    const __shaPad = (bytes) => {
        const ml = bytes.length * 8;
        const total = (bytes.length + 1 + 8 + 63) & ~63;
        const m = new Uint8Array(total);
        m.set(bytes); m[bytes.length] = 0x80;
        const dv = new DataView(m.buffer);
        dv.setUint32(total - 8, Math.floor(ml / 0x100000000));
        dv.setUint32(total - 4, ml >>> 0);
        return { m, dv, total };
    };
    function __sha1(bytes) {
        const { dv, total } = __shaPad(bytes);
        let h0 = 0x67452301, h1 = 0xEFCDAB89, h2 = 0x98BADCFE, h3 = 0x10325476, h4 = 0xC3D2E1F0;
        const w = new Uint32Array(80);
        for (let off = 0; off < total; off += 64) {
            for (let i = 0; i < 16; i++) w[i] = dv.getUint32(off + i * 4);
            for (let i = 16; i < 80; i++) { const v = w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]; w[i] = (v << 1) | (v >>> 31); }
            let a = h0, b = h1, c = h2, d = h3, e = h4;
            for (let i = 0; i < 80; i++) {
                let f, k;
                if (i < 20) { f = (b & c) | (~b & d); k = 0x5A827999; }
                else if (i < 40) { f = b ^ c ^ d; k = 0x6ED9EBA1; }
                else if (i < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8F1BBCDC; }
                else { f = b ^ c ^ d; k = 0xCA62C1D6; }
                const t = (((a << 5) | (a >>> 27)) + f + e + k + w[i]) >>> 0;
                e = d; d = c; c = (b << 30) | (b >>> 2); b = a; a = t;
            }
            h0 = (h0 + a) >>> 0; h1 = (h1 + b) >>> 0; h2 = (h2 + c) >>> 0; h3 = (h3 + d) >>> 0; h4 = (h4 + e) >>> 0;
        }
        const out = new Uint8Array(20), o = new DataView(out.buffer);
        o.setUint32(0, h0); o.setUint32(4, h1); o.setUint32(8, h2); o.setUint32(12, h3); o.setUint32(16, h4);
        return out;
    }
    const __SHA256_K = [0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2];
    function __sha256(bytes) {
        const { dv, total } = __shaPad(bytes);
        const h = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
        const w = new Uint32Array(64);
        const rotr = (x, n) => (x >>> n) | (x << (32 - n));
        for (let off = 0; off < total; off += 64) {
            for (let i = 0; i < 16; i++) w[i] = dv.getUint32(off + i * 4);
            for (let i = 16; i < 64; i++) {
                const s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >>> 3);
                const s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >>> 10);
                w[i] = (w[i - 16] + s0 + w[i - 7] + s1) >>> 0;
            }
            let a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g2 = h[6], hh = h[7];
            for (let i = 0; i < 64; i++) {
                const S1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25), ch = (e & f) ^ (~e & g2);
                const t1 = (hh + S1 + ch + __SHA256_K[i] + w[i]) >>> 0;
                const S0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22), maj = (a & b) ^ (a & c) ^ (b & c);
                const t2 = (S0 + maj) >>> 0;
                hh = g2; g2 = f; f = e; e = (d + t1) >>> 0; d = c; c = b; b = a; a = (t1 + t2) >>> 0;
            }
            h[0] = (h[0] + a) >>> 0; h[1] = (h[1] + b) >>> 0; h[2] = (h[2] + c) >>> 0; h[3] = (h[3] + d) >>> 0;
            h[4] = (h[4] + e) >>> 0; h[5] = (h[5] + f) >>> 0; h[6] = (h[6] + g2) >>> 0; h[7] = (h[7] + hh) >>> 0;
        }
        const out = new Uint8Array(32), o = new DataView(out.buffer);
        for (let i = 0; i < 8; i++) o.setUint32(i * 4, h[i]);
        return out;
    }
    // SHA-384/512 need 64-bit words JS lacks natively, so the core runs on
    // BigInt (masked to 64 bits). digest() inputs are small (request uids,
    // token hashes), so BigInt's cost is irrelevant; correctness is what
    // matters. Block size 128B, 128-bit big-endian length field. ChatGPT's
    // boot hashes with SHA-512; without it the reject aborted its init.
    const __M64 = (1n << 64n) - 1n;
    const __SHA512_K = [
        0x428a2f98d728ae22n, 0x7137449123ef65cdn, 0xb5c0fbcfec4d3b2fn, 0xe9b5dba58189dbbcn,
        0x3956c25bf348b538n, 0x59f111f1b605d019n, 0x923f82a4af194f9bn, 0xab1c5ed5da6d8118n,
        0xd807aa98a3030242n, 0x12835b0145706fben, 0x243185be4ee4b28cn, 0x550c7dc3d5ffb4e2n,
        0x72be5d74f27b896fn, 0x80deb1fe3b1696b1n, 0x9bdc06a725c71235n, 0xc19bf174cf692694n,
        0xe49b69c19ef14ad2n, 0xefbe4786384f25e3n, 0x0fc19dc68b8cd5b5n, 0x240ca1cc77ac9c65n,
        0x2de92c6f592b0275n, 0x4a7484aa6ea6e483n, 0x5cb0a9dcbd41fbd4n, 0x76f988da831153b5n,
        0x983e5152ee66dfabn, 0xa831c66d2db43210n, 0xb00327c898fb213fn, 0xbf597fc7beef0ee4n,
        0xc6e00bf33da88fc2n, 0xd5a79147930aa725n, 0x06ca6351e003826fn, 0x142929670a0e6e70n,
        0x27b70a8546d22ffcn, 0x2e1b21385c26c926n, 0x4d2c6dfc5ac42aedn, 0x53380d139d95b3dfn,
        0x650a73548baf63den, 0x766a0abb3c77b2a8n, 0x81c2c92e47edaee6n, 0x92722c851482353bn,
        0xa2bfe8a14cf10364n, 0xa81a664bbc423001n, 0xc24b8b70d0f89791n, 0xc76c51a30654be30n,
        0xd192e819d6ef5218n, 0xd69906245565a910n, 0xf40e35855771202an, 0x106aa07032bbd1b8n,
        0x19a4c116b8d2d0c8n, 0x1e376c085141ab53n, 0x2748774cdf8eeb99n, 0x34b0bcb5e19b48a8n,
        0x391c0cb3c5c95a63n, 0x4ed8aa4ae3418acbn, 0x5b9cca4f7763e373n, 0x682e6ff3d6b2b8a3n,
        0x748f82ee5defb2fcn, 0x78a5636f43172f60n, 0x84c87814a1f0ab72n, 0x8cc702081a6439ecn,
        0x90befffa23631e28n, 0xa4506cebde82bde9n, 0xbef9a3f7b2c67915n, 0xc67178f2e372532bn,
        0xca273eceea26619cn, 0xd186b8c721c0c207n, 0xeada7dd6cde0eb1en, 0xf57d4f7fee6ed178n,
        0x06f067aa72176fban, 0x0a637dc5a2c898a6n, 0x113f9804bef90daen, 0x1b710b35131c471bn,
        0x28db77f523047d84n, 0x32caab7b40c72493n, 0x3c9ebe0a15c9bebcn, 0x431d67c49c100d4cn,
        0x4cc5d4becb3e42b6n, 0x597f299cfc657e2an, 0x5fcb6fab3ad6faecn, 0x6c44198c4a475817n,
    ];
    function __sha512core(bytes, h) {
        const bl = bytes.length;
        const total = (bl + 1 + 16 + 127) & ~127;
        const m = new Uint8Array(total);
        m.set(bytes); m[bl] = 0x80;
        const dv = new DataView(m.buffer);
        const ml = BigInt(bl) * 8n; // fits 64 bits for any realistic input; high half stays 0
        dv.setUint32(total - 8, Number((ml >> 32n) & 0xffffffffn));
        dv.setUint32(total - 4, Number(ml & 0xffffffffn));
        const w = new Array(80);
        const rotr = (x, n) => ((x >> n) | (x << (64n - n))) & __M64;
        for (let off = 0; off < total; off += 128) {
            for (let i = 0; i < 16; i++) {
                w[i] = (BigInt(dv.getUint32(off + i * 8)) << 32n) | BigInt(dv.getUint32(off + i * 8 + 4));
            }
            for (let i = 16; i < 80; i++) {
                const s0 = rotr(w[i - 15], 1n) ^ rotr(w[i - 15], 8n) ^ (w[i - 15] >> 7n);
                const s1 = rotr(w[i - 2], 19n) ^ rotr(w[i - 2], 61n) ^ (w[i - 2] >> 6n);
                w[i] = (w[i - 16] + s0 + w[i - 7] + s1) & __M64;
            }
            let a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g2 = h[6], hh = h[7];
            for (let i = 0; i < 80; i++) {
                const S1 = rotr(e, 14n) ^ rotr(e, 18n) ^ rotr(e, 41n), ch = (e & f) ^ ((~e & __M64) & g2);
                const t1 = (hh + S1 + ch + __SHA512_K[i] + w[i]) & __M64;
                const S0 = rotr(a, 28n) ^ rotr(a, 34n) ^ rotr(a, 39n), maj = (a & b) ^ (a & c) ^ (b & c);
                const t2 = (S0 + maj) & __M64;
                hh = g2; g2 = f; f = e; e = (d + t1) & __M64; d = c; c = b; b = a; a = (t1 + t2) & __M64;
            }
            h[0] = (h[0] + a) & __M64; h[1] = (h[1] + b) & __M64; h[2] = (h[2] + c) & __M64; h[3] = (h[3] + d) & __M64;
            h[4] = (h[4] + e) & __M64; h[5] = (h[5] + f) & __M64; h[6] = (h[6] + g2) & __M64; h[7] = (h[7] + hh) & __M64;
        }
        return h;
    }
    function __sha512(bytes) {
        const h = __sha512core(bytes, [
            0x6a09e667f3bcc908n, 0xbb67ae8584caa73bn, 0x3c6ef372fe94f82bn, 0xa54ff53a5f1d36f1n,
            0x510e527fade682d1n, 0x9b05688c2b3e6c1fn, 0x1f83d9abfb41bd6bn, 0x5be0cd19137e2179n]);
        const out = new Uint8Array(64), o = new DataView(out.buffer);
        for (let i = 0; i < 8; i++) { o.setUint32(i * 8, Number((h[i] >> 32n) & 0xffffffffn)); o.setUint32(i * 8 + 4, Number(h[i] & 0xffffffffn)); }
        return out;
    }
    function __sha384(bytes) {
        const h = __sha512core(bytes, [
            0xcbbb9d5dc1059ed8n, 0x629a292a367cd507n, 0x9159015a3070dd17n, 0x152fecd8f70e5939n,
            0x67332667ffc00b31n, 0x8eb44a8768581511n, 0xdb0c2e0d64f98fa7n, 0x47b5481dbefa4fa4n]);
        const out = new Uint8Array(48), o = new DataView(out.buffer); // 384 bits = first 6 words
        for (let i = 0; i < 6; i++) { o.setUint32(i * 8, Number((h[i] >> 32n) & 0xffffffffn)); o.setUint32(i * 8 + 4, Number(h[i] & 0xffffffffn)); }
        return out;
    }
    g.crypto = {
        getRandomValues(a) {
            if (a && a.length !== undefined) for (let i = 0; i < a.length; i++) a[i] = Math.floor(Math.random() * 0x100000000);
            return a;
        },
        randomUUID() {
            return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (ch) => {
                const r = Math.random() * 16 | 0;
                return (ch === "x" ? r : (r & 0x3 | 0x8)).toString(16);
            });
        },
        subtle: {
            digest(algo, data) {
                const name = (typeof algo === "string" ? algo : (algo && algo.name) || "").toUpperCase();
                if (name === "SHA-256") return __nativeSha256(data);
                const bytes = __cryptoBytes(data);
                if (name === "SHA-1") return Promise.resolve(__sha1(bytes).buffer);
                if (name === "SHA-384") return Promise.resolve(__sha384(bytes).buffer);
                if (name === "SHA-512") return Promise.resolve(__sha512(bytes).buffer);
                return Promise.reject(new Error("Unsupported digest algorithm: " + name));
            },
        },
    };
    /*__CRYPTO_END__*/

    // DOMException — a real constructor (extends Error). core-js's
    // DOMException polyfill does `getBuiltIn("DOMException").prototype`
    // during feature detection; with it undefined that throws ToObject
    // ("cannot convert undefined to object") UNCAUGHT, which tore down
    // danbooru's whole init and stripped its server-rendered post grid.
    const __DE_CODES = {
        IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
        InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
        NotSupportedError: 9, InUseAttributeError: 10, InvalidStateError: 11,
        SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14,
        InvalidAccessError: 15, SecurityError: 18, NetworkError: 19, AbortError: 20,
        URLMismatchError: 21, QuotaExceededError: 22, TimeoutError: 23,
        InvalidNodeTypeError: 24, DataCloneError: 25,
    };
    class DOMException extends Error {
        constructor(message, name) {
            super(message === undefined ? "" : String(message));
            this.name = name === undefined ? "Error" : String(name);
            this.message = message === undefined ? "" : String(message);
            this.code = __DE_CODES[this.name] || 0;
        }
        get [Symbol.toStringTag]() { return "DOMException"; }
    }
    {
        const legacy = {
            INDEX_SIZE_ERR: 1, DOMSTRING_SIZE_ERR: 2, HIERARCHY_REQUEST_ERR: 3,
            WRONG_DOCUMENT_ERR: 4, INVALID_CHARACTER_ERR: 5, NO_DATA_ALLOWED_ERR: 6,
            NO_MODIFICATION_ALLOWED_ERR: 7, NOT_FOUND_ERR: 8, NOT_SUPPORTED_ERR: 9,
            INUSE_ATTRIBUTE_ERR: 10, INVALID_STATE_ERR: 11, SYNTAX_ERR: 12,
            INVALID_MODIFICATION_ERR: 13, NAMESPACE_ERR: 14, INVALID_ACCESS_ERR: 15,
            VALIDATION_ERR: 16, TYPE_MISMATCH_ERR: 17, SECURITY_ERR: 18, NETWORK_ERR: 19,
            ABORT_ERR: 20, URL_MISMATCH_ERR: 21, QUOTA_EXCEEDED_ERR: 22, TIMEOUT_ERR: 23,
            INVALID_NODE_TYPE_ERR: 24, DATA_CLONE_ERR: 25,
        };
        for (const k in legacy) { DOMException[k] = legacy[k]; DOMException.prototype[k] = legacy[k]; }
    }
    g.DOMException = DOMException;

    // MediaError — the interface a media element's `.error` exposes. Ad/video
    // SDKs (tsyndicate, players) reference the global during feature
    // detection; without it a bare `MediaError` reference throws
    // ReferenceError and aborts their init. Constants live on both the
    // constructor and the prototype, per the IDL.
    {
        const codes = {
            MEDIA_ERR_ABORTED: 1, MEDIA_ERR_NETWORK: 2,
            MEDIA_ERR_DECODE: 3, MEDIA_ERR_SRC_NOT_SUPPORTED: 4,
        };
        class MediaError {
            constructor(code, message) {
                this.code = code === undefined ? 0 : code | 0;
                this.message = message === undefined ? "" : String(message);
            }
            get [Symbol.toStringTag]() { return "MediaError"; }
        }
        for (const k in codes) { MediaError[k] = codes[k]; MediaError.prototype[k] = codes[k]; }
        g.MediaError = MediaError;
    }

    // Bound wrappers (installEventHandlers calls them unbound), routing
    // through the shared options-aware registry.
    g.addEventListener = (t, f, o) => { addL(g, t, f, o); };
    g.removeEventListener = (t, f, o) => { removeL(g, t, f, o); };
    g.dispatchEvent = (ev) => dispatch(g, ev, false);
    // `window.postMessage(message[, targetOrigin][, transfer])` (HTML web
    // messaging). With no foreign frames the only valid target is ourselves, so
    // we deliver `message` to our own window ASYNCHRONOUSLY (a task) as a
    // `MessageEvent` carrying data/origin/source — exactly the observable spec
    // behaviour a single-window page sees. `targetOrigin` is accepted and the
    // transferable MessagePort list is preserved; a structured clone is still
    // approximated as identity. Pages post to themselves to
    // defer work or hand off across a microtask boundary (Steam's focus-restore
    // handshake posts `"FocusRestoreReady"` and listens for it); a missing
    // `window.postMessage` was an uncaught TypeError in that timer.
    g.postMessage = function (message, targetOrigin, transfer) {
        const targetFrame = trust.__activeFrame || null;
        postMessageToFrame(targetFrame, message, g, transferPorts(targetOrigin, transfer),
            g.location.origin);
    };
    // `on<event>` IDL attributes (window.onload = fn). Standard semantics:
    // the attribute is backed by an event listener, so the existing
    // dispatch loop fires it — get returns the handler, set swaps the
    // backing listener. Defining them as properties of the global object
    // is ALSO what lets a module's bare `onload = fn` resolve (Boa's
    // module scope assigns through the global object; without the property
    // it throws "cannot assign to uninitialized global property"). css3test
    // runs its entire suite from `onload`.
    function installEventHandlers(obj, add, remove, types) {
        for (const type of types) {
            let current = null;
            Object.defineProperty(obj, "on" + type, {
                configurable: true,
                enumerable: true,
                get() { return current; },
                set(v) {
                    if (current) remove(type, current);
                    current = typeof v === "function" ? v : null;
                    if (current) add(type, current);
                },
            });
        }
    }
    installEventHandlers(g, g.addEventListener, g.removeEventListener, [
        "load", "unload", "beforeunload", "pageshow", "pagehide",
        "resize", "scroll", "scrollend", "hashchange", "popstate", "message",
        "error", "online", "offline", "focus", "blur", "languagechange",
    ]);
    // GlobalEventHandlers on* IDL attributes on Document and Element (they
    // share Node.prototype). The spec backs each by add/removeEventListener;
    // `this`-relative so a setter registers on the node itself. Two reasons
    // this matters broadly: (1) feature detection — libraries probe event
    // support via `('on'+name) in document` / `in element` (React's change
    // plugin gates the whole `input`-event path on `'oninput' in document`,
    // and without it falls back to a legacy keyup/selectionchange polyfill
    // that never sees our input dispatch → controlled inputs go dead); (2)
    // `el.onclick = fn` assignment works as a real listener.
    // `on<event>` storage is NAMESPACED `__trustOn` (not `__on`): D3's
    // `selection.on()` stores its listener descriptors in `node.__on` (and YT
    // bundles D3), so a shared `__on` would cross our handler map with D3's.
    function installHandlerProps(proto, types) {
        for (const type of types) {
            Object.defineProperty(proto, "on" + type, {
                configurable: true,
                enumerable: false,
                get() { return (this.__trustOn && this.__trustOn[type]) || null; },
                set(v) {
                    if (!this.__trustOn) this.__trustOn = {};
                    const prev = this.__trustOn[type];
                    if (prev) this.removeEventListener(type, prev);
                    const fn = typeof v === "function" ? v : null;
                    this.__trustOn[type] = fn;
                    if (fn) this.addEventListener(type, fn);
                },
            });
        }
    }
    installHandlerProps(Node.prototype, [
        "click", "dblclick", "auxclick", "contextmenu",
        "mousedown", "mouseup", "mousemove", "mouseover", "mouseout",
        "mouseenter", "mouseleave", "wheel",
        "keydown", "keyup", "keypress",
        "input", "beforeinput", "change", "submit", "reset", "invalid",
        "focus", "blur", "focusin", "focusout",
        "select", "selectionchange",
        "scroll", "scrollend", "load", "error", "abort", "loadstart", "loadend", "progress",
        "drag", "dragstart", "dragend", "dragenter", "dragleave", "dragover", "drop",
        "pointerdown", "pointerup", "pointermove", "pointerover", "pointerout",
        "pointerenter", "pointerleave", "pointercancel", "gotpointercapture", "lostpointercapture",
        "touchstart", "touchend", "touchmove", "touchcancel",
        "animationstart", "animationend", "animationiteration",
        "transitionstart", "transitionend", "transitioncancel",
        "copy", "cut", "paste", "compositionstart", "compositionupdate", "compositionend",
        "play", "pause", "ended", "canplay", "canplaythrough", "durationchange",
        "timeupdate", "volumechange", "waiting", "seeked", "seeking",
        "toggle", "beforetoggle", "cancel", "close",
    ]);
    // Performance + the Performance Timeline API. We keep no real timing
    // buffer, so the getEntries* trio returns empty arrays — but they MUST
    // exist: GitHub's React Router calls `performance.getEntriesByName(url,
    // "resource")` during render to detect a prefetch, and a missing method
    // throws a TypeError its top-level error boundary catches ("Unable to load
    // page"). All no-ops/empty are safe (no entry found -> the caller skips
    // the optimization).
    // `timeOrigin` and the (deprecated but ubiquitous) `PerformanceTiming`
    // fields must be REAL epoch-ms timestamps, not 0/undefined: RUM/latency
    // libraries compute durations off `performance.timing.navigationStart`
    // (e.g. Twitch's latency tracker: `startTimestamp - navigationStart`), and
    // an undefined `navigationStart` yields `NaN` durations that stall their
    // page-load state machine. `now()` is virtual (ms since load); `timeOrigin
    // + now()` ≈ wall-clock epoch ms, so the origin is the load-start epoch.
    const __perfOrigin = Date.now();
    const __perfTiming = {
        navigationStart: __perfOrigin, fetchStart: __perfOrigin,
        domainLookupStart: __perfOrigin, domainLookupEnd: __perfOrigin,
        connectStart: __perfOrigin, connectEnd: __perfOrigin,
        secureConnectionStart: __perfOrigin, requestStart: __perfOrigin,
        responseStart: __perfOrigin, responseEnd: __perfOrigin,
        domLoading: __perfOrigin, domInteractive: __perfOrigin,
        domContentLoadedEventStart: __perfOrigin, domContentLoadedEventEnd: __perfOrigin,
        domComplete: __perfOrigin, loadEventStart: __perfOrigin, loadEventEnd: __perfOrigin,
        unloadEventStart: 0, unloadEventEnd: 0, redirectStart: 0, redirectEnd: 0,
        toJSON() { return Object.assign({}, this); },
    };
    g.performance = {
        now: () => 0,
        timeOrigin: __perfOrigin,
        timing: __perfTiming, navigation: { type: 0, redirectCount: 0 }, memory: {},
        mark() { return undefined; },
        measure() { return undefined; },
        clearMarks() {}, clearMeasures() {}, clearResourceTimings() {},
        setResourceTimingBufferSize() {},
        getEntries: () => [],
        getEntriesByName: () => [],
        getEntriesByType: () => [],
        addEventListener() {}, removeEventListener() {}, dispatchEvent() { return true; },
        toJSON() { return {}; },
    };
    // PerformanceObserver: observing never delivers entries (we keep no
    // buffer), but the constructor + methods must exist (libraries probe it).
    if (typeof g.PerformanceObserver === "undefined") {
        g.PerformanceObserver = class PerformanceObserver {
            constructor(cb) { this.__cb = cb; }
            observe() {}
            disconnect() {}
            takeRecords() { return []; }
        };
        g.PerformanceObserver.supportedEntryTypes = [];
    }

    // RAM-only, session-lifetime storage: origin-bucketed maps shared
    // across pages, dead with the process, never disk.
    function makeStorage(kind) {
        return {
            getItem: (k) => __storage_get(kind, String(k)),
            setItem: (k, v) => { __storage_set(kind, String(k), String(v)); },
            removeItem: (k) => { __storage_remove(kind, String(k)); },
            clear: () => { __storage_clear(kind); },
            key: (i) => __storage_key(kind, Number(i)),
            get length() { return __storage_len(kind); },
        };
    }
    g.localStorage = makeStorage("local");
    g.sessionStorage = makeStorage("session");

    // --- timers on virtual time, driven by the Rust settle loop ---
    // (`timers` itself is declared at the top of the prelude — the Event
    // class stamps `timeStamp` from it — with the `__clockSync` anchor that
    // mirrors every `timers.now` advance into the Rust-side Date clock.)
    // HTML Timers "timer initialization steps": TimerHandler is the Web IDL
    // union `(DOMString or Function)`. A non-callable value is converted to a
    // string when the API is invoked, then compiled as a classic script when
    // the timer task runs (equivalent to an indirect/global eval). This legacy
    // form remains normative and powers old-web cursor trails such as
    // `setTimeout("tick()", 40)`. Preserve ToString's Symbol exception rather
    // than using String(Symbol), whose special constructor behavior differs.
    function prepareTimerHandler(handler) {
        if (typeof handler === "function") return handler;
        if (typeof handler === "symbol") throw new TypeError("Cannot convert a Symbol value to a string");
        const source = String(handler);
        return function () { return (0, eval)(source); };
    }
    // Web IDL `long` conversion followed by HTML's negative-timeout clamp.
    function timerTimeout(value) {
        let number = Number(value);
        if (!Number.isFinite(number) || number === 0) return 0;
        number = Math.trunc(number);
        number = ((number % 4294967296) + 4294967296) % 4294967296;
        if (number >= 2147483648) number -= 4294967296;
        return Math.max(0, number);
    }
    function timerDelay(timeout, parentNesting) {
        return parentNesting > 5 && timeout < 4 ? 4 : timeout;
    }
    function addTimer(handler, timeout, args, repeat, previousId, parentNesting) {
        const id = previousId === undefined ? timers.seq++ : previousId;
        const nesting = parentNesting + 1;
        const wait = timerDelay(timeout, parentNesting);
        timers.ids.add(id);
        timers.q.push({
            id,
            at: currentTime() + wait,
            fn: handler,
            every: repeat ? timeout : null,
            args,
            nesting,
            wait,
            frame: trust.__activeFrame || null,
        });
        return id;
    }
    // Function handlers receive the trailing arguments. String handlers ignore
    // them when their prepared classic script runs, as the standard requires.
    g.setTimeout = function (fn, d) {
        fn = prepareTimerHandler(fn);
        const args = Array.prototype.slice.call(arguments, 2);
        return addTimer(fn, timerTimeout(d), args, false, undefined, timers.activeNesting);
    };
    // Fetch/XHR response processing and HTML web messaging use runnable task
    // sources, not the timer task source. Keep networking and DOM manipulation
    // separate from the immediately-enabled message FIFO so source selection
    // remains explicit; MessagePort's initially-disabled per-port queue is
    // handled beside its implementation.
    const networkTasks = [];
    const __queue_network_task = function (fn, frame) {
        if (typeof fn === "function") {
            networkTasks.push({ fn: fn, frame: frame === undefined ? (trust.__activeFrame || null) : frame });
        }
    };
    const domTasks = [];
    const __queue_dom_task = function (fn, frame) {
        if (typeof fn === "function") {
            domTasks.push({ fn: fn, frame: frame === undefined ? (trust.__activeFrame || null) : frame });
        }
    };
    const messageTasks = [];
    const __queue_message_task = function (fn) {
        if (typeof fn === "function") {
            messageTasks.push({ fn: fn, frame: trust.__activeFrame || null });
        }
    };
    g.setInterval = function (fn, d) {
        fn = prepareTimerHandler(fn);
        const args = Array.prototype.slice.call(arguments, 2);
        return addTimer(fn, timerTimeout(d), args, true, undefined, timers.activeNesting);
    };
    g.clearTimeout = g.clearInterval = function (id) {
        id = Number(id) | 0;
        timers.ids.delete(id);
        timers.q = timers.q.filter((timer) => timer.id !== id);
    };
    function runTimerTask(task) {
        const previousNesting = timers.activeNesting;
        timers.activeNesting = task.nesting;
        try {
            runInFrame(task.frame, () => task.fn.apply(g, task.args || []));
        } catch (e) {
            trust.errors.push("timer: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : ""));
        } finally {
            timers.activeNesting = previousNesting;
        }
        if (task.every !== null && timers.ids.has(task.id)) {
            addTimer(task.fn, task.every, task.args, true, task.id, task.nesting);
        } else {
            timers.ids.delete(task.id);
        }
    }
    g.requestAnimationFrame = function (callback) {
        if (typeof callback !== "function") throw new TypeError("requestAnimationFrame callback must be callable");
        const id = ++animationFrames.seq;
        animationFrames.q.push({ id, callback, frame: trust.__activeFrame || null });
        if (animationFrames.deadline === null) animationFrames.deadline = currentTime() + 16;
        return id;
    };
    g.cancelAnimationFrame = function (handle) {
        handle = Number(handle);
        animationFrames.q = animationFrames.q.filter((entry) => entry.id !== handle);
        if (!animationFrames.q.length) animationFrames.deadline = null;
    };
    function runAnimationFrameCallbacks(now) {
        // HTML "run the animation frame callbacks": snapshot the callback-map
        // keys, then remove each callback immediately before invoking it. A
        // callback queued during this pass is therefore deferred to the next
        // rendering opportunity, while an earlier callback can still cancel a
        // later handle from this snapshot.
        const handles = animationFrames.q.map((entry) => entry.id);
        animationFrames.deadline = null;
        let invoked = 0;
        for (const handle of handles) {
            const index = animationFrames.q.findIndex((entry) => entry.id === handle);
            if (index < 0) continue;
            const entry = animationFrames.q.splice(index, 1)[0];
            invoked++;
            try { runInFrame(entry.frame, () => entry.callback(now)); }
            catch (e) { trust.errors.push("animation frame: " + ((e && e.message) || e) + (e && e.stack ? "\n" + e.stack : "")); }
        }
        if (animationFrames.q.length && animationFrames.deadline === null) {
            animationFrames.deadline = currentTime() + 16;
        }
        return invoked;
    }
    // Background fetch (dispatch/at-rest, resident actor): the request runs OFF
    // the JS thread so the dispatch doesn't block on the wire. `bgFetch(id)`
    // hands back a promise the actor settles via `settleFetch(id, value)` once
    // `(id, result)` posts back (see Rust `sys_http_fetch_async`/
    // `dispatch_fetch_done`). The resolver lives in this JS-side map — GC-rooted
    // by `trust` — so Rust never holds a GC object across threads.
    trust.pendingFetches = {};
    trust.bgFetch = (id) => new Promise((resolve) => { trust.pendingFetches[id] = resolve; });
    trust.settleFetch = function (id, value) {
        const resolve = trust.pendingFetches[id];
        if (resolve) { delete trust.pendingFetches[id]; resolve(value); }
    };
    g.queueMicrotask = (fn) => { Promise.resolve().then(fn).catch((e) => trust.errors.push("microtask: " + ((e && e.message) || e))); };
    // FinalizationRegistry (ES2021). Boa ships WeakRef/WeakMap/WeakSet but not
    // this; libraries that build caches keyed on GC (Apollo's @wry/caches,
    // emotion, lit's reactive controllers) feature-detect it, and some construct
    // it unconditionally → a bare ReferenceError without it. The cleanup callback
    // is NEVER guaranteed to run (spec §FinalizationRegistry — implementations
    // "may never" call it), and a headless parse-time engine has no JS-visible GC
    // finalization hook, so we never fire it. We still validate inputs like a real
    // engine and track the unregister token WEAKLY (a WeakSet — no leak) so
    // unregister() answers correctly. This is a conformant "never collects"
    // registry, not a fake.
    class FinalizationRegistry {
        constructor(cleanup) {
            if (typeof cleanup !== "function") throw new TypeError("FinalizationRegistry: cleanup callback must be callable");
            this.__cleanup = cleanup;
            this.__tokens = new WeakSet();
        }
        get [Symbol.toStringTag]() { return "FinalizationRegistry"; }
        register(target, heldValue, unregisterToken) {
            if (target === null || (typeof target !== "object" && typeof target !== "function"))
                throw new TypeError("FinalizationRegistry.register: target must be an object");
            if (target === heldValue)
                throw new TypeError("FinalizationRegistry.register: target and held value must not be the same");
            if (unregisterToken !== undefined) {
                if (typeof unregisterToken !== "object" && typeof unregisterToken !== "function")
                    throw new TypeError("FinalizationRegistry.register: unregister token must be an object");
                this.__tokens.add(unregisterToken);
            }
        }
        unregister(unregisterToken) {
            if (unregisterToken === null || (typeof unregisterToken !== "object" && typeof unregisterToken !== "function"))
                throw new TypeError("FinalizationRegistry.unregister: token must be an object");
            return this.__tokens.delete(unregisterToken);
        }
    }
    g.FinalizationRegistry = FinalizationRegistry;
    // The HTML structured-clone algorithm — via the SAME wire codec workers
    // use (`__sc_serialize`/`__sc_deserialize`, defined below; single source
    // of truth). Cycles, Map/Set, ArrayBuffer/typed arrays/DataView, Date/
    // RegExp/Error/Blob/File all round-trip; functions, symbols, and DOM
    // nodes throw DataCloneError — the old inline clone silently copied a DOM
    // node as a plain object (`__id` included), a wrapper aliasing a live
    // arena node. No transfer support (`options.transfer` ignored).
    g.structuredClone = function (value, _options) {
        return g.__sc_deserialize(g.__sc_serialize(value));
    };
    trust.oneShot = false;
    trust.tick = function () {
        // The settle primitive. Two modes, set by `trust.oneShot`:
        //
        // LIVE (oneShot=false, the actor/production path): fire only ONE-SHOT
        // timers ALREADY DUE at the current monotonic instant —
        // deferred-init `setTimeout(0)` and 0-delay cascades. It does NOT
        // fast-forward virtual time, so `requestAnimationFrame` (now+16),
        // `setInterval`, and `setTimeout(_, N>0)` are LEFT PENDING and fire at
        // their REAL wall-clock time at rest (`tickTo`, driven by the wake
        // loop). The browser model: paint the initial state, then run
        // time-driven work as real time elapses — no counter showing ~400, no
        // rAF pre-running hundreds of frames before first paint.
        //
        // ONE-SHOT (oneShot=true, `transform` — diagnostics/canaries/tests):
        // there is NO at-rest loop to run deferred work later, so fire the
        // earliest one-shot within a rolling `now+1000` WINDOW and advance
        // virtual time to it, draining the page to its eventual rendered state
        // (rAF-deferred content, delayed timeouts). Intervals are still skipped
        // (`every !== null`) so a snapshot doesn't show a jumped counter.
        // One-shot snapshots also select runnable networking/messaging tasks.
        // The live actor selects these sources independently (without timer
        // throttling), but a transform has no resident event loop to do so later.
        if (trust.runPlatformTask && trust.runPlatformTask()) return true;
        if (trust.oneShot && trust.hasIdleRequest && trust.hasIdleRequest()) {
            trust.startIdlePeriod(currentTime() + 50);
            if (trust.runPlatformTask()) return true;
        }
        const observedNow = currentTime();
        const limit = trust.oneShot ? observedNow + 1000 : observedNow;
        let best = null;
        for (const t of timers.q) {
            if (t.every === null && t.at <= limit && (!best || t.at < best.at)) best = t;
        }
        if (animationFrames.deadline !== null && animationFrames.deadline <= limit &&
            (!best || animationFrames.deadline <= best.at)) {
            timers.now = Math.max(timers.now, observedNow, animationFrames.deadline);
            __clockSync();
            return runAnimationFrameCallbacks(timers.now) > 0;
        }
        if (!best) return false;
        timers.q.splice(timers.q.indexOf(best), 1);
        // Advancing a one-shot snapshot to `best.at` must never move the
        // realm's monotonic clock behind the instant at which this turn
        // selected its task. In particular, sibling zero-delay timers have
        // already completed their wait by `observedNow`; a zero-delay timer
        // created by the first callback must not acquire an earlier deadline
        // merely because `__clockSync()` re-anchored Date to an older value.
        // HTML Timers' "run steps after a timeout" orders earlier invocations
        // before later ones once the corresponding waits complete.
        timers.now = Math.max(timers.now, observedNow, best.at);
        __clockSync();
        runTimerTask(best);
        return true;
    };
    // At REST (not the load/dispatch fast-forward settle above), the actor
    // advances time by the REAL wall clock and fires the earliest timer task
    // due by `absMs`. HTML timers queue tasks; each task gets its own microtask
    // checkpoint/event-loop turn, so a large overdue set must not execute as
    // one uninterruptible callback batch. `nextDeadline` is the absolute
    // deadline the actor sleeps until;
    // `now` anchors the wall-clock delta it measures forward from. A re-armed
    // repeating timer is initialized again after its handler finishes, as the
    // standard requires, so a long callback never causes a catch-up burst.
    trust.now = () => currentTime();
    trust.nextDeadline = function () {
        let best = animationFrames.deadline;
        for (const t of timers.q) if (best === null || t.at < best) best = t.at;
        return best;
    };
    trust.nextTimerInfo = function () {
        let best = null;
        for (const timer of timers.q) {
            if (!best || timer.at < best.at || (timer.at === best.at && timer.id < best.id)) best = timer;
        }
        return best ? { id: best.id, nesting: best.nesting, wait: best.wait } : null;
    };
    trust.tickTo = function (absMs) {
        absMs = Math.max(currentTime(), absMs);
        let task = null;
        for (const t of timers.q) {
            if (t.at > absMs) continue;
            if (!task || t.at < task.at || (t.at === task.at && t.id < task.id)) {
                task = t;
            }
        }
        if (animationFrames.deadline !== null && animationFrames.deadline <= absMs &&
            (!task || animationFrames.deadline <= task.at)) {
            timers.now = absMs;
            __clockSync();
            return runAnimationFrameCallbacks(absMs);
        }
        if (task) {
            timers.q.splice(timers.q.indexOf(task), 1);
            timers.now = absMs;
            __clockSync();
            runTimerTask(task);
        }
        timers.now = absMs;
        __clockSync();
        return task ? 1 : 0;
    };
    // The clocks share the page time origin. Explicit fast-forward advances
    // `timers.now`; between checkpoints PageClock continues from host monotonic
    // time, so deadline-based synchronous work can yield normally.
    //
    // The WHOLE Date surface follows this clock: the old JS-side
    // `Date.now = () => __epoch0 + timers.now` override is gone — `__clockSync`
    // mirrors every `timers.now` advance into the Rust-side Boa clock
    // (`__clock_set` → `PageClock`), which `Date.now()`, `new Date()`, and
    // every other host time read share. Before this, `new Date()` kept reading
    // the REAL host clock while `Date.now()` was fast-forwarded — a page
    // comparing them (or diffing two `new Date()`s across a settle) saw time
    // stand still or run backwards.
    g.performance.now = () => currentTime();

    // --- console into the outcome's ring ---
    const log = (level) => (...a) => {
        if (trust.logs.length < 100) trust.logs.push(level + ": " + a.map((x) => { try { return String(x); } catch { return "?"; } }).join(" "));
    };
    g.console = { log: log("log"), info: log("info"), warn: log("warn"), error: log("error"), debug: log("debug"), trace: log("trace"), dir: log("dir"), group() {}, groupEnd() {}, table: log("table"), time() {}, timeEnd() {}, count() {}, assert() {} };

    // --- small web APIs ---
    const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    g.btoa = (s) => {
        s = String(s); let out = "";
        for (let i = 0; i < s.length; i += 3) {
            const c1 = s.charCodeAt(i), c2 = s.charCodeAt(i + 1), c3 = s.charCodeAt(i + 2);
            if (c1 > 255 || c2 > 255 || c3 > 255) throw new Error("btoa: invalid character");
            out += B64[c1 >> 2] + B64[((c1 & 3) << 4) | (isNaN(c2) ? 0 : c2 >> 4)]
                + (isNaN(c2) ? "=" : B64[((c2 & 15) << 2) | (isNaN(c3) ? 0 : c3 >> 6)])
                + (isNaN(c3) ? "=" : B64[c3 & 63]);
        }
        return out;
    };
    g.atob = (s) => {
        // Forgiving-base64 decode (WHATWG Infra §4.5): strip ASCII whitespace,
        // then up to two trailing `=` when the length is a multiple of 4; a
        // remaining non-alphabet character or a length ≡ 1 (mod 4) throws
        // InvalidCharacterError — we used to skip bad input silently, which
        // turned corrupt base64 into corrupt bytes instead of the error the
        // caller's catch path expects.
        s = String(s).replace(/[\t\n\f\r ]+/g, "");
        if (s.length % 4 === 0) s = s.replace(/={1,2}$/, "");
        if (s.length % 4 === 1) throw new DOMException("Failed to execute 'atob': The string to be decoded is not correctly encoded.", "InvalidCharacterError");
        let out = "", buf = 0, bits = 0;
        for (const ch of s) {
            const v = B64.indexOf(ch);
            if (v < 0) throw new DOMException("Failed to execute 'atob': The string to be decoded is not correctly encoded.", "InvalidCharacterError");
            buf = (buf << 6) | v; bits += 6;
            if (bits >= 8) { bits -= 8; out += String.fromCharCode((buf >> bits) & 255); }
        }
        return out;
    };
    g.TextEncoder = class TextEncoder {
        get encoding() { return "utf-8"; }
        encode(s) {
            return __text_encode(String(s === undefined ? "" : s));
        }
        encodeInto(s, destination) {
            s = String(s === undefined ? "" : s);
            if (!(destination instanceof Uint8Array)) {
                throw new TypeError("TextEncoder.encodeInto destination must be a Uint8Array");
            }
            let read = 0, written = 0;
            while (read < s.length) {
                const first = s.charCodeAt(read);
                let codePoint = first, units = 1;
                if (first >= 0xd800 && first <= 0xdbff) {
                    const second = read + 1 < s.length ? s.charCodeAt(read + 1) : 0;
                    if (second >= 0xdc00 && second <= 0xdfff) {
                        codePoint = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                        units = 2;
                    } else {
                        codePoint = 0xfffd;
                    }
                } else if (first >= 0xdc00 && first <= 0xdfff) {
                    codePoint = 0xfffd;
                }
                const needed = codePoint < 0x80 ? 1 : codePoint < 0x800 ? 2 : codePoint < 0x10000 ? 3 : 4;
                if (written + needed > destination.byteLength) break;
                if (needed === 1) {
                    destination[written++] = codePoint;
                } else if (needed === 2) {
                    destination[written++] = 0xc0 | (codePoint >> 6);
                    destination[written++] = 0x80 | (codePoint & 0x3f);
                } else if (needed === 3) {
                    destination[written++] = 0xe0 | (codePoint >> 12);
                    destination[written++] = 0x80 | ((codePoint >> 6) & 0x3f);
                    destination[written++] = 0x80 | (codePoint & 0x3f);
                } else {
                    destination[written++] = 0xf0 | (codePoint >> 18);
                    destination[written++] = 0x80 | ((codePoint >> 12) & 0x3f);
                    destination[written++] = 0x80 | ((codePoint >> 6) & 0x3f);
                    destination[written++] = 0x80 | (codePoint & 0x3f);
                }
                read += units;
            }
            return { read, written };
        }
    };
    g.TextDecoder = class TextDecoder {
        // Encoding §4.2 and §7.2: labels are ASCII-case-insensitive and the
        // UTF-16 labels select the corresponding endian decoder. UTF-16 has
        // no encoder in the standard, but its decoder is required by deployed
        // web content (including .NET's WebAssembly bootstrap).
        constructor(label, options) {
            const l = String(label === undefined ? "utf-8" : label).trim().toLowerCase();
            const utf8 = ["unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8", "utf-8", "utf8", "x-unicode20utf8"];
            if (utf8.indexOf(l) >= 0) this.__encoding = "utf-8";
            else if (l === "unicodefffe" || l === "utf-16be") this.__encoding = "utf-16be";
            else if (["csunicode", "iso-10646-ucs-2", "ucs-2", "unicode", "unicodefeff", "utf-16", "utf-16le"].indexOf(l) >= 0) this.__encoding = "utf-16le";
            else throw new RangeError("The encoding label is invalid");
            this.__fatal = !!(options && options.fatal);
            this.__ignoreBOM = !!(options && options.ignoreBOM);
            this.__doNotFlush = false;
            this.__pendingBytes = [];
            this.__pendingHigh = null;
            this.__bomSeen = false;
        }
        get encoding() { return this.__encoding; }
        get fatal() { return this.__fatal; }
        get ignoreBOM() { return this.__ignoreBOM; }
        decode(input, options) {
            const stream = !!(options && options.stream);
            if (!this.__doNotFlush) {
                this.__pendingBytes = [];
                this.__pendingHigh = null;
                this.__bomSeen = false;
            }
            this.__doNotFlush = stream;
            let b;
            if (input === undefined) b = new Uint8Array(0);
            else if (input instanceof ArrayBuffer) b = new Uint8Array(input);
            else if (ArrayBuffer.isView(input)) b = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
            else b = new Uint8Array(input);
            if (this.__pendingBytes.length) {
                const joined = new Uint8Array(this.__pendingBytes.length + b.length);
                joined.set(this.__pendingBytes); joined.set(b, this.__pendingBytes.length); b = joined;
                this.__pendingBytes = [];
            }
            if (this.__encoding === "utf-8") {
                let out = "", i = 0;
                if (!this.__ignoreBOM && !this.__bomSeen && b.length >= 3 && b[0] === 0xef && b[1] === 0xbb && b[2] === 0xbf) i = 3;
                this.__bomSeen = true;
                while (i < b.length) {
                    const x = b[i];
                    let c, n;
                    if (x < 0x80) { c = x; n = 0; }
                    else if ((x & 0xe0) === 0xc0) { c = x & 31; n = 1; }
                    else if ((x & 0xf0) === 0xe0) { c = x & 15; n = 2; }
                    else if ((x & 0xf8) === 0xf0) { c = x & 7; n = 3; }
                    else { if (this.__fatal) throw new TypeError("The encoded data was not valid"); out += "�"; i += 1; continue; }
                    if (i + n >= b.length) {
                        if (stream) { this.__pendingBytes = Array.from(b.slice(i)); break; }
                        if (this.__fatal) throw new TypeError("The encoded data was not valid");
                        out += "�"; i += 1; continue;
                    }
                    let ok = true;
                    for (let k = 1; k <= n; k++) {
                        if ((b[i + k] & 0xc0) !== 0x80) { ok = false; break; }
                        c = (c << 6) | (b[i + k] & 63);
                    }
                    if (!ok || c > 0x10ffff || (c >= 0xd800 && c <= 0xdfff) || (n === 1 && c < 0x80) || (n === 2 && c < 0x800) || (n === 3 && c < 0x10000)) {
                        if (this.__fatal) throw new TypeError("The encoded data was not valid");
                        out += "�"; i += 1; continue;
                    }
                    out += String.fromCodePoint(c); i += n + 1;
                }
                return out;
            }

            let orderLE = this.__encoding === "utf-16le", offset = 0;
            if (!this.__bomSeen && b.length >= 2) {
                if (b[0] === 0xfe && b[1] === 0xff) orderLE = false;
                else if (b[0] === 0xff && b[1] === 0xfe) orderLE = true;
                if (!this.__ignoreBOM && ((orderLE && b[0] === 0xff && b[1] === 0xfe) || (!orderLE && b[0] === 0xfe && b[1] === 0xff))) offset = 2;
                this.__bomSeen = true;
            }
            let danglingByte = false;
            if (((b.length - offset) & 1) !== 0) {
                if (stream) {
                    this.__pendingBytes = [b[b.length - 1]];
                    b = b.slice(0, b.length - 1);
                } else {
                    if (this.__fatal) throw new TypeError("The encoded data was not valid");
                    danglingByte = true;
                    b = b.slice(0, b.length - 1);
                }
            }
            let out = "", high = this.__pendingHigh;
            const error = () => { if (this.__fatal) throw new TypeError("The encoded data was not valid"); out += "�"; };
            if (danglingByte) error();
            for (let i = offset; i < b.length; i += 2) {
                const u = orderLE ? b[i] | (b[i + 1] << 8) : (b[i] << 8) | b[i + 1];
                if (high !== null) {
                    if (u >= 0xdc00 && u <= 0xdfff) { out += String.fromCodePoint(0x10000 + ((high - 0xd800) << 10) + u - 0xdc00); high = null; continue; }
                    error(); high = null;
                }
                if (u >= 0xd800 && u <= 0xdbff) high = u;
                else if (u >= 0xdc00 && u <= 0xdfff) error();
                else out += String.fromCharCode(u);
            }
            if (high !== null) {
                if (stream) this.__pendingHigh = high;
                else { error(); this.__pendingHigh = null; }
            } else this.__pendingHigh = null;
            return out;
        }
    };
    /*__STREAMS_BEGIN__*/
    // --- WHATWG Streams (in-memory). Real constructors so streaming code
    // both LOADS and RUNS: Open WebUI's chat/SSE pipeline does
    // `class X extends TransformStream` at module-eval time and pipes
    // `body.pipeThrough(new TextDecoderStream()).pipeThrough(parser)` — a
    // missing `TransformStream` threw a ReferenceError that 500'd the whole
    // route. Single-consumer, no BYOB/byte-stream/backpressure tuning;
    // faithful enough for producer→transform→reader pipelines. (A fetch
    // Response.body is still null — actual network streaming is a separate,
    // deeper feature — so the streams a page builds itself work, but reading a
    // response as a stream follows the same buffered producer pipeline.) ---
    {
        const deferred = () => {
            let resolve, reject;
            const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
            return { promise, resolve, reject };
        };
        class ReadableStream {
            constructor(source, strategy) {
                source = source || {};
                this._source = source;
                this._queue = [];
                this._pending = []; // waiting read()s: {resolve,reject}
                this._closed = false;
                this._error = null;
                this._reader = null;
                this._closedDef = deferred();
                this._closedDef.promise.catch(() => {});
                const self = this;
                this._controller = {
                    enqueue(chunk) {
                        if (self._closed || self._error) return;
                        if (self._pending.length) self._pending.shift().resolve({ value: chunk, done: false });
                        else self._queue.push(chunk);
                    },
                    close() {
                        if (self._closed || self._error) return;
                        self._closed = true;
                        while (self._pending.length) self._pending.shift().resolve({ value: undefined, done: true });
                        self._closedDef.resolve(undefined);
                    },
                    error(e) {
                        if (self._closed || self._error) return;
                        self._error = e || new TypeError("stream error");
                        while (self._pending.length) self._pending.shift().reject(self._error);
                        self._closedDef.reject(self._error);
                    },
                    get desiredSize() { return self._closed || self._error ? null : 1; },
                };
                try {
                    const r = source.start ? source.start(this._controller) : undefined;
                    Promise.resolve(r).then(() => self._pull(), (e) => self._controller.error(e));
                } catch (e) { this._controller.error(e); }
            }
            _pull() {
                const self = this;
                if (this._closed || this._error || !this._source.pull) return;
                if (this._pending.length && this._queue.length === 0) {
                    try { Promise.resolve(this._source.pull(this._controller)).catch((e) => self._controller.error(e)); }
                    catch (e) { self._controller.error(e); }
                }
            }
            get locked() { return this._reader !== null; }
            getReader(opts) {
                if (this._reader) throw new TypeError("ReadableStream is already locked");
                const self = this;
                const reader = {
                    read() {
                        if (self._queue.length) return Promise.resolve({ value: self._queue.shift(), done: false });
                        if (self._error) return Promise.reject(self._error);
                        if (self._closed) return Promise.resolve({ value: undefined, done: true });
                        return new Promise((resolve, reject) => { self._pending.push({ resolve, reject }); self._pull(); });
                    },
                    cancel(reason) { return self.cancel(reason); },
                    releaseLock() { self._reader = null; },
                    get closed() { return self._closedDef.promise; },
                };
                this._reader = reader;
                return reader;
            }
            cancel(reason) {
                if (!this._closed && !this._error) {
                    this._closed = true;
                    this._queue = [];
                    while (this._pending.length) this._pending.shift().resolve({ value: undefined, done: true });
                    this._closedDef.resolve(undefined);
                    try { if (this._source.cancel) this._source.cancel(reason); } catch (e) {}
                }
                return Promise.resolve(undefined);
            }
            pipeTo(dest, opts) {
                const reader = this.getReader();
                const writer = dest.getWriter();
                return new Promise((resolve, reject) => {
                    const step = () => {
                        reader.read().then((res) => {
                            if (res.done) { Promise.resolve(writer.close()).then(resolve, resolve); return; }
                            Promise.resolve(writer.write(res.value)).then(step, reject);
                        }, reject);
                    };
                    step();
                });
            }
            pipeThrough(pair, opts) {
                this.pipeTo(pair.writable, opts);
                return pair.readable;
            }
            tee() {
                const reader = this.getReader();
                let c1 = null, c2 = null, reading = false;
                const pump = () => {
                    if (reading) return;
                    reading = true;
                    reader.read().then((res) => {
                        reading = false;
                        if (res.done) { if (c1) c1.close(); if (c2) c2.close(); return; }
                        if (c1) c1.enqueue(res.value);
                        if (c2) c2.enqueue(res.value);
                    }, (e) => { if (c1) c1.error(e); if (c2) c2.error(e); });
                };
                const b1 = new ReadableStream({ start(c) { c1 = c; }, pull: pump });
                const b2 = new ReadableStream({ start(c) { c2 = c; }, pull: pump });
                return [b1, b2];
            }
        }
        ReadableStream.prototype[Symbol.asyncIterator] = function () {
            const reader = this.getReader();
            return {
                next() { return reader.read(); },
                return() { reader.releaseLock(); return Promise.resolve({ value: undefined, done: true }); },
            };
        };
        class WritableStream {
            constructor(sink, strategy) {
                sink = sink || {};
                this._sink = sink;
                this._writer = null;
                this._closed = false;
                this._error = null;
                const self = this;
                this._controller = { error(e) { self._error = e; }, get signal() { return undefined; } };
                try { this._ready = Promise.resolve(sink.start ? sink.start(this._controller) : undefined); }
                catch (e) { this._ready = Promise.reject(e); }
                this._chain = this._ready.catch(() => {});
            }
            get locked() { return this._writer !== null; }
            getWriter() {
                if (this._writer) throw new TypeError("WritableStream is already locked");
                const self = this;
                const writer = {
                    write(chunk) {
                        self._chain = self._chain.then(() => {
                            if (self._error) throw self._error;
                            return self._sink.write ? self._sink.write(chunk, self._controller) : undefined;
                        });
                        return self._chain;
                    },
                    close() {
                        self._chain = self._chain.then(() => {
                            if (self._closed) return undefined;
                            self._closed = true;
                            return self._sink.close ? self._sink.close() : undefined;
                        });
                        return self._chain;
                    },
                    abort(reason) {
                        self._error = reason || new TypeError("aborted");
                        return Promise.resolve(self._sink.abort ? self._sink.abort(reason) : undefined);
                    },
                    releaseLock() { self._writer = null; },
                    get ready() { return self._ready.then(() => undefined); },
                    get closed() { return self._chain.then(() => undefined); },
                    get desiredSize() { return self._error ? null : (self._closed ? 0 : 1); },
                };
                this._writer = writer;
                return writer;
            }
            abort(reason) { this._error = reason; return Promise.resolve(this._sink.abort ? this._sink.abort(reason) : undefined); }
            close() { return this.getWriter().close(); }
        }
        class TransformStream {
            constructor(transformer, writableStrategy, readableStrategy) {
                transformer = transformer || {};
                let rc;
                this.readable = new ReadableStream({ start(c) { rc = c; } });
                const tc = {
                    enqueue(chunk) { rc.enqueue(chunk); },
                    terminate() { rc.close(); },
                    error(e) { rc.error(e); },
                    get desiredSize() { return rc.desiredSize; },
                };
                let started;
                try { started = transformer.start ? transformer.start(tc) : undefined; }
                catch (e) { rc.error(e); started = Promise.reject(e); }
                this.writable = new WritableStream({
                    start() { return Promise.resolve(started); },
                    write(chunk) {
                        if (transformer.transform) return Promise.resolve(transformer.transform(chunk, tc));
                        tc.enqueue(chunk);
                        return undefined;
                    },
                    close() {
                        return Promise.resolve(transformer.flush ? transformer.flush(tc) : undefined).then(() => rc.close());
                    },
                    abort(e) { rc.error(e); },
                });
            }
        }
        class TextDecoderStream extends TransformStream {
            constructor(label, options) {
                const dec = new g.TextDecoder(label, options);
                super({
                    transform(chunk, c) { const s = dec.decode(chunk); if (s) c.enqueue(s); },
                });
                this._encoding = dec.encoding;
            }
            get encoding() { return this._encoding; }
        }
        class TextEncoderStream extends TransformStream {
            constructor() {
                const enc = new g.TextEncoder();
                super({ transform(chunk, c) { c.enqueue(enc.encode(String(chunk))); } });
            }
            get encoding() { return "utf-8"; }
        }
        g.ReadableStream = ReadableStream;
        g.WritableStream = WritableStream;
        g.TransformStream = TransformStream;
        g.TextDecoderStream = TextDecoderStream;
        g.TextEncoderStream = TextEncoderStream;

        // Compression Streams §§4 and 6. A compression context may buffer a
        // chunk and return no output until the finish flag, so accumulating
        // copied BufferSources here and invoking the native DEFLATE codec from
        // flush preserves the standard TransformStream contract while keeping
        // this expensive primitive out of interpreted page JavaScript.
        class CompressionStream extends TransformStream {
            constructor(format) {
                format = String(format);
                if (format !== "deflate" && format !== "deflate-raw" && format !== "gzip")
                    throw new TypeError("Unsupported compression format: " + format);
                const chunks = [];
                let total = 0;
                super({
                    transform(chunk) {
                        let view;
                        if (chunk instanceof ArrayBuffer) view = new Uint8Array(chunk);
                        else if (ArrayBuffer.isView(chunk)) view = new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
                        else throw new TypeError("CompressionStream input must be a BufferSource");
                        const copy = view.slice();
                        chunks.push(copy);
                        total += copy.byteLength;
                        if (total > 16 * 1024 * 1024) throw new RangeError("CompressionStream input exceeds the 16 MiB page limit");
                    },
                    flush(controller) {
                        const input = new Uint8Array(total);
                        let offset = 0;
                        for (const chunk of chunks) { input.set(chunk, offset); offset += chunk.byteLength; }
                        const output = __compression_encode(format, input);
                        if (output.byteLength) controller.enqueue(output);
                    },
                });
            }
        }
        g.CompressionStream = CompressionStream;
    }
    /*__STREAMS_END__*/
    // --- Web Animations API (Element.animate). A terminal has no real
    // animation, so an Animation settles to "finished" immediately — on a
    // MACROTASK, so a caller assigning `onfinish` AFTER animate() (Svelte 5's
    // transition system does exactly this) still sees it fire. Critical, not
    // cosmetic: `element.animate(...)` being undefined threw inside Svelte 5's
    // intro-transition effect, and a thrown effect ABORTS the whole effect-
    // flush batch — so a sibling effect in the same flush (a TipTap/ProseMirror
    // editor's mount) silently never ran, leaving Open WebUI's chat input
    // unrendered. `finished` always resolves exactly once (finish/cancel/auto),
    // so nothing awaiting it hangs or reports an unhandled rejection. ---
    {
        class Animation extends EventTarget {
            constructor(effect, timeline) {
                super();
                this.effect = effect || null;
                this.timeline = timeline || null;
                this.id = "";
                this.playbackRate = 1;
                this.startTime = 0;
                this.currentTime = 0;
                this.pending = false;
                this.playState = "running";
                this.replaceState = "active";
                this.onfinish = null;
                this.oncancel = null;
                this.onremove = null;
                this._done = false;
                let res;
                this.finished = new Promise((r) => { res = r; });
                const self = this;
                this._settle = (kind) => {
                    if (self._done) return;
                    self._done = true;
                    self.playState = kind === "cancel" ? "idle" : "finished";
                    const ev = new Event(kind === "cancel" ? "cancel" : "finish");
                    const cb = kind === "cancel" ? self.oncancel : self.onfinish;
                    if (typeof cb === "function") { try { cb.call(self, ev); } catch (e) {} }
                    self.dispatchEvent(ev);
                    res(self);
                };
                setTimeout(() => this._settle("finish"), 0);
            }
            play() {}
            pause() { this.playState = "paused"; }
            reverse() {}
            finish() { this._settle("finish"); }
            cancel() { this._settle("cancel"); }
            updatePlaybackRate(r) { this.playbackRate = r; }
            persist() {}
            commitStyles() {}
        }
        g.Animation = Animation;
        Element.prototype.animate = function (keyframes, options) { return new Animation(null, null); };
        Element.prototype.getAnimations = function () { return []; };
        if (g.document) {
            g.document.getAnimations = function () { return []; };
            try { g.document.timeline = { currentTime: 0 }; } catch (e) {}
        }
    }
    // --- Intl: an en-only prelude shim. Measured 2026-06-12: Boa's
    // bundled ICU costs +11MB and its DateTimeFormat/DisplayNames are
    // broken anyway. Honest-enough English output for a terminal;
    // resolvedOptions/supportedLocalesOf exist so feature-detection
    // passes and pages stop taking polyfill/error paths.
    {
        // ECMA-402 §DefaultLocale recommends matching navigator.language in a
        // browser environment. This is the one default every Intl constructor
        // below reports when no explicit supported locale was requested.
        const defaultLocale = (g.navigator && g.navigator.language) || cfg.language || "en-US";
        const localeList = (l) => (l === undefined ? [] : Array.isArray(l) ? Array.from(l) : [l]).map(String);
        const supEn = (l) => localeList(l).filter((s) => /^en($|-)/i.test(s));
        const grouped = (s) => {
            const i = s.indexOf(".");
            const head = i < 0 ? s : s.slice(0, i), tail = i < 0 ? "" : s.slice(i);
            return head.replace(/\B(?=(\d{3})+$)/g, ",") + tail;
        };
        const CURRENCY = { USD: "$", EUR: "€", GBP: "£", JPY: "¥" };
        class NumberFormat {
            constructor(locales, options) { this.__o = options || {}; }
            format(n) {
                const o = this.__o;
                n = Number(n);
                if (!isFinite(n)) return isNaN(n) ? "NaN" : n > 0 ? "∞" : "-∞";
                const neg = n < 0 || (n === 0 && 1 / n < 0);
                let v = Math.abs(n);
                if (o.style === "percent") v *= 100;
                let min = o.minimumFractionDigits, max = o.maximumFractionDigits;
                if (min === undefined) min = o.style === "currency" ? 2 : 0;
                if (max === undefined) max = Math.max(min, o.style === "currency" ? 2 : o.style === "percent" ? 0 : 3);
                let s = v.toFixed(Math.min(20, max));
                const dot = s.indexOf(".");
                if (max > min && dot >= 0) {
                    const h = s.slice(0, dot);
                    let f = s.slice(dot + 1).replace(/0+$/, "");
                    while (f.length < min) f += "0";
                    s = f ? h + "." + f : h;
                }
                if (o.useGrouping !== false) s = grouped(s);
                if (o.style === "currency") {
                    const c = String(o.currency || "USD").toUpperCase();
                    s = (CURRENCY[c] || c + " ") + s;
                }
                if (o.style === "percent") s += "%";
                return neg ? "-" + s : s;
            }
            formatToParts(n) { return [{ type: "literal", value: this.format(n) }]; }
            resolvedOptions() {
                const o = this.__o;
                return Object.assign({ locale: defaultLocale, numberingSystem: "latn", notation: "standard", style: "decimal", useGrouping: o.useGrouping === false ? false : "auto", minimumIntegerDigits: 1 }, o);
            }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        const p2 = (x) => String(x).padStart(2, "0");
        class DateTimeFormat {
            constructor(locales, options) { this.__o = options || {}; }
            format(d) {
                d = d === undefined ? new Date() : new Date(d);
                if (isNaN(d.getTime())) return "Invalid Date";
                const o = this.__o;
                const wantsDate = !!(o.year || o.month || o.day || o.weekday || o.dateStyle);
                const wantsTime = !!(o.hour || o.minute || o.second || o.timeStyle);
                const date = d.getFullYear() + "-" + p2(d.getMonth() + 1) + "-" + p2(d.getDate());
                const secs = o.second || o.timeStyle ? ":" + p2(d.getSeconds()) : "";
                const time = p2(d.getHours()) + ":" + p2(d.getMinutes()) + secs;
                if (wantsTime && !wantsDate) return time;
                if (wantsDate && wantsTime) return date + ", " + time;
                return date;
            }
            formatToParts(d) { return [{ type: "literal", value: this.format(d) }]; }
            resolvedOptions() { return Object.assign({ locale: defaultLocale, calendar: "gregory", numberingSystem: "latn", timeZone: "UTC" }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        class Collator {
            constructor(locales, options) {
                // `var`, NOT const: Boa 0.21 panics (define opcode, OOB
                // binding slot) when a closure capturing a block-scoped
                // constructor local is invoked from a native callback —
                // and `compare` exists to be handed to Array#sort.
                var o = this.__o = options || {};
                var fold = o.sensitivity === "base" || o.sensitivity === "accent"
                    ? (s) => String(s).toLowerCase() : (s) => String(s);
                this.compare = (a, b) => {
                    a = fold(a); b = fold(b);
                    var na = o.numeric ? parseFloat(a) : NaN;
                    var nb = o.numeric ? parseFloat(b) : NaN;
                    if (!isNaN(na) && !isNaN(nb) && na !== nb) return na < nb ? -1 : 1;
                    return a < b ? -1 : a > b ? 1 : 0;
                };
            }
            resolvedOptions() { return Object.assign({ locale: defaultLocale, usage: "sort", sensitivity: "variant", numeric: false }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        class DisplayNames {
            constructor(locales, options) { this.__o = options || {}; }
            of(code) { return String(code); }
            resolvedOptions() { return Object.assign({ locale: defaultLocale, style: "long", fallback: "code" }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        class PluralRules {
            constructor(locales, options) { this.__o = options || {}; }
            select(n) { return Number(n) === 1 ? "one" : "other"; }
            resolvedOptions() { return Object.assign({ locale: defaultLocale, type: "cardinal", pluralCategories: ["one", "other"] }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        class RelativeTimeFormat {
            constructor(locales, options) { this.__o = options || {}; }
            format(v, unit) {
                v = Number(v);
                unit = String(unit).replace(/s$/, "");
                const n = Math.abs(v), u = n === 1 ? unit : unit + "s";
                return v < 0 ? n + " " + u + " ago" : "in " + n + " " + u;
            }
            formatToParts(v, unit) { return [{ type: "literal", value: this.format(v, unit) }]; }
            resolvedOptions() { return Object.assign({ locale: defaultLocale, numeric: "always", style: "long" }, this.__o); }
            static supportedLocalesOf(l) { return supEn(l); }
        }
        // Intl.Locale (ECMA-402): parse a Unicode BCP-47 locale identifier
        // into its subtags + -u- keywords, with maximize()/minimize() over a
        // COMPACT likely-subtags map. A full CLDR likelySubtags table is the
        // +11MB ICU we deliberately rejected; this covers the world's common
        // languages honestly and defaults unknowns to a Latn script (same
        // en-only ethos as the rest of this shim). @formatjs/intl-localematcher
        // (Mastodon's i18n boot; the "best fit" matcher) does
        // `new Intl.Locale(tag).maximize()` then reads .language/.script/
        // .region/.toString() — without Intl.Locale the whole SPA fails to
        // mount ("TypeError: not a constructor"), a blank screen.
        const LIKELY = {
            en: ["Latn", "US"], es: ["Latn", "ES"], fr: ["Latn", "FR"], de: ["Latn", "DE"],
            it: ["Latn", "IT"], pt: ["Latn", "BR"], nl: ["Latn", "NL"], sv: ["Latn", "SE"],
            da: ["Latn", "DK"], nb: ["Latn", "NO"], nn: ["Latn", "NO"], no: ["Latn", "NO"],
            fi: ["Latn", "FI"], is: ["Latn", "IS"], pl: ["Latn", "PL"], cs: ["Latn", "CZ"],
            sk: ["Latn", "SK"], sl: ["Latn", "SI"], hu: ["Latn", "HU"], ro: ["Latn", "RO"],
            hr: ["Latn", "HR"], et: ["Latn", "EE"], lv: ["Latn", "LV"], lt: ["Latn", "LT"],
            tr: ["Latn", "TR"], id: ["Latn", "ID"], ms: ["Latn", "MY"], vi: ["Latn", "VN"],
            tl: ["Latn", "PH"], sw: ["Latn", "TZ"], af: ["Latn", "ZA"], ca: ["Latn", "ES"],
            eu: ["Latn", "ES"], gl: ["Latn", "ES"], cy: ["Latn", "GB"], ga: ["Latn", "IE"],
            ru: ["Cyrl", "RU"], uk: ["Cyrl", "UA"], be: ["Cyrl", "BY"], bg: ["Cyrl", "BG"],
            sr: ["Cyrl", "RS"], mk: ["Cyrl", "MK"], kk: ["Cyrl", "KZ"], el: ["Grek", "GR"],
            hy: ["Armn", "AM"], ka: ["Geor", "GE"], he: ["Hebr", "IL"], yi: ["Hebr", "UA"],
            ar: ["Arab", "EG"], fa: ["Arab", "IR"], ur: ["Arab", "PK"], ps: ["Arab", "AF"],
            hi: ["Deva", "IN"], mr: ["Deva", "IN"], ne: ["Deva", "NP"], bn: ["Beng", "BD"],
            pa: ["Guru", "IN"], gu: ["Gujr", "IN"], ta: ["Taml", "IN"], te: ["Telu", "IN"],
            kn: ["Knda", "IN"], ml: ["Mlym", "IN"], si: ["Sinh", "LK"], th: ["Thai", "TH"],
            lo: ["Laoo", "LA"], my: ["Mymr", "MM"], km: ["Khmr", "KH"], am: ["Ethi", "ET"],
            ja: ["Jpan", "JP"], ko: ["Kore", "KR"], zh: ["Hans", "CN"], und: ["Latn", "US"],
        };
        const titleCase = (s) => s.charAt(0).toUpperCase() + s.slice(1).toLowerCase();
        class Locale {
            constructor(tag, options) {
                if (tag && typeof tag === "object" && tag.__isLocale) tag = tag.toString();
                tag = String(tag == null ? "" : tag).replace(/_/g, "-").trim();
                var o = options || {};
                var parts = tag.length ? tag.split("-") : [];
                var i = 0, len = parts.length;
                var language = "", script = "", region = "", variants = [], kw = {};
                if (i < len && /^([a-z]{2,3}|[a-z]{5,8})$/i.test(parts[i])) language = parts[i++].toLowerCase();
                if (i < len && /^[a-z]{4}$/i.test(parts[i])) { script = titleCase(parts[i]); i++; }
                if (i < len && /^([a-z]{2}|[0-9]{3})$/i.test(parts[i])) region = parts[i++].toUpperCase();
                while (i < len && /^([a-z0-9]{5,8}|[0-9][a-z0-9]{3})$/i.test(parts[i])) variants.push(parts[i++].toLowerCase());
                // Extensions: only the -u- (Unicode) extension is interpreted;
                // any other singleton's subtags are consumed and dropped.
                while (i < len) {
                    var sing = parts[i++];
                    if (!sing || sing.length !== 1) continue;
                    if (sing.toLowerCase() === "u") {
                        var key = "";
                        while (i < len && parts[i].length > 1) {
                            var pv = parts[i++].toLowerCase();
                            if (pv.length === 2) { key = pv; if (!(key in kw)) kw[key] = ""; }
                            else if (key) kw[key] = kw[key] ? kw[key] + "-" + pv : pv;
                        }
                    } else {
                        while (i < len && parts[i].length > 1) i++;
                    }
                }
                // options override subtags + add -u- keywords (ECMA-402
                // ApplyOptionsToTag / ApplyUnicodeExtensionToTag).
                if (o.language != null && /^[a-z]{2,3}$/i.test(String(o.language))) language = String(o.language).toLowerCase();
                if (o.script != null && /^[a-z]{4}$/i.test(String(o.script))) script = titleCase(String(o.script));
                if (o.region != null && /^([a-z]{2}|[0-9]{3})$/i.test(String(o.region))) region = String(o.region).toUpperCase();
                var okw = { calendar: "ca", collation: "co", hourCycle: "hc", caseFirst: "kf", numberingSystem: "nu" };
                for (var ke in okw) if (o[ke] != null) kw[okw[ke]] = String(o[ke]).toLowerCase();
                if (o.numeric != null) kw.kn = o.numeric ? "true" : "false";
                this.__isLocale = true;
                this.__lang = language || "und";
                this.__script = script;
                this.__region = region;
                this.__variants = variants;
                this.__kw = kw;
            }
            get language() { return this.__lang; }
            get script() { return this.__script; }
            get region() { return this.__region; }
            get calendar() { return this.__kw.ca; }
            get collation() { return this.__kw.co; }
            get hourCycle() { return this.__kw.hc; }
            get caseFirst() { return this.__kw.kf; }
            get numberingSystem() { return this.__kw.nu; }
            get numeric() { return "kn" in this.__kw && this.__kw.kn !== "false"; }
            get baseName() {
                var b = [this.__lang];
                if (this.__script) b.push(this.__script);
                if (this.__region) b.push(this.__region);
                for (var k = 0; k < this.__variants.length; k++) b.push(this.__variants[k]);
                return b.join("-");
            }
            __ext() {
                var keys = Object.keys(this.__kw).sort();
                if (!keys.length) return "";
                var s = "-u";
                for (var k = 0; k < keys.length; k++) { s += "-" + keys[k]; if (this.__kw[keys[k]]) s += "-" + this.__kw[keys[k]]; }
                return s;
            }
            toString() { return this.baseName + this.__ext(); }
            __build(lang, scr, reg) {
                var b = [lang];
                if (scr) b.push(scr);
                if (reg) b.push(reg);
                for (var k = 0; k < this.__variants.length; k++) b.push(this.__variants[k]);
                return new Locale(b.join("-") + this.__ext());
            }
            maximize() {
                var lang = this.__lang, scr = this.__script, reg = this.__region;
                var like = LIKELY[lang];
                if (like) { if (!scr) scr = like[0]; if (!reg) reg = like[1]; }
                else if (!scr) scr = "Latn";
                return this.__build(lang, scr, reg);
            }
            minimize() {
                var m = this.maximize(), lang = m.__lang, scr = m.__script, reg = m.__region, t;
                t = new Locale(lang).maximize();
                if (t.__script === scr && t.__region === reg) return this.__build(lang, "", "");
                if (reg) { t = new Locale(lang + "-" + reg).maximize(); if (t.__script === scr && t.__region === reg) return this.__build(lang, "", reg); }
                if (scr) { t = new Locale(lang + "-" + scr).maximize(); if (t.__script === scr && t.__region === reg) return this.__build(lang, scr, ""); }
                return this.__build(lang, scr, reg);
            }
        }
        // Intl.NumberFormat/DateTimeFormat/Collator are specced callable
        // WITHOUT `new` (legacy web-compat — they construct an instance
        // either way); only the newer ctors require `new`. ES classes throw
        // when called as functions, so wrap the three legacy ones in a
        // function that forwards to `new`, preserving prototype/instanceof
        // and the static supportedLocalesOf. Humble Bundle does
        // `Intl.NumberFormat(locale, opts).format(amount)` (no `new`).
        const callable = (Cls) => {
            const F = function (locales, options) { return new Cls(locales, options); };
            F.prototype = Cls.prototype;
            F.prototype.constructor = F;
            F.supportedLocalesOf = Cls.supportedLocalesOf;
            return F;
        };
        // Intl.Segmenter (ECMA-402 §Segmenter) — text segmentation by grapheme/
        // word/sentence. Like the rest of this shim it's en/approximate (NOT the
        // full ICU/UAX-29 machinery): grapheme clusters cover combining marks,
        // variation selectors, emoji skin-tone modifiers, ZWJ sequences and
        // regional-indicator (flag) pairs; word/sentence use Unicode-aware regex
        // over `\p{L}`/`\p{N}` and terminator scanning. chatgpt (and many apps)
        // do `new Intl.Segmenter(...)` for text metrics; without it that threw
        // "not a constructor". `index` is the UTF-16 code-unit offset (spec).
        const SEG_MARK = /\p{M}/u;
        const segGraphemes = (str) => {
            const res = [];
            const n = str.length;
            let i = 0;
            while (i < n) {
                const start = i;
                let cp = str.codePointAt(i);
                i += cp > 0xffff ? 2 : 1;
                // Regional-indicator pair (a flag is exactly two RIs).
                if (cp >= 0x1f1e6 && cp <= 0x1f1ff && i < n) {
                    const ncp = str.codePointAt(i);
                    if (ncp >= 0x1f1e6 && ncp <= 0x1f1ff) i += 2;
                }
                for (;;) {
                    if (i >= n) break;
                    const ncp = str.codePointAt(i);
                    const nlen = ncp > 0xffff ? 2 : 1;
                    if ((ncp >= 0x1f3fb && ncp <= 0x1f3ff) || ncp === 0xfe0e || ncp === 0xfe0f || SEG_MARK.test(String.fromCodePoint(ncp))) {
                        i += nlen; continue;
                    }
                    if (ncp === 0x200d) { // ZWJ joins the following scalar into this cluster
                        i += 1;
                        if (i < n) { const jcp = str.codePointAt(i); i += jcp > 0xffff ? 2 : 1; }
                        continue;
                    }
                    break;
                }
                res.push({ segment: str.slice(start, i), index: start });
            }
            return res;
        };
        const segWords = (str) => {
            const res = [];
            const re = /[\p{L}\p{N}_]+(?:['’.·’][\p{L}\p{N}_]+)*|\s+|[\s\S]/gu;
            let m;
            while ((m = re.exec(str)) !== null) {
                res.push({ segment: m[0], index: m.index, isWordLike: /[\p{L}\p{N}]/u.test(m[0]) });
                if (re.lastIndex === m.index) re.lastIndex++;
            }
            return res;
        };
        const segSentences = (str) => {
            const res = [];
            const n = str.length;
            let i = 0, start = 0;
            const term = /[.!?。！？]/;
            const close = /[)\]'"”’»]/;
            const ws = /\s/;
            while (i < n) {
                if (term.test(str[i])) {
                    i++;
                    while (i < n && term.test(str[i])) i++;
                    while (i < n && close.test(str[i])) i++;
                    while (i < n && ws.test(str[i])) i++;
                    res.push({ segment: str.slice(start, i), index: start });
                    start = i;
                } else i++;
            }
            if (start < n) res.push({ segment: str.slice(start, n), index: start });
            return res;
        };
        class Segments {
            constructor(input, gran) {
                this.__input = input;
                this.__segs = gran === "word" ? segWords(input) : gran === "sentence" ? segSentences(input) : segGraphemes(input);
                for (let k = 0; k < this.__segs.length; k++) this.__segs[k].input = input;
            }
            [Symbol.iterator]() {
                const segs = this.__segs;
                let i = 0;
                return { next() { return i < segs.length ? { done: false, value: segs[i++] } : { done: true, value: undefined }; } };
            }
            containing(index) {
                index = index === undefined ? 0 : Math.trunc(Number(index)) || 0;
                for (let k = 0; k < this.__segs.length; k++) {
                    const s = this.__segs[k];
                    if (index >= s.index && index < s.index + s.segment.length) return s;
                }
                return undefined;
            }
        }
        class Segmenter {
            constructor(locales, options) {
                const gran = (options && options.granularity !== undefined) ? String(options.granularity) : "grapheme";
                if (gran !== "grapheme" && gran !== "word" && gran !== "sentence")
                    throw new RangeError("Value " + gran + " out of range for Intl.Segmenter options property granularity");
                this.__gran = gran;
                const loc = Array.isArray(locales) ? locales[0] : locales;
                this.__locale = loc ? String(loc) : defaultLocale;
            }
            resolvedOptions() { return { locale: this.__locale, granularity: this.__gran }; }
            segment(input) { return new Segments(String(input), this.__gran); }
            get [Symbol.toStringTag]() { return "Intl.Segmenter"; }
        }
        Segmenter.supportedLocalesOf = (locales) => localeList(locales);
        g.Intl = {
            NumberFormat: callable(NumberFormat),
            DateTimeFormat: callable(DateTimeFormat),
            Collator: callable(Collator),
            DisplayNames, PluralRules, RelativeTimeFormat, Locale, Segmenter,
            getCanonicalLocales: localeList,
        };
        Number.prototype.toLocaleString = function (locales, options) { return new NumberFormat(locales, options).format(this); };
        Date.prototype.toLocaleDateString = function () { return new DateTimeFormat(0, { year: "numeric", month: "numeric", day: "numeric" }).format(this); };
        Date.prototype.toLocaleTimeString = function () { return new DateTimeFormat(0, { hour: "numeric", minute: "numeric", second: "numeric" }).format(this); };
        Date.prototype.toLocaleString = function () { return new DateTimeFormat(0, { year: "numeric", hour: "numeric", second: "numeric" }).format(this); };
    }
    const dec = (s) => { try { return decodeURIComponent(String(s).replace(/\+/g, " ")); } catch { return String(s); } };
    // The application/x-www-form-urlencoded byte serializer (URL Standard §"urlencoded
    // serializing"): 0x20→"+", keep only `* - . _ 0-9 A-Z a-z`, percent-encode
    // (UTF-8) everything else. NOT encodeURIComponent, which emits "%20" for space
    // and leaves `! ' ( ) ~` unescaped — both wrong for a query string.
    const fenc = (s) => encodeURIComponent(String(s)).replace(/[!'()~]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase()).replace(/%20/g, "+");
    class URLSearchParams {
        // WHATWG: init may be a string ("?"-prefixed query), another
        // URLSearchParams (copy its list), a sequence of [name,value] pairs, or
        // a record object. Getting all four right matters beyond correctness:
        // core-js feature-tests `new URLSearchParams(new URLSearchParams("a=b"))`
        // (and URL.username, live forEach) and REPLACES our whole URL/USP with
        // its own polyfill if any check fails — and that polyfill then misbehaves
        // on later pages (Twitch's app threw on it). Passing the battery keeps our
        // native, url-crate-backed implementation in play.
        constructor(init) {
            this.__p = [];
            if (init === undefined || init === null) return;
            if (init instanceof URLSearchParams) {
                for (const p of init.__p) this.__p.push([p[0], p[1]]);
            } else if (typeof init === "string") {
                for (const kv of init.replace(/^\?/, "").split("&")) {
                    if (!kv) continue;
                    const i = kv.indexOf("=");
                    this.__p.push(i < 0 ? [dec(kv), ""] : [dec(kv.slice(0, i)), dec(kv.slice(i + 1))]);
                }
            } else if (typeof init[Symbol.iterator] === "function") {
                for (const pair of init) {
                    const a = Array.from(pair);
                    this.__p.push([String(a[0]), String(a[1])]);
                }
            } else if (typeof init === "object") {
                for (const k of Object.keys(init)) this.__p.push([String(k), String(init[k])]);
            }
        }
        get(k) { const e = this.__p.find((p) => p[0] === String(k)); return e ? e[1] : null; }
        getAll(k) { return this.__p.filter((p) => p[0] === String(k)).map((p) => p[1]); }
        has(k) { return this.__p.some((p) => p[0] === String(k)); }
        set(k, v) { this.__p = this.__p.filter((p) => p[0] !== String(k)); this.__p.push([String(k), String(v)]); this.__notify(); }
        append(k, v) { this.__p.push([String(k), String(v)]); this.__notify(); }
        delete(k) { this.__p = this.__p.filter((p) => p[0] !== String(k)); this.__notify(); }
        get size() { return this.__p.length; }
        sort() { this.__p.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)); this.__notify(); }
        // Live binding to an owning URL (set by URL.searchParams). WHATWG makes
        // url.searchParams the URL's "query object": mutating it reflows the
        // URL's query. Undefined for a standalone URLSearchParams (no-op).
        __notify() { if (this.__url) this.__url.__setSearchFromParams(this.toString()); }
        // Rebuild the list from a query string (called by the owning URL when its
        // .search/.href is set, so the shared object stays in sync both ways).
        __setList(query) {
            this.__p = [];
            for (const kv of String(query).replace(/^\?/, "").split("&")) {
                if (!kv) continue;
                const i = kv.indexOf("=");
                this.__p.push(i < 0 ? [dec(kv), ""] : [dec(kv.slice(0, i)), dec(kv.slice(i + 1))]);
            }
        }
        // Live iteration (WebIDL maplike forEach): re-read length/index each step,
        // so deleting/appending during the callback affects what's visited — what
        // core-js's `r.delete("b")`-inside-forEach probe asserts ("a1c3").
        forEach(fn, thisArg) { for (let i = 0; i < this.__p.length; i++) { const e = this.__p[i]; fn.call(thisArg, e[1], e[0], this); } }
        keys() { return this.__p.map((p) => p[0])[Symbol.iterator](); }
        values() { return this.__p.map((p) => p[1])[Symbol.iterator](); }
        entries() { return this.__p.slice()[Symbol.iterator](); }
        [Symbol.iterator]() { return this.entries(); }
        toString() { return this.__p.map(([k, v]) => fenc(k) + "=" + fenc(v)).join("&"); }
    }
    // --- Blob URL store (File API §"Creating and Revoking a blob URL") ---
    // RAM-only, page-lifetime, per-realm: a map from a minted `blob:` URL string
    // to its Blob object, kept entirely in this JS realm — zero I/O, like the rest
    // of TRust's web storage. `createObjectURL` mints a spec-shaped URL + stores
    // the object; `revokeObjectURL` drops it; `fetch`/XHR below resolve a `blob:`
    // URL straight from the store WITHOUT touching the network syscall (a blob URL
    // never hits the wire). A null-proto object (not a Boa Map) keeps it off the
    // GC-iterator trap and it is only ever keyed by string.
    const __blobURLStore = Object.create(null);
    // A latin1 byte string of a Blob's underlying bytes: string parts UTF-8
    // encoded, BufferSource parts raw, nested Blobs recursed. More faithful than
    // our string-backed Blob.arrayBuffer() (which leaves binary empty), so a blob
    // built from a Uint8Array still round-trips through createObjectURL+fetch.
    function __blobBytes(b) {
        if (!b || !Array.isArray(b.__parts)) return "";
        let out = "";
        for (const p of b.__parts) {
            if (typeof p === "string") out += utf8Binary(p);
            else if (p instanceof ArrayBuffer) { const v = new Uint8Array(p); for (let i = 0; i < v.length; i++) out += String.fromCharCode(v[i]); }
            else if (p && typeof p.byteLength === "number" && p.buffer) { const v = new Uint8Array(p.buffer, p.byteOffset || 0, p.byteLength); for (let i = 0; i < v.length; i++) out += String.fromCharCode(v[i]); }
            else if (p && Array.isArray(p.__parts)) out += __blobBytes(p);
            else if (p != null) out += utf8Binary(String(p));
        }
        return out;
    }
    function __latin1ToBytes(s) { const u = new Uint8Array(s.length); for (let i = 0; i < s.length; i++) u[i] = s.charCodeAt(i) & 0xff; return u; }
    // Network responses arrive from Rust as a native ArrayBuffer. Keep the
    // latin1 loop only for in-realm data/blob URLs and old callers that still
    // provide the compatibility string representation.
    function __bodyBytes(value) {
        if (value instanceof ArrayBuffer) return new Uint8Array(value);
        if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        return __latin1ToBytes(value || "");
    }
    // Resolve a `blob:` URL to { bytes (latin1), type } or null (no entry → a
    // network error at the call site). Keyed without a fragment, per spec.
    function __resolveBlobURL(u) {
        const h = u.indexOf("#"); const key = h >= 0 ? u.slice(0, h) : u;
        const obj = __blobURLStore[key];
        if (!obj) return null;
        // Only Blob-shaped entries carry retrievable bytes; an unmodeled MediaSource
        // still mints a URL but yields no media here (no media pipeline — a
        // documented terminal deviation: we delegate playback to mpv).
        if (Array.isArray(obj.__parts)) return { bytes: __blobBytes(obj), type: obj.type || "" };
        return { bytes: "", type: "" };
    }
    class URL {
        // A WHATWG URL is a LIVE object: assigning any component re-serializes
        // href (and every other component). We keep the parsed parts in `__p`
        // (the 11-tuple __url_parse returns) and expose each field as an
        // accessor; a component setter runs the `__url_set` syscall (the url
        // crate's WHATWG setter algorithms) and swaps in the new parts. This is
        // load-bearing beyond correctness: the webcomponents/core-js URL
        // polyfills feature-test `u.pathname="c%20d"; u.href==="…/c%20d"` and, if
        // the native URL doesn't reflow, force-replace it with a searchParams-less
        // polyfill — which then throws "cannot convert undefined to object" the
        // moment a page reads `new URL(x).searchParams` (archive.org's item pages).
        constructor(href, base) {
            const r = __url_parse(String(href), base === undefined || base === null ? null : String(base));
            if (!r) throw new TypeError("Invalid URL: " + href);
            this.__p = r;      // [href, protocol, host, hostname, port, pathname, search, hash, origin, username, password]
            this.__sp = null;  // lazily-created bound URLSearchParams (the "query object")
        }
        get href() { return this.__p[0]; }
        // The href setter re-parses from scratch (no base) and throws on failure.
        set href(v) { const r = __url_parse(String(v), null); if (!r) throw new TypeError("Invalid URL: " + v); this.__p = r; this.__syncSP(); }
        get protocol() { return this.__p[1]; } set protocol(v) { this.__set("protocol", v); }
        get host() { return this.__p[2]; } set host(v) { this.__set("host", v); }
        get hostname() { return this.__p[3]; } set hostname(v) { this.__set("hostname", v); }
        get port() { return this.__p[4]; } set port(v) { this.__set("port", v); }
        get pathname() { return this.__p[5]; } set pathname(v) { this.__set("pathname", v); }
        get search() { return this.__p[6]; } set search(v) { this.__set("search", v); this.__syncSP(); }
        get hash() { return this.__p[7]; } set hash(v) { this.__set("hash", v); }
        get origin() { return this.__p[8]; }
        get username() { return this.__p[9]; } set username(v) { this.__set("username", v); }
        get password() { return this.__p[10]; } set password(v) { this.__set("password", v); }
        // Apply a component setter; a spec no-op (invalid value) returns the
        // unchanged parts, so href only moves when the assignment is valid.
        __set(which, v) { const r = __url_set(this.__p[0], which, String(v)); if (r) this.__p = r; }
        // Refresh the bound query object after .search/.href changes (one-way,
        // URL→params; the reverse, params→URL, is __setSearchFromParams).
        __syncSP() { if (this.__sp) this.__sp.__setList(this.__p[6]); }
        // Called BY the bound searchParams when it is mutated: reflow the query.
        __setSearchFromParams(qs) { const r = __url_set(this.__p[0], "search", qs); if (r) this.__p = r; }
        get searchParams() { if (!this.__sp) { this.__sp = new URLSearchParams(this.__p[6]); this.__sp.__url = this; } return this.__sp; }
        toString() { return this.__p[0]; }
        toJSON() { return this.__p[0]; }
        // createObjectURL/revokeObjectURL (File API). The minted URL is
        // `blob:<origin>/<uuid>`; the store is RAM-only (above).
        static createObjectURL(obj) {
            if (obj === null || typeof obj !== "object") throw new TypeError("Failed to execute 'createObjectURL' on 'URL': Overload resolution failed.");
            const origin = (g.location && g.location.origin) || "null";
            const u = "blob:" + (origin || "null") + "/" + g.crypto.randomUUID();
            __blobURLStore[u] = obj;
            // Mirror the bytes Rust-side so the APP can decode an
            // `<img src="blob:…">` (Steam's client-generated QR code); only
            // Blob-shaped objects carry bytes (a MediaSource mints a URL but
            // has no retrievable data here).
            if (Array.isArray(obj.__parts) && typeof __blob_mirror === "function") {
                try { __blob_mirror(u, __blobBytes(obj), obj.type || ""); } catch (e) {}
            }
            return u;
        }
        static revokeObjectURL(u) {
            u = String(u);
            const h = u.indexOf("#"); if (h >= 0) u = u.slice(0, h);
            if (u.slice(0, 5) !== "blob:") return;
            delete __blobURLStore[u];
        }
    }
    g.URLSearchParams = URLSearchParams;
    g.URL = URL;

    // --- URLPattern (URL Pattern Standard, https://urlpattern.spec.whatwg.org/) ---
    // A spec-aligned implementation of the common syntax: literal text, named
    // groups (:name) with the component's default segment wildcard, the full
    // wildcard (*), custom regex groups ((re) and :name(re)), the {…} grouping,
    // and the ? optional modifier with path-to-regexp's automatic-prefix rule (a
    // `/` before an optional pathname group is itself made optional). Each of the
    // eight components compiles to an anchored RegExp + an ordered group-name
    // list; test()/exec() parse the input (a URL string via __url_parse, or an
    // init object) into components and run each regex. Honest deferrals (NOT
    // faked): the +/* REPEAT modifiers compile to a functional (?:prefix(seg))+
    // form whose captured value is the last iteration (not path-to-regexp's whole-
    // run capture); the string constructor's authority parser handles
    // scheme://[user[:pass]@]host[:port]/path?search#hash but not every exotic
    // userinfo/`:port`-vs-`:group` ambiguity; a custom regex containing its OWN
    // capturing groups can misalign the named-group map. These are rarely-used
    // edges of an API most code drives with a single :name or * pathname pattern.
    // Wrapped as a self-contained block (its own `g`) + markers so the WORKER
    // realm reuses the SAME source — URLPattern is exposed in Workers (see
    // worker_prelude()); only __url_parse/RegExp (both present in both realms)
    // are referenced from outside.
    /*__URLPATTERN_BEGIN__*/
    (function (g) {
    const UP_COMPONENTS = ["protocol", "username", "password", "hostname", "port", "pathname", "search", "hash"];
    function upSeg(comp) { return comp === "pathname" ? "[^/]+?" : comp === "hostname" ? "[^.]+?" : ".+?"; }
    function upSepChar(comp) { return comp === "pathname" ? "/" : comp === "hostname" ? "." : ""; }
    function upEscRe(s) { return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"); }
    function upStripLead(s, ch) { return s && s.charAt(0) === ch ? s.slice(1) : s; }
    // Compile one component pattern into a shared state (names/unnamed/hasReg),
    // returning the anchored-body regex source. Recurses for {…} groups.
    function upCompileInto(pattern, seg, sep, st) {
        let re = "", lit = "", i = 0;
        const n = pattern.length;
        const flush = function () { if (lit) { re += upEscRe(lit); lit = ""; } };
        // Peel a trailing separator from the pending literal to use as a group's
        // automatic prefix (so `/books/:id?` makes the leading `/` optional too).
        const peel = function () {
            if (sep && lit.length && lit.charAt(lit.length - 1) === sep) { lit = lit.slice(0, -1); flush(); return sep; }
            flush(); return "";
        };
        const emit = function (prefix, body, name, mod) {
            st.names.push(name === null ? String(st.unnamed++) : name);
            const cap = "(" + body + ")", p = upEscRe(prefix);
            if (mod === "?") re += "(?:" + p + cap + ")?";
            else if (mod === "*") re += "(?:" + p + cap + ")*";
            else if (mod === "+") re += "(?:" + p + cap + ")+";
            else re += p + cap;
        };
        const readName = function () { let s = ""; while (i < n && /[A-Za-z0-9_$]/.test(pattern.charAt(i))) { s += pattern.charAt(i); i++; } return s; };
        const readParen = function () { let depth = 0, s = ""; for (; i < n; i++) { const c = pattern.charAt(i); if (c === "\\") { s += c + (pattern.charAt(i + 1) || ""); i++; continue; } if (c === "(") { depth++; if (depth === 1) continue; } else if (c === ")") { depth--; if (depth === 0) { i++; break; } } s += c; } return s; };
        const readBrace = function () { let depth = 0, s = ""; for (; i < n; i++) { const c = pattern.charAt(i); if (c === "\\") { s += c + (pattern.charAt(i + 1) || ""); i++; continue; } if (c === "{") { depth++; if (depth === 1) continue; } else if (c === "}") { depth--; if (depth === 0) { i++; break; } } s += c; } return s; };
        const readMod = function () { const c = pattern.charAt(i); if (c === "?" || c === "+" || c === "*") { i++; return c; } return ""; };
        while (i < n) {
            const c = pattern.charAt(i);
            if (c === "\\") { lit += pattern.charAt(i + 1) || ""; i += 2; continue; }
            if (c === ":") { i++; const nm = readName(); const pre = peel(); let body = seg; if (pattern.charAt(i) === "(") { body = readParen(); st.hasReg = true; } emit(pre, body, nm, readMod()); continue; }
            if (c === "(") { const pre = peel(); const body = readParen(); st.hasReg = true; emit(pre, body, null, readMod()); continue; }
            if (c === "*") { i++; const pre = peel(); emit(pre, ".*", null, readMod()); continue; }
            if (c === "{") { const pre = peel(); const inner = readBrace(); const mod = readMod(); const sub = upCompileInto(inner, seg, sep, st); const p = upEscRe(pre); if (mod === "?") re += "(?:" + p + sub + ")?"; else if (mod === "*") re += "(?:" + p + sub + ")*"; else if (mod === "+") re += "(?:" + p + sub + ")+"; else re += p + "(?:" + sub + ")"; continue; }
            lit += c; i++;
        }
        flush();
        return re;
    }
    function upCompile(pattern, comp) {
        const st = { names: [], unnamed: 0, hasReg: false };
        const src = upCompileInto(String(pattern), upSeg(comp), upSepChar(comp), st);
        return { source: "^" + src + "$", names: st.names, hasReg: st.hasReg };
    }
    // A parsed URL's 11-part __url_parse array → the eight URLPattern components
    // (protocol without its trailing `:`, search/hash without their `?`/`#`).
    function upFromParsed(p) {
        if (!p) return null;
        return {
            protocol: (p[1] || "").replace(/:$/, ""), username: p[9] || "", password: p[10] || "",
            hostname: p[3] || "", port: p[4] || "", pathname: p[5] || "",
            search: upStripLead(p[6] || "", "?"), hash: upStripLead(p[7] || "", "#"),
        };
    }
    function upResolveInput(input, base) {
        if (typeof input === "string") return upFromParsed(__url_parse(input, base != null ? String(base) : null));
        if (input && typeof input === "object") { const o = {}; for (const cc of UP_COMPONENTS) o[cc] = input[cc] !== undefined ? String(input[cc]) : ""; return o; }
        return null;
    }
    // Split a PATTERN STRING into its component patterns; omitted components
    // default to the wildcard "*" (or, with a baseURL, the leading components come
    // from the base). Hash/search are peeled first, then scheme://authority.
    function upParsePatternStr(str, base) {
        const out = { protocol: "*", username: "*", password: "*", hostname: "*", port: "*", pathname: "*", search: "*", hash: "*" };
        let s = String(str);
        const h = s.indexOf("#"); if (h >= 0) { out.hash = s.slice(h + 1); s = s.slice(0, h); }
        const q = s.indexOf("?"); if (q >= 0) { out.search = s.slice(q + 1); s = s.slice(0, q); }
        const pm = s.match(/^([^\/:{()}\\]+):\/\//);
        if (pm) {
            out.protocol = pm[1];
            s = s.slice(pm[0].length);
            const slash = s.indexOf("/");
            let authority = slash >= 0 ? s.slice(0, slash) : s;
            const rest = slash >= 0 ? s.slice(slash) : "";
            const at = authority.lastIndexOf("@");
            if (at >= 0) { const ui = authority.slice(0, at); authority = authority.slice(at + 1); const cix = ui.indexOf(":"); if (cix >= 0) { out.username = ui.slice(0, cix); out.password = ui.slice(cix + 1); } else out.username = ui; }
            const portm = authority.match(/:([0-9*][^\/]*)$/);
            if (portm) { out.port = portm[1]; out.hostname = authority.slice(0, authority.length - portm[0].length); }
            else out.hostname = authority;
            if (rest) out.pathname = rest;
        } else if (base) {
            const bp = upFromParsed(__url_parse(String(base), null));
            if (bp) { out.protocol = bp.protocol; out.username = bp.username || "*"; out.password = bp.password || "*"; out.hostname = bp.hostname; out.port = bp.port || "*"; }
            out.pathname = s || "*";
        } else out.pathname = s || "*";
        return out;
    }
    class URLPattern {
        constructor(input, a, b) {
            let options = {}, base;
            if (typeof input === "string") {
                if (typeof a === "string") { base = a; options = b || {}; } else options = a || {};
                this.__parts = upParsePatternStr(input, base);
            } else if (input && typeof input === "object") {
                options = a || {};
                const bp = input.baseURL ? upFromParsed(__url_parse(String(input.baseURL), null)) : null;
                this.__parts = {};
                for (const cc of UP_COMPONENTS) this.__parts[cc] = input[cc] !== undefined ? String(input[cc]) : (bp ? bp[cc] : "*");
            } else { this.__parts = {}; for (const cc of UP_COMPONENTS) this.__parts[cc] = "*"; }
            this.__ic = !!(options && options.ignoreCase);
            this.__re = {}; this.__hasReg = false;
            for (const cc of UP_COMPONENTS) {
                const cm = upCompile(this.__parts[cc], cc);
                this.__re[cc] = { rx: new RegExp(cm.source, this.__ic ? "i" : ""), names: cm.names };
                if (cm.hasReg) this.__hasReg = true;
            }
        }
        get protocol() { return this.__parts.protocol; }
        get username() { return this.__parts.username; }
        get password() { return this.__parts.password; }
        get hostname() { return this.__parts.hostname; }
        get port() { return this.__parts.port; }
        get pathname() { return this.__parts.pathname; }
        get search() { return this.__parts.search; }
        get hash() { return this.__parts.hash; }
        get hasRegExpGroups() { return this.__hasReg; }
        test(input, base) { return this.exec(input, base) !== null; }
        exec(input, base) {
            const parts = upResolveInput(input, base);
            if (!parts) return null;
            const out = { inputs: base != null ? [input, base] : [input] };
            for (const cc of UP_COMPONENTS) {
                const rc = this.__re[cc], val = parts[cc] || "";
                const m = rc.rx.exec(val);
                if (!m) return null;
                const groups = {};
                for (let k = 0; k < rc.names.length; k++) groups[rc.names[k]] = m[k + 1];
                out[cc] = { input: val, groups: groups };
            }
            return out;
        }
    }
    g.URLPattern = URLPattern;
    })(typeof globalThis !== "undefined" ? globalThis : this);
    /*__URLPATTERN_END__*/

    // --- the network, over the __http_fetch_async syscall ---
    // Requests fire as async jobs; the JS thread does NOT block on them,
    // so many in-flight fetches overlap and Promise.all runs them in
    // parallel. The promise settles when the bytes arrive. Only legacy
    // synchronous XHR still blocks (via the __http_fetch syscall).
    class Headers {
        constructor(init) {
            // Null-proto: header names are arbitrary strings, and a plain {}
            // leaks Object.prototype ("constructor" in {} is true, so
            // has("constructor") lied and get() returned a function).
            this.__h = Object.create(null);
            if (init) {
                // A sequence init APPENDS each pair (Fetch §Headers: "fill" runs
                // append), so `[["accept","a"],["accept","b"]]` combines to
                // "a, b" instead of the last one clobbering.
                if (Array.isArray(init)) { for (const kv of init) this.append(kv[0], kv[1]); }
                else if (init.__h) { Object.assign(this.__h, init.__h); }
                else if (typeof init === "object") { for (const k of Object.keys(init)) this.append(k, init[k]); }
            }
        }
        get(k) { const v = this.__h[String(k).toLowerCase()]; return v === undefined ? null : v; }
        set(k, v) { this.__h[String(k).toLowerCase()] = String(v); }
        // append COMBINES with an existing value (Fetch §"header list append":
        // `", "`-joined) — it is not set. Pages building multi-value headers
        // (Accept variants, custom lists) get the spec wire form.
        append(k, v) {
            const key = String(k).toLowerCase();
            const cur = this.__h[key];
            this.__h[key] = cur === undefined ? String(v) : cur + ", " + String(v);
        }
        has(k) { return String(k).toLowerCase() in this.__h; }
        delete(k) { delete this.__h[String(k).toLowerCase()]; }
        // Set-Cookie never reaches page JS (the Rust side strips it — a
        // forbidden response-header name), so the list is honestly empty.
        getSetCookie() { return []; }
        // Iteration is SORTED by (lowercased) name with combined values —
        // the Fetch spec's "sort and combine" — and Headers is iterable
        // (`for (const [k, v] of resp.headers)`).
        __sorted() { return Object.keys(this.__h).sort(); }
        forEach(fn, thisArg) { for (const k of this.__sorted()) fn.call(thisArg, this.__h[k], k, this); }
        keys() { return this.__sorted()[Symbol.iterator](); }
        values() { const out = []; for (const k of this.__sorted()) out.push(this.__h[k]); return out[Symbol.iterator](); }
        entries() { const out = []; for (const k of this.__sorted()) out.push([k, this.__h[k]]); return out[Symbol.iterator](); }
        [Symbol.iterator]() { return this.entries(); }
        get [Symbol.toStringTag]() { return "Headers"; }
    }
    g.Headers = Headers;
    // The wire body for a request/response: the platform accepts strings,
    // URLSearchParams, Blob/File, and ArrayBuffer views; our syscall takes a
    // string, so flatten to one. Unknown objects stringify (no multipart and no
    // FormData yet — the encoder just isn't built; the urlencoded path covers
    // ~every form), null stays null.
    const __bodyText = (body) => {
        if (body === null || body === undefined) return null;
        if (typeof body === "string") return body;
        if (body instanceof URLSearchParams) return body.toString();
        if (Array.isArray(body.__parts)) return new g.TextDecoder().decode(__latin1ToBytes(__blobBytes(body))); // Blob/File, byte-faithful
        if (typeof body.byteLength === "number") {
            try {
                const v = body instanceof ArrayBuffer ? new Uint8Array(body)
                    : new Uint8Array(body.buffer || body);
                let s = ""; for (let i = 0; i < v.length; i++) s += String.fromCharCode(v[i]);
                return s;
            } catch (e) { return ""; }
        }
        return String(body);
    };
    // The WIRE encoding of a request body: the exact bytes to put on the socket,
    // as a LATIN1 byte-string (one code unit per byte) the Rust syscall reads
    // byte-exact (`arg_bytes_latin1`). A text string is UTF-8-encoded (Fetch
    // §"Body" — a string body is UTF-8); URLSearchParams is UTF-8 of its
    // serialization; a Blob/File and an ArrayBuffer(view) are already raw bytes,
    // so they map straight to latin1. Without this a binary body (e.g. a page
    // that gzips its own POST — YouTube's `youtubei` continuation) was sent as
    // UTF-8 text, doubling every byte >= 0x80 and getting rejected.
    const __bodyWire = (body) => {
        if (body === null || body === undefined) return null;
        if (typeof body === "string") return utf8Binary(body);
        if (body instanceof URLSearchParams) return utf8Binary(body.toString());
        if (Array.isArray(body.__parts)) return __blobBytes(body); // Blob/File: the true bytes ARE the wire form
        if (typeof body.byteLength === "number") {
            try {
                const v = body instanceof ArrayBuffer ? new Uint8Array(body)
                    : new Uint8Array(body.buffer || body);
                let s = ""; for (let i = 0; i < v.length; i++) s += String.fromCharCode(v[i]);
                return s;
            } catch (e) { return ""; }
        }
        return utf8Binary(String(body));
    };
    // The multipart/form-data wire encoding of a FormData body (RFC 7578 +
    // HTML §"multipart/form-data encoding algorithm"): each entry is one part;
    // `"`/CR/LF in names and filenames percent-escape (%22/%0D/%0A), newlines
    // in string values normalize to CRLF, File parts carry their content type.
    // Returns the latin1 wire body plus the content type with its boundary.
    const __formDataWire = (fd) => {
        let boundary = "----TRustFormBoundary";
        for (let i = 0; i < 16; i++) boundary += "0123456789abcdef"[(Math.random() * 16) | 0];
        const escName = (s) => String(s).replace(/\r/g, "%0D").replace(/\n/g, "%0A").replace(/"/g, "%22");
        let out = "";
        const es = fd.__entries;
        for (let i = 0; i < es.length; i++) {
            const e = es[i];
            out += "--" + boundary + "\r\n";
            if (e.value && Array.isArray(e.value.__parts)) { // File
                out += utf8Binary('Content-Disposition: form-data; name="' + escName(e.name) + '"; filename="' + escName(e.value.name == null ? "blob" : e.value.name) + '"') + "\r\n";
                out += "Content-Type: " + (e.value.type || "application/octet-stream") + "\r\n\r\n";
                out += __blobBytes(e.value) + "\r\n";
            } else {
                out += utf8Binary('Content-Disposition: form-data; name="' + escName(e.name) + '"') + "\r\n\r\n";
                out += utf8Binary(String(e.value).replace(/\r\n|\r|\n/g, "\r\n")) + "\r\n";
            }
        }
        out += "--" + boundary + "--\r\n";
        return { wire: out, type: "multipart/form-data; boundary=" + boundary };
    };
    // The DEFAULT content type a body implies when the request sets none
    // (Fetch §"BodyInit extract"): a string is text/plain, URLSearchParams is
    // its form-urlencoded serialization, a Blob carries its own type, and raw
    // buffers advertise nothing. (FormData is handled at the call sites — its
    // type must carry the boundary of the encoded body.)
    const __bodyType = (body) => {
        if (body === null || body === undefined) return null;
        if (typeof body === "string") return "text/plain;charset=UTF-8";
        if (body instanceof URLSearchParams) return "application/x-www-form-urlencoded;charset=UTF-8";
        if (Array.isArray(body.__parts)) return body.type || null; // Blob/File
        if (typeof body.byteLength === "number") return null;
        return "text/plain;charset=UTF-8";
    };
    // Fetch §5.3 "consume body": fully read a ReadableStream BodyInit and join
    // its BufferSource chunks into one byte sequence before converting it to
    // the method-specific result. This is also the final consumer in the
    // standard CompressionStream example (`new Response(stream).arrayBuffer`).
    const __consumeBodyBytes = (owner) => {
        if (owner.__body instanceof g.ReadableStream) {
            if (owner.__bodyUsed || owner.__body.locked)
                return Promise.reject(new TypeError("Body is unusable"));
            owner.__bodyUsed = true;
            const reader = owner.__body.getReader();
            const chunks = [];
            let total = 0;
            const pump = () => reader.read().then((result) => {
                if (result.done) {
                    const all = new Uint8Array(total);
                    let offset = 0;
                    for (const chunk of chunks) { all.set(chunk, offset); offset += chunk.byteLength; }
                    return all;
                }
                const value = result.value;
                let view;
                if (value instanceof ArrayBuffer) view = new Uint8Array(value);
                else if (ArrayBuffer.isView(value)) view = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
                else throw new TypeError("ReadableStream body chunk must be a BufferSource");
                const copy = view.slice();
                total += copy.byteLength;
                if (total > 16 * 1024 * 1024) throw new RangeError("Body exceeds the 16 MiB page limit");
                chunks.push(copy);
                return pump();
            });
            return pump();
        }
        owner.__bodyUsed = true;
        const bin = owner.__bytes != null ? owner.__bytes : __bodyWire(owner.__body || "");
        return Promise.resolve(__bodyBytes(bin));
    };
    // Body mixin shared by Request and Response. Stream bodies follow Fetch's
    // disturbed/locked rules; legacy buffered bodies remain repeat-readable
    // for compatibility with defensive sites that probe a response twice.
    const __bodyMethods = {
        text() { return __consumeBodyBytes(this).then((bytes) => new g.TextDecoder().decode(bytes)); },
        json() { return this.text().then((text) => JSON.parse(text)); },
        arrayBuffer() {
            return __consumeBodyBytes(this).then((bytes) => bytes.buffer);
        },
        bytes() { return __consumeBodyBytes(this); },
        blob() {
            const t = (this.headers && this.headers.get && this.headers.get("content-type")) || "";
            return __consumeBodyBytes(this).then((bytes) => new g.Blob([bytes], { type: t || "" }));
        },
        formData() { return Promise.reject(new TypeError("formData unsupported")); },
    };
    // Fetch API Request (https://fetch.spec.whatwg.org/#request-class). The bare
    // `Request` global is referenced by many bundles (GitHub's react-core throws
    // a ReferenceError without it).
    class Request {
        constructor(input, init) {
            init = init || {};
            const fromReq = input instanceof Request;
            // Fetch §5.4: a Request input contributes its associated internal
            // request, not reads of its public Web IDL attributes. Keep every
            // request field in an internal slot and expose readonly prototype
            // accessors below. An author may shadow `request.url`/`method`/body
            // with own properties; cloning/fetch must still copy these slots.
            this.__url = fromReq ? input.__url
                : resolveURL(String((input && input.url !== undefined) ? input.url : input));
            this.__method = String(init.method || (fromReq ? input.__method : null) || "GET").toUpperCase();
            this.__headers = new Headers(init.headers !== undefined ? init.headers : (fromReq ? input.__headers : undefined));
            this.__body = init.body !== undefined ? init.body : (fromReq ? input.__body : null);
            this.__credentials = init.credentials || (fromReq ? input.__credentials : "same-origin");
            this.__mode = init.mode || (fromReq ? input.__mode : "cors");
            this.__cache = init.cache || (fromReq ? input.__cache : "default");
            this.__redirect = init.redirect || (fromReq ? input.__redirect : "follow");
            this.__referrer = init.referrer !== undefined ? init.referrer : (fromReq ? input.__referrer : "about:client");
            this.__referrerPolicy = init.referrerPolicy || (fromReq ? input.__referrerPolicy : "");
            this.__integrity = init.integrity || (fromReq ? input.__integrity : "");
            this.__keepalive = init.keepalive !== undefined ? !!init.keepalive : (fromReq ? input.__keepalive : false);
            this.__signal = init.signal || (fromReq ? input.__signal : null);
            this.__destination = "";
            this.__bodyUsed = false;
        }
        get url() { return this.__url; }
        get method() { return this.__method; }
        get headers() { return this.__headers; }
        get credentials() { return this.__credentials; }
        get mode() { return this.__mode; }
        get cache() { return this.__cache; }
        get redirect() { return this.__redirect; }
        get referrer() { return this.__referrer; }
        get referrerPolicy() { return this.__referrerPolicy; }
        get integrity() { return this.__integrity; }
        get keepalive() { return this.__keepalive; }
        get signal() { return this.__signal; }
        get destination() { return this.__destination; }
        get bodyUsed() { return this.__bodyUsed; }
        get body() { return null; } // no request ReadableStream yet
        clone() { return new Request(this); }
    }
    Object.assign(Request.prototype, __bodyMethods);
    g.Request = Request;
    // Fetch API Response (https://fetch.spec.whatwg.org/#response-class).
    class Response {
        constructor(body, init) {
            init = init || {};
            this.__body = body !== undefined ? body : null;
            this.status = init.status !== undefined ? (init.status | 0) : 200;
            this.statusText = init.statusText !== undefined ? String(init.statusText) : "";
            this.headers = new Headers(init.headers);
            this.ok = this.status >= 200 && this.status < 300;
            this.url = init.url ? String(init.url) : "";
            this.redirected = false;
            this.type = "default";
            this.__bodyUsed = false;
            this.__bodyStream = undefined;
        }
        get bodyUsed() { return this.__bodyUsed; }
        // The response body as a ReadableStream (lazy + cached). Streaming
        // consumers read `response.body.getReader()` — Open WebUI reads chat
        // completions (SSE) exactly this way; a null body made `getReader()`
        // throw, so the assistant reply never read back. Our network layer
        // buffers the whole body, so the stream yields it as one UTF-8 chunk then
        // closes; an SSE parser splits it identically. null only for empty bodies.
        get body() {
            if (this.__bodyStream !== undefined) return this.__bodyStream;
            if ((this.__body === null || this.__body === undefined) && this.__bytes == null) { this.__bodyStream = null; return null; }
            if (this.__body instanceof g.ReadableStream) { this.__bodyStream = this.__body; return this.__bodyStream; }
            // A fetched response streams its byte-exact native ArrayBuffer;
            // only a Response constructed in JS from a text body falls back
            // to UTF-8 bytes of that text.
            const bytes = this.__bytes != null
                ? __bodyBytes(this.__bytes)
                : new g.TextEncoder().encode(__bodyText(this.__body) || "");
            this.__bodyStream = new g.ReadableStream({
                start(c) { if (bytes.length) c.enqueue(bytes); c.close(); },
            });
            return this.__bodyStream;
        }
        clone() {
            const r = new Response(this.__body, { status: this.status, statusText: this.statusText, headers: this.headers, url: this.url });
            r.type = this.type; r.redirected = this.redirected; r.__bytes = this.__bytes; return r;
        }
        static error() { const r = new Response(null, { status: 0 }); r.type = "error"; return r; }
        static redirect(url, status) { const r = new Response(null, { status: status || 302 }); r.headers.set("location", String(url)); return r; }
        static json(data, init) { const r = new Response(JSON.stringify(data), init); if (!r.headers.has("content-type")) r.headers.set("content-type", "application/json"); return r; }
    }
    Object.assign(Response.prototype, __bodyMethods);
    g.Response = Response;
    // AbortSignal is a real EventTarget (it dispatches "abort"), and the
    // statics `abort`/`timeout`/`any` are widely referenced — YouTube's
    // kevlar bundle reads the bare `AbortSignal` global, a ReferenceError
    // without it.
    class AbortSignal extends EventTarget {
        constructor() { super(); this.aborted = false; this.reason = undefined; this.onabort = null; }
        throwIfAborted() { if (this.aborted) throw this.reason; }
        __abort(reason) {
            if (this.aborted) return;
            this.aborted = true;
            this.reason = reason !== undefined ? reason : new DOMException("signal is aborted without reason", "AbortError");
            const ev = new Event("abort");
            if (typeof this.onabort === "function") { try { this.onabort.call(this, ev); } catch (e) {} }
            this.dispatchEvent(ev);
        }
        static abort(reason) { const s = new AbortSignal(); s.__abort(reason); return s; }
        static timeout(ms) {
            const s = new AbortSignal();
            g.setTimeout(() => s.__abort(new DOMException("signal timed out", "TimeoutError")), Number(ms) || 0);
            return s;
        }
        static any(signals) {
            const s = new AbortSignal();
            for (const sig of signals || []) {
                if (sig && sig.aborted) { s.__abort(sig.reason); break; }
                if (sig && sig.addEventListener) sig.addEventListener("abort", () => s.__abort(sig.reason));
            }
            return s;
        }
    }
    g.AbortSignal = AbortSignal;
    g.AbortController = class AbortController {
        constructor() { this.signal = new AbortSignal(); }
        abort(reason) { this.signal.__abort(reason); }
    };

    // HTML §9.4: MessagePort owns a port-message TASK SOURCE. It is deliberately
    // separate from the timer queue: React's scheduler uses MessageChannel to
    // yield between units of work, and treating each post as setTimeout(0)
    // incorrectly applies timer cadence/clamping to runnable message tasks.
    const portMessages = [];
    class MessagePort extends EventTarget {
        constructor() {
            super(); this.__onmessage = null; this.__other = null;
            this.__frame = trust.__activeFrame || null;
            this.__started = false; this.__closed = false;
        }
        get onmessage() { return this.__onmessage; }
        set onmessage(handler) {
            this.__onmessage = typeof handler === "function" ? handler : null;
            // Setting onmessage enables the port message queue as if start()
            // had been called (HTML §9.4.4).
            if (this.__onmessage !== null) this.start();
        }
        postMessage(data) {
            const other = this.__other;
            if (!other || this.__closed || other.__closed) return;
            portMessages.push({ target: other, data: data, frame: other.__frame || null });
        }
        start() { if (!this.__closed) this.__started = true; }
        close() {
            this.__closed = true;
            const other = this.__other;
            this.__other = null;
            if (other && other.__other === this) other.__other = null;
            for (let i = portMessages.length - 1; i >= 0; i--) {
                if (portMessages[i].target === this) portMessages.splice(i, 1);
            }
        }
    }
    class MessageChannel {
        constructor() {
            this.port1 = new MessagePort(); this.port2 = new MessagePort();
            this.port1.__other = this.port2; this.port2.__other = this.port1;
        }
    }
    g.MessagePort = MessagePort; g.MessageChannel = MessageChannel;
    // WHATWG XHR §3.5.6 invokes response processing from Fetch's networking
    // task. It is deliberately not an author timer: replacing/clearing
    // setTimeout must not cancel readystatechange/load/loadend.
    trust.hasPortMessageTask = function () {
        for (const task of portMessages) {
            if (task.target.__started && !task.target.__closed) return true;
        }
        return false;
    };
    trust.runPortMessageTask = function () {
        let index = -1;
        for (let i = 0; i < portMessages.length; i++) {
            const target = portMessages[i].target;
            if (target.__started && !target.__closed) { index = i; break; }
        }
        if (index < 0) return false;
        const task = portMessages.splice(index, 1)[0];
        const target = task.target;
        try {
            runInFrame(task.frame, function () {
                const ev = new MessageEvent("message", { data: task.data, origin: "", source: null, ports: [] });
                if (typeof target.onmessage === "function") {
                    try { target.onmessage.call(target, ev); }
                    catch (e) { trust.errors.push("message port: " + ((e && e.message) || e)); }
                }
                target.dispatchEvent(ev);
            });
        } catch (e) { trust.errors.push("message port: " + ((e && e.message) || e)); }
        return true;
    };
    trust.hasPostedMessageTask = function () { return messageTasks.length > 0; };
    trust.runPostedMessageTask = function () {
        if (!messageTasks.length) return false;
        const task = messageTasks.shift();
        try { runInFrame(task.frame, task.fn); }
        catch (e) { trust.errors.push("message task: " + ((e && e.message) || e)); }
        return true;
    };
    trust.hasMessageTask = function () {
        return trust.hasPostedMessageTask() || trust.hasPortMessageTask();
    };
    // Window.postMessage uses HTML's posted-message task source. Every enabled
    // MessagePort instead contributes its queue through the unshipped port
    // message task source (HTML §9.3.3 and §9.4.4). They are not one priority
    // queue: a self-replenishing posted-message loop must not strand a port.
    // Keep this compatibility helper fair too, although normal event-loop
    // selection below treats the sources independently.
    let messageSourceCursor = 0;
    trust.runMessageTask = function () {
        for (let offset = 0; offset < 2; ++offset) {
            const source = (messageSourceCursor + offset) % 2;
            if (source === 0 && trust.hasPostedMessageTask()) {
                messageSourceCursor = 1;
                return trust.runPostedMessageTask();
            }
            if (source === 1 && trust.hasPortMessageTask()) {
                messageSourceCursor = 0;
                return trust.runPortMessageTask();
            }
        }
        return false;
    };
    // HTML leaves selection among runnable task sources implementation-defined,
    // while requiring the event loop to keep making progress. Rotate among the
    // five represented sources so a self-replenishing source cannot starve
    // another one. FIFO ordering remains intact within each source.
    let platformSourceCursor = 0;
    trust.hasPlatformTask = function () {
        return networkTasks.length > 0 || domTasks.length > 0 || trust.hasMessageTask() || idleTasks.length > 0;
    };
    trust.runPlatformTask = function () {
        for (let offset = 0; offset < 5; offset++) {
            const source = (platformSourceCursor + offset) % 5;
            let task = null;
            let label = "";
            if (source === 0 && networkTasks.length) {
                task = networkTasks.shift(); label = "network task";
            } else if (source === 1 && domTasks.length) {
                task = domTasks.shift(); label = "DOM manipulation task";
            } else if (source === 2 && trust.hasPostedMessageTask()) {
                platformSourceCursor = 3;
                return trust.runPostedMessageTask();
            } else if (source === 3 && trust.hasPortMessageTask()) {
                platformSourceCursor = 4;
                return trust.runPortMessageTask();
            } else if (source === 4 && idleTasks.length) {
                platformSourceCursor = 0;
                return trust.runIdleTask();
            }
            if (task) {
                platformSourceCursor = (source + 1) % 5;
                try { runInFrame(task.frame, task.fn); }
                catch (e) { trust.errors.push(label + ": " + ((e && e.message) || e)); }
                return true;
            }
        }
        return false;
    };
    // Diagnostic-only queue census used by the low-rate resident-actor trace.
    // It observes queue state without selecting or running a task.
    trust.taskQueueState = function () {
        let oneShots = 0, intervals = 0;
        for (const timer of timers.q) {
            if (timer.every === null) oneShots++;
            else intervals++;
        }
        const sample = timers.q.slice().sort((a, b) => a.at - b.at || a.id - b.id)
            .slice(0, 4).map(function (timer) {
                let handler;
                try { handler = Function.prototype.toString.call(timer.fn); }
                catch (_) { handler = timer.fn && timer.fn.name || "<unknown>"; }
                handler = String(handler).replace(/\s+/g, " ").slice(0, 72);
                return Math.round(timer.at - currentTime()) + "ms/n" + timer.nesting + "/w" + timer.wait + ":" + handler;
            }).join("|");
        return "timers=" + timers.q.length + "(once=" + oneShots + ",interval=" + intervals + ")" +
            ",raf=" + animationFrames.q.length +
            ",idle=" + idleCallbacks.pending.length + "/" + idleCallbacks.runnable.length + "/" + idleTasks.length +
            ",network=" + networkTasks.length + ",dom=" + domTasks.length +
            ",posted=" + messageTasks.length + ",next=[" + sample + "]";
    };

    // BroadcastChannel: same-origin cross-context messaging. A terminal
    // browser has one page (no other tabs/workers), so the only peers are
    // other channels of the same name in THIS page — we deliver to them
    // (excluding the sender, per spec) on a macrotask, and a lone channel
    // simply never receives, exactly as a single tab would. SvelteKit opens
    // one at boot for session sync; a missing global was a ReferenceError
    // that aborted the whole app mount. `BC` maps name→array of live
    // channels (an array, never iterated as a Boa Map — see the MO trap).
    const BC = new Map();
    class BroadcastChannel extends EventTarget {
        constructor(name) {
            super();
            this.name = String(name);
            this.onmessage = null; this.onmessageerror = null;
            this.__closed = false;
            let list = BC.get(this.name);
            if (!list) { list = []; BC.set(this.name, list); }
            list.push(this);
        }
        postMessage(message) {
            if (this.__closed) throw new DOMException("channel is closed", "InvalidStateError");
            const list = BC.get(this.name) || [];
            for (const ch of list.slice()) {
                if (ch === this || ch.__closed) continue;
                __queue_dom_task(() => {
                    if (ch.__closed) return;
                    const ev = new MessageEvent("message", {
                        data: message,
                        origin: (g.location && g.location.origin) || "",
                    });
                    if (typeof ch.onmessage === "function") { try { ch.onmessage.call(ch, ev); } catch (e) {} }
                    ch.dispatchEvent(ev);
                }, 0);
            }
        }
        close() {
            this.__closed = true;
            const list = BC.get(this.name);
            if (list) { const i = list.indexOf(this); if (i >= 0) list.splice(i, 1); }
        }
    }
    g.BroadcastChannel = BroadcastChannel;

    // --- WebSocket (RFC 6455 transport in ws.rs; socket.io rides it) ---
    // A real connection: `__ws_open` spawns the Rust task, inbound frames arrive
    // as `__trust.wsEvent` calls (the actor dispatches them like clicks). This is
    // what lets a websocket-enabled app (Open WebUI) stream chat tokens back —
    // the page's own socket.io-client runs the protocol over these frames.
    const WS_REGISTRY = {};
    class WebSocket extends EventTarget {
        constructor(url, protocols) {
            super();
            let parsed;
            try {
                parsed = new g.URL(String(url), (g.document && g.document.baseURI) || (g.location && g.location.href) || "about:blank");
            } catch (_) {
                throw new DOMException("Invalid WebSocket URL", "SyntaxError");
            }
            if (parsed.protocol === "http:") parsed.protocol = "ws:";
            else if (parsed.protocol === "https:") parsed.protocol = "wss:";
            if ((parsed.protocol !== "ws:" && parsed.protocol !== "wss:") || parsed.hash) {
                throw new DOMException("Invalid WebSocket URL", "SyntaxError");
            }
            let protocolList;
            if (protocols === undefined) protocolList = [];
            else if (typeof protocols === "string") protocolList = [protocols];
            else if (protocols !== null && protocols[Symbol.iterator] !== undefined) protocolList = Array.from(protocols, (value) => String(value));
            else protocolList = [String(protocols)];
            const protocolToken = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
            for (let i = 0; i < protocolList.length; i++) {
                const protocol = protocolList[i];
                if (!protocolToken.test(protocol) || protocolList.indexOf(protocol) !== i) {
                    throw new DOMException("Invalid WebSocket subprotocol", "SyntaxError");
                }
            }
            this.url = parsed.href;
            this.readyState = 0; // CONNECTING
            this.bufferedAmount = 0;
            this.extensions = "";
            this.protocol = "";
            this.__binaryType = "blob";
            this.__id = __ws_open(this.url, protocolList.join(","));
            if (this.__id < 0) {
                // Synchronous open failure (bad URL / blocked / no net grant):
                // a browser still reports it asynchronously as error + close.
                const self = this;
                g.setTimeout(() => {
                    self.readyState = 3;
                    self.__fire("error", {});
                    self.__fire("close", { code: 1006, reason: "", wasClean: false });
                }, 0);
            } else {
                WS_REGISTRY[this.__id] = this;
            }
        }
        get CONNECTING() { return 0; } get OPEN() { return 1; }
        get CLOSING() { return 2; } get CLOSED() { return 3; }
        get binaryType() { return this.__binaryType; }
        set binaryType(value) {
            value = String(value);
            if (value !== "blob" && value !== "arraybuffer") throw new TypeError("Invalid WebSocket binaryType");
            this.__binaryType = value;
        }
        send(data) {
            if (this.readyState === 0) throw new DOMException("WebSocket is still CONNECTING", "InvalidStateError");
            let wire, binary = false, byteLength;
            if (typeof data === "string") {
                wire = data;
                byteLength = new g.TextEncoder().encode(data).byteLength;
            } else if (data instanceof g.Blob) {
                wire = __blobBytes(data); binary = true; byteLength = wire.length;
            } else {
                let bytes = null;
                if (data instanceof ArrayBuffer) bytes = new Uint8Array(data);
                else if (data && data.buffer instanceof ArrayBuffer) bytes = new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength);
                if (bytes) {
                    wire = ""; for (let i = 0; i < bytes.length; i++) wire += String.fromCharCode(bytes[i]);
                    binary = true; byteLength = bytes.length;
                } else {
                    wire = String(data);
                    byteLength = new g.TextEncoder().encode(wire).byteLength;
                }
            }
            this.bufferedAmount += byteLength;
            if (this.readyState === 1) __ws_send(this.__id, wire, binary);
        }
        close(code, reason) {
            if (code !== undefined) {
                code = Number(code);
                code = !Number.isFinite(code) ? 0 : Math.max(0, Math.min(65535, Math.round(code)));
                if (code !== 1000 && (code < 3000 || code > 4999)) {
                    throw new DOMException("Invalid WebSocket close code", "InvalidAccessError");
                }
            }
            reason = reason === undefined ? "" : String(reason);
            if (new g.TextEncoder().encode(reason).byteLength > 123) {
                throw new DOMException("WebSocket close reason exceeds 123 UTF-8 bytes", "SyntaxError");
            }
            if (this.readyState >= 2) return;
            this.readyState = 2; // CLOSING
            __ws_close(this.__id, code === undefined ? 0 : code, reason);
        }
        __fire(type, init) {
            let ev;
            if (type === "message") ev = createTrustedEvent(MessageEvent, "message", init);
            else if (type === "close") ev = createTrustedEvent(CloseEvent, "close", init);
            else ev = createTrustedEvent(Event, type, init);
            dispatch(this, ev, false);
        }
    }
    installHandlerProps(WebSocket.prototype, ["open", "message", "error", "close"]);
    WebSocket.CONNECTING = 0; WebSocket.OPEN = 1; WebSocket.CLOSING = 2; WebSocket.CLOSED = 3;
    g.WebSocket = WebSocket;
    // The actor calls this for every inbound WebSocket event (open/message/close).
    trust.wsEvent = function (id, kind, data, isBinary, code, reason, wasClean, failed, protocol) {
        const ws = WS_REGISTRY[id];
        if (!ws) return;
        if (kind === "open") {
            ws.readyState = 1; // OPEN
            ws.protocol = protocol || "";
            ws.__fire("open", {});
        } else if (kind === "message") {
            let payload = data;
            if (isBinary) {
                const len = data.length, buf = new ArrayBuffer(len), view = new Uint8Array(buf);
                for (let i = 0; i < len; i++) view[i] = data.charCodeAt(i) & 0xFF;
                payload = (ws.binaryType === "arraybuffer") ? buf : new g.Blob([buf]);
            }
            let origin = "";
            try { origin = new g.URL(ws.url).origin; } catch (_) {}
            ws.__fire("message", { data: payload, origin: origin });
        } else if (kind === "drain") {
            ws.bufferedAmount = Math.max(0, ws.bufferedAmount - (Number(code) || 0));
        } else if (kind === "close") {
            ws.readyState = 3; // CLOSED
            delete WS_REGISTRY[id];
            if (failed) ws.__fire("error", {});
            ws.__fire("close", { code: code, reason: reason || "", wasClean: !!wasClean });
        }
    };

    // ---- Web Workers --------------------------------------------------------
    // The structured-clone WIRE CODEC (HTML "StructuredSerialize" /
    // "StructuredDeserialize"): serialize a value to a self-contained JSON string
    // and back, for cross-thread postMessage. OBJECTS enter a heap array
    // (referenced by index) so cycles and shared identity round-trip; primitives
    // encode inline. Symbols, functions, and DOM/platform nodes throw
    // DataCloneError, per spec. This exact block is SHARED with every worker —
    // the Rust `worker_prelude()` extracts it between the two markers below, so
    // edit it ONCE here. (Self-contained IIFE over globalThis → runs identically
    // in the page realm and a worker realm.)
    /*__SC_CODEC_BEGIN__*/
    (function (G) {
        var TYPED = { Int8Array: 1, Uint8Array: 1, Uint8ClampedArray: 1, Int16Array: 1, Uint16Array: 1, Int32Array: 1, Uint32Array: 1, Float32Array: 1, Float64Array: 1, BigInt64Array: 1, BigUint64Array: 1 };
        function dce(what) {
            try { return new (G.DOMException || Error)(what + " could not be cloned.", "DataCloneError"); }
            catch (e) { var er = new Error(what + " could not be cloned."); er.name = "DataCloneError"; return er; }
        }
        // A Blob/File's true bytes as a latin1 string: string parts UTF-8,
        // BufferSource parts raw, nested blobs recursed — so a binary Blob
        // survives postMessage instead of arriving empty. Self-contained
        // (the codec runs in both the page and worker realms).
        function blobBytes(b) {
            var parts = b.__parts || [], out = "", i, j, v, p;
            for (i = 0; i < parts.length; i++) {
                p = parts[i];
                if (typeof p === "string") { v = new G.TextEncoder().encode(p); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
                else if (p instanceof ArrayBuffer) { v = new Uint8Array(p); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
                else if (p && typeof p.byteLength === "number" && p.buffer) { v = new Uint8Array(p.buffer, p.byteOffset || 0, p.byteLength); for (j = 0; j < v.length; j++) out += String.fromCharCode(v[j]); }
                else if (p && p.__parts) out += blobBytes(p);
            }
            return out;
        }
        // The decode side: rebuild the bytes as a Uint8Array part (a string
        // part would be re-UTF-8'd by the byte-faithful Blob readers).
        function blobPart(s) {
            var u = new Uint8Array(s.length);
            for (var i = 0; i < s.length; i++) u[i] = s.charCodeAt(i) & 0xff;
            return u;
        }
        function enc(value, heap, seen) {
            var t = typeof value;
            if (value === undefined) return ["u"];
            if (value === null) return ["z"];
            if (t === "boolean") return ["b", value];
            if (t === "string") return ["s", value];
            if (t === "number") {
                if (value !== value) return ["fs", "n"];
                if (value === Infinity) return ["fs", "i"];
                if (value === -Infinity) return ["fs", "g"];
                if (value === 0 && 1 / value === -Infinity) return ["fs", "z"];
                return ["d", value];
            }
            if (t === "bigint") return ["bi", value.toString()];
            if (t === "symbol") throw dce("A Symbol");
            if (t === "function") throw dce("A function");
            if (seen.has(value)) return ["r", seen.get(value)];
            var idx = heap.length;
            heap.push(0);
            seen.set(value, idx);
            heap[idx] = encObj(value, heap, seen);
            return ["r", idx];
        }
        function encObj(v, heap, seen) {
            if (G.Node && v instanceof G.Node) throw dce("A DOM node");
            if (v instanceof Date) return ["D", v.getTime()];
            if (v instanceof RegExp) return ["R", v.source, v.flags];
            if (typeof Map !== "undefined" && v instanceof Map) {
                var m = []; v.forEach(function (val, key) { m.push([enc(key, heap, seen), enc(val, heap, seen)]); });
                return ["M", m];
            }
            if (typeof Set !== "undefined" && v instanceof Set) {
                var s = []; v.forEach(function (val) { s.push(enc(val, heap, seen)); });
                return ["S", s];
            }
            if (v instanceof ArrayBuffer) return ["AB", Array.prototype.slice.call(new Uint8Array(v))];
            var cn = v.constructor && v.constructor.name;
            if (cn === "DataView" && v.buffer instanceof ArrayBuffer) return ["DV", enc(v.buffer, heap, seen), v.byteOffset, v.byteLength];
            if (cn && TYPED[cn] && v.buffer instanceof ArrayBuffer) return ["TA", cn, enc(v.buffer, heap, seen), v.byteOffset, v.length];
            if (G.File && v instanceof G.File) return ["F", blobBytes(v), v.type || "", v.name || "", v.lastModified || 0];
            if (G.Blob && v instanceof G.Blob) return ["B", blobBytes(v), v.type || ""];
            if (v instanceof Error) return ["E", v.name || "Error", v.message || "", v.stack || "", (G.DOMException && v instanceof G.DOMException) ? 1 : 0];
            if (Array.isArray(v)) {
                var ap = [];
                for (var k in v) if (Object.prototype.hasOwnProperty.call(v, k)) ap.push([k, enc(v[k], heap, seen)]);
                return ["A", v.length, ap];
            }
            var op = [], keys = Object.keys(v);
            for (var i = 0; i < keys.length; i++) op.push([keys[i], enc(v[keys[i]], heap, seen)]);
            return ["O", op];
        }
        G.__sc_serialize = function (value) {
            var heap = [];
            var root = enc(value, heap, new Map());
            return JSON.stringify([root, heap]);
        };
        // Pass 1: build leaves fully + empty containers (so refs/cycles resolve).
        function shell(node) {
            switch (node[0]) {
                case "D": return new Date(node[1]);
                case "R": return new RegExp(node[1], node[2]);
                case "M": return new Map();
                case "S": return new Set();
                case "AB": return new Uint8Array(node[1]).buffer;
                case "F": return G.File ? new G.File([blobPart(node[1])], node[3], { type: node[2], lastModified: node[4] }) : new G.Blob([blobPart(node[1])], { type: node[2] });
                case "B": return G.Blob ? new G.Blob([blobPart(node[1])], { type: node[2] }) : { __blobText: node[1], type: node[2] };
                case "E": {
                    // HTML structured deserialize: a name from the native-error
                    // set reconstructs the matching subclass (instanceof
                    // survives the round trip); a DOMException goes back
                    // through its constructor keeping its name; any other name
                    // rides a plain Error.
                    var nm = node[1] || "Error", e;
                    if (node[4] && typeof G.DOMException === "function") e = new G.DOMException(node[2], nm);
                    else if (nm === "EvalError" || nm === "RangeError" || nm === "ReferenceError" || nm === "SyntaxError" || nm === "TypeError" || nm === "URIError") e = new G[nm](node[2]);
                    else { e = new Error(node[2]); if (nm !== "Error") try { e.name = nm; } catch (x) {} }
                    if (node[3]) try { e.stack = node[3]; } catch (x) {}
                    return e;
                }
                case "A": return new Array(node[1]);
                case "O": return {};
            }
            return null; // TA / DV: built in pass 2 (need the buffer ref resolved)
        }
        function decRef(val, built) {
            switch (val[0]) {
                case "u": return undefined;
                case "z": return null;
                case "b": case "s": case "d": return val[1];
                case "fs": return val[1] === "n" ? NaN : val[1] === "i" ? Infinity : val[1] === "g" ? -Infinity : -0;
                case "bi": return BigInt(val[1]);
                case "r": return built[val[1]];
            }
            return undefined;
        }
        G.__sc_deserialize = function (str) {
            var parsed = JSON.parse(str), root = parsed[0], heap = parsed[1];
            var built = new Array(heap.length), i;
            for (i = 0; i < heap.length; i++) built[i] = shell(heap[i]);
            // Pass 2: typed arrays / DataView (their buffer is an AB leaf, now built).
            for (i = 0; i < heap.length; i++) {
                var n = heap[i];
                if (n[0] === "TA") built[i] = new G[n[1]](decRef(n[2], built), n[3], n[4]);
                else if (n[0] === "DV") built[i] = new DataView(decRef(n[1], built), n[2], n[3]);
            }
            // Pass 3: fill containers (all referents now exist → cycles close).
            for (i = 0; i < heap.length; i++) {
                var node = heap[i], obj = built[i], j;
                if (node[0] === "M") { for (j = 0; j < node[1].length; j++) obj.set(decRef(node[1][j][0], built), decRef(node[1][j][1], built)); }
                else if (node[0] === "S") { for (j = 0; j < node[1].length; j++) obj.add(decRef(node[1][j], built)); }
                else if (node[0] === "A") { for (j = 0; j < node[2].length; j++) obj[node[2][j][0]] = decRef(node[2][j][1], built); }
                else if (node[0] === "O") { for (j = 0; j < node[1].length; j++) obj[node[1][j][0]] = decRef(node[1][j][1], built); }
            }
            return decRef(root, built);
        };
    })(typeof globalThis !== "undefined" ? globalThis : this);
    /*__SC_CODEC_END__*/

    // The page side of Web Workers. `new Worker(url)` spawns a real second engine
    // on its own thread (`__worker_spawn`); messages cross as structured-clone
    // wire strings. Worker→page events arrive via `trust.workerMessage/Error`
    // (the actor dispatches them like a click). Mirrors the WebSocket class.
    trust.workers = {};
    class Worker extends EventTarget {
        constructor(url, options) {
            super();
            options = options || {};
            const type = options.type === undefined ? "classic" : String(options.type);
            if (type !== "classic" && type !== "module") throw new TypeError("Invalid Worker type");
            if (options.credentials !== undefined && !["omit", "same-origin", "include"].includes(String(options.credentials))) {
                throw new TypeError("Invalid Worker credentials mode");
            }
            const name = options.name != null ? String(options.name) : "";
            let href;
            try { href = new g.URL(String(url), g.location.href).href; }
            catch (e) { throw new DOMException("Invalid worker script URL", "SyntaxError"); }
            // HTML's Worker constructor fetches Blob URLs from the File API
            // Blob URL Store. The store is authoritative in this realm; pass
            // an active entry's byte string to the Rust worker launcher rather
            // than asking the HTTP client to dereference a `blob:` URL. This
            // also makes a revoked URL fail closed because __resolveBlobURL()
            // returns null after URL.revokeObjectURL().
            let blobSource = null;
            if (href.slice(0, 5) === "blob:") {
                const entry = __resolveBlobURL(href);
                if (entry) blobSource = entry.bytes;
            }
            this.__id = __worker_spawn(href, type, name, blobSource);
            if (this.__id > 0) trust.workers[this.__id] = this;
        }
        postMessage(message, _transfer) {
            if (this.__id <= 0) return;
            // Structured clone (may throw DataCloneError synchronously, per spec).
            __worker_post(this.__id, g.__sc_serialize(message));
        }
        terminate() {
            if (this.__id > 0) {
                __worker_terminate(this.__id);
                delete trust.workers[this.__id];
                this.__id = -1;
            }
        }
        __fire(type, ev) { dispatch(this, ev, false); }
    }
    installHandlerProps(Worker.prototype, ["message", "messageerror", "error"]);
    g.Worker = Worker;
    trust.workerMessage = function (id, s) {
        const w = trust.workers[id];
        if (!w) return;
        let data;
        try { data = g.__sc_deserialize(s); }
        catch (e) { w.__fire("messageerror", createTrustedEvent(MessageEvent, "messageerror", { origin: "" })); return; }
        w.__fire("message", createTrustedEvent(MessageEvent, "message", { data: data, origin: "" }));
    };
    trust.workerError = function (id, msg) {
        const w = trust.workers[id];
        if (w) w.__fire("error", createTrustedEvent(ErrorEvent, "error", { message: String(msg), cancelable: true }));
    };

    // Flatten a header map ({lowercased-name: value}) into the `k\nv\nk\nv`
    // blob the `__http_fetch` syscalls forward to the request. Lets a page's
    // `setRequestHeader`/`init.headers` (X-Requested-With, Authorization, …)
    // actually reach the wire instead of being dropped.
    function __hdrBlob(h) {
        let s = "";
        for (const k in h) {
            if (!Object.prototype.hasOwnProperty.call(h, k)) continue;
            s += (s ? "\n" : "") + k + "\n" + h[k];
        }
        return s;
    }
    // Decode a `data:` URL (RFC 2397) to response parts, or null on a
    // malformed one: { ctype, text, bytes } — bytes as a latin1 string.
    // base64 payloads go through the strict atob; percent-encoded payloads
    // decode BYTEWISE (`%XX` → the byte; `+` stays literal — the trap the
    // Instagram data:-script work pinned). Serves fetch()/XHR below, so a
    // page's `fetch("data:…")` resolves in-realm instead of being rejected
    // by the http(s)-only network syscall.
    function __dataURLParts(u) {
        const m = /^data:([^,]*),([\s\S]*)$/.exec(u);
        if (!m) return null;
        let meta = m[1];
        let b64 = false;
        if (/;base64$/i.test(meta)) { b64 = true; meta = meta.replace(/;base64$/i, ""); }
        let bytes;
        if (b64) {
            try { bytes = g.atob(m[2].replace(/[\t\n\f\r ]+/g, "")); } catch (e) { return null; }
        } else {
            bytes = m[2].replace(/%([0-9a-fA-F]{2})/g, (_, h) => String.fromCharCode(parseInt(h, 16)));
        }
        return {
            ctype: meta || "text/plain;charset=US-ASCII",
            text: new g.TextDecoder().decode(__latin1ToBytes(bytes)),
            bytes: bytes,
        };
    }
    // The inverse, for RESPONSE headers: the syscalls return all response
    // headers as the same `name\nvalue\n…` blob (r[4]); split it back into a
    // lowercased name→value map for `Response.headers` / XHR header getters.
    function __parseHdrBlob(s) {
        const out = {};
        if (!s) return out;
        const parts = String(s).split("\n");
        for (let i = 0; i + 1 < parts.length; i += 2) {
            if (parts[i]) out[parts[i].toLowerCase()] = parts[i + 1];
        }
        return out;
    }
    g.fetch = function (input, init) {
        try {
            // Normalize input+init into a Request (input may be a URL string,
            // a Request, or a URL object).
            const req = new Request(input, init);
            // Operate on the associated request, never on shadowable public
            // attributes (Fetch §5.6 step 3). YouTube intentionally shadows
            // these getters as a platform-integrity probe.
            const url = req.__url;
            // AbortSignal (Fetch §"abort fetch"): an already-aborted signal
            // rejects immediately with its reason; an abort while in flight
            // wins the race below. The wire request itself isn't torn down —
            // it completes into a dropped promise — but the OBSERVABLE
            // contract (the rejection, and stale responses never reaching
            // .then) is the part pages depend on (abort-and-retype search).
            const sig = req.__signal;
            const abortReason = () => (sig && sig.reason !== undefined && sig.reason !== null)
                ? sig.reason : new DOMException("The operation was aborted.", "AbortError");
            if (sig && sig.aborted) return Promise.reject(abortReason());
            const raceAbort = (p) => {
                if (!sig || typeof sig.addEventListener !== "function") return p;
                return new Promise((resolve, reject) => {
                    let done = false;
                    const onAbort = () => { if (!done) { done = true; reject(abortReason()); } };
                    sig.addEventListener("abort", onAbort, { once: true });
                    p.then(
                        (v) => { if (!done) { done = true; sig.removeEventListener("abort", onAbort); resolve(v); } },
                        (e) => { if (!done) { done = true; sig.removeEventListener("abort", onAbort); reject(e); } }
                    );
                });
            };
            // A `data:` URL decodes in-realm (Fetch §"scheme fetch" for data)
            // — the network syscall is http(s)-only and used to reject these.
            if (url.slice(0, 5) === "data:") {
                const dp = __dataURLParts(url);
                if (!dp) return Promise.reject(new TypeError("fetch failed: invalid data: URL"));
                const dresp = new Response(dp.text, { status: 200, statusText: "", headers: { "content-type": dp.ctype }, url: url });
                dresp.type = "basic";
                dresp.__bytes = dp.bytes; // byte-exact body for arrayBuffer()/blob()
                return Promise.resolve(dresp);
            }
            // A `blob:` URL is served from the in-realm store, never the wire
            // (File API / Fetch §"scheme fetch" for blob). Missing/revoked entry
            // → network error (a rejected fetch), exactly like the platform.
            if (url.slice(0, 5) === "blob:") {
                const be = __resolveBlobURL(url);
                if (!be) return Promise.reject(new TypeError("fetch failed: no blob URL entry: " + url));
                const bh = {}; if (be.type) bh["content-type"] = be.type;
                const bresp = new Response(new g.TextDecoder().decode(__latin1ToBytes(be.bytes)), { status: 200, statusText: "", headers: bh, url: url });
                bresp.type = "basic";
                bresp.__bytes = be.bytes; // byte-exact body for arrayBuffer()/blob()
                return Promise.resolve(bresp);
            }
            const isFD = req.__body instanceof g.FormData && Array.isArray(req.__body.__entries);
            const enc = isFD ? __formDataWire(req.__body) : null;
            const body = isFD ? enc.wire : __bodyWire(req.__body);
            const ctype = req.__headers.get("content-type")
                || (isFD ? enc.type : __bodyType(req.__body));
            return raceAbort(__http_fetch_async(url, req.__method, body, ctype, __hdrBlob(req.__headers.__h)).then(function (r) {
                if (!r) throw new TypeError("fetch failed or blocked: " + url);
                const status = r[0], respCType = r[1], text = r[2];
                // All response headers (pages read out-of-band API results —
                // Steam's `x-eresult`); older 3/4-element shapes degrade to
                // content-type only.
                const hdrs = __parseHdrBlob(r[4]);
                if (respCType && hdrs["content-type"] === undefined) hdrs["content-type"] = respCType;
                const resp = new Response(text, { status: status, statusText: "", headers: hdrs, url: url });
                resp.type = "basic";
                resp.__bytes = r[3]; // native byte-exact body for arrayBuffer()
                return resp;
            }));
        } catch (e) { return Promise.reject(e); }
    };

    // XMLHttpRequest IS an EventTarget (spec: XMLHttpRequest : XMLHttpRequest-
    // EventTarget : EventTarget). It MUST sit in the real `EventTarget.prototype`
    // chain so its events flow through the shared `addEventListener`/`dispatch`
    // machinery — Zone.js (Angular) patches `EventTarget.prototype.addEventListener`
    // and, when scheduling an XHR macrotask, reads the stashed original back as
    // `XMLHttpRequest.prototype[__zone_symbol__addEventListener]` to register its
    // own `readystatechange` listener. A standalone XHR (own `__ls`, not in the
    // chain) left that stash undefined → `undefined.call` aborted every Angular
    // HttpClient request (the tilvids/PeerTube "Cannot load more videos").
    class XMLHttpRequest extends EventTarget {
        constructor() {
            super();
            this.readyState = 0; this.status = 0; this.statusText = "";
            this.__text = ""; this.__bytes = null; this.__respType = ""; this.__respObj = undefined;
            this.responseURL = ""; this.__timeout = 0; this.withCredentials = false;
            this.__h = {}; this.__aborted = false; this.__inFlight = false;
        }
        // XHR §the timeout attribute: setting it while the request is
        // synchronous (in a window realm — ours always is) throws
        // InvalidStateError. All send paths (wire, data:, blob:) read the
        // getter, so the rule can't be bypassed per-scheme.
        get timeout() { return this.__timeout; }
        set timeout(v) {
            if (this.__sync) throw new DOMException("timeout cannot be set on a synchronous XMLHttpRequest in a window context", "InvalidStateError");
            this.__timeout = Math.max(0, Number(v) || 0);
        }
        // `responseType` is a WebIDL enum: an invalid assignment is silently
        // ignored; changing it once loading, or on a sync request, throws
        // InvalidStateError (XHR spec §the responseType attribute).
        get responseType() { return this.__respType; }
        set responseType(v) {
            v = String(v);
            if (v !== "" && v !== "text" && v !== "arraybuffer" && v !== "blob" && v !== "document" && v !== "json") return;
            if (this.readyState >= 3) throw new DOMException("responseType cannot be set once loading", "InvalidStateError");
            if (this.__sync) throw new DOMException("responseType is unsupported on a synchronous request", "InvalidStateError");
            this.__respType = v;
        }
        // `responseText` is only readable in text mode (spec: throw unless
        // responseType is "" or "text"). Binary consumers must use `response`,
        // which is built from the byte-EXACT body (`r[3]`) — the UTF-8-lossy
        // text corrupts binary payloads (Steam's protobuf WebAPI responses).
        get responseText() {
            if (this.__respType !== "" && this.__respType !== "text") throw new DOMException("responseText is only available for '' or 'text' responseType", "InvalidStateError");
            return this.__ensureText();
        }
        get response() {
            const rt = this.__respType;
            if (rt === "" || rt === "text") return this.__ensureText();
            if (this.readyState !== 4) return null;
            if (this.__respObj !== undefined) return this.__respObj;
            let out = null;
            if (rt === "arraybuffer" || rt === "blob") {
                const bytes = __bodyBytes(this.__bytes != null ? this.__bytes : utf8Binary(this.__text));
                const buf = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
                out = rt === "blob" ? new g.Blob([buf], { type: this.__ctype || "" }) : buf;
            } else if (rt === "json") {
                try { out = JSON.parse(this.__ensureText()); } catch (e) { out = null; }
            } else if (rt === "document") {
                const xml = /xml/i.test(this.__ctype || "") && !/html/i.test(this.__ctype || "");
                try { out = new g.DOMParser().parseFromString(this.__ensureText(), xml ? "text/xml" : "text/html"); } catch (e) { out = null; }
            }
            this.__respObj = out;
            return out;
        }
        // XHR §the responseXML attribute: only readable in ""/"document" mode;
        // a document only when the response's MIME type is a document type —
        // for "" that means XML ONLY (an HTML response reads null, which is
        // what jQuery-era code expects); "document" mode accepts HTML too.
        // Parsed via DOMParser (our parser is HTML — the honest approximation
        // for XML input, same as the `response` document mode).
        get responseXML() {
            if (this.__respType !== "" && this.__respType !== "document")
                throw new DOMException("responseXML is only available for '' or 'document' responseType", "InvalidStateError");
            if (this.readyState !== 4) return null;
            if (this.__respXML !== undefined) return this.__respXML;
            const ct = String(this.__ctype || "").toLowerCase();
            const isXml = /xml/.test(ct) && !/html/.test(ct);
            const isHtml = /html/.test(ct);
            let out = null;
            if (isXml || (isHtml && this.__respType === "document")) {
                try { out = new g.DOMParser().parseFromString(this.__ensureText(), isXml ? "text/xml" : "text/html"); } catch (e) { out = null; }
            }
            this.__respXML = out;
            return out;
        }
        open(method, url, isAsync) {
            this.__method = String(method).toUpperCase();
            // Resolve against the document base URL (XHR `open()`: parse url with
            // the API base URL of the relevant settings object). blob: stays as-is.
            this.__url = resolveURL(String(url));
            // XHR §open() step 11: a sync request in a window realm with a
            // non-zero timeout or a non-"" responseType already set is an
            // InvalidAccessError (the setters catch the after-open order).
            if (isAsync === false && (this.__timeout !== 0 || this.__respType !== ""))
                throw new DOMException("synchronous XMLHttpRequest cannot have a timeout or responseType", "InvalidAccessError");
            this.__sync = isAsync === false;
            this.readyState = 1;
            this.__fire("readystatechange");
        }
        setRequestHeader(k, v) { this.__h[String(k).toLowerCase()] = String(v); }
        getResponseHeader(k) {
            k = String(k).toLowerCase();
            if (this.__hdrs) return Object.prototype.hasOwnProperty.call(this.__hdrs, k) ? this.__hdrs[k] : null;
            return k === "content-type" ? (this.__ctype || null) : null;
        }
        getAllResponseHeaders() {
            if (this.__hdrs) {
                let s = "";
                for (const k of Object.keys(this.__hdrs).sort()) s += k + ": " + this.__hdrs[k] + "\r\n";
                return s;
            }
            return this.__ctype ? "content-type: " + this.__ctype + "\r\n" : "";
        }
        __ensureText() {
            if ((this.__text === null || this.__text === undefined || this.__text === "") && this.__bytes != null) {
                this.__text = new g.TextDecoder().decode(__bodyBytes(this.__bytes));
            }
            return this.__text || "";
        }
        overrideMimeType() {}
        // XHR §the abort() method: an in-flight request runs the "request error
        // steps" for abort (state DONE, readystatechange, abort, loadend), then
        // state resets to UNSENT. The wire request isn't torn down — its late
        // result is discarded by the `__aborted` guard in `__finish` — but the
        // page-observable contract (events fire, the response never lands) holds.
        abort() {
            this.__aborted = true;
            if (this.__inFlight) {
                this.__inFlight = false;
                this.status = 0; this.__text = ""; this.__bytes = null; this.__respObj = undefined; this.__respXML = undefined;
                this.readyState = 4;
                this.__fire("readystatechange");
                this.__fire("abort");
                this.__fire("loadend");
            }
            if (this.readyState === 4) this.readyState = 0; // DONE → UNSENT, silently
        }
        // addEventListener/removeEventListener are inherited from EventTarget so
        // listeners land in the shared `lsFor` store (and Zone's patched wrapper
        // sees them). The `on<type>` JS properties stay plain instance props.
        __fire(t) {
            const ev = new Event(t); ev.target = this;
            const on = this["on" + t];
            if (typeof on === "function") { try { on.call(this, ev); } catch (e) { trust.errors.push("xhr on" + t + ": " + ((e && e.message) || e)); } }
            // Fire addEventListener listeners (app's + Zone's internal
            // readystatechange handler) through the shared EventTarget dispatch.
            dispatch(this, ev, false);
        }
        __finish(r) {
            // A late result for an aborted/timed-out request is discarded —
            // its events already fired from abort()/the timeout timer.
            if (this.__aborted || !this.__inFlight) return;
            this.__inFlight = false;
            if (!r) {
                this.readyState = 4; this.status = 0;
                this.__fire("readystatechange"); this.__fire("error"); this.__fire("loadend");
                return;
            }
            this.status = r[0]; this.__ctype = r[1];
            this.__text = r[2] == null ? "" : r[2];
            // Keep XHR's internal response body as a byte view. The host Fetch
            // result is an ArrayBuffer so it can cross the realm boundary
            // without stale typed-array bookkeeping; XHR's byte sequence is
            // exposed through Array.from() and response consumers via this view.
            this.__bytes = r[3] != null ? __bodyBytes(r[3]) : null;
            this.__hdrs = r.length > 4 ? __parseHdrBlob(r[4]) : null;
            if (this.__hdrs && this.__ctype && this.__hdrs["content-type"] === undefined) this.__hdrs["content-type"] = this.__ctype;
            this.__respObj = undefined; this.__respXML = undefined;
            this.responseURL = this.__url;
            this.readyState = 4;
            this.__fire("readystatechange"); this.__fire("load"); this.__fire("loadend");
        }
        send(body) {
            this.__aborted = false;
            this.__inFlight = true;
            this.__frame = trust.__activeFrame || null;
            // Async requests fire `loadstart` synchronously from send() (XHR
            // §the send() method; a SYNC request deliberately doesn't), and an
            // armed `timeout` runs the timeout request-error steps if the
            // response hasn't landed by then (the late result is then dropped).
            if (!this.__sync) {
                this.__fire("loadstart");
                if (this.timeout > 0) {
                    const xhr = this;
                    g.setTimeout(function () {
                        if (!xhr.__inFlight || xhr.__aborted) return;
                        xhr.__inFlight = false; xhr.__aborted = true;
                        xhr.status = 0; xhr.__text = ""; xhr.__bytes = null; xhr.__respObj = undefined;
                        xhr.readyState = 4;
                        xhr.__fire("readystatechange");
                        xhr.__fire("timeout");
                        xhr.__fire("loadend");
                    }, this.timeout);
                }
            }
            // A `data:` URL decodes in-realm, mirroring fetch (a malformed one
            // is a network error → the `error` event).
            if (typeof this.__url === "string" && this.__url.slice(0, 5) === "data:") {
                const dp = __dataURLParts(this.__url);
                const arr = dp ? [200, dp.ctype || null, dp.text, dp.bytes] : null;
                const xhr = this;
                if (this.__sync) this.__finish(arr);
                else __queue_network_task(function () { xhr.__finish(arr); }, this.__frame);
                return;
            }
            // A `blob:` URL resolves from the in-realm store, off the wire; a
            // missing/revoked entry is a network error (__finish(null) → error
            // event). Async still delivers __finish as a macrotask, like a real GET.
            if (typeof this.__url === "string" && this.__url.slice(0, 5) === "blob:") {
                const be = __resolveBlobURL(this.__url);
                const arr = be ? [200, be.type || null, new g.TextDecoder().decode(__latin1ToBytes(be.bytes)), be.bytes] : null;
                const xhr = this;
                if (this.__sync) this.__finish(arr);
                else __queue_network_task(function () { xhr.__finish(arr); }, this.__frame);
                return;
            }
            const isFD = body instanceof g.FormData && Array.isArray(body.__entries);
            const enc = isFD ? __formDataWire(body) : null;
            const b = isFD ? enc.wire : __bodyWire(body);
            const ctype = this.__h["content-type"] || (isFD ? enc.type : __bodyType(body));
            const hdrs = __hdrBlob(this.__h);
            if (this.__sync) {
                this.__finish(__http_fetch(this.__url, this.__method || "GET", b, ctype, hdrs));
            } else {
                // XHR §3.5.6 supplies processResponse/processEndOfBody to
                // Fetch; response state and readystatechange/load/loadend are
                // processed by that networking task. __http_fetch_async's
                // resident-page promise is settled by a host networking task.
                // Its reaction queues response processing on TRust's explicit
                // networking source. Re-queuing through author setTimeout
                // misclassified completion, let pages replace/cancel it, and
                // starved consent mutations behind the throttled timer source.
                const xhr = this;
                __http_fetch_async(this.__url, this.__method || "GET", b, ctype, hdrs)
                    .then(function (r) {
                        __queue_network_task(function () { xhr.__finish(r); }, xhr.__frame);
                    });
            }
        }
    }
    g.XMLHttpRequest = XMLHttpRequest;

    // ============================ WebAssembly =============================
// The `WebAssembly` namespace (js-api / web-api specs) over the pure-Rust wasmi
// engine. The Rust side (`sys_wasm_*` in js.rs) is a thin integer boundary:
// compile/validate/introspect modules by id. Everything observable — the
// classes, the error hierarchy, the BufferSource handling — lives here, so the
// spec's object model is expressed in JS, the language it is specified in.
// This whole block is self-contained (takes its `g` as a parameter) so the
// worker scope reuses it verbatim — `worker_prelude()` extracts it between the
// markers below, exactly as it does the structured-clone codec.
/*__WASM_BEGIN__*/
(function (g) {
    // The three error types are real `Error` subclasses; `name` is fixed on the
    // prototype so `(new WebAssembly.CompileError("x")).toString()` is
    // "CompileError: x" and `instanceof Error` holds (js-api §Error types).
    function makeError(tag) {
        const E = class extends Error {};
        Object.defineProperty(E, "name", { value: tag, configurable: true });
        Object.defineProperty(E.prototype, "name", {
            value: tag,
            writable: true,
            configurable: true,
        });
        return E;
    }
    const CompileError = makeError("CompileError");
    const LinkError = makeError("LinkError");
    const RuntimeError = makeError("RuntimeError");

    // Normalize a `BufferSource` to an `ArrayBuffer` holding exactly its bytes:
    // an `ArrayBuffer` passes through; a typed-array/DataView view is sliced to
    // a fresh buffer of just its bytes. The Rust side reads the ArrayBuffer
    // bytes EXACTLY (no latin1/UTF-8 round trip), which wasm modules require.
    function wasmBytes(src) {
        if (src instanceof ArrayBuffer) return src;
        if (ArrayBuffer.isView(src)) {
            return src.buffer.slice(src.byteOffset, src.byteOffset + src.byteLength);
        }
        throw new TypeError(
            "WebAssembly: expected a BufferSource (ArrayBuffer or ArrayBuffer view)"
        );
    }

    class Module {
        constructor(bufferSource) {
            const id = __wasm_compile(wasmBytes(bufferSource));
            if (typeof id !== "number") throw new CompileError(String(id));
            Object.defineProperty(this, "__id", { value: id });
        }
        static exports(module) {
            const flat = __wasm_module_exports(moduleId(module));
            const out = [];
            for (let i = 0; i + 1 < flat.length; i += 2) {
                out.push({ name: flat[i], kind: flat[i + 1] });
            }
            return out;
        }
        static imports(module) {
            const flat = __wasm_module_imports(moduleId(module));
            const out = [];
            for (let i = 0; i + 2 < flat.length; i += 3) {
                out.push({ module: flat[i], name: flat[i + 1], kind: flat[i + 2] });
            }
            return out;
        }
        static customSections(module, sectionName) {
            return __wasm_module_custom_sections(moduleId(module), String(sectionName));
        }
    }

    // The static `Module.*` methods take a `Module` object; a non-Module is a
    // TypeError (js-api). Exposed via a closure so it isn't a public method.
    function moduleId(m) {
        if (!(m instanceof Module) || typeof m.__id !== "number") {
            throw new TypeError("WebAssembly.Module argument expected");
        }
        return m.__id;
    }

    function validate(bufferSource) {
        return __wasm_validate(wasmBytes(bufferSource));
    }

    function compile(bufferSource) {
        return new Promise(function (resolve, reject) {
            try {
                resolve(new Module(bufferSource));
            } catch (e) {
                reject(e);
            }
        });
    }

    // Fallible wasm syscalls return an envelope `[code, payload]`: code 0 means
    // success (payload is the value), otherwise code is an error-kind string the
    // syscall chose, mapped here to the right error class. (Argument-coercion
    // TypeErrors are thrown directly from the syscall, not via this envelope.)
    function unwrap(r) {
        if (r[0] === 0) return r[1];
        const C = { Compile: CompileError, Link: LinkError, Runtime: RuntimeError };
        throw new (C[r[0]] || Error)(String(r[1]));
    }

    // Identity cache: a wasm function address maps to ONE Exported Function JS
    // object (js-api), so reading the same export twice — and (Stage 6) a
    // funcref round-tripped through a Table/Global — yields the same function.
    const funcWrappers = new Map();
    function exportedFunction(funcId, arity) {
        let f = funcWrappers.get(funcId);
        if (f) return f;
        f = function () {
            return unwrap(__wasm_call_export(funcId, Array.prototype.slice.call(arguments)));
        };
        Object.defineProperty(f, "length", { value: Number.isFinite(arity) ? arity : 0 });
        Object.defineProperty(f, "__wasmFunc", { value: funcId });
        funcWrappers.set(funcId, f);
        return f;
    }
    // Rust calls this to wrap a funcref read back from a Table/Global.
    g.__wasm_make_func = function (funcId) {
        return exportedFunction(funcId);
    };

    // The externref intern map: a wasm externref carries an integer id; the JS
    // value it wraps lives here (JS-land), so reading it back returns the SAME
    // value (identity preserved, js-api). Entries persist for the page lifetime
    // (wasm-GC of externrefs is unobservable) — RAM-only like other session
    // state. id 0 is reserved so a missing/0 id reads back as undefined.
    const externRefs = [undefined];
    g.__wasm_extern_intern = function (value) {
        externRefs.push(value);
        return externRefs.length - 1;
    };
    g.__wasm_extern_get = function (id) {
        return externRefs[id];
    };

    // WebAssembly JS API §5.6 AddressValueToU64 for the currently-supported
    // i32 address type. This is Web IDL [EnforceRange] unsigned long: NaN
    // becomes +0, finite values truncate, and negative/out-of-range values
    // throw instead of wrapping modulo 2^32.
    function addressU32(value) {
        let number = Number(value);
        if (Number.isNaN(number)) number = 0;
        number = Math.trunc(number);
        if (!Number.isFinite(number) || number < 0 || number > 0xffffffff) {
            throw new TypeError("WebAssembly address value is out of range");
        }
        return number;
    }

    function defaultWasmValue(type) {
        if (type === "i64") return 0n;
        if (type === "f32" || type === "f64" || type === "i32") return 0;
        if (type === "externref") return undefined;
        if (type === "anyfunc") return null;
        return undefined;
    }

    // WebAssembly.Global. The value lives in the wasm store; the JS object is a
    // thin handle carrying its registry id. `makeGlobal` wraps an EXISTING global
    // (an export or an import round-tripped back) without re-creating it, and is
    // identity-cached so the same global address is the same JS object (js-api).
    const globalWrappers = new Map();
    function makeGlobal(globalId) {
        let g = globalWrappers.get(globalId);
        if (g) return g;
        g = Object.create(Global.prototype);
        Object.defineProperty(g, "__globalId", { value: globalId });
        globalWrappers.set(globalId, g);
        return g;
    }
    class Global {
        constructor(descriptor, v) {
            if (typeof descriptor !== "object" || descriptor === null) {
                throw new TypeError("WebAssembly.Global: descriptor must be an object");
            }
            const type = String(descriptor.value);
            const value = arguments.length < 2 ? defaultWasmValue(type) : v;
            const id = __wasm_global_new(type, !!descriptor.mutable, value);
            Object.defineProperty(this, "__globalId", { value: id });
            globalWrappers.set(id, this);
        }
        get value() {
            return __wasm_global_get(this.__globalId);
        }
        set value(v) {
            __wasm_global_set(this.__globalId, v);
        }
        valueOf() {
            return __wasm_global_get(this.__globalId);
        }
    }

    // WebAssembly.Memory. The bytes live in the wasm store; `.buffer` is a
    // live ArrayBuffer identified with them (rebuilt + the old one detached
    // whenever the memory grows). Each engine adapter preserves the same
    // observable data-block semantics; the JS object carries only the memory id.
    const memoryWrappers = new Map();
    function makeMemory(memId) {
        let m = memoryWrappers.get(memId);
        if (m) return m;
        m = Object.create(Memory.prototype);
        Object.defineProperty(m, "__memId", { value: memId });
        memoryWrappers.set(memId, m);
        return m;
    }
    class Memory {
        constructor(descriptor) {
            if (typeof descriptor !== "object" || descriptor === null) {
                throw new TypeError("WebAssembly.Memory: descriptor must be an object");
            }
            if (descriptor.initial === undefined) {
                throw new TypeError("WebAssembly.Memory: initial is required");
            }
            if (descriptor.address !== undefined && descriptor.address !== "i32") {
                throw new TypeError("WebAssembly.Memory: unsupported address type");
            }
            if (descriptor.shared === true) {
                throw new TypeError("WebAssembly.Memory: shared memory is not supported");
            }
            const initial = addressU32(descriptor.initial);
            const maximum =
                descriptor.maximum === undefined ? -1 : addressU32(descriptor.maximum);
            const id = __wasm_memory_new(initial, maximum);
            Object.defineProperty(this, "__memId", { value: id });
            memoryWrappers.set(id, this);
        }
        get buffer() {
            return __wasm_memory_buffer(this.__memId);
        }
        grow(delta) {
            return __wasm_memory_grow(this.__memId, addressU32(delta));
        }
        toFixedLengthBuffer() {
            return this.buffer;
        }
        toResizableBuffer() {
            throw new TypeError("WebAssembly.Memory: resizable buffers are not supported");
        }
    }

    // WebAssembly.Table — a growable array of funcref/externref values living in
    // the wasm store. The JS object carries only the table id. get/set convert
    // funcref ↔ Exported Function and externref ↔ any JS value (identity-
    // preserving for both externref values and exported-function addresses).
    const tableWrappers = new Map();
    function makeTable(tableId, element) {
        let t = tableWrappers.get(tableId);
        if (t) return t;
        t = Object.create(Table.prototype);
        Object.defineProperty(t, "__tableId", { value: tableId });
        Object.defineProperty(t, "__element", { value: element });
        tableWrappers.set(tableId, t);
        return t;
    }
    class Table {
        constructor(descriptor, value) {
            if (typeof descriptor !== "object" || descriptor === null) {
                throw new TypeError("WebAssembly.Table: descriptor must be an object");
            }
            if (descriptor.initial === undefined) {
                throw new TypeError("WebAssembly.Table: initial is required");
            }
            if (descriptor.address !== undefined && descriptor.address !== "i32") {
                throw new TypeError("WebAssembly.Table: unsupported address type");
            }
            const element = String(descriptor.element);
            const initial = addressU32(descriptor.initial);
            const maximum =
                descriptor.maximum === undefined ? -1 : addressU32(descriptor.maximum);
            if (arguments.length < 2) value = defaultWasmValue(element);
            const id = __wasm_table_new(element, initial, maximum, value);
            Object.defineProperty(this, "__tableId", { value: id });
            Object.defineProperty(this, "__element", { value: element });
            tableWrappers.set(id, this);
        }
        get length() {
            return __wasm_table_length(this.__tableId);
        }
        get(index) {
            return __wasm_table_get(this.__tableId, addressU32(index));
        }
        set(index, value) {
            if (arguments.length < 2) value = defaultWasmValue(this.__element);
            return __wasm_table_set(this.__tableId, addressU32(index), value);
        }
        grow(delta, value) {
            if (arguments.length < 2) value = defaultWasmValue(this.__element);
            return __wasm_table_grow(this.__tableId, addressU32(delta), value);
        }
    }

    // Build an Instance's exports object (js-api "create the exports"): a
    // null-prototype, frozen object mapping each export name to its wrapper.
    function buildExports(instanceId, moduleId) {
        const flat = __wasm_instance_exports(instanceId, moduleId);
        const exports = Object.create(null);
        for (let i = 0; i + 3 < flat.length; i += 4) {
            const name = flat[i],
                kind = flat[i + 1],
                sub = flat[i + 2],
                auxiliaryType = flat[i + 3];
            if (kind === "function") exports[name] = exportedFunction(sub, Number(auxiliaryType));
            else if (kind === "global") exports[name] = makeGlobal(sub);
            else if (kind === "memory") exports[name] = makeMemory(sub);
            else if (kind === "table") exports[name] = makeTable(sub, auxiliaryType);
        }
        return Object.freeze(exports);
    }

    // Import functions live in JS-land (the canonical-state-in-Rust ethos): a
    // per-instantiation token keys the array of import functions, and a wasmi
    // host func forwards each call here. (token, index) is all Rust carries.
    let nextImportToken = 1;
    const wasmImports = Object.create(null);
    g.__wasm_invoke_import = function (token, index, args) {
        const fns = wasmImports[token];
        return fns[index].apply(undefined, args);
    };

    // js-api "read the imports": for each module import, resolve
    // importObject[module][name], validate its kind, and collect the binding.
    // Existing Exported Functions retain their core address; ordinary JS
    // functions receive a host-function address keyed by the import token.
    function readImports(module, importObject) {
        const imps = Module.imports(module);
        if (imps.length > 0 && (typeof importObject !== "object" || importObject === null)) {
            throw new TypeError(
                "WebAssembly.Instance: an importObject is required for a module with imports"
            );
        }
        const token = nextImportToken++;
        const funcs = [];
        const descriptor = [];
        for (const imp of imps) {
            const ns = importObject[imp.module];
            if (typeof ns !== "object" || ns === null) {
                throw new LinkError("import namespace '" + imp.module + "' is not an object");
            }
            const value = ns[imp.name];
            switch (imp.kind) {
                case "function":
                    if (typeof value !== "function") {
                        throw new LinkError(
                            "import '" + imp.module + "." + imp.name + "' is not a function"
                        );
                    }
                    if (typeof value.__wasmFunc === "number") {
                        descriptor.push(["fr", value.__wasmFunc]);
                    } else {
                        descriptor.push(["f", funcs.length]);
                        funcs.push(value);
                    }
                    break;
                case "global":
                    if (value instanceof Global) {
                        descriptor.push(["g", value.__globalId]);
                    } else if (typeof value === "number" || typeof value === "bigint") {
                        descriptor.push(["gv", value]);
                    } else {
                        throw new LinkError(
                            "import '" + imp.module + "." + imp.name +
                            "' must be a WebAssembly.Global or a number"
                        );
                    }
                    break;
                case "memory":
                    if (value instanceof Memory) {
                        descriptor.push(["m", value.__memId]);
                    } else {
                        throw new LinkError(
                            "import '" + imp.module + "." + imp.name +
                            "' must be a WebAssembly.Memory"
                        );
                    }
                    break;
                case "table":
                    if (value instanceof Table) {
                        descriptor.push(["t", value.__tableId]);
                    } else {
                        throw new LinkError(
                            "import '" + imp.module + "." + imp.name +
                            "' must be a WebAssembly.Table"
                        );
                    }
                    break;
                default:
                    throw new LinkError(
                        "import '" + imp.module + "." + imp.name + "' of kind '" + imp.kind +
                        "' is not supported yet"
                    );
            }
        }
        wasmImports[token] = funcs;
        return { token: token, descriptor: descriptor };
    }

    class Instance {
        constructor(module, importObject) {
            if (!(module instanceof Module)) {
                throw new TypeError("WebAssembly.Instance: a Module argument is required");
            }
            if (
                importObject !== undefined &&
                (typeof importObject !== "object" || importObject === null)
            ) {
                throw new TypeError("WebAssembly.Instance: importObject must be an object");
            }
            const binding = readImports(module, importObject);
            const id = unwrap(
                __wasm_instantiate(module.__id, binding.token, binding.descriptor)
            );
            Object.defineProperty(this, "__id", { value: id });
            Object.defineProperty(this, "exports", {
                value: buildExports(id, module.__id),
                enumerable: true,
            });
        }
    }

    function instantiate(source, importObject) {
        if (source instanceof Module) {
            // instantiate(moduleObject, importObject) → Promise<Instance>
            return new Promise(function (resolve, reject) {
                try {
                    resolve(new Instance(source, importObject));
                } catch (e) {
                    reject(e);
                }
            });
        }
        // instantiate(bufferSource, importObject) → Promise<{module, instance}>
        return new Promise(function (resolve, reject) {
            try {
                const module = new Module(source);
                const instance = new Instance(module, importObject);
                resolve({ module: module, instance: instance });
            } catch (e) {
                reject(e);
            }
        });
    }

    // Read the wasm bytes from a streaming source (a Response or a Promise of one),
    // validating it per the Web API: ok status + MIME type 'application/wasm'.
    function streamBytes(source) {
        return Promise.resolve(source).then(function (resp) {
            if (!resp || typeof resp.arrayBuffer !== "function") {
                throw new TypeError("WebAssembly streaming: expected a Response");
            }
            if (resp.ok === false) {
                throw new TypeError("WebAssembly streaming: response was not ok");
            }
            const ct = (resp.headers && resp.headers.get && resp.headers.get("content-type")) || "";
            if (String(ct).split(";")[0].trim().toLowerCase() !== "application/wasm") {
                throw new TypeError(
                    "WebAssembly streaming: response must have MIME type 'application/wasm'"
                );
            }
            return resp.arrayBuffer();
        });
    }

    function compileStreaming(source) {
        return streamBytes(source).then(function (buf) {
            return new Module(buf);
        });
    }

    function instantiateStreaming(source, importObject) {
        return streamBytes(source).then(function (buf) {
            const module = new Module(buf);
            const instance = new Instance(module, importObject);
            return { module: module, instance: instance };
        });
    }

    const WebAssembly = {
        Module: Module,
        Instance: Instance,
        Global: Global,
        Memory: Memory,
        Table: Table,
        CompileError: CompileError,
        LinkError: LinkError,
        RuntimeError: RuntimeError,
        validate: validate,
        compile: compile,
        compileStreaming: compileStreaming,
        instantiate: instantiate,
        instantiateStreaming: instantiateStreaming,
    };
    Object.defineProperty(WebAssembly, Symbol.toStringTag, {
        value: "WebAssembly",
        configurable: true,
    });
    g.WebAssembly = WebAssembly;
})(typeof globalThis !== "undefined" ? globalThis : this);
/*__WASM_END__*/
})();
