// DOM #xpath (XPathEvaluatorBase, XPathExpression, XPathResult) with the
// semantics of DOM Level 3 XPath and XPath 1.0, and HTML #xpath-1.0-processors.
(() => {
    const check = (condition, message) => { if (!condition) throw new Error(message); };
    const thrown = (f) => { try { f(); return 'none'; } catch (e) { return e.name; } };
    const SVG = 'http://www.w3.org/2000/svg';
    document.body.innerHTML = '<div id=a><p id=p1 class=x>one</p><p id=p2>two<b>bold</b></p><!--c--></div>' +
        '<svg id=s><rect id=r /></svg>';
    const p1 = document.getElementById('p1');

    // Result types (DOM L3 XPath XPathResultType).
    check(XPathResult.ORDERED_NODE_SNAPSHOT_TYPE === 7 && XPathResult.prototype.ANY_TYPE === 0, 'constants');
    let result = document.evaluate('//p', document, null, XPathResult.ANY_TYPE, null);
    check(result instanceof XPathResult && result.resultType === XPathResult.UNORDERED_NODE_ITERATOR_TYPE, 'ANY_TYPE node-set');
    check(result.iterateNext() === p1 && result.iterateNext().id === 'p2' && result.iterateNext() === null, 'iteration');
    result = document.evaluate('//p', document, null, XPathResult.ORDERED_NODE_ITERATOR_TYPE, null);
    result.iterateNext();
    check(!result.invalidIteratorState, 'valid iterator');
    document.body.append(document.createElement('i'));
    check(result.invalidIteratorState && thrown(() => result.iterateNext()) === 'InvalidStateError', 'mutation invalidates iterators');
    const snapshot = document.evaluate('//p', document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
    p1.remove();
    check(snapshot.snapshotLength === 2 && snapshot.snapshotItem(0) === p1 && snapshot.snapshotItem(5) === null &&
        !snapshot.invalidIteratorState, 'snapshots survive mutation');
    document.getElementById('a').prepend(p1);
    check(document.evaluate('//p', document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue === p1, 'first node');
    check(document.evaluate('//nothing', document, null, XPathResult.ANY_UNORDERED_NODE_TYPE, null).singleNodeValue === null, 'no node');
    const count = document.evaluate('count(//p)', document, null, XPathResult.ANY_TYPE, null);
    check(count.resultType === 1 && count.numberValue === 2, 'number');
    check(document.evaluate('//p', document, null, XPathResult.STRING_TYPE, null).stringValue === 'one', 'string conversion');
    check(document.evaluate('//p', document, null, XPathResult.BOOLEAN_TYPE, null).booleanValue === true, 'boolean conversion');
    check(document.evaluate('//p', document, null, XPathResult.NUMBER_TYPE, null).numberValue !== document.evaluate('//p', document, null, 1, null).numberValue, 'NaN');
    check(document.evaluate('1 div 0', document, null, XPathResult.NUMBER_TYPE, null).numberValue === Infinity, 'Infinity');
    check(document.evaluate("concat('a', 'b')", document).stringValue === 'ab', 'default type and result');

    // Errors.
    check(thrown(() => document.evaluate('1', document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null)) === 'TypeError', 'not a node-set');
    check(thrown(() => snapshot.numberValue) === 'TypeError' && thrown(() => snapshot.iterateNext()) === 'TypeError', 'wrong result type');
    check(thrown(() => document.evaluate('//p[', document)) === 'SyntaxError', 'syntax error');
    check(thrown(() => document.evaluate('$x', document)) === 'SyntaxError', 'variables');
    check(thrown(() => document.evaluate('//x:p', document)) === 'NamespaceError', 'unresolved prefix');
    check(thrown(() => document.evaluate('//p', document, null, 10)) === 'NotSupportedError', 'unknown type');
    check(thrown(() => document.evaluate('//p', document.createDocumentFragment())) === 'NotSupportedError', 'fragment context');
    check(thrown(() => document.evaluate('//p', {})) === 'TypeError', 'non-node context');
    check(thrown(() => document.evaluate('//p', document, null, 0, {})) === 'TypeError', 'result argument');
    check(thrown(() => new XPathExpression()) === 'TypeError' && thrown(() => new XPathResult()) === 'TypeError', 'no constructors');

    // Namespace resolvers: a function, an object, a node.
    check(document.evaluate('//svg:rect', document, (prefix) => prefix === 'svg' ? SVG : null,
        XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue.id === 'r', 'function resolver');
    check(document.evaluate('//svg:rect', document, {lookupNamespaceURI: () => SVG}, 9, null).singleNodeValue.id === 'r', 'object resolver');
    const svg = document.getElementById('s');
    check(document.createNSResolver(svg) === svg, 'createNSResolver returns the node');
    svg.setAttribute('xmlns:v', SVG);
    check(thrown(() => document.evaluate('//v:rect', document, document.createNSResolver(document.body))) === 'NamespaceError', 'node resolver scope');

    // HTML documents: unprefixed names are HTML names, ASCII case-insensitive.
    check(document.evaluate('count(//P)', document).numberValue === 2 && document.evaluate('count(//rect)', document).numberValue === 0, 'HTML name tests');
    check(document.evaluate('count(/html/namespace::*)', document).numberValue === 2, 'namespace nodes in expressions');
    check(thrown(() => document.evaluate('/html/namespace::*', document, null, 7)) === 'NotSupportedError', 'namespace nodes have no DOM node');

    // Attr nodes and context nodes.
    const attr = document.evaluate('//p/@class', document, null, 9, null).singleNodeValue;
    check(attr instanceof Attr && attr.value === 'x' && attr === p1.getAttributeNode('class'), 'Attr results');
    check(document.evaluate('name(..)', attr, null, XPathResult.STRING_TYPE, null).stringValue === 'p', 'Attr context');
    const holder = document.createElement('q');
    const first = document.createTextNode('A'), second = document.createTextNode('B');
    holder.append(first, second);
    check(document.evaluate('string(.)', second, null, 2, null).stringValue === 'AB', 'logical text node');
    check(document.evaluate('text()', holder, null, 7, null).snapshotLength === 1, 'one text node');
    // The root node of a detached subtree is its root element.
    check(document.evaluate('count(/self::q)', second).numberValue === 1 &&
        document.evaluate('count(//q)', holder).numberValue === 0, 'detached tree root');

    // XPathExpression and XPathEvaluator.
    const evaluator = new XPathEvaluator();
    const expression = evaluator.createExpression('count(p)', null);
    check(expression instanceof XPathExpression && expression.evaluate(document.getElementById('a'), 1).numberValue === 2, 'XPathExpression');
    check(evaluator.evaluate('string(//b)', document, null, 2, null).stringValue === 'bold', 'XPathEvaluator.evaluate');
    check(Document.prototype.evaluate.length === 2 && XPathEvaluator.prototype.createExpression.length === 1, 'lengths');

    // XML documents: names compare exactly in the null namespace.
    const xml = new DOMParser().parseFromString('<r xmlns:a="urn:a"><Item/><a:item/></r>', 'application/xml');
    check(xml.evaluate('count(//Item)', xml).numberValue === 1 && xml.evaluate('count(//item)', xml).numberValue === 0, 'XML names');
    check(xml.evaluate('count(//a:item)', xml, xml.createNSResolver(xml.documentElement)).numberValue === 1, 'XML prefixes');
    return 'xpath-interfaces-ok';
})()
