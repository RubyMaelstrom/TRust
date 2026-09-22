// DOM a2331a45 #concept-event-dispatch / #concept-event-listener-inner-invoke.
// Flat worker EventTargets still have two at-target listener invocations.
(function () {
    const check=(ok,text)=>{if(!ok)throw Error(text);};
    const throws=(name,fn)=>{let error;try{fn();}catch(e){error=e;}check(error&&error.name===name,'expected '+name);};
    for (const C of [Event,MessageEvent,ErrorEvent,CustomEvent]) {
        throws('TypeError',()=>C('without-new'));
        throws('TypeError',()=>new C());
        throws('TypeError',()=>new C(Symbol()));
        const event=new C('ready');
        check(event instanceof Event && event.constructor===C && C.length===1,'event construction');
    }
    check(!(new CustomEvent('x') instanceof MessageEvent),'CustomEvent inherits Event directly');
    throws('TypeError',()=>Event.prototype.preventDefault.call({}));
    throws('TypeError',()=>new Event('x',1));
    const target=new EventTarget(), log=[];
    const event=new Event('test',{bubbles:true,cancelable:true,composed:true});
    const handler=e=>{log.push('normal');e.preventDefault();};
    target.addEventListener('test',handler);
    target.addEventListener('test',handler); // Duplicate is ignored.
    target.addEventListener('test',e=>{
        log.push('capture');check(e.eventPhase===2 && e.currentTarget===target && e.target===target,'at target');
        check(e.composedPath().join()===String(target),'flat path');
        throws('InvalidStateError',()=>target.dispatchEvent(e));
        target.addEventListener('test',()=>log.push('added'),{once:true});
    },{capture:true,once:true});
    check(!target.dispatchEvent(event) && log.join()==='capture,normal,added','phase ordering and cancelation');
    check(event.defaultPrevented && !event.returnValue && event.composed && event.target===target &&
        event.currentTarget===null && event.eventPhase===0 && event.composedPath().length===0,'post-dispatch state');
    event.initEvent('passive',false,true);
    target.addEventListener('passive',e=>{e.preventDefault();e.returnValue=false;},{passive:true});
    check(target.dispatchEvent(event) && !event.defaultPrevented,'passive listener cannot cancel');
    let immediate=0;
    target.addEventListener('stop',e=>{immediate++;e.stopImmediatePropagation();},{once:true});
    target.addEventListener('stop',()=>immediate++);
    const stopped=new Event('stop');target.dispatchEvent(stopped);check(immediate===1,'immediate stop');
    target.dispatchEvent(stopped);check(immediate===2 && !stopped.cancelBubble,'stop flags reset and once listener removed');
    throws('TypeError',()=>target.dispatchEvent({type:'test'}));
    throws('TypeError',()=>EventTarget.prototype.dispatchEvent.call({},new Event('x')));
    throws('TypeError',()=>target.addEventListener(Symbol(),null));
    throws('TypeError',()=>target.addEventListener('x',1));
    let reads=0, objectCalls=0;
    const objectListener={get handleEvent(){reads++;return function(){check(this===objectListener,'callback receiver');objectCalls++;};}};
    target.addEventListener('object',objectListener,1);
    check(reads===0,'callback interface conversion does not read handleEvent');
    target.dispatchEvent(new Event('object'));
    target.removeEventListener('object',objectListener,1);
    target.dispatchEvent(new Event('object'));
    check(reads===1 && objectCalls===1,'late callback lookup and primitive capture option');
    let afterError=false;
    target.addEventListener('error-report',()=>{throw {get message(){throw Error('formatting');}};},{once:true});
    target.addEventListener('error-report',()=>{afterError=true;},{once:true});
    target.dispatchEvent(new Event('error-report'));
    check(afterError && __wkr.takeErrors().includes('exception could not be formatted'),'listener exceptions remain contained');
    const dateNow=Date.now;
    Date.now=()=>-1000000;
    let timed;try{timed=new Event('timed');}finally{Date.now=dateNow;}
    check(timed.timeStamp>=0 && timed.timeStamp<=performance.now(),'monotonic relative event timestamp');
    const immutable=Object.getOwnPropertyDescriptor(timed,'isTrusted');
    check(!immutable.configurable && !immutable.set && !timed.isTrusted,'unforgeable trust');
    return 'worker-events-ok';
})();
