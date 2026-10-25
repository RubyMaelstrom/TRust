// Reduced regression cases from the August 2026 Servo review. Normative
// authority: the local WHATWG HTML e5071a20c856 and Pointer Events
// 49c398264c8d snapshots (2026-09-06), rather than another engine's behavior.

#[test]
fn servo_audit_pointer_capture_chords_preserve_triggering_button_state() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = platform_engine_with_body();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            string_value(
                &mut engine,
                r#"(() => {
            const a=document.body.appendChild(document.createElement('div'));
            const b=document.body.appendChild(document.createElement('div'));
            const events=[];
            for (const node of [a,b]) for (const type of ['gotpointercapture','lostpointercapture','pointermove','pointerup'])
                node.addEventListener(type,e=>events.push([type,e.target===a?'a':'b',e.button,e.buttons,e.clientX,e.ctrlKey,e.isTrusted].join(':')));
            __trust.pointerButton(a.__id,true,10,20,30,40,0,0);
            a.setPointerCapture(1);
            __trust.pointerButton(b.__id,true,11,21,31,41,2,2);
            b.setPointerCapture(1);
            __trust.pointerButton(a.__id,false,12,22,32,42,0,0);
            __trust.pointerButton(a.__id,false,13,23,33,43,2,2);
            return events.join('|');
        })()"#
            ),
            "gotpointercapture:a:2:3:11:true:true|pointermove:a:2:3:11:true:true|lostpointercapture:a:0:2:12:false:true|gotpointercapture:b:0:2:12:false:true|pointermove:b:0:2:12:false:true|pointerup:b:2:0:13:true:true|lostpointercapture:b:2:0:13:true:true",
            "{tier:?}"
        );
    }
}

#[test]
fn servo_audit_history_checks_inactive_documents_before_state_serialization() {
    // HTML #shared-history-push/replace-state-steps performs its fully-active
    // check before StructuredSerializeForStorage; Web IDL conversion is earlier.
    let mut engine = platform_engine_with_body();
    assert_eq!(
        string_value(
            &mut engine,
            r#"(() => {
        const frame=document.createElement('iframe');document.body.append(frame);
        const h=frame.contentWindow.history;
        const retiredDocument=frame.contentDocument;
        frame.remove();
        let serialized=0,converted=0,errors=[];
        for(const method of ['pushState','replaceState']) {
            try { h[method]({get x(){serialized++;return 1}}, {toString(){converted++;return ''}}, '#late'); }
            catch(e){errors.push(e.name);}
        }
        try { h.back(); } catch(e){errors.push(e.name);}
        return [errors.join(),serialized,converted,retiredDocument.defaultView===null].join('|');
    })()"#
        ),
        "SecurityError,SecurityError,SecurityError|0|2|true"
    );
}

#[test]
fn servo_audit_initial_blank_is_synchronous_complete_and_reused_only_once() {
    // HTML #current-document-readiness and #create-new-agent-and-window:
    // initial about:blank is already populated and complete; only its first
    // same-origin cross-document navigation can retain that Window.
    let mut engine = platform_engine_with_body();
    assert_eq!(
        string_value(
            &mut engine,
            r#"(() => {
        const frame=document.createElement('iframe');let loads=0;frame.onload=()=>loads++;
        document.body.append(frame);
        const first=frame.contentWindow, initial=first.document;
        if(initial.readyState!=='complete'||!initial.head||!initial.body)throw Error('synchronous initial document '+initial.readyState);
        first.marker=42;
        frame.srcdoc='<body>first replacement</body>';__trust.hydrateFrames();
        if(frame.contentWindow!==first||frame.contentDocument===initial||first.marker!==42)throw Error('initial Window reuse');
        const second=frame.contentDocument;
        frame.srcdoc='<body>second replacement</body>';__trust.hydrateFrames();
        if(frame.contentWindow===first||frame.contentDocument===second||frame.contentWindow.marker!==undefined)throw Error('non-initial navigation reuses Window');
        if(initial.readyState!=='complete')throw Error('retired initial Document readiness '+initial.readyState);
        globalThis.servoInitialProbe=()=>[initial.readyState,frame.contentDocument.readyState,loads].join('|');
        return 'initial-ok';
    })()"#
        ),
        "initial-ok"
    );
    for _ in 0..24 {
        run_microtask_checkpoint(&mut engine);
        call_trust_method(&mut engine, "runPlatformTask", &[]);
    }
    assert_eq!(
        string_value(&mut engine, "servoInitialProbe()"),
        "complete|complete|2"
    );
}

