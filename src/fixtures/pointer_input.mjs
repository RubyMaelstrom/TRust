// Local W3C Pointer Events 49c3982, UI Events 8c1b809, CSSOM View 81c27f6.
(function () {
    const check = (ok, name) => { if (!ok) throw Error(name); };
    function throws(fn, name) {
        let error; try { fn(); } catch (e) { error = e; }
        check(error && error.name === name, 'expected ' + name);
    }
    for (const C of [MouseEvent, PointerEvent, KeyboardEvent]) {
        const e = new C('test', {ctrlKey:true, shiftKey:true, modifierAltGraph:true, modifierCapsLock:true,
            modifierFn:true, modifierFnLock:true, modifierHyper:true, modifierNumLock:true,
            modifierScrollLock:true, modifierSuper:true, modifierSymbol:true, modifierSymbolLock:true});
        for (const key of ['Control','Shift','AltGraph','CapsLock','Fn','FnLock','Hyper','NumLock','ScrollLock','Super','Symbol','SymbolLock'])
            check(e.getModifierState(key), C.name + ' modifier ' + key);
        check(!e.getModifierState('Alt') && !e.getModifierState('Meta') && !e.getModifierState('shift'), 'case-sensitive state');
        e.ctrlKey = false; check(e.getModifierState('Control'), 'internal state is not an expando');
        throws(() => C.prototype.getModifierState.call({}), 'TypeError');
        throws(() => e.getModifierState(), 'TypeError');
        throws(() => e.getModifierState(Symbol()), 'TypeError');
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.append(html); html.append(body);
    body.innerHTML = '<div id=a style="position:absolute;left:20px;top:30px;width:100px;height:80px;border:3px solid;padding:5px"></div><div id=b></div>';
    const a = document.getElementById('a'), b = document.getElementById('b');
    let offset, stored;
    a.addEventListener('mousemove', e => { offset=[e.offsetX,e.offsetY]; stored=e; });
    const rect = a.getBoundingClientRect();
    __trust.hover(a.__id,rect.x+13.5,rect.y+20.25,234.5,345.25,3);
    check(offset[0]===10.5 && offset[1]===17.25, 'padding-edge offsets: '+offset);
    check(stored.offsetX===stored.pageX && stored.offsetY===stored.pageY, 'offset outside dispatch');
    check(stored.ctrlKey && stored.shiftKey && stored.getModifierState('Control') && stored.screenX===234.5, 'motion metadata');
    check(new MouseEvent('x',{clientX:12.5}).offsetX===12.5, 'synthetic offset default');
    throws(()=>Object.getOwnPropertyDescriptor(MouseEvent.prototype,'offsetX').get.call({}), 'TypeError');
    check(!a.hasPointerCapture(123), 'unknown has capture');
    throws(()=>a.setPointerCapture(123), 'NotFoundError');
    throws(()=>a.releasePointerCapture(123), 'NotFoundError');
    throws(()=>Element.prototype.setPointerCapture.call({},1), 'TypeError');
    throws(()=>a.hasPointerCapture(), 'TypeError');
    a.setPointerCapture(1); check(!a.hasPointerCapture(1), 'hover alone cannot capture');
    const detached=document.createElement('div');
    throws(()=>detached.setPointerCapture(1), 'InvalidStateError');
    const events=[];
    for (const type of ['pointerdown','mousedown','gotpointercapture','pointermove','pointerup','mouseup','lostpointercapture','click'])
        a.addEventListener(type,e=>events.push([type,e.target,e.button,e.buttons,e.isTrusted,e.ctrlKey,e.screenX]));
    a.onpointerdown=e=>{ a.setPointerCapture(e.pointerId); check(a.hasPointerCapture(1), 'pending capture visible'); };
    __trust.pointerButton(a.__id,true,35,50,235,350,0,2);
    check(!events.some(e=>e[0]==='gotpointercapture'), 'capture is pending');
    __trust.hover(b.__id,200,210,400,510,2);
    check(events.at(-2)[0]==='gotpointercapture' && events.at(-1)[0]==='pointermove', 'capture before motion');
    check(events.at(-1)[1]===a && events.at(-1)[3]===1 && events.at(-1)[5] && events.at(-1)[6]===400, 'captured target and state');
    __trust.pointerButton(b.__id,false,200,210,400,510,0,2);
    check(!a.hasPointerCapture(1) && events.at(-1)[0]==='lostpointercapture', 'implicit release');
    __trust.click(b.__id);
    check(events.at(-1)[0]==='click' && events.at(-1)[1]===a && events.at(-1)[4] && events.at(-1)[5], 'click retains up capture target');
    a.onpointerdown=null;
    const counts=[];
    b.addEventListener('click',e=>counts.push('click:'+e.detail));
    b.addEventListener('dblclick',e=>counts.push('dblclick:'+e.detail));
    for(let i=0;i<2;i++) {
        __trust.pointerButton(b.__id,true,201,211,401,511);
        __trust.pointerButton(b.__id,false,201,211,401,511); __trust.click(b.__id);
    }
    check(counts.join('|')==='click:1|click:2|dblclick:2', 'click counts: '+counts);
    const chords=[], aux=[];
    for(const type of ['pointerdown','pointermove','pointerup']) b.addEventListener(type,e=>chords.push(type+':'+e.button+':'+e.buttons));
    for(const type of ['auxclick','contextmenu']) b.addEventListener(type,e=>aux.push(type+':'+e.button+':'+e.isPrimary));
    __trust.pointerButton(b.__id,true,1,2,101,102,2,4);
    __trust.pointerButton(b.__id,true,1,2,101,102,1,4);
    __trust.pointerButton(b.__id,false,1,2,101,102,2,4);
    __trust.pointerButton(b.__id,false,1,2,101,102,1,4);
    check(chords.join('|')==='pointerdown:2:2|pointermove:1:6|pointermove:2:4|pointerup:1:0', 'chording: '+chords);
    check(aux.join('|')==='contextmenu:2:false|auxclick:2:false|auxclick:1:false', 'auxiliary clicks: '+aux);
    __trust.pointerButton(a.__id,true,1,2); a.setPointerCapture(1); __trust.hover(a.__id,1,2);
    let removedLost=false; document.addEventListener('lostpointercapture',e=>{ if(e.target===document)removedLost=true; });
    a.remove(); __trust.hover(b.__id,1,2);
    check(removedLost && !a.hasPointerCapture(1), 'disconnected capture lost at document');
    __trust.pointerButton(b.__id,false,1,2);
    const frame=document.createElement('iframe'); body.append(frame); __trust.hydrateFrames();
    const child=frame.contentWindow, inner=frame.contentDocument.createElement('a'); frame.contentDocument.body.append(inner);
    inner.href='/captured'; inner.setAttribute('target','_top');
    let move, click;
    inner.onpointerdown=e=>inner.setPointerCapture(e.pointerId);
    inner.onpointermove=e=>move=e; inner.onclick=e=>click=e;
    __trust.pointerButton(inner.__id,true,30,40,130,140,0,8);
    __trust.hover(b.__id,60,70,160,170,8);
    check(move && move.target===inner && move instanceof child.PointerEvent && move.screenX===160 && move.metaKey, 'capture crosses iframe boundary');
    __trust.pointerButton(b.__id,false,60,70,160,170,0,8); __trust.click(b.__id);
    check(click && click.target===inner && click instanceof child.PointerEvent && click.metaKey, 'captured iframe click');
    const followed=__trust.followAnchorDefault(b.__id);
    check(followed===new URL('/captured',location.href).href,'captured iframe hyperlink default: '+followed);
    check(MouseEvent.prototype.getModifierState.call(click,'Meta'), 'cross-realm modifier brand');
    const form=child.document.createElement('form'), submit=child.document.createElement('button'); form.append(submit); child.document.body.append(form);
    submit.onpointerdown=e=>submit.setPointerCapture(e.pointerId);
    __trust.pointerButton(submit.__id,true,30,40);
    __trust.pointerButton(b.__id,false,60,70); __trust.click(b.__id);
    check(__trust.lastClickSubmit && __trust.lastClickSubmit.form===form.__id,'captured iframe submission acknowledgement');
    check(trustErrorsEmpty(), 'listener errors: '+__trust.errors.join('|'));
    function trustErrorsEmpty(){return !__trust.errors.length && !child.__trust.errors.length;}
    return 'pointer-input-ok';
})()
