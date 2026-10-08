// DOM #dom-parentnode-movebefore and #concept-node-move; HTML
// #html-element-moving-steps and the connectedMoveCallback reaction.
(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function throws(name, callback, message) {
        try { callback(); } catch (e) { check(e.name === name, (message || '') + ' wrong error ' + e); return; }
        throw Error('missing ' + name + ' ' + (message || ''));
    }
    const kids = node => Array.from(node.childNodes, n => n.id || n.nodeName).join();

    // ParentNode: Element, Document and DocumentFragment (and so ShadowRoot).
    for (const proto of [Element.prototype, Document.prototype, DocumentFragment.prototype]) {
        const d = Object.getOwnPropertyDescriptor(proto, 'moveBefore');
        check(d && d.enumerable && d.writable && d.configurable && d.value.length === 2 &&
            d.value.name === 'moveBefore', 'IDL ' + proto);
    }
    check(!('moveBefore' in Node.prototype) && !('moveBefore' in CharacterData.prototype) &&
        typeof ShadowRoot.prototype.moveBefore === 'function', 'mixin placement');

    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    body.innerHTML = '<div id=a><b id=b1></b><b id=b2></b><b id=b3></b></div><div id=c></div>';
    const a = document.getElementById('a'), c = document.getElementById('c');
    const [b1, b2, b3] = ['b1', 'b2', 'b3'].map(id => document.getElementById(id));
    check(a.moveBefore(b3, b1) === undefined && kids(a) === 'b3,b1,b2', 'reorder');
    a.moveBefore(b3, null);
    check(kids(a) === 'b1,b2,b3', 'append');
    // A node moved before itself stays put (before its next sibling).
    a.moveBefore(b2, b2);
    check(kids(a) === 'b1,b2,b3', 'before itself');
    a.moveBefore(b3, b3);
    check(kids(a) === 'b1,b2,b3', 'last before itself');
    c.moveBefore(b2, undefined);
    check(kids(a) === 'b1,b3' && kids(c) === 'b2', 'between parents');

    // Arguments and preconditions, in order.
    throws('TypeError', () => a.moveBefore(b1));
    throws('TypeError', () => a.moveBefore({}, null));
    throws('TypeError', () => a.moveBefore(b1, {}));
    const detached = document.createElement('p');
    throws('HierarchyRequestError', () => a.moveBefore(detached, null), 'detached node');
    throws('HierarchyRequestError', () => detached.moveBefore(b1, null), 'detached parent');
    throws('HierarchyRequestError', () => a.moveBefore(detached, b2), 'root check before child check');
    throws('HierarchyRequestError', () => b1.moveBefore(a, null), 'ancestor');
    throws('HierarchyRequestError', () => a.moveBefore(a, null), 'itself');
    throws('NotFoundError', () => a.moveBefore(b1, b2), 'foreign reference');
    throws('NotFoundError', () => a.moveBefore(b1, document.createAttribute('x')), 'Attr reference');
    throws('HierarchyRequestError', () => a.moveBefore(document.createAttribute('x'), null), 'Attr node');
    const text = document.createTextNode('t');
    a.appendChild(text);
    throws('HierarchyRequestError', () => document.moveBefore(text, null), 'Text into a Document');
    throws('HierarchyRequestError', () => document.moveBefore(b1, null), 'second document element');
    throws('HierarchyRequestError', () => document.moveBefore(html, null), 'document element onto itself');
    const comment = document.createComment('c');
    a.appendChild(comment);
    document.moveBefore(comment, html);
    check(document.firstChild === comment, 'comment into the Document');
    const parsed = new DOMParser().parseFromString('<p id=x>', 'text/html');
    throws('HierarchyRequestError', () => a.moveBefore(parsed.getElementById('x'), null), 'other document');
    throws('HierarchyRequestError', () => parsed.body.moveBefore(b1, null), 'into other document');
    // Within a disconnected tree.
    const tree = document.createElement('div');
    tree.innerHTML = '<i id=t1></i><span><i id=t2></i></span>';
    tree.moveBefore(tree.lastChild.firstChild, tree.firstChild);
    check(tree.firstChild.id === 't2', 'disconnected tree');

    // Into and out of a shadow tree of the same document.
    const host = document.createElement('div');
    body.appendChild(host);
    const root = host.attachShadow({ mode: 'open' });
    root.moveBefore(b1, null);
    check(b1.parentNode === root && b1.getRootNode() === root, 'into a shadow tree');
    c.moveBefore(b1, null);
    check(b1.parentNode === c, 'out of a shadow tree');
    throws('HierarchyRequestError', () => root.moveBefore(host, null), 'host into its own shadow tree');

    // Slot assignment follows the move (DOM #concept-node-move steps 14–20).
    const slotHost = document.createElement('div');
    body.appendChild(slotHost);
    const slotRoot = slotHost.attachShadow({ mode: 'open' });
    const slot = document.createElement('slot');
    slotRoot.appendChild(slot);
    const slotted = document.createElement('em');
    c.appendChild(slotted);
    slotHost.moveBefore(slotted, null);
    check(slot.assignedNodes()[0] === slotted && slotted.assignedSlot === slot, 'moved into a host');
    c.moveBefore(slotted, null);
    check(slot.assignedNodes().length === 0 && slotted.assignedSlot === null, 'moved out of a host');

    // State survives: focus, form control values and selection.
    const input = document.createElement('input');
    a.appendChild(input);
    input.value = 'typed';
    input.focus();
    check(document.activeElement === input, 'focused');
    c.moveBefore(input, null);
    check(document.activeElement === input && input.value === 'typed', 'focus and value survive');
    a.appendChild(input);
    check(document.activeElement !== input, 'insertion (removal) loses focus');

    // MutationObserver: a removal record and an insertion record, without
    // transient registrations.
    const records = [];
    const observer = new MutationObserver(list => records.push(...list));
    observer.observe(body, { childList: true, subtree: true });
    a.innerHTML = '<u id=u1></u><u id=u2></u><u id=u3></u>';
    observer.takeRecords();
    const u2 = document.getElementById('u2');
    c.moveBefore(u2, b2);
    const taken = observer.takeRecords();
    check(taken.length === 2, 'two records ' + taken.length);
    check(taken[0].target === a && taken[0].removedNodes[0] === u2 && taken[0].addedNodes.length === 0 &&
        taken[0].previousSibling.id === 'u1' && taken[0].nextSibling.id === 'u3', 'removal record');
    check(taken[1].target === c && taken[1].addedNodes[0] === u2 && taken[1].removedNodes.length === 0 &&
        taken[1].previousSibling === null && taken[1].nextSibling === b2, 'insertion record');
    observer.disconnect();

    // Live ranges: the pre-remove steps, then insertion offsets.
    a.innerHTML = '<s id=s1></s><s id=s2></s><s id=s3></s>';
    const s2 = document.getElementById('s2'), s3 = document.getElementById('s3');
    const range = document.createRange();
    range.setStart(a, 1); range.setEnd(a, 3);
    const inner = document.createRange();
    inner.selectNodeContents(s2);
    a.moveBefore(s3, a.firstChild);
    check(range.startContainer === a && range.startOffset === 2 && range.endOffset === 3, 'range offsets ' +
        range.startOffset + ',' + range.endOffset);
    c.moveBefore(s2, null);
    check(inner.startContainer === a && inner.startOffset === 2 && inner.collapsed, 'range in the moved node');

    // HTML option moving steps: selectedness of both selects.
    const one = document.createElement('select'), two = document.createElement('select');
    body.append(one, two);
    one.innerHTML = '<option id=o1>1</option><option id=o2 selected>2</option>';
    two.innerHTML = '<option id=o3 selected>3</option>';
    two.moveBefore(document.getElementById('o2'), null);
    check(one.selectedIndex === 0 && two.value === '2' && two.selectedOptions.length === 1, 'select moves ' +
        one.selectedIndex + ',' + two.value);

    // Custom elements: connectedMoveCallback, or disconnected+connected.
    const log = [];
    customElements.define('move-aware', class extends HTMLElement {
        connectedCallback() { log.push('connected:' + this.id); }
        disconnectedCallback() { log.push('disconnected:' + this.id); }
        connectedMoveCallback() { log.push('move:' + this.id + ':' + this.parentNode.id); }
    });
    customElements.define('move-legacy', class extends HTMLElement {
        connectedCallback() { log.push('connected:' + this.id); }
        disconnectedCallback() { log.push('disconnected:' + this.id); }
    });
    customElements.define('move-inert', class extends HTMLElement {});
    a.innerHTML = '<move-aware id=m1><move-legacy id=m2><move-inert id=m3></move-inert></move-legacy></move-aware>';
    log.length = 0;
    c.moveBefore(document.getElementById('m1'), null);
    check(log.join() === 'move:m1:c,disconnected:m2,connected:m2', 'move reactions ' + log.join());
    log.length = 0;
    const detachedTree = document.createElement('div');
    detachedTree.innerHTML = '<move-aware id=d1></move-aware><p></p>';
    customElements.upgrade(detachedTree);
    detachedTree.moveBefore(detachedTree.firstChild, null);
    check(log.length === 0, 'no reactions in a disconnected tree ' + log.join());
    // Reactions run after the move completed and its records were queued.
    const order = [];
    const watcher = new MutationObserver(() => {});
    watcher.observe(body, { childList: true, subtree: true });
    customElements.define('move-probe', class extends HTMLElement {
        connectedMoveCallback() { order.push(this.parentNode.id + ':' + watcher.takeRecords().length); }
    });
    a.innerHTML = '<move-probe id=p1></move-probe>';
    watcher.takeRecords();
    c.moveBefore(document.getElementById('p1'), null);
    check(order.join() === 'c:2', 'reaction after records ' + order.join());
    watcher.disconnect();
    return 'move-before-ok';
})();
