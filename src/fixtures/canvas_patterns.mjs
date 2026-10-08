// HTML canvas: #dom-context-2d-createpattern and pattern rendering,
// #dom-canvaspattern-settransform, #is-point-in-path / #is-point-in-stroke,
// #draw-focus-if-needed, #dom-context-2d-imagesmoothingquality and
// #dom-canvas-todataurl (local whatwg/html@e5071a2).
(function () {
    const check = (ok, text) => { if (!ok) throw Error(text); };
    const throws = (name, fn) => {
        let error; try { fn(); } catch (e) { error = e; }
        check(error && error.name === name, 'expected ' + name + ', got ' + (error && error.name));
    };
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    // A 2x2 source: red, green / blue, transparent.
    const source = document.createElement('canvas'); source.width = 2; source.height = 2;
    const sourceContext = source.getContext('2d');
    const image = sourceContext.createImageData(2, 2);
    image.data.set([255,0,0,255, 0,255,0,255, 0,0,255,255, 0,0,0,0]);
    sourceContext.putImageData(image, 0, 0);
    const canvas = document.createElement('canvas'); canvas.width = 8; canvas.height = 8; body.appendChild(canvas);
    const ctx = canvas.getContext('2d');
    ctx.imageSmoothingEnabled = false;
    const px = (x, y) => Array.from(ctx.getImageData(x, y, 1, 1).data).join(',');
    throws('TypeError', () => ctx.createPattern(source));
    throws('SyntaxError', () => ctx.createPattern(source, 'repeat-z'));
    throws('TypeError', () => ctx.createPattern({}, 'repeat'));
    const empty = document.createElement('canvas'); empty.width = 0;
    throws('InvalidStateError', () => ctx.createPattern(empty, 'repeat'));
    const pattern = ctx.createPattern(source, null);
    check(pattern instanceof CanvasPattern && Reflect.ownKeys(pattern).length === 0, 'CanvasPattern');
    throws('TypeError', () => new CanvasPattern());
    // The pattern is a snapshot of its image.
    sourceContext.fillStyle = 'white'; sourceContext.fillRect(0, 0, 2, 2);
    ctx.fillStyle = pattern;
    check(ctx.fillStyle === pattern, 'fillStyle returns the pattern');
    ctx.fillRect(0, 0, 8, 8);
    check(px(0,0) === '255,0,0,255' && px(1,0) === '0,255,0,255' && px(0,1) === '0,0,255,255' && px(1,1) === '0,0,0,0', 'tile ' + px(0,0));
    check(px(2,0) === '255,0,0,255' && px(5,7) === '0,0,0,0' && px(4,3) === '0,0,255,255', 'repeat');
    ctx.fillStyle = 'black';
    check(ctx.fillStyle === '#000000', 'a color replaces the pattern');
    sourceContext.putImageData(image, 0, 0);
    // no-repeat: only the copy at the origin.
    ctx.clearRect(0, 0, 8, 8);
    ctx.fillStyle = ctx.createPattern(source, 'no-repeat'); ctx.fillRect(0, 0, 8, 8);
    check(px(0,0) === '255,0,0,255' && px(2,0) === '0,0,0,0' && px(0,2) === '0,0,0,0', 'no-repeat ' + px(2,0));
    // repeat-x under a pattern transform: one row of copies, moved down by 2.
    ctx.clearRect(0, 0, 8, 8);
    const strip = ctx.createPattern(source, 'repeat-x');
    strip.setTransform({f: 2});
    strip.setTransform({f: NaN});
    ctx.fillStyle = strip; ctx.fillRect(0, 0, 8, 8);
    check(px(0,0) === '0,0,0,0' && px(0,2) === '255,0,0,255' && px(4,3) === '0,0,255,255' && px(4,4) === '0,0,0,0',
        'repeat-x ' + [px(0,2), px(4,3), px(4,4)]);
    throws('TypeError', () => strip.setTransform({a: 1, m11: 2}));
    // Hit testing uses canvas coordinates, unaffected by the transformation.
    ctx.beginPath(); ctx.rect(1, 1, 4, 4);
    check(ctx.isPointInPath(2, 2) && ctx.isPointInPath(1, 1) && !ctx.isPointInPath(6, 6) && !ctx.isPointInPath(NaN, 2), 'isPointInPath');
    ctx.translate(10, 0);
    check(ctx.isPointInPath(2, 2), 'the default path ignores later transforms');
    const square = new Path2D(); square.rect(0, 0, 2, 2);
    check(ctx.isPointInPath(square, 11, 1) && !ctx.isPointInPath(square, 1, 1), 'a Path2D is transformed');
    ctx.resetTransform();
    ctx.lineWidth = 2;
    check(ctx.isPointInStroke(1, 3) && ctx.isPointInStroke(0.2, 3) && !ctx.isPointInStroke(3, 3), 'isPointInStroke');
    const open = new Path2D(); open.moveTo(0, 0); open.lineTo(4, 0); open.lineTo(4, 4); open.lineTo(0, 4);
    check(ctx.isPointInPath(open, 2, 2) && ctx.isPointInPath(open, 2, 2, 'evenodd'), 'open subpaths are closed');
    throws('TypeError', () => ctx.isPointInPath(1));
    throws('TypeError', () => ctx.isPointInPath(1, 1, 'odd'));
    throws('TypeError', () => ctx.isPointInStroke({}, 1, 1));
    check(CanvasRenderingContext2D.prototype.isPointInPath.length === 2, 'operation length');
    // ImageSmoothingQuality is an enumeration.
    check(ctx.imageSmoothingQuality === 'low', 'quality default');
    ctx.imageSmoothingQuality = 'high'; ctx.imageSmoothingQuality = 'best';
    check(ctx.imageSmoothingQuality === 'high', 'quality enum');
    // Serialization.
    check(canvas.toDataURL().startsWith('data:image/png;base64,') &&
        canvas.toDataURL('image/jpeg', 0.5).startsWith('data:image/jpeg;base64,') &&
        canvas.toDataURL('IMAGE/WEBP').startsWith('data:image/webp;base64,') &&
        canvas.toDataURL('image/bmp').startsWith('data:image/png;base64,'), 'toDataURL types');
    const zero = document.createElement('canvas'); zero.width = 0;
    check(zero.toDataURL() === 'data:,', 'a bitmap without pixels');
    throws('TypeError', () => canvas.toBlob());
    // drawFocusIfNeeded draws only for a focused descendant of the canvas.
    ctx.clearRect(0, 0, 8, 8);
    const button = document.createElement('button'); canvas.appendChild(button);
    ctx.beginPath(); ctx.rect(2, 2, 4, 4);
    ctx.drawFocusIfNeeded(button);
    check(!ctx.getImageData(0, 0, 8, 8).data.some(v => v), 'an unfocused element draws nothing');
    throws('TypeError', () => ctx.drawFocusIfNeeded({}));
    button.focus();
    if (document.activeElement === button) {
        ctx.drawFocusIfNeeded(button);
        check(ctx.getImageData(0, 0, 8, 8).data.some(v => v), 'a focus ring');
    }
    // Text paints with a pattern too.
    const solid = document.createElement('canvas'); solid.width = solid.height = 1;
    const solidContext = solid.getContext('2d');
    solidContext.fillStyle = 'rgb(0,128,0)'; solidContext.fillRect(0, 0, 1, 1);
    const textCanvas = document.createElement('canvas'); textCanvas.width = 40; textCanvas.height = 24;
    const text = textCanvas.getContext('2d');
    text.fillStyle = text.createPattern(solid, 'repeat'); text.font = '20px sans-serif'; text.fillText('W', 2, 20);
    const ink = text.getImageData(0, 0, 40, 24).data;
    let green = 0, other = 0;
    for (let i = 0; i < ink.length; i += 4) if (ink[i + 3] === 255) { if (ink[i] === 0 && ink[i + 1] === 128 && ink[i + 2] === 0) green++; else other++; }
    check(green > 0 && other === 0, 'pattern text ' + green + '/' + other);
    globalThis.focusedForRing = document.activeElement === button;
    return 'canvas-patterns-ok';
})();
