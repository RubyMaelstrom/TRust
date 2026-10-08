// HTML #dom-slot-assign and #manually-assigned-nodes; DOM #find-a-slot,
// #find-slotables, #assign-slotables-for-a-tree and #signaling-slot-change.
(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function same(actual, expected, message) {
        check(actual.length === expected.length && actual.every((node, i) => node === expected[i]),
            message + ': ' + actual.map(n => n.nodeName).join());
    }
    function throws(name, callback) {
        try { callback(); } catch (e) { check(e.name === name, 'wrong error ' + e); return; }
        throw Error('missing ' + name);
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);

    // The operation: [Exposed=Window] undefined assign((Element or Text)... nodes).
    const desc = Object.getOwnPropertyDescriptor(HTMLSlotElement.prototype, 'assign');
    check(desc && desc.enumerable && desc.writable && desc.configurable && desc.value.length === 0, 'assign IDL');

    const host = document.createElement('div');
    body.appendChild(host);
    const root = host.attachShadow({ mode: 'open', slotAssignment: 'manual' });
    check(root.slotAssignment === 'manual', 'slotAssignment');
    const s1 = document.createElement('slot'), s2 = document.createElement('slot');
    s1.name = 'x';
    root.append(s1, s2);
    const a = document.createElement('span'), b = document.createTextNode('b'), c = document.createElement('i');
    a.slot = 'x';
    host.append(a, b);
    // Names do not assign in manual mode.
    check(s1.assignedNodes().length === 0 && a.assignedSlot === null, 'manual ignores names');

    s1.assign(b, c, a, b);
    same(s1.assignedNodes(), [b, a], 'host children in assignment order');
    check(a.assignedSlot === s1 && b.assignedSlot === s1 && c.assignedSlot === null, 'assignedSlot');
    host.appendChild(c);
    same(s1.assignedNodes(), [b, c, a], 'inserting an assigned node');
    same(s1.assignedElements(), [c, a], 'assignedElements');
    // A node leaves the slot it was assigned to.
    s2.assign(a);
    same(s1.assignedNodes(), [b, c], 'removed from the other slot');
    same(s2.assignedNodes(), [a], 'second slot');
    // Removing a node from the host unassigns it; reinsertion restores it.
    c.remove();
    same(s1.assignedNodes(), [b], 'removed child');
    host.appendChild(c);
    same(s1.assignedNodes(), [b, c], 'reinserted child');
    // Reassigning clears the old nodes.
    s1.assign(c);
    check(b.assignedSlot === null, 'old node unassigned');
    s1.assign();
    check(s1.assignedNodes().length === 0 && c.assignedSlot === null, 'assign() empties');
    // Conversion failures throw before anything changes.
    s1.assign(b);
    for (const bad of [null, undefined, 1, {}, document.createComment('c'), document, document.createAttribute('x')])
        throws('TypeError', () => s1.assign(c, bad));
    same(s1.assignedNodes(), [b], 'failed conversion changed nothing');
    throws('TypeError', () => HTMLSlotElement.prototype.assign.call(document.createElement('div'), b));
    // A slot moved out of the shadow tree has no slottables; back in, it does.
    s1.remove();
    check(s1.assignedNodes().length === 0 && b.assignedSlot === null, 'slot outside');
    root.appendChild(s1);
    same(s1.assignedNodes(), [b], 'slot back');
    // Flattening and the flat tree.
    same(s1.assignedNodes({ flatten: true }), [b], 'flatten');
    // A slot in a named shadow tree keeps manual nodes without using them.
    const namedHost = document.createElement('div');
    body.appendChild(namedHost);
    const namedRoot = namedHost.attachShadow({ mode: 'open' });
    const named = document.createElement('slot');
    namedRoot.appendChild(named);
    const n1 = document.createElement('b');
    namedHost.appendChild(n1);
    named.assign();
    same(named.assignedNodes(), [n1], 'named mode ignores assign');
    // Closed trees hide the slot from assignedSlot.
    const closedHost = document.createElement('div');
    const closed = closedHost.attachShadow({ mode: 'closed', slotAssignment: 'manual' });
    const hidden = document.createElement('slot');
    closed.appendChild(hidden);
    const child = document.createElement('p');
    closedHost.appendChild(child);
    hidden.assign(child);
    check(child.assignedSlot === null && hidden.assignedNodes()[0] === child, 'closed');

    globalThis.slotFixture = { host, root, s1, s2, a, b, c };
    return 'slot-assignment-ok';
})();
