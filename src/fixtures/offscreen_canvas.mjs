// HTML #the-offscreencanvas-interface, #the-offscreen-2d-rendering-context and
// #the-imagebitmaprenderingcontext-interface (local WHATWG e5071a20); shared
// Window and dedicated worker cases.
globalThis.offscreenResult = "pending";
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
    const worker = typeof WorkerGlobalScope === "function";
    // Interface objects ([Exposed=(Window,Worker)]).
    check(typeof OffscreenCanvas === "function" && OffscreenCanvas.length === 2, "constructor");
    for (const name of ["OffscreenCanvasRenderingContext2D", "ImageBitmapRenderingContext", "Path2D",
        "CanvasGradient", "CanvasPattern", "TextMetrics"])
        check(typeof globalThis[name] === "function", "exposed " + name);
    check(Object.getPrototypeOf(OffscreenCanvas.prototype) === EventTarget.prototype, "EventTarget");
    check(worker === (typeof CanvasRenderingContext2D === "undefined"), "Window-only CanvasRenderingContext2D");
    throws(() => new OffscreenCanvasRenderingContext2D(), "TypeError");
    throws(() => new ImageBitmapRenderingContext(), "TypeError");
    throws(() => OffscreenCanvas(1, 1), "TypeError");
    throws(() => new OffscreenCanvas(1), "TypeError");
    // [EnforceRange] unsigned long long.
    for (const bad of [-1, NaN, Infinity, 2 ** 53])
        throws(() => new OffscreenCanvas(bad, 1), "TypeError");
    const canvas = new OffscreenCanvas(4.9, 3);
    check(canvas.width === 4 && canvas.height === 3, "EnforceRange truncation");
    check(Object.prototype.toString.call(canvas) === "[object OffscreenCanvas]", "brand");
    check(Object.keys(canvas).length === 0, "internal slots only");
    const widthGet = Object.getOwnPropertyDescriptor(OffscreenCanvas.prototype, "width").get;
    throws(() => widthGet.call({}), "TypeError");
    // getContext(): an OffscreenRenderingContextId and the context mode table.
    throws(() => canvas.getContext("experimental-webgl"), "TypeError");
    throws(() => canvas.getContext(), "TypeError");
    check(canvas.getContext("webgl2") === null && canvas.getContext("webgpu") === null, "unprovided contexts");
    throws(() => canvas.getContext("2d", {colorType: "bogus"}), "TypeError");
    const ctx = canvas.getContext("2d", {willReadFrequently: true});
    check(ctx instanceof OffscreenCanvasRenderingContext2D && ctx.canvas === canvas, "2d context");
    check(canvas.getContext("2d") === ctx && canvas.getContext("bitmaprenderer") === null
        && canvas.getContext("webgl") === null, "context mode");
    check(ctx.getContextAttributes().willReadFrequently === true && ctx.getContextAttributes().alpha === true, "settings");
    check(!("drawFocusIfNeeded" in ctx) && !("scrollPathIntoView" in ctx), "no user interface features");
    check(Object.prototype.toString.call(ctx) === "[object OffscreenCanvasRenderingContext2D]", "context brand");
    if (!worker) throws(() => CanvasRenderingContext2D.prototype.fillRect.call(ctx, 0, 0, 1, 1), "TypeError");
    throws(() => OffscreenCanvasRenderingContext2D.prototype.fillRect.call({}, 0, 0, 1, 1), "TypeError");
    throws(() => OffscreenCanvasRenderingContext2D.prototype.moveTo.call(new Path2D(), 0, 0), "TypeError");
    const proto = OffscreenCanvasRenderingContext2D.prototype;
    check(proto.fillRect.length === 4 && proto.drawImage.length === 3 && proto.arc.length === 5
        && proto.setTransform.length === 0 && Object.getOwnPropertyDescriptor(proto, "fillRect").enumerable, "Web IDL operations");
    check(Object.getOwnPropertyDescriptor(proto, "filter").get.name === "get filter", "Web IDL attributes");
    check(proto.moveTo !== Path2D.prototype.moveTo, "one function object per interface");
    // Drawing and pixel readback.
    ctx.fillStyle = "rgb(255, 0, 0)"; ctx.fillRect(0, 0, 2, 3);
    let pixels = ctx.getImageData(0, 0, 4, 3).data;
    check(pixels[0] === 255 && pixels[3] === 255 && pixels[11] === 0, "fillRect pixels");
    const path = new Path2D(); path.rect(2, 0, 1, 1); ctx.fill(path);
    check(ctx.getImageData(2, 0, 1, 1).data[3] === 255 && ctx.isPointInPath(path, 2.5, 0.5), "Path2D");
    check(ctx.getTransform().is2D && ctx.measureText("Hi").width > 0, "transform and text");
    // HTML OffscreenCanvas width/height setters reset a 2D context.
    ctx.fillStyle = "blue"; canvas.width = 5;
    check(canvas.width === 5 && canvas.height === 3 && ctx.fillStyle === "#000000", "resize resets state");
    check(ctx.getImageData(0, 0, 1, 1).data[3] === 0, "resize clears bitmap");
    throws(() => { canvas.height = -2; }, "TypeError");
    // transferToImageBitmap: the bitmap moves; the context keeps its state.
    ctx.fillStyle = "lime"; ctx.fillRect(0, 0, 5, 3);
    const bitmap = canvas.transferToImageBitmap();
    check(bitmap instanceof ImageBitmap && bitmap.width === 5 && bitmap.height === 3, "ImageBitmap dimensions");
    check(ctx.getImageData(0, 0, 1, 1).data[3] === 0 && ctx.fillStyle === "#00ff00", "new blank bitmap, same state");
    throws(() => new OffscreenCanvas(1, 1).transferToImageBitmap(), "InvalidStateError");
    const opaque = new OffscreenCanvas(2, 2), opaqueContext = opaque.getContext("2d", {alpha: false});
    opaqueContext.fillStyle = "red"; opaqueContext.fillRect(0, 0, 2, 2); opaque.transferToImageBitmap();
    check(opaqueContext.getImageData(0, 0, 1, 1).data.join() === "0,0,0,255", "opaque black after transfer");
    // CanvasImageSource: ImageBitmap and OffscreenCanvas.
    const target = new OffscreenCanvas(5, 3), tctx = target.getContext("2d");
    tctx.drawImage(bitmap, 0, 0);
    check(tctx.getImageData(1, 1, 1, 1).data.join() === "0,255,0,255", "drawImage(ImageBitmap)");
    const source = new OffscreenCanvas(2, 2), sctx = source.getContext("2d");
    sctx.fillStyle = "blue"; sctx.fillRect(0, 0, 2, 2);
    tctx.drawImage(source, 3, 1);
    check(tctx.getImageData(4, 2, 1, 1).data.join() === "0,0,255,255", "drawImage(OffscreenCanvas)");
    throws(() => tctx.drawImage(new OffscreenCanvas(0, 1), 0, 0), "InvalidStateError");
    tctx.drawImage(new OffscreenCanvas(0, 1), NaN, 0);
    check(tctx.createPattern(source, "") instanceof CanvasPattern, "createPattern(OffscreenCanvas)");
    tctx.drawImage(target, 0, 0);
    check(tctx.getImageData(4, 2, 1, 1).data.join() === "0,0,255,255", "drawImage(self)");
    const fromCanvas = await createImageBitmap(source);
    check(fromCanvas.width === 2 && fromCanvas.height === 2, "createImageBitmap(OffscreenCanvas)");
    const back = new OffscreenCanvas(2, 2).getContext("2d"); back.drawImage(fromCanvas, 0, 0);
    check(back.getImageData(0, 0, 1, 1).data.join() === "0,0,255,255", "OffscreenCanvas bitmap pixels");
    // convertToBlob(): a promise resolved in a later task.
    let settled = false;
    const pending = target.convertToBlob();
    pending.then(() => { settled = true; });
    await Promise.resolve();
    check(!settled, "convertToBlob resolves in a task");
    const blob = await pending;
    check(blob instanceof Blob && blob.type === "image/png" && blob.size > 0, "PNG blob");
    check((await target.convertToBlob({type: "image/jpeg", quality: 0.5})).type === "image/jpeg", "JPEG blob");
    check((await target.convertToBlob({type: "image/bogus"})).type === "image/png", "unsupported types are PNG");
    await rejects(new OffscreenCanvas(0, 2).convertToBlob(), "IndexSizeError");
    await rejects(target.convertToBlob(7), "TypeError");
    await rejects(OffscreenCanvas.prototype.convertToBlob.call({}), "TypeError");
    // bitmaprenderer: transferFromImageBitmap moves the bitmap data.
    const shown = new OffscreenCanvas(1, 1), renderer = shown.getContext("bitmaprenderer");
    check(renderer instanceof ImageBitmapRenderingContext && renderer.canvas === shown, "bitmaprenderer");
    check(shown.getContext("bitmaprenderer") === renderer && shown.getContext("2d") === null, "bitmaprenderer mode");
    const frame = target.transferToImageBitmap();
    renderer.transferFromImageBitmap(frame);
    check(frame.width === 0 && shown.width === 5 && shown.height === 3, "transferFromImageBitmap detaches and sizes");
    const moved = shown.transferToImageBitmap();
    const check2d = new OffscreenCanvas(5, 3).getContext("2d"); check2d.drawImage(moved, 0, 0);
    check(check2d.getImageData(4, 2, 1, 1).data.join() === "0,0,255,255", "bitmaprenderer bitmap");
    throws(() => renderer.transferFromImageBitmap(frame), "InvalidStateError");
    throws(() => renderer.transferFromImageBitmap({}), "TypeError");
    renderer.transferFromImageBitmap(null);
    // Transfer (HTML OffscreenCanvas transfer steps).
    const moving = new OffscreenCanvas(7, 8);
    throws(() => structuredClone(moving), "DataCloneError");
    const received = structuredClone({a: moving, b: moving}, {transfer: [moving]});
    check(received.a instanceof OffscreenCanvas && received.a === received.b
        && received.a.width === 7 && received.a.height === 8, "transfer receiving");
    check(moving.width === 0 && moving.height === 0, "transfer detaches");
    throws(() => moving.getContext("2d"), "InvalidStateError");
    throws(() => moving.transferToImageBitmap(), "InvalidStateError");
    await rejects(moving.convertToBlob(), "InvalidStateError");
    throws(() => structuredClone(moving, {transfer: [moving]}), "DataCloneError");
    throws(() => structuredClone(canvas, {transfer: [canvas]}), "InvalidStateError");
    check(canvas.width === 5 && canvas.getContext("2d") === ctx, "a failed transfer leaves the source");
    const channel = new MessageChannel();
    const delivery = new Promise(resolve => { channel.port2.onmessage = event => resolve(event.data); });
    channel.port1.postMessage({canvas: received.a}, [received.a]);
    check(received.a.width === 0, "port transfer detaches synchronously");
    const ported = (await delivery).canvas;
    check(ported instanceof OffscreenCanvas && ported.width === 7, "port transfer");
    const portedContext = ported.getContext("2d");
    portedContext.fillRect(0, 0, 7, 8);
    check(portedContext.getImageData(6, 7, 1, 1).data[3] === 255, "received canvas draws");
    channel.port1.close(); channel.port2.close();
    // HTML #event-handlers: oncontextlost and oncontextrestored.
    check(ported.oncontextlost === null && ported.oncontextrestored === null, "handlers default");
    const handler = () => {};
    ported.oncontextlost = handler;
    check(ported.oncontextlost === handler, "handler set");
    ported.oncontextlost = 5;
    check(ported.oncontextlost === null, "non-object handler");
    offscreenResult = "ok";
})().catch(error => { offscreenResult = String(error && error.stack || error); });
"offscreen-pending";
