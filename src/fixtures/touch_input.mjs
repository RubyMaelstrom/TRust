(() => {
    const check = (value, message) => { if (!value) throw Error(message); };
    document.body.style.margin = '0';
    const a = document.createElement('button'), b = document.createElement('button');
    a.textContent = 'first'; b.textContent = 'second';
    a.style.cssText = 'position:absolute;left:0;top:0;width:80px;height:80px;touch-action:none';
    b.style.cssText = 'position:absolute;left:100px;top:0;width:80px;height:80px';
    document.body.append(a, b);
    const seen = [];
    for (const name of ['pointerover','pointerdown','gotpointercapture','pointermove','pointerup',
        'pointercancel','lostpointercapture','pointerout','mousedown','mouseup','click']) {
        document.addEventListener(name, e => seen.push(e));
    }
    const send = (phase, id = 2, extra = {}) => {
        const text = JSON.stringify({phase, id, node:a.__id, x:20, y:30,
            screenX:120, screenY:230, pressure:0.4, primary:true, modifiers:3, ...extra});
        if (phase === 'down') __trust.touchPrepare(text);
        __trust.touchInput(text);
    };
    let implicit = false;
    a.addEventListener('pointerdown', e => { implicit = a.hasPointerCapture(e.pointerId); });
    send('down');
    check(navigator.maxTouchPoints>=1, 'observed digitizer minimum');
    check(implicit && __trust.hasTouchInput(), 'pending capture before pointerdown');
    check(seen.map(e=>e.type).join(',') === 'pointerover,pointerdown', 'initial events');
    send('move', 2, {node:b.__id, x:120});
    const move = seen.find(e=>e.type==='pointermove');
    check(move.target===a && move.pointerId===2 && move.pointerType==='touch' && move.isPrimary, 'implicit capture');
    check(move.isTrusted && move.clientX===120 && move.screenX===120 && move.ctrlKey && move.shiftKey, 'native metadata');
    check(move.getCoalescedEvents().length===1 && move.getCoalescedEvents()[0].isTrusted, 'coalesced sample');
    send('cancel', 2, {x:300, y:400, pressure:0.9});
    const cancel = seen.find(e=>e.type==='pointercancel');
    check(cancel.clientX===move.clientX && cancel.clientY===move.clientY && cancel.pressure===move.pressure, 'cancel retains last sample');
    check(!cancel.cancelable && !cancel.getCoalescedEvents().length && !cancel.getPredictedEvents().length, 'cancel fields');
    check(seen.slice(-3).map(e=>e.type).join(',')==='pointercancel,lostpointercapture,pointerout', 'cancel releases capture before exit');
    check(!__trust.hasTouchInput() && !a.hasPointerCapture(2), 'canceled contact removed');
    check(!seen.some(e=>e.type==='click'||e.type==='mousedown'), 'pan does not activate');

    seen.length = 0;
    a.addEventListener('pointerdown', e=>e.preventDefault(), {once:true});
    send('down', 3);
    send('up', 3, {tap:true});
    check(__trust.pointerClickPending(), 'tap queues native activation');
    __trust.click(a.__id);
    const click=seen.find(e=>e.type==='click');
    check(click instanceof PointerEvent && click.pointerType==='touch' && click.pointerId===3 && click.isTrusted, 'trusted touch click');
    check(!seen.some(e=>e.type==='mousedown'||e.type==='mouseup'), 'pointerdown cancellation suppresses compatibility mouse only');

    seen.length=0;
    send('down', 4);
    send('down', 5, {primary:false,node:b.__id,x:120});
    send('up', 4, {tap:true});
    __trust.click(a.__id);
    send('move', 5, {primary:false,node:b.__id,x:125});
    check(seen.filter(e=>e.type==='pointermove').at(-1).isPrimary===false, 'remaining contact is not promoted');
    send('up', 5, {primary:false,node:b.__id,x:125,tap:true});
    check(!__trust.pointerClickPending() && !__trust.hasTouchInput(), 'secondary contact does not activate');

    seen.length=0;
    a.addEventListener('pointerdown', e=>a.releasePointerCapture(e.pointerId), {once:true});
    send('down', 6);
    send('move', 6, {node:b.__id,x:120,mouse:true});
    check(seen.find(e=>e.type==='pointermove').target===b, 'released capture resumes hit testing');
    send('up', 6, {node:b.__id,x:120});
    check(!__trust.pointerClickPending(), 'drag does not activate');
    check(seen.filter(e=>e.type==='mousedown').length===1 && seen.filter(e=>e.type==='mouseup').length===1, 'deferred mouse pair');

    seen.length=0;
    send('down', 7);
    send('move', 7);
    __trust.key(a.__id, 'a', 'KeyA', false, false, false, false, false, false);
    a.requestPointerLock();
    const request = __trust.takePointerLockRequest();
    check(request && request[1]===a.__id, 'native lock request');
    __trust.pointerLockResult(request[0], null);
    check(!a.hasPointerCapture(7) && seen.at(-1).type==='lostpointercapture', 'pointer lock releases touch capture');
    send('cancel', 7);
    __trust.releasePointerLock(true);
    __trust.takePointerLockRequest();

    seen.length=0;
    send('down', 8);
    send('move', 8);
    a.remove();
    send('cancel', 8);
    check(seen.some(e=>e.type==='lostpointercapture'&&e.target===document), 'detached capture releases at document');
    check(!__trust.hasTouchInput(), 'all contacts ended');
    return 'touch-input-ok';
})()
