// Native path and decoded-image pixel regression fixture.
(function () {
    function check(value, message) { if (!value) throw Error(message); }
    function pixel(ctx,x,y) { return Array.from(ctx.getImageData(x,y,1,1).data).join(','); }
    const c = document.createElement('canvas'); c.width=32; c.height=32;
    const ctx = c.getContext('2d');
    ctx.fillStyle='red'; ctx.beginPath(); ctx.roundRect(0,0,32,32,12); ctx.fill();
    check(pixel(ctx,16,16)==='255,0,0,255' && pixel(ctx,0,0)==='0,0,0,0','rounded rectangle interior and exterior');
    c.width=8;c.height=8; ctx.beginPath(); ctx.moveTo(0,0); ctx.arcTo(8,0,8,8,4); ctx.lineTo(0,8); ctx.closePath(); ctx.fill();
    check(pixel(ctx,2,2)==='0,0,0,255' && pixel(ctx,7,0)==='0,0,0,0','tangent arc geometry');
    ctx.reset(); ctx.lineWidth=2; ctx.setLineDash([2,2]); ctx.beginPath(); ctx.moveTo(0,4); ctx.lineTo(8,4); ctx.stroke();
    check(pixel(ctx,1,4)==='0,0,0,255' && pixel(ctx,3,4)==='0,0,0,0','native dash gaps');
    ctx.save(); ctx.setLineDash([1]); ctx.lineDashOffset=0.125; ctx.restore();
    check(ctx.getLineDash().join(',')==='2,2' && ctx.lineDashOffset===0,'saved dash state');
    ctx.reset(); ctx.fillStyle='red'; ctx.fillRect(0,0,4,8); ctx.fillStyle='lime'; ctx.fillRect(4,0,4,8);
    const img = new Image(); img.src=c.toDataURL(); img.width=123; img.height=456;
    ctx.clearRect(0,0,8,8); ctx.drawImage(img,0,0);
    check(pixel(ctx,1,1)==='255,0,0,255' && pixel(ctx,6,1)==='0,255,0,255','decoded image uses natural pixels not element dimensions');
    return 'canvas-paths-images-ok';
})();
