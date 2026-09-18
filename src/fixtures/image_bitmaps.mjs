// HTML #imagebitmap / #dom-createImageBitmap; shared Window/Worker cases.
globalThis.bitmapResult = "pending";
(async () => {
    function check(value, label) { if (!value) throw Error(label); }
    async function rejects(promise, name) {
        try { await promise; } catch (error) { check(error.name === name, name + ": " + error); return; }
        throw Error("missing " + name);
    }
    function throws(fn, name) {
        try { fn(); } catch (error) { check(error.name === name, name + ": " + error); return; }
        throw Error("missing " + name);
    }
    check(createImageBitmap.length === 1, "factory arity");
    check(Object.getPrototypeOf(createImageBitmap) === Function.prototype, "ordinary operation");
    throws(() => new ImageBitmap(), "TypeError");
    throws(() => new createImageBitmap(new ImageData(1,1)), "TypeError");
    throws(() => Object.getOwnPropertyDescriptor(ImageBitmap.prototype, "width").get.call({}), "TypeError");
    const data = new ImageData(new Uint8ClampedArray([255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,255,255]), 2, 2);
    let settled = false;
    const pending = createImageBitmap(data);
    pending.then(() => { settled = true; });
    data.data[0] = 0;
    await Promise.resolve();
    check(!settled, "bitmap fulfillment requires a task");
    const bitmap = await pending;
    check(bitmap.width === 2 && bitmap.height === 2, "dimensions");
    check(Object.prototype.toString.call(bitmap) === "[object ImageBitmap]", "brand");
    check(Object.keys(bitmap).length === 0, "private pixels");
    const copied = structuredClone([bitmap, bitmap]);
    check(copied[0] === copied[1] && copied[0] !== bitmap && bitmap.width === 2, "clone identity");
    const transferred = structuredClone({a:copied[0], b:copied[0]}, {transfer:[copied[0]]});
    check(copied[0].width === 0 && transferred.a === transferred.b && transferred.a.width === 2, "transfer identity/detachment");
    const channel = new MessageChannel();
    const delivery = new Promise(resolve => { channel.port2.onmessage = event => resolve(event.data); });
    channel.port1.postMessage(transferred.a, [transferred.a]);
    check(transferred.a.height === 0, "port transfer detaches synchronously");
    const received = await delivery;
    check(received instanceof ImageBitmap && received.width === 2, "port transfer brand");
    channel.port1.close(); channel.port2.close();
    const resized = await createImageBitmap(bitmap, 0, 0, 2, 1, {resizeHeight:3, resizeQuality:"pixelated"});
    check(resized.width === 6 && resized.height === 3, "aspect ratio");
    await rejects(createImageBitmap(data,0,0,0,1), "RangeError");
    await rejects(createImageBitmap(data,{resizeWidth:0}), "InvalidStateError");
    await rejects(createImageBitmap(data,{resizeWidth:-1}), "TypeError");
    await rejects(createImageBitmap(data,{imageOrientation:"none"}), "TypeError");
    await rejects(createImageBitmap({}), "TypeError");
    await rejects(createImageBitmap(), "TypeError");
    await rejects(createImageBitmap(data, 0, 0), "TypeError");
    const order = [];
    const options = {};
    for (const name of ["colorSpaceConversion","imageOrientation","premultiplyAlpha","resizeHeight","resizeQuality","resizeWidth"])
        Object.defineProperty(options, name, {get(){ order.push(name); return undefined; }});
    const ordered = await createImageBitmap(data, options);
    check(order.join() === "colorSpaceConversion,imageOrientation,premultiplyAlpha,resizeHeight,resizeQuality,resizeWidth", "dictionary order");
    ordered.close();
    const detached = new ImageData(1,1); detached.data.buffer.transfer();
    await rejects(createImageBitmap(detached), "InvalidStateError");
    await rejects(createImageBitmap(new Blob(["invalid image"])), "InvalidStateError");
    // Valid 1x1 PNG, independent of the Blob's declared MIME type.
    const png = Uint8Array.fromBase64("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==");
    const decoded = await createImageBitmap(new Blob([png], {type:"text/plain"}));
    check(decoded.width === 1 && decoded.height === 1, "Blob decoding");
    if (typeof document !== "undefined") {
        const canvas = document.createElement("canvas"); canvas.width = 2; canvas.height = 2;
        const context = canvas.getContext("2d"); context.drawImage(bitmap,0,0);
        check(Array.from(context.getImageData(0,0,1,1).data).join() === "255,0,0,255", "source snapshotted before mutation");
        const crop = await createImageBitmap(bitmap,1,2,-2,-2,{imageOrientation:"flipY"});
        context.clearRect(0,0,2,2); context.drawImage(crop,0,0);
        check(Array.from(context.getImageData(0,0,2,2).data).join() === "0,0,0,0,0,0,255,255,0,0,0,0,255,0,0,255", "crop padding and flip");
        const source = await createImageBitmap(canvas, {get resizeWidth(){context.fillStyle="lime";context.fillRect(0,0,2,2);return 2;}});
        context.clearRect(0,0,2,2); context.drawImage(source,0,0);
        check(Array.from(context.getImageData(0,0,1,1).data).join() === "0,255,0,255", "IDL conversion precedes source snapshot");
        source.close(); crop.close();
        throws(() => context.drawImage(source,0,0), "InvalidStateError");
    }
    bitmap.close(); bitmap.close();
    check(bitmap.width === 0 && bitmap.height === 0, "close");
    await rejects(createImageBitmap(bitmap), "InvalidStateError");
    throws(() => structuredClone(bitmap), "DataCloneError");
    throws(() => structuredClone(null,{transfer:[bitmap]}), "DataCloneError");
    received.close(); resized.close(); decoded.close();
    bitmapResult = "ok";
})().catch(error => { bitmapResult = error.name + ": " + error.message; });
"bitmap-pending";
