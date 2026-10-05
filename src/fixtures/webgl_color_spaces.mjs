// WebGL 1.0 #DOM-WebGLRenderingContext-drawingBufferColorSpace and
// #DOM-WebGLRenderingContext-unpackColorSpace (PredefinedColorSpace).
(() => {
    const check = (value, message) => { if (!value) throw Error(message); };
    const same = (actual, expected, message) => check(JSON.stringify(Array.from(actual)) === JSON.stringify(expected), message + ': ' + Array.from(actual));
    const canvas = document.createElement('canvas'); canvas.width = 2; canvas.height = 2;
    const gl = canvas.getContext('webgl', {preserveDrawingBuffer: true, antialias: false});
    check(gl, 'WebGL context');
    for (const name of ['drawingBufferColorSpace', 'unpackColorSpace']) {
        const d = Object.getOwnPropertyDescriptor(WebGLRenderingContext.prototype, name);
        check(d && d.enumerable && d.configurable && typeof d.get === 'function' && typeof d.set === 'function', 'attribute ' + name);
        check(gl[name] === 'srgb', 'created as srgb: ' + name);
        gl[name] = 'adobe-rgb';
        check(gl[name] === 'srgb', 'an invalid enumeration value is ignored: ' + name);
        let threw = false; try { gl[name] = Symbol(); } catch (e) { threw = e instanceof TypeError; }
        check(threw, 'DOMString conversion: ' + name);
        threw = false; try { d.get.call({}); } catch (e) { threw = e instanceof TypeError; }
        check(threw, 'brand check: ' + name);
    }
    const pixel = new Uint8Array(4);
    gl.clearColor(1, 0, 0, 1); gl.clear(gl.COLOR_BUFFER_BIT);
    gl.drawingBufferColorSpace = 'display-p3';
    check(gl.drawingBufferColorSpace === 'display-p3', 'display-p3 selected');
    gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, pixel);
    same(pixel, [0, 0, 0, 0], 'changing the space reallocates the drawing buffer');
    // sRGB red is color(display-p3 0.9175 0.2003 0.1386) (CSS Color 4).
    gl.clearColor(234 / 255, 51 / 255, 35 / 255, 1); gl.clear(gl.COLOR_BUFFER_BIT);
    gl.drawingBufferColorSpace = 'display-p3';
    gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, pixel);
    same(pixel, [234, 51, 35, 255], 'readPixels returns the buffer values; an unchanged space keeps them');
    const target = document.createElement('canvas'); target.width = 2; target.height = 2;
    const context2d = target.getContext('2d');
    context2d.drawImage(canvas, 0, 0);
    same(context2d.getImageData(0, 0, 1, 1).data, [255, 0, 0, 255], 'a P3 drawing buffer is converted for an sRGB consumer');
    gl.drawingBufferColorSpace = 'srgb';
    gl.clearColor(234 / 255, 51 / 255, 35 / 255, 1); gl.clear(gl.COLOR_BUFFER_BIT);
    context2d.drawImage(canvas, 0, 0);
    same(context2d.getImageData(0, 0, 1, 1).data, [234, 51, 35, 255], 'an sRGB drawing buffer is not converted');
    // unpackColorSpace converts DOM sources on upload unless the colorspace
    // conversion is NONE.
    const source = document.createElement('canvas'); source.width = 1; source.height = 1;
    const sourceContext = source.getContext('2d'); sourceContext.fillStyle = 'rgb(255 0 0)'; sourceContext.fillRect(0, 0, 1, 1);
    const texture = gl.createTexture(); gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST); gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    const framebuffer = gl.createFramebuffer();
    const uploaded = () => {
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, source);
        gl.bindFramebuffer(gl.FRAMEBUFFER, framebuffer);
        gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, texture, 0);
        gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, pixel);
        return Array.from(pixel);
    };
    same(uploaded(), [255, 0, 0, 255], 'sRGB upload');
    gl.unpackColorSpace = 'display-p3';
    same(uploaded(), [234, 51, 35, 255], 'DOM sources are converted into unpackColorSpace');
    gl.pixelStorei(gl.UNPACK_COLORSPACE_CONVERSION_WEBGL, gl.NONE);
    same(uploaded(), [255, 0, 0, 255], 'UNPACK_COLORSPACE_CONVERSION_WEBGL NONE skips the conversion');
    check(gl.getError() === gl.NO_ERROR, 'no GL error');
    return 'webgl-color-spaces-ok';
})();