#[test]
fn servo_audit_cross_origin_window_and_location_exotic_descriptors() {
    // HTML #windowproxy-getprototypeof through #windowproxy-ownpropertykeys,
    // #location-internal-methods and #CrossOriginGetOwnPropertyHelper.
    let mut engine = platform_engine_with_body();
    eval(
        &mut engine,
        r#"
        const frame=document.createElement('iframe');frame.sandbox='allow-scripts';
        frame.srcdoc='<body>child</body>';document.body.append(frame);__trust.hydrateFrames();
    "#,
        "exotic descriptor setup",
    )
    .unwrap();
    let realm = {
        let state = engine.host_mut::<HostState>().unwrap();
        let context = *state.window_realms.keys().max().unwrap();
        state.window_realms[&context].clone()
    };
    let result=engine.with_embed_realm(&realm, |engine| string_value(engine, r#"(() => {
        const check=(ok,label)=>{if(!ok)throw Error(label)};
        const w=parent,l=w.location;
        const extras=['then',Symbol.toStringTag,Symbol.hasInstance,Symbol.isConcatSpreadable];
        const windowNames=['window','self','location','close','closed','focus','blur','frames','length','top','opener','parent','postMessage'];
        const keys=Reflect.ownKeys(w),expected=['0',...windowNames,...extras];
        check(keys.length===expected.length&&keys.every((key,i)=>key===expected[i]),'Window ownKeys: '+keys.map(String).join());
        check(Reflect.ownKeys(l).every((key,i)=>key===['href','replace',...extras][i])&&Reflect.ownKeys(l).length===6,'Location ownKeys');
        check(w[0]===window,'indexed child Window');
        const index=Reflect.getOwnPropertyDescriptor(w,'0');
        check(index.value===window&&index.enumerable&&!index.writable&&index.configurable,'indexed descriptor');
        for(const object of [w,l]) {
            check(Reflect.getPrototypeOf(object)===null,'cross-origin prototype');
            check(Reflect.setPrototypeOf(object,null)&&!Reflect.setPrototypeOf(object,{}),'immutable prototype');
            check(Reflect.isExtensible(object)&&!Reflect.preventExtensions(object)&&Reflect.isExtensible(object),'extensibility');
            for(const key of extras) {
                const d=Reflect.getOwnPropertyDescriptor(object,key);
                check(d&&'value'in d&&d.value===undefined&&!d.enumerable&&!d.writable&&d.configurable,'fallback descriptor '+String(key));
                check(Reflect.has(object,key),'fallback has '+String(key));
            }
        }
        for(const key of ['window','self','location','closed','frames','length','top','opener','parent']) {
            const a=Reflect.getOwnPropertyDescriptor(w,key),b=Reflect.getOwnPropertyDescriptor(w,key);
            check(typeof a.get==='function'&&a.get===b.get&&!a.enumerable&&a.configurable,'stable Window getter '+key);
            check(a.get.call(w)===w[key],'Window getter value '+key);
            check((key==='location')=== (typeof a.set==='function'),'Window setter '+key);
        }
        const a=Reflect.getOwnPropertyDescriptor(l,'href'),b=Reflect.getOwnPropertyDescriptor(l,'href');
        check(a.get===undefined&&typeof a.set==='function'&&a.set===b.set&&!a.enumerable&&a.configurable,'stable Location href setter');
        return 'exotics-ok';
    })()"#)).unwrap_or_else(|_| panic!("enter child Realm"));
    assert_eq!(result, "exotics-ok");
}

#[test]
fn servo_audit_same_origin_window_and_location_prototypes_remain_immutable() {
    // HTML #windowproxy-setprototypeof / #windowproxy-preventextensions and
    // #location-internal-methods; ECMA-262 #sec-set-immutable-prototype.
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = platform_engine_with_body();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            string_value(
                &mut engine,
                r#"(() => {
            const check=(ok,label)=>{if(!ok)throw Error(label)};
            for(const object of [window,location]) {
                const prototype=Reflect.getPrototypeOf(object);
                check(Reflect.setPrototypeOf(object,prototype),'same prototype');
                check(!Reflect.setPrototypeOf(object,{}),'immutable prototype');
                check(Reflect.getPrototypeOf(object)===prototype,'prototype retained');
                check(!Reflect.preventExtensions(object)&&Reflect.isExtensible(object),'always extensible');
                let error='';try{Object.freeze(object)}catch(e){error=e.name}
                check(error==='TypeError','freeze rejected');
                object.auditExpando=17;check(object.auditExpando===17,'expandos still work');
                delete object.auditExpando;
            }
            const descriptor=Reflect.getOwnPropertyDescriptor(location,'href');
            check(descriptor.configurable&&!Reflect.defineProperty(location,'href',descriptor),'Location defaults cannot be redefined');
            check(!Reflect.deleteProperty(location,'href'),'Location defaults cannot be removed');
            return 'same-origin-exotics-ok';
        })()"#
            ),
            "same-origin-exotics-ok",
            "{tier:?}"
        );
    }
}
