// HTML content document/window getters return the existing child navigable.
// A javascript: navigation in a grandchild must not recreate its ancestors.
document.appendChild(document.createElement('body'));
const outer = document.createElement('iframe');
outer.srcdoc = '<body><p id="sentinel">original</p><iframe id="inner"></iframe>';
document.body.appendChild(outer);
__trust.hydrateFrames();
const original = outer.contentDocument;
original.getElementById('sentinel').textContent = 'kept';
const fallback = document.createTextNode('fallback');
outer.appendChild(fallback);
const inner = original.getElementById('inner');
inner.srcdoc = '<body><script>globalThis.probe = function (ancestor, fallback) {' +
    'const getParent = Object.getOwnPropertyDescriptor(Node.prototype, "parentNode").get;' +
    'const same = getParent.call(fallback) === ancestor;' +
    'const ad = document.createElement("iframe");' +
    'ad.src = "javascript:\'<body id=ad>advertisement</body>\'";' +
    'document.body.appendChild(ad);' +
    'return same + "|" + ad.contentDocument.body.id;' +
    '};<\/script>';
const child = inner.contentWindow;
const result = child.probe(outer, fallback);
[outer.contentDocument === original, outer.contentDocument.getElementById('sentinel').textContent, result].join('|')
