// Original HTML CanvasText / CanvasTextDrawingStyles checks.
(function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    function throws(name, fn) { try { fn(); } catch(e) { check(e.name === name, 'wrong exception '+e.name); return; } throw Error('missing '+name); }
    const canvas = document.createElement('canvas'); canvas.width=240; canvas.height=100;
    const ctx=canvas.getContext('2d');
    check(ctx.font==='10px sans-serif' && ctx.textAlign==='start' && ctx.textBaseline==='alphabetic', 'text defaults');
    check(ctx.direction==='inherit', 'direction default');
    ctx.font='20px monospace';
    const narrow=ctx.measureText('iiii'), wide=ctx.measureText('WWWW');
    check(Math.abs(narrow.width-wide.width)<0.05 && narrow.width>20, 'real monospace advances');
    check(narrow instanceof TextMetrics && Object.prototype.toString.call(narrow)==='[object TextMetrics]', 'metric brand');
    check(narrow!==ctx.measureText('iiii'), 'fresh metric objects');
    ctx.font='20px sans-serif';
    check(ctx.measureText('WWWW').width>ctx.measureText('iiii').width*2, 'proportional glyph metrics');
    check(ctx.measureText('a\tb\nc\rd\fe').width===ctx.measureText('a b c d e').width,'ASCII whitespace preparation');
    const before=ctx.measureText('Hello').width;
    ctx.scale(2,3); check(ctx.measureText('Hello').width===before,'measurement ignores transform'); ctx.resetTransform();
    ctx.font='40px sans-serif'; check(Math.abs(ctx.measureText('Hello').width-before*2)<0.15,'font size changes advances');
    const old=ctx.font;
    for(const value of ['inherit','initial','garbage','12px','12 serif']) { ctx.font=value; check(ctx.font===old,'invalid font retains state'); }
    ctx.textAlign='right'; ctx.textBaseline='top'; ctx.save();
    ctx.font='12px serif'; ctx.textAlign='center'; ctx.textBaseline='bottom'; ctx.restore();
    check(ctx.font===old && ctx.textAlign==='right' && ctx.textBaseline==='top','saved text state');
    ctx.textAlign='bogus'; ctx.textBaseline='bogus'; check(ctx.textAlign==='right' && ctx.textBaseline==='top','invalid enums ignored');
    ctx.reset(); ctx.font='40px sans-serif'; ctx.fillStyle='red';
    ctx.fillText('Hello',10,50);
    let pixels=ctx.getImageData(0,0,240,100).data, ink=0;
    for(let i=0;i<pixels.length;i+=4) if(pixels[i+3]) { ink++; check(pixels[i]>240 && pixels[i+1]===0 && pixels[i+2]===0,'fillText uses fill style'); }
    check(ink>200,'fillText paints glyphs');
    const metrics=ctx.measureText('Hello');
    check(metrics.actualBoundingBoxAscent>20 && metrics.actualBoundingBoxDescent>=0 && metrics.actualBoundingBoxRight>50,'real glyph ink metrics');
    check(metrics.fontBoundingBoxAscent>0 && metrics.emHeightAscent+metrics.emHeightDescent===40,'em and font metrics');
    ctx.reset(); ctx.font='40px sans-serif'; ctx.strokeStyle='blue'; ctx.lineWidth=1;
    ctx.strokeText('Hello',10,50); pixels=ctx.getImageData(0,0,240,100).data; ink=0;
    for(let i=0;i<pixels.length;i+=4) if(pixels[i+3]) { ink++; check(pixels[i]===0 && pixels[i+1]===0 && pixels[i+2]>240,'strokeText uses stroke style'); }
    check(ink>100,'strokeText paints outlines');
    ctx.reset(); ctx.font='40px sans-serif'; ctx.fillText('Hello',10,50,20);
    pixels=ctx.getImageData(0,0,240,100).data; ink=0;
    for(let y=0;y<100;y++) for(let x=0;x<240;x++) if(pixels[(y*240+x)*4+3]) { ink++; check(x<32,'maxWidth constrains text'); }
    check(ink>20,'maxWidth still paints');
    ctx.reset(); ctx.fillText('x',0,20,0); ctx.fillText('x',NaN,20); ctx.fillText('x',0,20,Infinity);
    check(ctx.getImageData(0,0,240,100).data.every(x=>x===0),'invalid finite coordinates/maxWidth no-op');
    ctx.reset(); ctx.beginPath(); ctx.rect(180,0,20,20); ctx.fillText('x',0,50); ctx.clearRect(0,0,240,100); ctx.fillStyle='lime'; ctx.fill();
    check(Array.from(ctx.getImageData(185,5,1,1).data).join(',')==='0,255,0,255','text leaves current path unchanged');
    throws('TypeError',()=>ctx.fillText('x',0)); throws('TypeError',()=>ctx.measureText());
    throws('TypeError',()=>ctx.measureText(Symbol()));
    const widthGet=Object.getOwnPropertyDescriptor(TextMetrics.prototype,'width').get;
    throws('TypeError',()=>new TextMetrics());
    throws('TypeError',()=>widthGet.call({}));
    const immutable=ctx.measureText('Hi'), immutableWidth=immutable.width;
    try { immutable.width=999; } catch(e) { check(e.name==='TypeError','readonly metric error'); }
    check(immutable.width===immutableWidth,'readonly metrics');
    ctx.font='40px serif'; ctx.textAlign='left';
    const left=ctx.measureText('Hi'); ctx.textAlign='center'; const center=ctx.measureText('Hi');
    check(Math.abs(center.actualBoundingBoxLeft-left.actualBoundingBoxLeft-left.width/2)<0.001,'center metric left');
    check(Math.abs(left.actualBoundingBoxRight-center.actualBoundingBoxRight-left.width/2)<0.001,'center metric right');
    ctx.textAlign='start'; ctx.direction='rtl'; const right=ctx.measureText('Hi');
    check(Math.abs(right.actualBoundingBoxLeft-left.actualBoundingBoxLeft-left.width)<0.001,'RTL start alignment');
    ctx.textBaseline='top'; const top=ctx.measureText('Hi');
    check(top.emHeightAscent===0 && top.emHeightDescent===40,'top baseline metrics');
    ctx.textBaseline='bottom'; const bottom=ctx.measureText('Hi');
    check(bottom.emHeightDescent===0 && bottom.emHeightAscent===40,'bottom baseline metrics');
    const empty=ctx.measureText('');
    check(empty.width===0 && empty.actualBoundingBoxLeft===0 && empty.actualBoundingBoxRight===0,'empty ink');
    ctx.reset(); ctx.font='40px sans-serif'; ctx.beginPath(); ctx.rect(15,0,20,100); ctx.clip();
    ctx.globalAlpha=0.5; ctx.fillStyle='red'; ctx.fillText('Hello',0,50);
    pixels=ctx.getImageData(0,0,240,100).data; ink=0;
    for(let y=0;y<100;y++) for(let x=0;x<240;x++) if(pixels[(y*240+x)*4+3]) {
        ink++; check(x>=15 && x<35 && pixels[(y*240+x)*4+3]<=128,'text clip and alpha');
    }
    check(ink>100,'clipped text remains visible');
    ctx.reset(); ctx.fillStyle='blue'; ctx.fillRect(0,0,240,100); ctx.globalCompositeOperation='copy';
    ctx.font='40px sans-serif'; ctx.fillStyle='red'; ctx.fillText('Hi',10,50);
    check(ctx.getImageData(200,80,1,1).data[3]===0,'text copy clears outside source glyphs');
    function bounds() {
        const p=ctx.getImageData(0,0,240,100).data; let lo=240, hi=-1;
        for(let y=0;y<100;y++) for(let x=0;x<240;x++) if(p[(y*240+x)*4+3]>128) { lo=Math.min(lo,x); hi=Math.max(hi,x); }
        return [lo,hi];
    }
    ctx.reset(); ctx.font='60px monospace'; ctx.fillText('M',30,70,6); const filled=bounds();
    ctx.clearRect(0,0,240,100); ctx.lineWidth=6; ctx.strokeText('M',30,70,6); const stroked=bounds();
    check(stroked[0]<=filled[0]-2 && stroked[1]>=filled[1]+2,'maxWidth condenses glyphs, not the stroke');
    // Text and paths must sample the same canvas-space gradient, despite text
    // translation, condensation, rotation, and the glyph renderer's own axes.
    for(let kind=0;kind<3;kind++) {
        ctx.reset(); ctx.font='45px sans-serif';
        const gradient=kind===0 ? ctx.createLinearGradient(0,0,160,0) : kind===1
            ? ctx.createRadialGradient(55,40,0,55,40,85) : ctx.createConicGradient(1.2,55,40);
        gradient.addColorStop(0,'red'); gradient.addColorStop(1,'blue'); ctx.fillStyle=gradient;
        ctx.translate(12,4); ctx.rotate(0.1); ctx.fillText('Hello',5,55,80);
        const textPixels=ctx.getImageData(0,0,240,100).data;
        ctx.clearRect(-100,-100,600,400); ctx.fillRect(-100,-100,600,400);
        const pathPixels=ctx.getImageData(0,0,240,100).data; let compared=0;
        for(let i=0;i<textPixels.length;i+=4) if(textPixels[i+3]===255) {
            compared++;
            check(Math.abs(textPixels[i]-pathPixels[i])<5 && Math.abs(textPixels[i+2]-pathPixels[i+2])<5,'text/path gradient agreement '+kind);
        }
        check(compared>100,'gradient text paints opaque interiors');
    }
    ctx.font='30px serif'; canvas.width=canvas.width; check(ctx.font==='10px sans-serif','resize resets text state');
    const html=document.createElement('html'), body=document.createElement('body');
    document.appendChild(html); html.appendChild(body); body.appendChild(canvas);
    canvas.style.fontSize='20px'; ctx.font='150% sans-serif';
    check(ctx.font==='30px sans-serif','relative font uses rendered canvas environment');
    body.style.display='none'; ctx.font='2em sans-serif';
    check(ctx.font==='20px sans-serif','hidden ancestor uses default font environment');
    canvas.remove(); ctx.font='2em sans-serif';
    check(ctx.font==='20px sans-serif','detached canvas uses default font environment');
    const order=[];
    ctx.fillText({toString(){order.push('text');return 'x';}},
        {valueOf(){order.push('x');return NaN;}},
        {valueOf(){order.push('y');return 20;}},
        {valueOf(){order.push('max');return 10;}});
    check(order.join(',')==='text,x,y,max','convert all arguments before finite-value early return');
    throws('TypeError',()=>ctx.fillText('x',NaN,Symbol()));
    ctx.reset(); ctx.font='30px sans-serif'; ctx.fillText('Hi',0,40,undefined);
    check(bounds()[1]>0,'explicit undefined maxWidth is absent');
    return 'canvas-text-ok';
})();
