// WHATWG HTML §4.9 + §16.3.10; DOM §4.2.10.2; Web IDL legacy
// platform objects and long/unsigned-long/interface conversions.
(() => {
    function assert(value, message) { if (!value) throw new Error(message); }
    function throws(name, fn, message) {
        try { fn(); } catch (error) {
            assert(error.name === name, message + ': ' + error);
            return;
        }
        throw new Error(message + ': did not throw');
    }
    const htmlNS = 'http://www.w3.org/1999/xhtml';
    const create = name => document.createElement(name);
    const table = create('table');
    document.body.appendChild(table);
    const rows = table.rows, bodies = table.tBodies;
    assert(rows === table.rows && bodies === table.tBodies, 'SameObject table collections');
    assert(rows instanceof HTMLCollection && !Array.isArray(rows), 'real HTMLCollection');
    assert(Object.prototype.toString.call(rows) === '[object HTMLCollection]', 'collection tag');
    assert(rows.length === 0 && rows.item(0) === null && rows[0] === undefined, 'empty collection');
    throws('TypeError', () => new HTMLCollection(), 'illegal collection constructor');
    throws('TypeError', () => rows.item(), 'required item index');
    throws('TypeError', () => rows.namedItem(), 'required namedItem name');
    throws('TypeError', () => rows.item(0n), 'item rejects BigInt');
    throws('TypeError', () => rows.namedItem(Symbol()), 'namedItem rejects Symbol');
    throws('TypeError', () => HTMLCollection.prototype.item.call({}, 0), 'collection brand');

    table.deleteRow(-1);
    throws('IndexSizeError', () => table.deleteRow(0), 'empty deleteRow');
    throws('TypeError', () => table.deleteRow(), 'required deleteRow index');
    throws('IndexSizeError', () => table.insertRow(1), 'insert beyond empty table');
    throws('IndexSizeError', () => table.insertRow(-2), 'negative insert');
    throws('TypeError', () => table.insertRow(1n), 'insert rejects BigInt');
    throws('TypeError', () => table.insertRow(Symbol()), 'insert rejects Symbol');
    throws('TypeError', () => HTMLTableElement.prototype.insertRow.call(create('div')), 'table brand');
    throws('TypeError', () => HTMLTableElement.prototype.insertRow.call(Object.create(HTMLTableElement.prototype)), 'forged brand');

    const first = table.insertRow();
    assert(first instanceof HTMLTableRowElement && first.ownerDocument === document, 'row interface/document');
    assert(bodies.length === 1 && first.parentNode === bodies[0] && rows[0] === first, 'automatic tbody');
    first.id = 'first'; first.setAttribute('name', 'named');
    assert(rows.first === first && rows.named === first && rows.namedItem('named') === first, 'named access');
    assert(rows.namedItem('') === null && rows.namedItem('missing') === null, 'missing named item');
    assert('0' in rows && 'named' in rows && !('1' in rows), 'collection has');
    assert(Object.keys(rows).join(',') === '0', 'only indices enumerable');
    assert(Object.getOwnPropertyNames(rows).join(',') === '0,first,named', 'supported own property names');
    assert(Object.getOwnPropertyDescriptor(rows, 'first').enumerable === false, 'names not enumerable');
    assert(Object.getOwnPropertyDescriptor(rows, '0').writable === false, 'index not writable');
    assert(!Reflect.set(rows, '0', null) && !Reflect.defineProperty(rows, '4', { value: first }), 'indexed writes rejected');
    assert(!Reflect.deleteProperty(rows, '0') && !Reflect.deleteProperty(rows, 'first'), 'supported deletes rejected');
    assert(Reflect.deleteProperty(rows, '4') && !Reflect.preventExtensions(rows), 'legacy extensibility');
    rows.note = 42;
    assert(rows.note === 42, 'collection expando');
    delete rows.note;
    first.id = 'item';
    assert(typeof rows.item === 'function' && rows.namedItem('item') === first, 'builtins mask names');
    first.id = 'first';
    assert(rows.item(4294967296) === first && rows.item(NaN) === first, 'unsigned long conversion');
    assert(rows.item(-1) === null, 'unsigned negative conversion');

    const secondBody = table.createTBody(), secondRows = secondBody.rows;
    assert(secondRows === secondBody.rows && bodies.length === 2, 'live section collection');
    first.remove();
    assert(rows.length === 0 && rows.first === undefined, 'live removal');
    const second = table.insertRow(undefined);
    assert(second.parentNode === secondBody && secondRows[0] === second, 'empty table uses last tbody');
    assert(second.rowIndex === 0 && second.sectionRowIndex === 0, 'row indices');
    const foot = table.createTFoot(), footRow = foot.insertRow();
    const head = table.createTHead(), headRow = head.insertRow();
    const direct = create('tr'); table.appendChild(direct);
    assert(Array.from(rows).every((row, i) => row === [headRow, second, direct, footRow][i]), 'thead first/tfoot last');
    assert(direct.sectionRowIndex === 2 && footRow.rowIndex === 3, 'direct and footer indices');
    const appended = table.insertRow(-1);
    assert(appended.parentNode === foot, 'append follows last row, not last DOM child');
    const inserted = table.insertRow(1);
    assert(inserted.parentNode === secondBody && inserted.nextSibling === second, 'insert into indexed parent');
    assert(second.rowIndex === 2 && second.sectionRowIndex === 1, 'indices update after insertion');
    table.deleteRow(-1);
    assert(appended.parentNode === null, 'delete last in collection order');
    assert(table.insertRow(4294967296).parentNode === head, 'signed long wrapping');
    const atZero = table.insertRow(NaN); table.deleteRow(undefined);
    assert(atZero.parentNode === null, 'explicit undefined delete converts to zero');
    const mutationIndex = { valueOf() { table.deleteRow(-1); return -1; } };
    table.insertRow(mutationIndex);
    assert(table.rows.length === 6, 'convert index before collecting rows');

    const nested = create('table');
    second.insertCell().appendChild(nested);
    nested.insertRow().insertCell();
    assert(rows.length === 6, 'nested rows excluded');
    secondBody.appendChild(document.createElementNS('urn:test', 'tr'));
    assert(secondRows.length === 2, 'foreign namespace rows excluded');
    const detached = create('tbody'), detachedRow = detached.insertRow();
    assert(detachedRow.rowIndex === -1 && detachedRow.sectionRowIndex === 0, 'detached section indices');
    detachedRow.remove();
    assert(detachedRow.sectionRowIndex === -1, 'detached row index');
    detached.deleteRow(-1);
    throws('TypeError', () => detached.deleteRow(), 'section required index');
    throws('IndexSizeError', () => detached.insertRow(2), 'section bounds');

    const row = create('tr'), cells = row.cells;
    assert(cells === row.cells && cells instanceof HTMLCollection, 'SameObject cells');
    row.deleteCell(-1);
    throws('TypeError', () => row.deleteCell(), 'required deleteCell index');
    throws('IndexSizeError', () => row.deleteCell(0), 'empty deleteCell');
    const td = row.insertCell();
    const th = create('th'); row.insertBefore(th, td);
    row.insertBefore(document.createTextNode('ignored'), td);
    row.appendChild(document.createElementNS('urn:test', 'td'));
    assert(cells.length === 2 && cells[0] === th && td.cellIndex === 1, 'mixed cells and text/namespace filtering');
    const middle = row.insertCell(1);
    assert(middle.localName === 'td' && middle.nextSibling === td && td.cellIndex === 2, 'insert before indexed cell');
    row.deleteCell(0);
    assert(th.cellIndex === -1 && td.cellIndex === 1, 'cell indices after deletion');
    row.deleteCell(-1);
    assert(td.cellIndex === -1 && cells.length === 1, 'live cells removal');
    throws('IndexSizeError', () => row.insertCell(2), 'cell insert bounds');
    throws('TypeError', () => HTMLTableRowElement.prototype.insertCell.call(table), 'cell method brand');

    const parts = create('table');
    const comment = document.createComment('before'); parts.appendChild(comment);
    const colgroup = create('colgroup'); parts.appendChild(colgroup);
    const body = parts.createTBody(), tail = parts.createTFoot();
    const caption = parts.createCaption(), top = parts.createTHead();
    assert(parts.firstChild === caption && caption.nextSibling === comment, 'caption inserted first node');
    assert(top.previousSibling === colgroup && top.nextSibling === body, 'head after caption/colgroup');
    assert(parts.createCaption() === caption && parts.createTHead() === top && parts.createTFoot() === tail, 'idempotent creators');
    assert(parts.createTBody().nextSibling === tail, 'new tbody follows last tbody');
    throws('HierarchyRequestError', () => { parts.tHead = tail; }, 'wrong section kind');
    throws('TypeError', () => { parts.caption = create('div'); }, 'caption interface conversion');
    assert(parts.tHead === top && parts.caption === caption, 'failed conversion preserves old part');
    const replacement = create('caption'); parts.caption = replacement;
    assert(parts.caption === replacement && caption.parentNode === null, 'replace caption');
    parts.caption = undefined; parts.tHead = null; parts.tFoot = null;
    assert(parts.caption === null && parts.tHead === null && parts.tFoot === null, 'nullable setters');
    parts.deleteCaption(); parts.deleteTHead(); parts.deleteTFoot();

    for (const name of ['td', 'th', 'col', 'colgroup']) {
        const cell = create(name), prop = name === 'td' || name === 'th' ? 'colSpan' : 'span';
        const attr = prop.toLowerCase();
        assert(cell[prop] === 1, name + ' default span');
        cell[prop] = 0;
        assert(cell[prop] === 1 && cell.getAttribute(attr) === '0', 'getter-only min clamping');
        cell[prop] = 1001;
        assert(cell[prop] === 1000 && cell.getAttribute(attr) === '1001', 'getter-only max clamping');
        cell[prop] = -1;
        assert(cell[prop] === 1 && cell.getAttribute(attr) === '1', 'unsigned reflection fallback');
        cell.setAttribute(attr, ' \t+23tail');
        assert(cell[prop] === 23, 'HTML integer parsing');
        cell.setAttribute(attr, '-2');
        assert(cell[prop] === 1, 'invalid negative attribute');
        throws('TypeError', () => { cell[prop] = 1n; }, 'span rejects BigInt');
    }
    td.rowSpan = 0;
    assert(td.rowSpan === 0, 'rowSpan zero is meaningful');
    td.rowSpan = 65535;
    assert(td.rowSpan === 65534 && td.getAttribute('rowspan') === '65535', 'rowSpan max');
    td.scope = 'ROW'; assert(td.scope === 'row' && td.getAttribute('scope') === 'ROW', 'scope canonical getter');
    td.scope = 'invalid'; assert(td.scope === '', 'scope unknown value');
    td.headers = 'one two'; td.abbr = 'short'; td.ch = '.'; td.chOff = '2'; td.vAlign = 'top';
    assert(td.getAttribute('headers') === 'one two' && td.getAttribute('abbr') === 'short', 'cell string reflectors');
    assert(td.getAttribute('char') === '.' && td.getAttribute('charoff') === '2' && td.getAttribute('valign') === 'top', 'legacy attribute aliases');
    td.noWrap = true; assert(td.hasAttribute('nowrap'), 'boolean reflection');
    td.noWrap = false; assert(!td.hasAttribute('nowrap'), 'boolean removal');
    table.cellPadding = null; td.bgColor = null;
    assert(table.getAttribute('cellpadding') === '' && td.getAttribute('bgcolor') === '', 'legacy null-to-empty');
    throws('TypeError', () => { td.headers = Symbol(); }, 'DOMString Symbol conversion');

    // An indexed collection iterator resolves its next member against the live
    // collection, including additions made after iteration begins.
    const live = create('tbody'); live.insertRow();
    const iterator = live.rows[Symbol.iterator]();
    assert(iterator.next().value === live.rows[0], 'iterator first');
    const added = live.insertRow();
    assert(iterator.next().value === added && iterator.next().done, 'live iterator');

    const xml = new DOMParser().parseFromString('<root/>', 'application/xml');
    const xmlTable = xml.createElementNS(htmlNS, 'table');
    xml.documentElement.appendChild(xmlTable);
    const xmlRow = xmlTable.insertRow(), xmlCell = xmlRow.insertCell();
    assert(xmlRow.namespaceURI === htmlNS && xmlCell.namespaceURI === htmlNS, 'created HTML namespace in XML');
    assert(xmlRow.ownerDocument === xml && xmlCell.ownerDocument === xml, 'owning XML document');
    table.remove();
    return 'html-tables-ok';
})();
