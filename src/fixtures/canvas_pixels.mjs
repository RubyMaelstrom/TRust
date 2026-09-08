// Native Canvas binding conformance fixture, evaluated in each execution tier.
(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function pixel(ctx,x,y) { return Array.from(ctx.getImageData(x,y,1,1).data).join(','); }
    function throws(name, fn) { try { fn(); } catch(e) { check(e.name === name,'wrong error '+e); return; } throw Error('missing '+name); }
    const canvas = document.createElement('canvas');
    check(canvas.width === 300 && canvas.height === 150,'default dimensions');
    canvas.width = 8; canvas.height = 8;
    const ctx = canvas.getContext('2d');
    check(ctx instanceof CanvasRenderingContext2D && ctx.canvas === canvas,'context identity');
    check(ctx === canvas.getContext('2d',{get alpha(){throw Error('second settings read');}}),'same context');
    check(canvas.getContext('webgl2') === null,'no fabricated WebGL');
    check(pixel(ctx,0,0) === '0,0,0,0','transparent black');
    ctx.fillStyle = 'red'; check(ctx.fillStyle === '#ff0000','serialized color');
    ctx.fillStyle = 'not-a-color'; check(ctx.fillStyle === '#ff0000','invalid color ignored');
    ctx.fillRect(0,0,4,4); check(pixel(ctx,2,2) === '255,0,0,255','native rectangle');
    ctx.save(); ctx.translate(4,0); ctx.fillStyle = '#00ff00'; ctx.fillRect(0,0,4,4); ctx.restore();
    check(ctx.fillStyle === '#ff0000' && pixel(ctx,6,2) === '0,255,0,255','saved transform and style');
    const data = new ImageData(2,2); data.data.set([0,0,255,255,255,255,0,255,0,255,255,255,255,0,255,255]);
    ctx.globalAlpha = 0; ctx.translate(100,100); ctx.beginPath(); ctx.rect(0,0,1,1); ctx.clip();
    ctx.putImageData(data,1,1,2,2,-1,-1);
    check(pixel(ctx,2,2) === '255,0,255,255','put ignores transform alpha clip; negative dirty rectangle');
    check(pixel(ctx,1,1) === '255,0,0,255','dirty rectangle excludes other source pixels');
    check(pixel(ctx,-1,-1) === '0,0,0,0','out of bounds read');
    check(ctx.getImageData(3,3,-1,-1).data[0] === 255,'negative read dimensions');
    throws('TypeError',()=>ctx.putImageData({width:1,height:1,data:new Uint8ClampedArray(4)},0,0));
    const detached = new ImageData(1,1); detached.data.buffer.transfer();
    throws('InvalidStateError',()=>ctx.putImageData(detached,0,0));
    throws('IndexSizeError',()=>ctx.getImageData(0,0,0,1));
    throws('TypeError',()=>ctx.getImageData(0n,0,1,1));
    const fresh = ctx.createImageData(data);
    check(fresh.width === 2 && fresh.height === 2 && fresh.data.every(v=>v===0),'create does not copy pixels');
    canvas.setAttribute('width','8');
    check(ctx.globalAlpha === 1 && ctx.fillStyle === '#000000' && pixel(ctx,2,2) === '0,0,0,0','idempotent width resets all state');
    ctx.restore(); check(ctx.fillStyle === '#000000','resize empties saved stack');
    ctx.fillStyle = '#ff0000'; ctx.beginPath(); ctx.moveTo(0,0); ctx.lineTo(8,0); ctx.lineTo(0,8); ctx.closePath(); ctx.fill();
    check(pixel(ctx,1,1) === '255,0,0,255' && pixel(ctx,7,7) === '0,0,0,0','raster path');
    ctx.beginPath(); ctx.rect(0,0,2,2); ctx.clip(); ctx.clearRect(0,0,8,8);
    check(pixel(ctx,0,0) === '0,0,0,0' && pixel(ctx,3,1) === '255,0,0,255','clear respects clip: '+pixel(ctx,0,0)+' / '+pixel(ctx,3,1));
    ctx.reset(); ctx.putImageData(data,0,0); ctx.imageSmoothingEnabled = false;
    ctx.drawImage(canvas,0,0,2,2,1,0,2,2);
    check(pixel(ctx,1,0) === '0,0,255,255' && pixel(ctx,2,0) === '255,255,0,255','overlapping self draw snapshots source');
    ctx.reset(); ctx.fillStyle = '#ff0000';
    const path = new Path2D('M0 0h2v2h-2Z');
    const copy = new Path2D(path); path.rect(4,4,4,4);
    ctx.fill(copy); check(pixel(ctx,1,1) === '255,0,0,255' && pixel(ctx,6,6) === '0,0,0,0','Path2D copy independent');
    const moved = new Path2D(); moved.addPath(copy,{e:4});
    ctx.fill(moved); check(pixel(ctx,5,1) === '255,0,0,255','Path2D matrix');
    ctx.fill(new Path2D('M0 4 L4 4 L0 8 L')); check(pixel(ctx,0,5) === '255,0,0,255','SVG path valid prefix');
    ctx.reset(); ctx.fillStyle='red'; ctx.fillRect(0,0,8,8); ctx.beginPath(); ctx.rect(0,0,4,4); ctx.clip();
    ctx.globalCompositeOperation='copy'; ctx.fillStyle='blue'; ctx.fillRect(0,0,2,2);
    check(pixel(ctx,1,1) === '0,0,255,255' && pixel(ctx,3,3) === '0,0,0,0' && pixel(ctx,6,6) === '255,0,0,255','copy whole source respects clip');
    ctx.reset();
    for (const colorSpace of ['srgb','srgb-linear','display-p3','display-p3-linear']) {
        const half = new ImageData(1,1,{pixelFormat:'rgba-float16',colorSpace}); half.data.set([1,1,1,1]);
        ctx.putImageData(half,0,0);
        const read = ctx.getImageData(0,0,1,1,{pixelFormat:'rgba-float16',colorSpace});
        check(read.data instanceof Float16Array && Math.abs(read.data[0]-1)<0.01 && read.data[3]===1,'float16 color conversion');
    }
    const opaque = document.createElement('canvas'); opaque.width=2; opaque.height=2;
    const opaqueCtx = opaque.getContext('2d',{alpha:false});
    check(pixel(opaqueCtx,0,0) === '0,0,0,255','opaque initial bitmap');
    opaqueCtx.fillStyle='rgba(255,255,255,0.5)'; opaqueCtx.fillRect(0,0,2,2);
    check(opaqueCtx.getImageData(0,0,1,1).data[0] >= 127,'opaque canvas still blends source alpha');
    opaqueCtx.clearRect(0,0,2,2); check(pixel(opaqueCtx,0,0) === '0,0,0,255','opaque clear');
    check(canvas.toDataURL().startsWith('data:image/png;base64,iVBOR'),'real PNG');
    canvas.width=0; check(canvas.toDataURL() === 'data:,','empty bitmap export');
    throws('InvalidStateError',()=>ctx.drawImage(canvas,0,0));
    throws('TypeError',()=>CanvasRenderingContext2D.prototype.fillRect.call({},0,0,1,1));
    check(typeof __canvas_2d === 'undefined','private host hook removed');
    return 'canvas-pixels-ok';
})();
