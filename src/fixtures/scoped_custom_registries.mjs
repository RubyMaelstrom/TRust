// HTML #custom-elements-api scoped registries: the CustomElementRegistry
// constructor and initialize(); DOM's customElementRegistry of Document,
// ShadowRoot and Element, the attachShadow(), createElement() and
// importNode() registry options, clone and adopt; HTML's declarative
// `shadowrootcustomelementregistry`.
(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function throws(name, callback, message) {
        try { callback(); } catch (e) { check(e.name === name, (message || '') + ' wrong error ' + e); return; }
        throw Error('missing ' + name + ' ' + (message || ''));
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);

    // Interfaces.
    check(CustomElementRegistry.length === 0, 'constructor length');
    const init = Object.getOwnPropertyDescriptor(CustomElementRegistry.prototype, 'initialize');
    check(init && init.enumerable && init.value.length === 1, 'initialize IDL');
    for (const proto of [Document.prototype, ShadowRoot.prototype, Element.prototype]) {
        const d = Object.getOwnPropertyDescriptor(proto, 'customElementRegistry');
        check(d && typeof d.get === 'function' && d.set === undefined, 'customElementRegistry getter ' + proto);
    }
    check(document.customElementRegistry === customElements, 'document registry');
    check(new DOMParser().parseFromString('<p>', 'text/html').customElementRegistry === null, 'DOMParser document');
    check(document.implementation.createHTMLDocument('').customElementRegistry === null, 'createHTMLDocument');
    check(document.body.customElementRegistry === customElements, 'element registry');

    const scoped = new CustomElementRegistry();
    check(scoped !== customElements && scoped instanceof CustomElementRegistry, 'new registry');
    class GlobalA extends HTMLElement { get kind() { return 'global'; } }
    class ScopedA extends HTMLElement { get kind() { return 'scoped'; } }
    customElements.define('x-a', GlobalA);
    scoped.define('x-a', ScopedA);
    throws('NotSupportedError', () => scoped.define('x-b', class extends HTMLParagraphElement {}, { extends: 'p' }),
        'customized built-ins');
    check(scoped.get('x-a') === ScopedA && scoped.getName(ScopedA) === 'x-a' && customElements.get('x-a') === GlobalA,
        'separate definition sets');

    // createElement's registry option.
    const plain = document.createElement('x-a');
    check(plain instanceof GlobalA && plain.customElementRegistry === customElements, 'global default');
    const own = document.createElement('x-a', { customElementRegistry: scoped });
    check(own instanceof ScopedA && own.customElementRegistry === scoped, 'scoped createElement');
    const ns = document.createElementNS('http://www.w3.org/1999/xhtml', 'x-a', { customElementRegistry: scoped });
    check(ns instanceof ScopedA && ns.customElementRegistry === scoped, 'scoped createElementNS');
    const none = document.createElement('x-a', { customElementRegistry: null });
    check(!(none instanceof GlobalA) && none.customElementRegistry === null, 'null registry');
    throws('NotSupportedError', () => document.createElement('x-a', { customElementRegistry: scoped, is: 'x-b' }), 'with is');
    throws('TypeError', () => document.createElement('x-a', { customElementRegistry: {} }), 'not a registry');
    const parsedDoc = new DOMParser().parseFromString('<p>', 'text/html');
    throws('NotSupportedError', () => parsedDoc.createElement('x-a', { customElementRegistry: customElements }),
        'another document global registry');
    check(parsedDoc.createElement('x-a', { customElementRegistry: scoped }) instanceof ScopedA,
        'a scoped registry in any document');
    // The HTML element constructor uses the active registry, else the
    // Document's.
    check(new GlobalA().customElementRegistry === customElements, 'new GlobalA');
    throws('TypeError', () => new ScopedA(), 'scoped-only constructor');

    // attachShadow's registry and fragment parsing in it.
    const host = document.createElement('div');
    body.appendChild(host);
    const root = host.attachShadow({ mode: 'open', customElementRegistry: scoped });
    check(root.customElementRegistry === scoped, 'shadow registry');
    root.innerHTML = '<x-a id=inner><x-a id=nested></x-a></x-a><x-late id=late></x-late>';
    const inner = root.getElementById('inner'), nested = root.getElementById('nested');
    check(inner instanceof ScopedA && nested instanceof ScopedA && inner.customElementRegistry === scoped,
        'innerHTML uses the shadow root registry');
    // The fragment parsing algorithm's target (or context element)
    // supplies the registry of the parsed elements.
    inner.insertAdjacentHTML('beforeend', '<x-a id=adjacent></x-a>');
    check(root.getElementById('adjacent') instanceof ScopedA, 'insertAdjacentHTML in a scoped tree');
    root.getElementById('adjacent').outerHTML = '<x-a id=outer></x-a>';
    check(root.getElementById('outer') instanceof ScopedA, 'outerHTML in a scoped tree');
    const range = document.createRange();
    range.selectNodeContents(inner);
    const contextual = range.createContextualFragment('<x-a id=contextual></x-a>');
    check(contextual.firstChild.customElementRegistry === scoped, 'createContextualFragment context registry');
    root.appendChild(contextual);
    check(root.getElementById('contextual') instanceof ScopedA, 'contextual fragment upgrades when connected');
    // An element created by the document keeps its registry anywhere.
    root.appendChild(plain);
    check(plain instanceof GlobalA && plain.customElementRegistry === customElements, 'appended global element');
    // A later definition upgrades the connected elements of the registry.
    const late = root.getElementById('late');
    check(late.constructor === HTMLElement, 'late not yet defined');
    class Late extends HTMLElement {}
    scoped.define('x-late', Late);
    check(late instanceof Late, 'scoped define upgrades connected elements');
    check(document.createElement('x-late').constructor === HTMLElement, 'global registry unaffected');
    const defaultRoot = document.createElement('span').attachShadow({ mode: 'open' });
    check(defaultRoot.customElementRegistry === customElements, 'default shadow registry');
    const nullRoot = document.createElement('span').attachShadow({ mode: 'open', customElementRegistry: null });
    check(nullRoot.customElementRegistry === null, 'null shadow registry');
    throws('TypeError', () => document.createElement('div').attachShadow({ mode: 'open', customElementRegistry: {} }),
        'not a registry');
    // ShadowRootInit members convert in lexicographic order, before mode is checked.
    const read = [];
    const probe = new Proxy({}, { get(target, key) { read.push(String(key)); return key === 'mode' ? 'open' : undefined; } });
    document.createElement('div').attachShadow(probe);
    check(read.join() === 'clonable,customElementRegistry,delegatesFocus,mode,serializable,slotAssignment',
        'ShadowRootInit order ' + read.join());

    // Declarative shadow roots with a null registry, then initialize().
    const declarativeHost = document.createElement('div');
    body.appendChild(declarativeHost);
    declarativeHost.setHTMLUnsafe('<section><template shadowrootmode=open shadowrootserializable ' +
        'shadowrootcustomelementregistry><x-a id=d1><x-a id=d2></x-a></x-a></template></section>');
    const section = declarativeHost.firstChild, dRoot = section.shadowRoot;
    const d1 = dRoot.getElementById('d1');
    check(dRoot.customElementRegistry === null && d1.customElementRegistry === null &&
        d1.constructor === HTMLElement, 'declarative null registry');
    check(section.getHTML({ serializableShadowRoots: true }).includes(' shadowrootcustomelementregistry=""'),
        'serialized registry attribute');
    throws('NotSupportedError', () => customElements.initialize(document), 'global registry and a Document');
    scoped.initialize(dRoot);
    check(dRoot.customElementRegistry === scoped && d1.customElementRegistry === scoped &&
        d1 instanceof ScopedA && dRoot.getElementById('d2') instanceof ScopedA, 'initialize');
    // initialize() never replaces a registry.
    customElements.initialize(d1);
    check(d1.customElementRegistry === scoped, 'initialize keeps registries');
    check(!host.getHTML({ serializableShadowRoots: true }).includes('shadowrootcustomelementregistry'),
        'non-serializable root');
    const globalRootHost = document.createElement('div');
    globalRootHost.attachShadow({ mode: 'open', serializable: true });
    check(!globalRootHost.getHTML({ serializableShadowRoots: true }).includes('shadowrootcustomelementregistry'),
        'global registry is not serialized');
    // A global registry initializes elements with a null registry.
    customElements.initialize(none);
    check(none.customElementRegistry === customElements && none instanceof GlobalA, 'global initialize');
    throws('NotSupportedError', () => customElements.initialize(parsedDoc.createElement('p')), 'other document');

    // upgrade() only upgrades elements of its registry.
    const pending = document.createElement('div');
    pending.innerHTML = '<x-up></x-up>';
    const pendingScoped = document.createElement('x-up', { customElementRegistry: scoped });
    pending.appendChild(pendingScoped);
    class UpScoped extends HTMLElement {}
    scoped.define('x-up', UpScoped);
    scoped.upgrade(pending);
    check(pendingScoped instanceof UpScoped && !(pending.firstChild instanceof UpScoped), 'upgrade filters by registry');

    // Clone and importNode.
    const copy = own.cloneNode(true);
    check(copy instanceof ScopedA && copy.customElementRegistry === scoped, 'clone keeps a scoped registry');
    const template = document.createElement('template');
    template.innerHTML = '<x-a id=t1></x-a>';
    const t1 = template.content.firstChild;
    check(t1.customElementRegistry === null && t1.constructor === HTMLElement, 'template contents have no registry');
    const fromContent = template.content.cloneNode(true).firstChild;
    check(fromContent.customElementRegistry === null && fromContent.constructor === HTMLElement,
        'template contents clone stays inert');
    body.appendChild(fromContent);
    check(fromContent.customElementRegistry === customElements && fromContent instanceof GlobalA,
        'inserting adopts into the document');
    const imported = document.importNode(template.content, true).firstChild;
    check(imported instanceof GlobalA && imported.customElementRegistry === customElements, 'importNode fallback');
    const importedScoped = document.importNode(template.content, { customElementRegistry: scoped }).firstChild;
    check(importedScoped instanceof ScopedA && importedScoped.customElementRegistry === scoped, 'importNode registry');
    const importedGlobal = document.importNode(plain, { customElementRegistry: scoped, selfOnly: true });
    check(importedGlobal instanceof GlobalA && importedGlobal.customElementRegistry === customElements,
        'a global registry is kept');
    throws('TypeError', () => document.importNode(plain, { customElementRegistry: null }), 'non-nullable member');
    throws('NotSupportedError', () => parsedDoc.importNode(plain, { customElementRegistry: customElements }),
        'another document global registry');
    check(document.importNode(plain, null).customElementRegistry === customElements, 'null options');

    // Adoption: a global registry becomes the new document's effective
    // global registry, a scoped one is kept.
    const adoptedGlobal = parsedDoc.adoptNode(document.createElement('x-a'));
    check(adoptedGlobal.customElementRegistry === null, 'adopted global');
    const adoptedScoped = parsedDoc.adoptNode(document.createElement('x-a', { customElementRegistry: scoped }));
    check(adoptedScoped.customElementRegistry === scoped, 'adopted scoped');
    document.body.appendChild(adoptedGlobal);
    check(adoptedGlobal.customElementRegistry === customElements, 'adopted back');

    // A kept-null declarative registry survives adoption; the host takes
    // its new document's registry.
    const unsafeDoc = Document.parseHTMLUnsafe('<div id=u><template shadowrootmode=open ' +
        'shadowrootcustomelementregistry><x-a></x-a></template></div>');
    const u = unsafeDoc.getElementById('u');
    check(unsafeDoc.customElementRegistry === null && u.customElementRegistry === null &&
        u.shadowRoot.customElementRegistry === null, 'parsed registries');
    document.body.appendChild(u);
    check(u.customElementRegistry === customElements && u.shadowRoot.customElementRegistry === null &&
        u.shadowRoot.firstChild.customElementRegistry === null &&
        u.shadowRoot.firstChild.constructor === HTMLElement, 'adoption keeps a kept-null registry');

    // whenDefined is per registry.
    const resolved = [];
    globalThis.scopedWhenDefined = resolved;
    scoped.whenDefined('x-when').then(c => resolved.push(c.name));
    customElements.define('x-when', class GlobalWhen extends HTMLElement {});
    scoped.define('x-when', class ScopedWhen extends HTMLElement {});
    return 'scoped-registries-ok';
})();
