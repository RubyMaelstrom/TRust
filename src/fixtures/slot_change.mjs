// DOM #signal-a-slot-change and #notify-mutation-observers: slotchange is
// queued with the mutation observer microtask and fired after the observer
// callbacks, at each signalled slot once, bubbling but not composed.
(function () {
    const { host, root, s1, s2, a, b, c } = globalThis.slotFixture;
    const log = [];
    globalThis.slotLog = log;
    const name = target => target === s1 ? 's1' : target === s2 ? 's2' : '?';
    const observer = new MutationObserver(() => log.push('mo'));
    observer.observe(host, { childList: true });
    root.addEventListener('slotchange', e => log.push('change:' + name(e.target) + ':' + e.bubbles + ':' +
        e.composed + ':' + e.isTrusted));
    host.addEventListener('slotchange', () => log.push('leaked'));
    s1.assign(b, c);
    host.appendChild(document.createElement('u'));
    if (log.length) throw Error('slotchange is not synchronous');
    Promise.resolve().then(() => {
        log.push('|');
        // The same assignment signals nothing; a change and its reversal
        // within one task signal once.
        s1.assign(b, c);
        s2.assign(b);
        s2.assign(a);
        Promise.resolve().then(() => {
            log.push('|');
            const empty = document.createElement('slot');
            root.appendChild(empty);
            empty.addEventListener('slotchange', () => log.push('fallback'));
            Promise.resolve().then(() => {
                // Fallback content of a slot without assigned nodes.
                empty.appendChild(document.createTextNode('fallback'));
                // A removed slot loses its assigned nodes.
                s2.remove();
                s2.addEventListener('slotchange', () => log.push('removed-s2'));
            });
        });
    });
    return 'slot-change-queued';
})();
