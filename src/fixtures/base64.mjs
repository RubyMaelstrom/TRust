function assertBase64(ok, label) { if (!ok) throw new Error(label); }
function rejectsBase64(fn, name) {
    try { fn(); } catch(e) {
        assertBase64(e.name === name, 'expected ' + name + ', got ' + e);
        if (name === 'InvalidCharacterError') assertBase64(e instanceof DOMException, 'DOMException brand');
        return;
    }
    throw new Error('expected ' + name);
}
for (const [encoded, decoded] of [['',''], ['Zg==','f'], ['Zm8=','fo'], ['Zm9v','foo'],
    ['Zm9vYg==','foob'], ['Zm9vYmE=','fooba'], ['Zm9vYmFy','foobar'], ['AP+A','\0\xff\x80']]) {
    assertBase64(atob(encoded) === decoded && btoa(decoded) === encoded, 'RFC 4648 / binary round-trip');
}
for (const encoded of ['YQ', 'YR', 'YQ==', 'YR==', ' Y\tR\n=\f=\r']) {
    assertBase64(atob(encoded) === 'a', 'forgiving padding and ASCII whitespace');
}
for (const encoded of ['=', '==', '===', '====', 'Y', 'YQ=', 'YQ===', 'Y=Q=', 'YQ==A',
    'YQ-=', 'YQ_=', 'YQ\v==', 'YQ\u00a0==', '\ud800', '\udfff', 'é']) {
    rejectsBase64(() => atob(encoded), 'InvalidCharacterError');
}
for (const decoded of ['\u0100', '\ud800', '\udfff', '😀']) {
    rejectsBase64(() => btoa(decoded), 'InvalidCharacterError');
}
for (const operation of [atob, btoa]) {
    assertBase64(operation.length === 1 && !('prototype' in operation), 'operation shape');
    rejectsBase64(() => operation(), 'TypeError');
    rejectsBase64(() => operation(Symbol()), 'TypeError');
    let calls = 0;
    operation({[Symbol.toPrimitive](hint) { assertBase64(hint === 'string', 'DOMString hint'); calls++; return 'YQ=='; }});
    assertBase64(calls === 1, 'one coercion');
    const error = {};
    try { operation({toString() { throw error; }}); } catch(e) { assertBase64(e === error, 'coercion error identity'); }
}
let bytes = '';
for (let i = 0; i < 256; ++i) bytes += String.fromCharCode(i);
bytes = bytes.repeat(256);
assertBase64(atob(btoa(bytes)) === bytes, 'large all-byte round-trip');
'base64-ok'
