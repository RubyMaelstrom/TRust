(() => {
    // Encoding #utf-8-decoder, #concept-td-serialize and TextDecoder.decode.
    const check = (ok, label) => { if (!ok) throw Error(label); };
    const vectors = [
        [[], ""], [[0, 65, 127], "\0A\x7f"],
        [[0xc2, 0xa2, 0xe2, 0x82, 0xac, 0xf0, 0x9f, 0x98, 0x80], "¢€😀"],
        [[0xef, 0xbb, 0xbf, 65, 0xef, 0xbb, 0xbf], "A\ufeff"],
        [[0xe1, 0x80], "�"], [[0xe1, 0x80, 65], "�A"],
        [[0xf0, 0x90, 0x80, 65], "�A"], [[0xef, 0xbb], "�"],
        [[0xc0, 0xaf], "��"], [[0xe0, 0x80, 0x80], "���"],
        [[0xed, 0xa0, 0x80], "���"], [[0xf4, 0x90, 0x80, 0x80], "����"],
        [[0xf5, 0x80, 0x80, 0x80], "����"], [[0xff, 65, 0x80], "�A�"],
    ];
    for (const [bytes, expected] of vectors) {
        check(new TextDecoder().decode(new Uint8Array(bytes)) === expected,
            "whole decode: " + bytes);
        // Every pair of boundaries includes empty chunks and incomplete
        // sequences followed by invalid continuation bytes in later chunks.
        for (let i = 0; i <= bytes.length; i++) {
            for (let j = i; j <= bytes.length; j++) {
                const d = new TextDecoder();
                const a = d.decode(new Uint8Array(bytes.slice(0, i)), {stream: true});
                const b = d.decode(new Uint8Array(bytes.slice(i, j)), {stream: true});
                const c = d.decode(new Uint8Array(bytes.slice(j)));
                check(a + b + c === expected, "stream boundaries: " + bytes + ":" + i + ":" + j);
            }
        }
    }
    const keep = new TextDecoder("utf-8", {ignoreBOM: true});
    check(keep.decode(new Uint8Array([0xef]), {stream: true}) === "", "partial kept BOM");
    check(keep.decode(new Uint8Array([0xbb, 0xbf, 65])) === "\ufeffA", "kept BOM");
    const snapshot = new TextDecoder();
    const prefix = new Uint8Array([0xef, 0xbb]);
    check(snapshot.decode(prefix, {stream: true}) === "", "pending BOM");
    prefix.fill(0);
    check(snapshot.decode(new Uint8Array([0xbf, 65])) === "A", "pending bytes copied");

    const fatal = new TextDecoder("utf-8", {fatal: true});
    let threw = false;
    try { fatal.decode(new Uint8Array([65, 0xe1, 0x80, 66, 67]), {stream: true}); }
    catch (error) { threw = error instanceof TypeError; }
    check(threw, "fatal invalid continuation");
    check(fatal.decode(new Uint8Array([68]), {stream: true}) === "BCD",
        "fatal preserves the restored byte and remaining I/O queue");
    // A failed call never serialized its output and thus never consumed a BOM.
    threw = false;
    const fatalBom = new TextDecoder("utf-8", {fatal: true});
    try { fatalBom.decode(new Uint8Array([0xef, 0xbb, 0xbf, 0xff]), {stream: true}); }
    catch (error) { threw = error instanceof TypeError; }
    check(threw && fatalBom.decode(new Uint8Array([0xef, 0xbb, 0xbf, 65])) === "A",
        "BOM state changes only during serialization");
    threw = false;
    try { fatal.decode(new Uint8Array([0xe1, 0x80])); }
    catch (error) { threw = error instanceof TypeError; }
    check(threw && fatal.decode(new Uint8Array([65])) === "A", "fatal EOF and new session");
    const buffer = new Uint8Array([88, 65, 0xc2, 0xa2, 89]);
    check(new TextDecoder().decode(new DataView(buffer.buffer, 1, 3)) === "A¢", "view offset");
    const text = "ASCII café € 😀".repeat(512);
    check(new TextDecoder().decode(new TextEncoder().encode(text)) === text, "response-size UTF-8");
    return "text-decoder-utf8-ok";
})()
