(function () {
    function check(ok,message){if(!ok)throw Error(message);}
    function throwsRealm(C,fn){try{fn();}catch(e){check(e instanceof C,'wrong exception Realm');return;}throw Error('missing exception');}
    const html=document.createElement('html'),body=document.createElement('body');document.appendChild(html);html.appendChild(body);
    const iframe=document.createElement('iframe');body.appendChild(iframe);const child=iframe.contentWindow;
    performance.clearMarks();child.performance.clearMarks();
    const entry=Performance.prototype.mark.call(child.performance,'foreign',{startTime:12,detail:{a:1}});
    check(performance.getEntriesByType('mark').length===0 && child.performance.getEntriesByType('mark')[0]===entry,'receiver owns the timeline');
    check(entry instanceof child.PerformanceMark && !(entry instanceof PerformanceMark),'return object uses receiver Realm');
    check(Object.getPrototypeOf(entry.detail)===child.Object.prototype,'detail uses receiver Realm');
    const getter=Object.getOwnPropertyDescriptor(child.PerformanceEntry.prototype,'startTime').get;
    check(getter.call(entry)===12,'foreign entry getter');
    throwsRealm(child.TypeError,()=>getter.call(new Proxy(entry,{})));
    throwsRealm(child.TypeError,()=>child.Performance.prototype.mark.call({},'bad'));
    const ctor=new child.PerformanceMark('constructor',{startTime:4});
    check(ctor instanceof child.PerformanceMark && child.performance.getEntriesByType('mark').length===1,'foreign constructor does not queue');
    const inherited=Object.create(PerformanceMark.prototype);
    throwsRealm(TypeError,()=>Object.getOwnPropertyDescriptor(PerformanceEntry.prototype,'name').get.call(inherited));
    const get=WeakMap.prototype.get,set=WeakMap.prototype.set;
    try {
        WeakMap.prototype.get=()=>{throw Error('author WeakMap getter');};
        WeakMap.prototype.set=()=>{throw Error('author WeakMap setter');};
        const second=performance.mark('intrinsics',{startTime:3});
        check(second.startTime===3 && performance.getEntriesByName('intrinsics')[0]===second,'captured private slot intrinsics');
    } finally {WeakMap.prototype.get=get;WeakMap.prototype.set=set;}
    check(typeof __performance_binding==='undefined' && child.eval('typeof __performance_binding')==='undefined','native identity binding consumed');
    performance.clearMarks();child.performance.clearMarks();iframe.remove();
    return 'user-timing-realms-ok';
})();
