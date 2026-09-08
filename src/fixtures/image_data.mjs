// Evaluated as a classic script by the Window/Worker conformance harness.
(function () {
    function check(value, message) { if (!value) throw new Error(message); }
    function throws(name, fn) {
        try { fn(); } catch (e) { check(e.name === name, name + " != " + e.name + ": " + e.message); return; }
        throw new Error("Expected " + name);
    }
    const image = new ImageData(3, 2);
    check(image.width === 3 && image.height === 2, "dimensions");
    check(image.data instanceof Uint8ClampedArray && image.data.length === 24, "pixel buffer");
    check(image.data.every(v => v === 0), "transparent black");
    check(image.data === image.data, "same data object");
    check(image.colorSpace === "srgb" && image.pixelFormat === "rgba-unorm8", "defaults");
    check(Object.prototype.toString.call(image) === "[object ImageData]", "brand tag");
    check(Object.keys(image).length === 0, "no exposed internal state");
    check(ImageData.length === 2, "constructor arity");
    for (const key of ["width", "height", "data", "colorSpace", "pixelFormat"]) {
        const d = Object.getOwnPropertyDescriptor(ImageData.prototype, key);
        check(d.enumerable && d.configurable && d.set === undefined, "attribute descriptor " + key);
        throws("TypeError", () => d.get.call({}));
        throws("TypeError", () => d.get.call(Object.create(ImageData.prototype)));
        throws("TypeError", () => d.get.call(new Proxy(image, {})));
    }
    throws("TypeError", () => { "use strict"; image.width = 8; });
    const pixels = new Uint8ClampedArray(new ArrayBuffer(40), 4, 24);
    pixels[0] = 300; pixels[1] = -10;
    const shared = new ImageData(pixels, 3);
    check(shared.height === 2 && shared.data === pixels && pixels[0] === 255 && pixels[1] === 0, "existing view");
    pixels[3] = 129;
    check(shared.data[3] === 129, "shared view writes");
    Object.defineProperties(pixels, {
        buffer: { get() { throw new Error("must use buffer internal slot"); } },
        byteLength: { get() { throw new Error("must use byteLength internal slot"); } },
        length: { get() { throw new Error("must use length internal slot"); } },
        constructor: { get() { throw new Error("must use typed-array brand"); } },
        [Symbol.toStringTag]: { value: "NotPixels" },
    });
    check(new ImageData(pixels, 3).height === 2, "ignore typed-array expandos");
    const graph = structuredClone({ image: shared, data: pixels, again: shared });
    check(graph.image instanceof ImageData && graph.image !== shared, "clone interface");
    check(graph.image === graph.again && graph.image.data === graph.data, "clone graph identity");
    check(graph.data !== pixels && graph.data[3] === 129 && graph.image.width === 3, "clone pixels");
    graph.data[3] = 8;
    check(shared.data[3] === 129, "clone independent buffer");
    for (const colorSpace of ["srgb", "srgb-linear", "display-p3", "display-p3-linear"]) {
        const half = new ImageData(2, 1, { pixelFormat: "rgba-float16", colorSpace });
        check(half.data instanceof Float16Array && half.data.length === 8, "half-float pixels");
        half.data[0] = 0.5;
        const copy = structuredClone(half);
        check(copy.colorSpace === colorSpace && copy.pixelFormat === "rgba-float16" && copy.data[0] === 0.5, "half-float clone");
    }
    check(new ImageData(new Float16Array(8), 2, undefined, { pixelFormat: "rgba-float16" }).height === 1, "inferred float16 height");
    check(new ImageData(4294967297, 1.9).width === 1, "IDL unsigned conversion");
    check(new ImageData("2", true).height === 1, "IDL coercion");
    class Derived extends ImageData {}
    check(new Derived(1, 1) instanceof Derived, "subclass");
    throws("TypeError", () => ImageData(1, 1));
    throws("TypeError", () => new ImageData());
    throws("TypeError", () => new ImageData(1));
    throws("TypeError", () => new ImageData(1, 1, {}, {}));
    throws("TypeError", () => new ImageData(1n, 1));
    throws("TypeError", () => new ImageData(1, Symbol()));
    throws("IndexSizeError", () => new ImageData(0, 1));
    throws("IndexSizeError", () => new ImageData(1, NaN));
    throws("IndexSizeError", () => new ImageData(Infinity, 1));
    throws("InvalidStateError", () => new ImageData(new Uint8ClampedArray(0), 1));
    throws("InvalidStateError", () => new ImageData(new Uint8ClampedArray(5), 1));
    throws("IndexSizeError", () => new ImageData(new Uint8ClampedArray(8), 3));
    throws("IndexSizeError", () => new ImageData(new Uint8ClampedArray(8), 1, 1));
    throws("InvalidStateError", () => new ImageData(new Float16Array(8), 2));
    throws("InvalidStateError", () => new ImageData(new Uint8ClampedArray(8), 1, 1, { pixelFormat: "rgba-float16" }));
    throws("TypeError", () => new ImageData(1, 1, { colorSpace: "nonsense" }));
    throws("TypeError", () => new ImageData(1, 1, { pixelFormat: Symbol() }));
    throws("TypeError", () => new ImageData(1, 1, 3));
    throws("TypeError", () => new ImageData(new Uint8ClampedArray(new SharedArrayBuffer(4)), 1));
    throws("TypeError", () => new ImageData(new Uint8ClampedArray(new ArrayBuffer(4, { maxByteLength: 8 })), 1));
    const detachedBuffer = new ArrayBuffer(4), detachedPixels = new Uint8ClampedArray(detachedBuffer);
    detachedBuffer.transfer();
    throws("InvalidStateError", () => new ImageData(detachedPixels, 1));
    const lateBuffer = new ArrayBuffer(4), latePixels = new Uint8ClampedArray(lateBuffer);
    throws("InvalidStateError", () => new ImageData(latePixels, { valueOf() { lateBuffer.transfer(); return 1; } }));
    throws("DataCloneError", () => { const x = new ImageData(1, 1); x.data.buffer.transfer(); structuredClone(x); });
    const order = [];
    new ImageData({ valueOf() { order.push("width"); return 1; } }, { valueOf() { order.push("height"); return 1; } }, {
        get colorSpace() { order.push("color"); return { toString() { order.push("color-string"); return "srgb"; } }; },
        get pixelFormat() { order.push("format"); return "rgba-unorm8"; },
    });
    check(order.join(",") === "width,height,color,color-string,format", "conversion order");
    const sentinel = {};
    let conversionThrew = false;
    try { new ImageData(0, 0, { get colorSpace() { throw sentinel; } }); }
    catch (error) { check(error === sentinel, "IDL conversion before dimension validation"); conversionThrew = true; }
    check(conversionThrew, "dictionary conversion must throw");
    const large = new ImageData(1024, 1024);
    large.data[large.data.length - 1] = 231;
    const largeCopy = structuredClone(large);
    check(largeCopy.width === 1024 && largeCopy.height === 1024 && largeCopy.data.length === 4194304
        && largeCopy.data[4194303] === 231 && large.data[4194303] === 231, "multi-megabyte image clone");
    const resizableCopy = structuredClone(new ArrayBuffer(8, { maxByteLength: 16 }));
    check(resizableCopy.resizable && resizableCopy.byteLength === 8 && resizableCopy.maxByteLength === 16,
        "clone buffer resize metadata");
    return "image-data-ok";
})();
