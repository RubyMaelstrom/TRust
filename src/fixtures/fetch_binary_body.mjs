(async function () {
    const check = (ok, message) => { if (!ok) throw new Error(message); };
    const source = new Uint8Array([91, 0, 128, 255, 92]);
    const response = new Response(source.subarray(1, 4));
    source.fill(7);
    check([...new Uint8Array(await response.arrayBuffer())].join() === '0,128,255',
        'Response snapshots the view range at construction');
    const data = new Uint8Array([99, 1, 129, 254, 88]);
    const stream = new Response(new DataView(data.buffer, 1, 3)).body;
    data.fill(0);
    const reader = stream.getReader();
    check([...(await reader.read()).value].join() === '1,129,254', 'DataView bytes stream unchanged');
    check((await reader.read()).done, 'buffered stream closes');
    const requestData = new Uint8Array([99, 0, 128, 255, 88]);
    const request = new Request('/post', {method: 'POST', body: new DataView(requestData.buffer, 1, 3)});
    requestData.fill(5);
    check([...new Uint8Array(await request.arrayBuffer())].join() === '0,128,255',
        'Request snapshots the view range at construction');

    const cache = await caches.open('binary-conformance');
    const large = new Uint8Array(1024 * 1024);
    for (let i = 0; i < large.length; i++) large[i] = i & 255;
    await cache.put('/binary', new Response(large, {headers: {'content-type': 'application/wasm'}}));
    large.fill(9);
    const hit = await cache.match('/binary');
    const bytes = new Uint8Array(await hit.arrayBuffer());
    check(bytes.length === large.length, 'cache retains full length');
    for (let i = 0; i < bytes.length; i++) check(bytes[i] === (i & 255), 'cache retains every byte');
    bytes.fill(3);
    const again = new Uint8Array(await (await cache.match('/binary')).arrayBuffer());
    check(again[0] === 0 && again[255] === 255, 'cache reads have independent byte storage');
    return 'fetch-binary-body-ok';
})()
