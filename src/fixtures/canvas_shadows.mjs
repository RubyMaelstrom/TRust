// Original HTML CanvasShadowStyles and drawing-model regression tests.
(function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    function throws(name, fn) { try { fn(); } catch(e) { check(e.name === name, 'wrong error '+e.name); return; } throw Error('missing '+name); }
    function make(width=64,height=64) { const c=document.createElement('canvas');c.width=width;c.height=height;return c.getContext('2d'); }
    function pixel(c,x,y) { return Array.from(c.getImageData(x,y,1,1).data); }
    function close(actual, expected, label) { check(actual.every((v,i)=>Math.abs(v-expected[i])<=2),label+': '+actual); }
    function setup(c, x=16, y=0, blur=0) { c.fillStyle='red';c.shadowColor='blue';c.shadowOffsetX=x;c.shadowOffsetY=y;c.shadowBlur=blur; }
    const c=make();
    check(c.shadowColor==='rgba(0, 0, 0, 0)' && c.shadowBlur===0 && c.shadowOffsetX===0 && c.shadowOffsetY===0,'initial shadow state');
    const proto=CanvasRenderingContext2D.prototype;
    for(const name of ['shadowColor','shadowBlur','shadowOffsetX','shadowOffsetY']) {
        const d=Object.getOwnPropertyDescriptor(proto,name);
        check(d.enumerable && d.configurable && typeof d.get==='function' && typeof d.set==='function','shadow descriptor '+name);
        throws('TypeError',()=>d.get.call({}));
        let converted=false;
        throws('TypeError',()=>d.set.call({}, {valueOf(){converted=true;return 1;},toString(){converted=true;return 'red';}}));
        check(!converted,'receiver check before conversion');
    }
    setup(c,16,4,2);c.save();setup(c,2,3,8);c.restore();
    check(c.shadowColor==='#0000ff' && c.shadowOffsetX===16 && c.shadowOffsetY===4 && c.shadowBlur===2,'save/restore shadow state');
    c.shadowColor='not-a-color';c.shadowBlur=-1;c.shadowOffsetX=Infinity;c.shadowOffsetY=NaN;
    check(c.shadowColor==='#0000ff' && c.shadowBlur===2 && c.shadowOffsetX===16 && c.shadowOffsetY===4,'invalid values ignored');
    throws('TypeError',()=>{c.shadowColor=Symbol();});throws('TypeError',()=>{c.shadowBlur=1n;});
    let converted=0;c.shadowBlur={valueOf(){converted++;return 4;}};check(converted===1 && c.shadowBlur===4,'one numeric conversion');
    c.canvas.width=c.canvas.width;
    check(c.shadowColor==='rgba(0, 0, 0, 0)' && c.shadowBlur===0 && c.shadowOffsetX===0 && c.shadowOffsetY===0,'bitmap resize resets shadows');
    c.restore();check(c.shadowColor==='rgba(0, 0, 0, 0)','resize resets saved shadow stack');

    setup(c);c.fillRect(4,4,8,8);
    close(pixel(c,6,6),[255,0,0,255],'source painted');close(pixel(c,22,6),[0,0,255,255],'offset shadow');close(pixel(c,16,6),[0,0,0,0],'gap transparent');
    c.reset();setup(c);c.fillRect(-12,4,8,8);
    close(pixel(c,6,6),[0,0,255,255],'off-canvas source still casts shadow');
    c.reset();setup(c);c.beginPath();c.rect(20,0,8,16);c.clip();c.fillRect(4,4,8,8);
    close(pixel(c,22,6),[0,0,255,255],'source clipping happens after shadow generation');close(pixel(c,6,6),[0,0,0,0],'source clipped');
    c.reset();setup(c,8);c.scale(2,2);c.fillRect(1,1,4,4);
    close(pixel(c,17,4),[0,0,255,255],'shadow offset not scaled');close(pixel(c,24,4),[0,0,0,0],'no double scaled shadow');
    c.reset();setup(c,12);c.beginPath();c.rect(-10,4,5,5);c.scale(2,2);c.fill();
    close(pixel(c,4,6),[0,0,255,255],'old default path stays in device coordinates');
    c.reset();setup(c);c.globalAlpha=0.5;c.shadowColor='rgba(0,0,255,0.5)';c.fillRect(4,4,8,8);
    close(pixel(c,22,6),[0,0,255,64],'shadow alpha includes source/global/shadow alpha once');
    c.reset();setup(c,8);c.globalCompositeOperation='xor';c.fillRect(8,8,12,12);
    close(pixel(c,10,10),[255,0,0,255],'xor source');close(pixel(c,18,10),[0,0,0,0],'shadow then source separately composited');close(pixel(c,24,10),[0,0,255,255],'xor shadow');
    c.reset();setup(c);c.globalCompositeOperation='copy';c.fillRect(4,4,8,8);
    close(pixel(c,22,6),[0,0,0,0],'copy source replaces shadow');
    c.reset();setup(c);c.strokeStyle='red';c.lineWidth=4;c.beginPath();c.moveTo(4,8);c.lineTo(12,8);c.stroke();
    close(pixel(c,22,8),[0,0,255,255],'stroked path shadow');
    c.reset();setup(c);const gradient=c.createLinearGradient(0,0,16,0);gradient.addColorStop(0,'rgba(255,0,0,0)');gradient.addColorStop(1,'red');c.fillStyle=gradient;c.fillRect(0,0,16,8);
    check(pixel(c,18,4)[3]<pixel(c,28,4)[3] && pixel(c,18,4)[2]===255,'gradient alpha defines shadow');
    c.reset();setup(c);c.clearRect(0,0,64,64);close(pixel(c,20,4),[0,0,0,0],'clearRect does not cast shadow');
    const data=new ImageData(new Uint8ClampedArray([255,0,0,255]),1);c.putImageData(data,2,2);
    close(pixel(c,18,2),[0,0,0,0],'putImageData does not cast shadow');
    const image=make(2,2);image.fillStyle='red';image.fillRect(0,0,1,2);
    c.reset();setup(c);c.imageSmoothingEnabled=false;c.drawImage(image.canvas,2,2,8,8);
    close(pixel(c,20,4),[0,0,255,255],'image alpha shadow');close(pixel(c,24,4),[0,0,0,0],'transparent image pixels do not cast shadow');

    c.reset();setup(c,0,0,4);c.fillRect(32,32,1,1);
    close(pixel(c,34,32),[0,0,255,6],'Gaussian impulse at sigma=blur/2');
    check(pixel(c,32,34)[3]===pixel(c,34,32)[3],'Gaussian isotropic');
    close(pixel(c,45,45),[0,0,0,0],'Gaussian tail vanishes');
    const a=make(), b=make();setup(a,0,0,6);setup(b,0,0,6);a.scale(2,2);a.fillRect(10,10,4,4);b.fillRect(20,20,8,8);
    check(Array.from(a.getImageData(0,0,64,64).data).join(',')===Array.from(b.getImageData(0,0,64,64).data).join(','),'blur is independent of CTM');
    for(const stroke of [false,true]) {
        const actual=make(),expected=make();actual.font=expected.font='18px monospace';
        setup(actual,32);actual.lineWidth=expected.lineWidth=2;expected.fillStyle=expected.strokeStyle='blue';
        if(stroke){actual.strokeText('M',-20,24);expected.strokeText('M',12,24);}else{actual.fillText('M',-20,24);expected.fillText('M',12,24);}
        const got=Array.from(actual.getImageData(0,0,64,64).data), want=Array.from(expected.getImageData(0,0,64,64).data);
        check(want.some(v=>v!==0) && got.every((v,i)=>Math.abs(v-want[i])<=2),'off-canvas glyph shadow '+stroke);
    }
    c.reset();c.canvas.style.color='rgb(12, 34, 56)';c.shadowColor='currentColor';
    check(c.shadowColor==='#0c2238','currentColor resolves against canvas element');
    c.shadowBlur=1e200;check(c.shadowBlur===1e200,'raster blur cap does not alter attribute');
    c.shadowOffsetX=-0;check(Object.is(c.shadowOffsetX,-0),'unrestricted double preserves negative zero');
    const wide=make(520,32),local=make(80,32);setup(wide,3,0,8);setup(local,3,0,8);
    wide.fillRect(248,8,16,8);local.fillRect(24,8,16,8);
    const tiled=Array.from(wide.getImageData(224,0,80,32).data),untiled=Array.from(local.getImageData(0,0,80,32).data);
    check(tiled.every((v,i)=>Math.abs(v-untiled[i])<=2),'Gaussian halo has no tile seam');
    for(const mode of ['source-over','destination-over','source-in','source-out','destination-in','destination-out','source-atop','destination-atop','xor','lighter','multiply','copy']) {
        const actual=make(520,20),expected=make(520,20);
        for(const context of [actual,expected]) {
            context.fillStyle='lime';context.fillRect(0,0,520,20);
            context.beginPath();context.rect(245.5,0.5,34,18);context.clip();
            context.globalCompositeOperation=mode;
        }
        setup(actual,16);actual.fillRect(236,4,16,8);
        expected.fillStyle='blue';expected.fillRect(252,4,16,8);expected.fillStyle='red';expected.fillRect(236,4,16,8);
        // Cover the complete clip and both sides of the 256px tile seam. Also
        // check the untouched outer tiles without expanding their bytes into
        // hundreds of thousands of interpreted JS comparison callbacks.
        const got=Array.from(actual.getImageData(224,0,80,20).data),want=Array.from(expected.getImageData(224,0,80,20).data);
        check(got.every((v,i)=>Math.abs(v-want[i])<=2),'clipped tiled compositing '+mode);
        close(pixel(actual,0,10),[0,255,0,255],'left outside clip '+mode);
        close(pixel(actual,519,10),[0,255,0,255],'right outside clip '+mode);
    }
    return 'canvas-shadows-ok';
})();
