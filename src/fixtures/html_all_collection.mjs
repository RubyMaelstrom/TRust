// HTML #the-htmlallcollection-interface and ECMA-262 Annex B.3.6.
(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function throws(name, callback) {
        try { callback(); } catch (e) { check(e.name === name, 'wrong error ' + e); return; }
        throw Error('missing ' + name);
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    body.innerHTML = '<a id=x name=n1></a><form name=f></form><div name=n1 id=d></div>' +
        '<img name=im><p id=dup></p><span id=dup></span><div name=plain></div>';
    const all = document.all;
    check(typeof all === 'undefined' && !all && all == null && all == undefined && all !== undefined && all !== null, 'IsHTMLDDA');
    check(all === document.all, 'SameObject');
    check(Object.getPrototypeOf(all) === HTMLAllCollection.prototype && String(all) === '[object HTMLAllCollection]' &&
        Object.prototype.toString.call(all) === '[object HTMLAllCollection]', 'interface');
    throws('TypeError', () => new HTMLAllCollection());
    throws('TypeError', () => HTMLAllCollection.prototype.item.call({}, 0));
    throws('TypeError', () => Object.getOwnPropertyDescriptor(HTMLAllCollection.prototype, 'length').get.call({}));
    check(!Object.hasOwn(all, 'length') && !Object.hasOwn(all, 'name'), 'no own function members');
    const elements = document.getElementsByTagName('*');
    check(all.length === elements.length && all.length === 9, 'length ' + all.length);
    check(all[0] === html && all[1] === body && all[2].id === 'x', 'indexed tree order');
    check(all[all.length] === undefined && !(String(all.length) in all) && '0' in all, 'indexed bounds');
    const a = document.getElementById('x'), form = document.querySelector('form');
    check(all.x === a && all.d === document.getElementById('d') && all.f === form && all.im === document.querySelector('img'), 'named access');
    check(all.n1 === a, 'name attribute counts only for all-named elements');
    check(all.plain === undefined && !('plain' in all), 'div name is not a supported name');
    const dup = all.dup;
    check(dup instanceof HTMLCollection && dup.length === 2 && dup[0].localName === 'p' && dup[1].localName === 'span', 'duplicate names collect');
    check(all.item('x') === a && all.item(0) === html && all.item('0') === html && all.item() === null &&
        all.item(undefined) === null && all.item('99999') === null && all.item(4294967295) === null, 'item');
    check(all('x') === a && all(0) === html && all() === null && all('dup').length === 2, 'legacy caller');
    check(all.namedItem('nope') === null && all.namedItem('f') === form, 'namedItem');
    throws('TypeError', () => all.namedItem());
    check(Object.keys(all).length === all.length && Object.getOwnPropertyNames(all).includes('x'), 'unenumerable named properties');
    check([...all].length === all.length && [...all][0] === html, 'iterable');
    const extra = document.createElement('em'); extra.id = 'late'; body.appendChild(extra);
    check(all.length === 10 && all[all.length - 1] === extra && all.late === extra && dup.length === 2, 'live');
    document.getElementById('dup').remove();
    check(dup.length === 1 && all.dup.localName === 'span', 'live named collection');
    const parsed = new DOMParser().parseFromString('<p id=q>', 'text/html');
    check(parsed.all !== all && parsed.all === parsed.all && parsed.all.q.localName === 'p' && !parsed.all, 'one collection per Document');
    return 'html-all-collection-ok';
})();
