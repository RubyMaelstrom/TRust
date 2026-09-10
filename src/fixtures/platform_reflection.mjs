(() => {
    function check(value, message) { if (!value) throw new Error(message); }
    function nativeSource(realm, fn, label) {
        const source = realm.Function.prototype.toString.call(fn);
        check(/^function\b[\s\S]*\{\s*\[native code\]\s*\}$/.test(source), label + ' implementation source');
    }
    function checkWindow(realm) {
        for (const name of ['Node', 'Element', 'Document', 'Event', 'EventTarget', 'DOMRect', 'CSSStyleSheet'])
            nativeSource(realm, realm[name], name);
        for (const fn of [realm.document.createElement, realm.document.querySelector,
            realm.Element.prototype.attachShadow, realm.getComputedStyle, realm.fetch])
            nativeSource(realm, fn, fn.name);
    }
    checkWindow(globalThis);
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const frame = document.createElement('iframe'); body.appendChild(frame);
    checkWindow(frame.contentWindow);
    nativeSource(globalThis, frame.contentWindow.document.createElement, 'cross-realm method');
    const childAuthor = frame.contentWindow.eval('(function childAuthor(){ return 23; })');
    check(Function.prototype.toString.call(childAuthor) === 'function childAuthor(){ return 23; }', 'child author source');
    const source = 'function author(){ /* preserve this comment */ return 17; }';
    const author = (0, eval)('(' + source + ')');
    check(author.toString() === source && author() === 17, 'author source and execution');
    const saved = document.createElement;
    document.createElement = author;
    check(document.createElement.toString() === source, 'author replacement stays an author function');
    document.createElement = saved;
    const dynamic = Function('return 19;');
    check(dynamic.toString().includes('return 19;') && dynamic() === 19, 'dynamic source');
    frame.remove();
    return 'platform-reflection-ok';
})()
