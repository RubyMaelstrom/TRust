// Pointer Events 49c398264c8d: fire a pointer event, implicit pointer capture,
// suppress a pointer event stream, and compatibility mapping with mouse events.
// The Window adapter supplies trusted dispatch and its existing activation path.
(function (g) {
    "use strict";
    const a = g.__touch_adapter;
    delete g.__touch_adapter;
    if (!a) return;
    const apply = Reflect.apply, define = Object.defineProperty;
    const getOwn = Object.getOwnPropertyDescriptor, parse = JSON.parse;
    const verticalAngle = Math.PI/2;
    const mapGet = Map.prototype.get, mapSet = Map.prototype.set, mapDelete = Map.prototype.delete;
    const mapForEach = Map.prototype.forEach;
    const weakGet=WeakMap.prototype.get, weakSet=WeakMap.prototype.set;
    const mapSize = getOwn(Map.prototype, "size").get;
    const exception = g.DOMException, TypeErrorCtor = TypeError;
    const canceled = getOwn(g.Event.prototype, "defaultPrevented").get;
    const connected = getOwn(g.Node.prototype, "isConnected").get;
    const owner = getOwn(g.Node.prototype, "ownerDocument").get;
    const nodeType = getOwn(g.Node.prototype, "nodeType").get;
    const put = (object,key,value) => define(object,key,
        {__proto__:null,value,writable:true,enumerable:true,configurable:true});
    if(!getOwn(a.capabilities,"touchKnownMinimum"))put(a.capabilities,"touchKnownMinimum",0);
    const doc = node => apply(nodeType,node,[])===9 ? node : apply(owner,node,[]);
    const attached = node => !!node && apply(connected,node,[]);
    // Pointer IDs belong to one top-level context, including its frames.
    // Other top-level Windows in the same agent can use the same IDs.
    const shared=a.topControl();
    const data = shared.touch || (shared.touch = {pointers:new Map(),legacy:null});
    const pointers = data.pointers;
    const documents=a.capabilities.touchDocuments || (a.capabilities.touchDocuments=new WeakMap());
    let prepared = null;

    function ownApi(node) {
        return apply(weakGet,documents,[doc(node)]) || a.controlForNode(node).touchApi;
    }
    function location(api,input) {
        const origin=api.origin(), scroll=api.scroll();
        return {...input,view:api.window,
            clientX:input.x-origin.x, clientY:input.y-origin.y,
            pageX:input.x-origin.x+scroll.x, pageY:input.y-origin.y+scroll.y};
    }
    function sample(input,api,phase) {
        const bits=input.modifiers|0;
        return location(api,{
            x:input.x,y:input.y,screenX:input.screenX,screenY:input.screenY,
            pointerId:input.id,pointerType:"touch",isPrimary:!!input.primary,
            width:1,height:1,pressure:phase==="up" ? 0 : input.pressure,
            tangentialPressure:0,tiltX:0,tiltY:0,twist:0,
            altitudeAngle:verticalAngle,azimuthAngle:0,
            button:phase==="move" ? -1 : 0,buttons:phase==="up" ? 0 : 1,
            ctrlKey:!!(bits&2),shiftKey:!!(bits&1),altKey:!!(bits&4),metaKey:!!(bits&8),
            detail:0,relatedTarget:null,
        });
    }
    function pointer(api,type,target,init,frozen=false) {
        const boundary=type==="pointerenter"||type==="pointerleave";
        const cancelable=!boundary&&type!=="pointercancel"&&type!=="gotpointercapture"&&type!=="lostpointercapture";
        const options={...(frozen ? init : location(api,init)),bubbles:!boundary,composed:!boundary,cancelable};
        if(type==="pointermove")options.coalescedEvents=[api.sample({...options,bubbles:false,composed:false,cancelable:false})];
        return api.pointer(type,target,options);
    }
    function mouse(api,type,target,init,bubbles=true,frozen=false) {
        return api.mouse(type,target,{...(frozen ? init : location(api,init)),bubbles,composed:bubbles,cancelable:bubbles,
            button:init.pointerType==="touch" ? 0 : (init.button??0),
            detail:type==="mousedown"||type==="mouseup" ? 1 : 0});
    }
    function same(left,right) { return left?.target===right?.target; }
    function includes(list,value) {
        for(let i=0;i<list.length;i++)if(list[i]===value)return true;
        return false;
    }
    function contextRecord(record, document) {
        for(;record;record=record.outer)if(record.document===document)return record;
        return null;
    }
    function boundary(previous,next,init,send) {
        if(same(previous,next))return;
        // Entry/exit state belongs to each Document. A child target projects
        // to its embedding iframe in the parent, never to a foreign Node.
        for(let old=previous;old;old=old.outer) {
            const current=contextRecord(next,old.document);
            if(same(old,current))continue;
            const target=current?.target||null, path=current?.path||[];
            send(old.api,"out",old.target,{...init,relatedTarget:target},true);
            for(let i=0;i<old.path.length;i++) {
                if(includes(path,old.path[i]))break;
                send(old.api,"leave",old.path[i],{...init,relatedTarget:target},false);
            }
        }
        const entries=[];
        for(let current=next;current;current=current.outer)put(entries,entries.length,current);
        for(let index=entries.length-1;index>=0;index--) {
            const current=entries[index], old=contextRecord(previous,current.document);
            if(same(old,current))continue;
            const target=old?.target||null, path=old?.path||[];
            send(current.api,"over",current.target,{...init,relatedTarget:target},true);
            for(let i=current.path.length-1;i>=0;i--)
                if(!includes(path,current.path[i]))send(current.api,"enter",current.path[i],{...init,relatedTarget:target},false);
        }
    }
    function targetRecord(api,target) {
        if(!target)return null;
        const first={api,target,path:api.path(target),document:doc(target),outer:null};
        let record=first;
        for(let frame=api.frame();frame;frame=api.frame()) {
            api=ownApi(frame);
            if(!api||contextRecord(first,doc(frame)))break;
            record.outer={api,target:frame,path:api.path(frame),document:doc(frame),outer:null};
            record=record.outer;
        }
        return first;
    }
    function legacy(next,init,frozen=false) {
        boundary(data.legacy,next,init,(api,name,target,init,bubbles)=>mouse(api,"mouse"+name,target,init,bubbles,frozen&&api.window===init.view));
        data.legacy=next;
    }
    function pointerBoundary(state,next,init,frozen=false) {
        const previous=state.target;
        boundary(previous,next,init,(api,name,target,init)=>pointer(api,"pointer"+name,target,init,frozen&&api.window===init.view));
        state.target=next;
    }
    function processCapture(state,init,frozen=false) {
        let previous=state.capture;
        if(previous&&!attached(previous.target))previous=state.capture=targetRecord(previous.api,doc(previous.target));
        if(state.pending&&!attached(state.pending.target))state.pending=null;
        if(previous&&!same(previous,state.pending))pointer(previous.api,"lostpointercapture",previous.target,init,frozen);
        const pending=state.pending;
        if(pending&&!same(previous,pending))pointer(pending.api,"gotpointercapture",pending.target,init,frozen);
        state.capture=state.pending;
    }
    function resolve(input) {
        let current=api, x=input.x, y=input.y, node=input.node;
        const visited=[];
        for(;;) {
            if(includes(visited,current))return null;
            put(visited,visited.length,current);
            const hit=current.resolve(node,x,y);
            if(!hit)return null;
            if(hit.child) {
                current=hit.child.touchApi;x=hit.x;y=hit.y;node=hit.node;
                if(!current)return null;
            } else return targetRecord(current,hit.target);
        }
    }
    function rememberedTarget(state) {
        if(!state.target||attached(state.target.target))return state.target;
        const old=state.target;
        for(let i=0;i<old.path.length;i++)if(attached(old.path[i]))return targetRecord(old.api,old.path[i]);
        return null;
    }
    function startMouse(state,current) {
        if(!state.primary||state.mouse)return;
        const down=state.down;
        if(!down||!attached(down.target))return;
        state.mouse=true;
        // Non-hover compatibility mapping begins with motion before the
        // mouse boundary/down sequence. These events may be grouped at tap.
        mouse(down.api,"mousemove",down.target,{...state.downSample,buttons:0});
        legacy(down,state.downSample);
        if(!state.suppress) {
            const event=mouse(down.api,"mousedown",down.target,state.downSample);
            if(!apply(canceled,event,[]))state.root.focus(down.target);
        }
        if(current)legacy(current,state.last);
    }

    const original = {};
    for(const name of ["setPointerCapture","releasePointerCapture","hasPointerCapture"])
        original[name]=g.Element.prototype[name];
    function captureArgument(receiver,id,count) {
        if(a.elementId(receiver)===undefined)throw new TypeErrorCtor("Expected an Element");
        if(!count)throw new TypeErrorCtor("Missing pointerId");
        return +id|0;
    }
    const capture = {
        setPointerCapture(id) {
            id=captureArgument(this,id,arguments.length);
            const receiver=ownApi(this), state=apply(mapGet,receiver.pointers,[id]);
            if(!state)return apply(original.setPointerCapture,this,[id]);
            const document=doc(this),locked=a.lockedElement();
            if(!receiver.connected(this)||locked&&doc(locked)===document)
                throw new exception("Cannot capture this pointer","InvalidStateError");
            if(!state.buttons||document!==state.document)return;
            state.pending=targetRecord(receiver,this);
        },
        releasePointerCapture(id) {
            id=captureArgument(this,id,arguments.length);
            const state=apply(mapGet,ownApi(this).pointers,[id]);
            if(!state)return apply(original.releasePointerCapture,this,[id]);
            if(state.pending?.target===this)state.pending=null;
        },
        hasPointerCapture(id) {
            id=captureArgument(this,id,arguments.length);
            const state=apply(mapGet,ownApi(this).pointers,[id]);
            return state ? state.pending?.target===this : apply(original.hasPointerCapture,this,[id]);
        },
    };
    for(const name of ["setPointerCapture","releasePointerCapture","hasPointerCapture"])
        put(g.Element.prototype,name,capture[name]);

    const api = a.control.touchApi = {
        window:g, frame:a.frame, origin:a.origin, scroll:a.scroll, path:a.path, pointer:a.pointer, sample:a.sample, mouse:a.mouse,
        focus:a.focus, resolve:a.resolve, activate:a.activate, click:a.click, lightDismiss:a.lightDismiss,
        pointers, connected:attached,
    };
    apply(weakSet,documents,[g.document,api]);
    // The mouse keeps its own PointerEvent hover/capture state. Only its
    // compatibility boundary events share this effective legacy position.
    a.installLegacy(function(name,target,related,init,allowMove) {
        const origin=a.origin();
        init={...init,x:init.clientX+origin.x,y:init.clientY+origin.y};
        if(name==="over") {
            // Parent hover routing visits the iframe before the child target.
            // Keep an existing child position when this ancestor is unchanged.
            if(contextRecord(data.legacy,g.document)?.target!==target)legacy(targetRecord(api,target),init);
        } else if(name==="move"||name==="position")legacy(targetRecord(api,target),init);
        else if(name==="out"&&!related) {
            const current=contextRecord(data.legacy,g.document);
            if(current)legacy(current.outer,init);
        }
        if(name==="move"&&allowMove)mouse(api,"mousemove",target,init);
    });

    a.control.touchPrepare=function(text) {
        const input=parse(text);
        const target=resolve(input);
        prepared={id:input.id,target};
        return target ? a.elementId(target.target) : null;
    };
    a.control.hasTouchInput=()=>apply(mapSize,pointers,[])!==0;
    a.control.touchReleaseCaptures=function(document) {
        apply(mapForEach,pointers,[state=>{
            if(state.pending&&doc(state.pending.target)===document || state.capture&&doc(state.capture.target)===document) {
                state.pending=null;
                processCapture(state,state.last);
            }
        }]);
    };
    a.control.touchInput=function(text) {
        const input=parse(text), id=input.id, phase=input.phase;
        let state=apply(mapGet,pointers,[id]);
        if(phase==="down") {
            if(state)return;
            // Pointer Events #dom-navigator-maxtouchpoints permits a known
            // minimum when the platform does not report its digitizer limit.
            a.capabilities.touchKnownMinimum=1;
            const hit=prepared?.id===id ? prepared.target : resolve(input);
            prepared=null;
            if(!hit)return;
            const init=sample(input,hit.api,phase);
            state={id,primary:!!input.primary,buttons:1,document:null,root:api,
                capture:null,pending:null,target:null,down:hit,downSample:init,last:init,mouse:false,suppress:false};
            apply(mapSet,pointers,[id,state]);
            pointerBoundary(state,hit,init);
            state.document=doc(hit.target);
            const locked=a.lockedElement();
            if(a.elementId(hit.target)!==undefined&&attached(hit.target)&&(!locked||doc(locked)!==state.document))state.pending=hit;
            hit.api.lightDismiss("pointerdown",hit.target,init.clientX,init.clientY);
            const event=pointer(hit.api,"pointerdown",hit.target,init);
            state.suppress=apply(canceled,event,[]);
            state.target=rememberedTarget(state);
            if(input.mouse)startMouse(state,hit);
            return;
        }
        if(!state)return;
        if(phase==="cancel") {
            // #pointercancel copies the last dispatched coordinates even if
            // the frame moved or its Document scrolled since that sample.
            const init={...state.last,button:-1,buttons:0};
            state.buttons=0;
            processCapture(state,init,true);
            state.target=rememberedTarget(state);
            const target=state.capture||rememberedTarget(state);
            if(target)pointer(target.api,"pointercancel",target.target,init,true);
            state.pending=null;processCapture(state,init,true);
            pointerBoundary(state,null,init,true);
            if(state.mouse) {
                const owner=target?.api||state.down.api;
                if(!state.suppress)mouse(owner,"mouseup",owner.window,init,true,true);
                legacy(null,init,true);
            }
            apply(mapDelete,pointers,[id]);
            return;
        }
        if(phase!=="move"&&phase!=="up")return;
        state.buttons=phase==="up" ? 0 : 1;
        const prospective=sample(input,state.target?.api||api,phase);
        processCapture(state,prospective);
        const target=state.capture||resolve(input)||rememberedTarget(state);
        if(!target) {
            if(phase==="up")apply(mapDelete,pointers,[id]);
            return;
        }
        const init=sample(input,target.api,phase), captured=!!state.capture;
        state.document=doc(target.target);state.last=init;
        pointerBoundary(state,target,init);
        if(phase==="up") {
            target.api.activate();
            target.api.lightDismiss("pointerup",target.target,init.clientX,init.clientY);
        }
        pointer(target.api,"pointer"+phase,target.target,init);
        state.target=rememberedTarget(state);
        if(phase==="move") {
            if(input.mouse)startMouse(state,target);
            if(state.mouse) {
                legacy(target,init);
                if(!state.suppress)mouse(target.api,"mousemove",target.target,init);
            }
            return;
        }
        state.pending=null;processCapture(state,init);
        if(input.tap)startMouse(state,target);
        if(state.mouse) {
            legacy(target,init);
            if(!state.suppress)mouse(target.api,"mouseup",target.target,init);
        }
        if(input.tap&&state.primary) {
            const click=captured ? target.target : a.commonTarget(state.down.target,target.target);
            if(click)ownApi(click).click(click,{...init,detail:1,button:0,buttons:0,pressure:0,isPrimary:false});
        }
        pointerBoundary(state,null,init);
        if(state.mouse)legacy(null,init);
        apply(mapDelete,pointers,[id]);
    };
})(globalThis);
