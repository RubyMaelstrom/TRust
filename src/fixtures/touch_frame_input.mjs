(() => {
    const check = (value, message) => { if (!value) throw Error(message); };
    document.body.style.margin = '0';
    const frame = document.createElement('iframe');
    frame.style.cssText = 'position:absolute;left:40px;top:30px;border:0;width:200px;height:150px';
    frame.srcdoc = '<body style="margin:0"><button id=b style="width:100px;height:100px;touch-action:none">tap</button></body>';
    document.body.append(frame);
    __trust.hydrateFrames();
    const child = frame.contentWindow;
    child.eval(`
        globalThis.events = [];
        for (const type of ['pointerdown','pointermove','pointerup','pointercancel','gotpointercapture','lostpointercapture','click'])
            document.addEventListener(type, e => {
                events.push({type:e.type, id:e.pointerId, x:e.clientX, y:e.clientY, px:e.pageX, py:e.pageY, sx:e.screenX, sy:e.screenY,
                    target:e.target.id, realm:e instanceof PointerEvent && e.target instanceof HTMLElement,
                    captured:e.target.hasPointerCapture(e.pointerId)});
            });
    `);
    const node = child.document.getElementById('b').__id;
    const send = (phase, extra = {}) => {
        const text = JSON.stringify({phase, id:8, node, x:60, y:50, screenX:160, screenY:250,
            pressure:0.5, primary:true, modifiers:0, ...extra});
        if (phase==='down') __trust.touchPrepare(text);
        __trust.touchInput(text);
    };
    send('down');
    send('move', {x:280,y:60});
    send('up', {tap:true});
    __trust.click(node);
    const down=child.events.find(e=>e.type==='pointerdown'), move=child.events.find(e=>e.type==='pointermove');
    check(down && down.realm && down.captured && down.x===20 && down.y===20, 'child implicit capture and coordinates: '+JSON.stringify(down));
    check(move && move.realm && move.target==='b' && move.x===240 && move.y===30, 'capture outside child viewport');
    check(child.events.every(e=>e.sx===160 && e.sy===250 && e.id===8), 'screen coordinates and identity cross realms');
    const click=child.events.find(e=>e.type==='click');
    check(click && click.realm && click.target==='b' && click.x===20 && click.y===20, 'child touch click');
    check(document.activeElement===frame && child.document.activeElement.id==='b', 'focus follows child tap');
    check(!__trust.hasTouchInput(), 'shared contact is released');
    send('down');
    const last = child.events.at(-1);
    frame.style.left='80px';
    child.document.body.style.height='500px';
    child.scrollTo(0,30);
    check(child.scrollY===30, 'child viewport scrolled before cancellation');
    send('cancel');
    check(child.events.slice(-3).map(e=>e.type).join(',')==='gotpointercapture,pointercancel,lostpointercapture', 'cancel capture sequence');
    check(child.events.slice(-3).every(e=>e.x===last.x&&e.y===last.y&&e.px===last.px&&e.py===last.py), 'cancel freezes coordinates across frame movement and scrolling');

    frame.style.left='40px'; child.scrollTo(0,0);
    let childUp=0, parentUp=0;
    child.addEventListener('mouseup', e=>{if(e.target===child)childUp++;});
    window.addEventListener('mouseup', e=>{if(e.target===window)parentUp++;});
    send('down'); send('move', {x:70,y:60,mouse:true}); send('cancel');
    check(childUp===1&&parentUp===0, 'canceled compatibility mouse release belongs to the child window');

    const parentButton=document.createElement('button');
    parentButton.style.cssText='position:absolute;left:300px;top:0;width:100px;height:100px';
    document.body.append(parentButton);
    parentButton.addEventListener('pointerdown', e=>child.Element.prototype.setPointerCapture.call(parentButton,e.pointerId));
    let capturedMove;
    parentButton.addEventListener('pointermove', e=>capturedMove=e);
    send('down', {node:parentButton.__id,x:310,y:20});
    send('move', {node:parentButton.__id,x:320,y:20});
    check(capturedMove instanceof PointerEvent && !(capturedMove instanceof child.PointerEvent) && capturedMove.clientX===320,
        'borrowed capture method keeps the receiving element realm');
    send('up', {node:parentButton.__id,x:320,y:20});
    // Effective mouse position includes the embedding iframe in each parent
    // Document. Routing into the child must not make the iframe immediately
    // leave, and relatedTarget must stay in the receiving Document.
    const boundaries=[];
    let foreignRelated=0;
    for(const target of [frame,parentButton,child.document.getElementById('b')]) {
        for(const type of ['mouseover','mouseout','mouseenter','mouseleave','pointerover','pointerout']) {
            target.addEventListener(type,e=>{
                if(e.target!==target)return;
                boundaries.push({type,target,related:e.relatedTarget,x:e.clientX,y:e.clientY});
                if(e.relatedTarget&&e.relatedTarget.ownerDocument!==target.ownerDocument)foreignRelated++;
            });
        }
    }
    const childButton=child.document.getElementById('b');
    __trust.hover(parentButton.__id,310,20,410,220);
    boundaries.length=0;
    __trust.hover(node,60,50,160,250);
    check(boundaries.filter(e=>e.target===frame&&e.type==='mouseover').length===1, 'mouse enters the embedding iframe once');
    check(!boundaries.some(e=>e.target===frame&&e.type==='mouseout'), 'entering the child preserves parent hover');
    check(boundaries.some(e=>e.target===parentButton&&e.type==='mouseout'&&e.related===frame), 'parent transition targets the iframe');
    check(boundaries.some(e=>e.target===childButton&&e.type==='mouseover'&&e.related===null), 'child entry has no foreign related target');
    const count=boundaries.length;
    __trust.hover(node,65,55,165,255);
    check(boundaries.length===count, 'motion inside the same child adds no boundary events');
    __trust.hover(parentButton.__id,310,20,410,220);
    check(boundaries.filter(e=>e.target===frame&&e.type==='mouseout').length===1, 'mouse exits the embedding iframe once');
    check(boundaries.some(e=>e.target===childButton&&e.type==='mouseout'&&e.related===null), 'child exit has no foreign related target');
    check(boundaries.some(e=>e.target===parentButton&&e.type==='mouseover'&&e.related===frame), 'parent return relates to its own iframe');

    boundaries.length=0;
    childButton.addEventListener('pointerdown',e=>childButton.releasePointerCapture(e.pointerId),{once:true});
    send('down');
    check(boundaries.some(e=>e.target===frame&&e.type==='pointerover'&&e.x===60&&e.y===50), 'touch entry reaches parent in parent coordinates');
    send('move',{node:parentButton.__id,x:310,y:20});
    check(boundaries.some(e=>e.target===childButton&&e.type==='pointerout'&&e.related===null), 'uncaptured touch exits child Document');
    check(boundaries.some(e=>e.target===parentButton&&e.type==='pointerover'&&e.related===frame), 'touch transition projects the previous child to its iframe');
    send('cancel');
    check(foreignRelated===0, 'boundary relatedTarget is local to the receiving Document');
    return 'touch-frame-input-ok';
})()
