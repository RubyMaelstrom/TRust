(() => {
    function check(value, label) { if (!value) throw new Error(label); }
    for (const C of [MouseEvent, PointerEvent, WheelEvent, DragEvent]) {
        const empty = new C('move');
        check(empty.clientX === 0 && empty.clientY === 0 && empty.x === 0 && empty.y === 0 &&
            empty.screenX === 0 && empty.screenY === 0, C.name + ' defaults');
        const event = new C('move', {clientX:12.5, clientY:-3.25, screenX:'9.5', screenY:null, x:999,y:999});
        check(event.x === 12.5 && event.y === -3.25 && event.screenX === 9.5 && event.screenY === 0,
            C.name + ' coordinate conversion and aliases');
        check(!Object.hasOwn(event,'x') && !Object.hasOwn(event,'y'), 'aliases live on prototype');
        check(!Reflect.set(event,'x',123) && event.x === event.clientX, 'readonly x alias');
        for (const value of [NaN,Infinity,-Infinity,Symbol(),1n]) {
            let error;
            try { new C('move',{clientX:value}); } catch(e) {error=e;}
            check(error instanceof TypeError, C.name + ' finite double');
        }
    }
    for (const name of ['x','y']) {
        const d=Object.getOwnPropertyDescriptor(MouseEvent.prototype,name);
        check(d.enumerable && d.configurable && typeof d.get === 'function' && !d.set, 'IDL alias descriptor');
    }
    const legacy=document.createEvent('MouseEvents');
    legacy.initMouseEvent('click',true,true,window,1,80,90,13,17,false,false,false,false,0,null);
    check(legacy.x===13 && legacy.y===17, 'legacy initializer aliases');
    return 'mouse-coordinates-ok';
})()
