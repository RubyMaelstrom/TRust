(() => {
    function check(value, message) { if (!value) throw new Error(message); }
    // Encoding Standard: every Latin-1 / ASCII label selects windows-1252.
    const labels = ['ansi_x3.4-1968','ascii','cp1252','cp819','csisolatin1','ibm819',
        'iso-8859-1','iso-ir-100','iso8859-1','iso88591','iso_8859-1','iso_8859-1:1987',
        'l1','latin1','us-ascii','windows-1252','x-cp1252'];
    const special = [0x20ac,0x81,0x201a,0x192,0x201e,0x2026,0x2020,0x2021,
        0x2c6,0x2030,0x160,0x2039,0x152,0x8d,0x17d,0x8f,0x90,0x2018,
        0x2019,0x201c,0x201d,0x2022,0x2013,0x2014,0x2dc,0x2122,0x161,
        0x203a,0x153,0x9d,0x17e,0x178];
    const bytes = Uint8Array.from({length:256}, (_, i) => i);
    const expected = Array.from(bytes, n => String.fromCharCode(n>=128 && n<160 ? special[n-128] : n)).join('');
    for (const label of labels) {
        const decoder = new TextDecoder('\t ' + label.toUpperCase() + '\r\n', {fatal:true});
        check(decoder.encoding === 'windows-1252', 'canonical label: ' + label);
        check(decoder.decode(bytes) === expected, 'all 256 byte mappings: ' + label);
        check(decoder.decode() === '', 'empty flush: ' + label);
        let streamed = '';
        for (let i = 0; i < bytes.length; i++) streamed += decoder.decode(bytes.subarray(i,i+1), {stream:true});
        check(streamed + decoder.decode() === expected, 'streamed byte mappings: ' + label);
    }
    const decoder = new TextDecoder('iso-8859-1');
    check(decoder.decode(new DataView(bytes.buffer, 128, 3)) === '€\x81‚', 'view offset and length');
    check(decoder.decode(new Uint8Array([0xef,0xbb,0xbf])) === 'ï»¿', 'single-byte decoder does not consume UTF-8 BOM');
    for (const label of ['\u00a0latin1','latin1\u000b','\u0085utf-8','utf-8\u00a0','replacement','']) {
        let error;
        try { new TextDecoder(label); } catch (e) { error=e; }
        check(error instanceof RangeError, 'reject non-label / non-ASCII whitespace: ' + JSON.stringify(label));
    }
    let symbolError;
    try { new TextDecoder(Symbol('utf-8')); } catch (e) { symbolError=e; }
    check(symbolError instanceof TypeError, 'DOMString conversion rejects Symbol');
    // Keep the actual producer/consumer contract small and site-independent.
    const decodePacket = function (buffer) { return decoder.decode(buffer); }.bind(null);
    const packet = decodePacket(new Uint8Array([0x41,0x80,0xe9]));
    check(typeof packet === 'string' && packet.charCodeAt(1) === 0x20ac, 'decoded packet is a string');
    if (typeof document !== 'undefined') {
        const html = document.createElement('html'), body = document.createElement('body');
        document.appendChild(html); html.appendChild(body);
        const frame = document.createElement('iframe'); body.appendChild(frame);
        const child = frame.contentWindow;
        const foreign = new child.Uint8Array([0x80,0xe9]);
        check(decoder.decode(foreign) === '€é', 'foreign-realm view');
        check(new child.TextDecoder('latin1').decode(bytes) === expected, 'child Window decoder');
        frame.remove();
    }
    return 'text-decoder-legacy-ok';
})()
