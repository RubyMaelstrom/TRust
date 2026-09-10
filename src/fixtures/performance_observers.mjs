// Original observer ordering, independent buffering, Web IDL and lifetime tests.
(function () {
    function check(ok,message) { if(!ok) throw Error(message); }
    function throws(name,fn) { try { fn(); } catch(error) { check(error.name===name,'wrong exception '+error.name);return; }throw Error('missing '+name); }
    performance.clearMarks();performance.clearMeasures();
    check(Object.isFrozen(PerformanceObserver.supportedEntryTypes) &&
        PerformanceObserver.supportedEntryTypes===PerformanceObserver.supportedEntryTypes &&
        PerformanceObserver.supportedEntryTypes.join(',')==='mark,measure,navigation,resource','truthful stable supported types');
    throws('TypeError',()=>new PerformanceObserver());
    throws('TypeError',()=>new PerformanceObserver({handleEvent(){}}));
    throws('TypeError',()=>new PerformanceObserverEntryList());
    const empty=new PerformanceObserver(()=>{});
    throws('TypeError',()=>empty.observe());
    throws('TypeError',()=>empty.observe({entryTypes:['mark'],buffered:false}));
    throws('TypeError',()=>empty.observe({entryTypes:'mark'}));
    throws('TypeError',()=>empty.observe({type:Symbol()}));
    empty.observe({type:'unsupported'});
    throws('InvalidModificationError',()=>empty.observe({entryTypes:['mark']}));
    empty.disconnect();
    throws('InvalidModificationError',()=>empty.observe({entryTypes:['mark']}));
    const dictionaryOrder=[];
    empty.observe({get buffered(){dictionaryOrder.push('buffered');return false;},
        get entryTypes(){dictionaryOrder.push('entryTypes');return undefined;},
        get type(){dictionaryOrder.push('type');return 'unsupported';}});
    check(dictionaryOrder.join(',')==='buffered,entryTypes,type','observer dictionary ordering');
    const prior=performance.mark('prior',{startTime:5});
    const records=new PerformanceObserver(()=>{throw Error('drained records must not deliver');});
    records.observe({type:'mark',buffered:true});
    const later=performance.mark('later',{startTime:1});
    performance.clearMarks();
    const drained=records.takeRecords();
    check(drained.length===2 && drained[0]===prior && drained[1]===later,'takeRecords preserves queue order and clearMarks independence');
    check(records.takeRecords().length===0,'takeRecords drains');records.disconnect();
    const disconnected=new PerformanceObserver(()=>{throw Error('disconnected callback');});
    disconnected.observe({type:'mark'});performance.mark('discard');disconnected.disconnect();
    check(disconnected.takeRecords().length===0,'disconnect empties records');
    disconnected.observe({type:'mark'});check(disconnected.takeRecords().length===0,'disconnect does not automatically replay history');disconnected.disconnect();
    performance.clearMarks();
    globalThis.performanceObserverResult=['sync'];
    let calls=0, retained;
    const first=new PerformanceObserver(function(list,self,options){
        check(this===first && self===first && list instanceof PerformanceObserverEntryList,'callback binding and list interface');
        performanceObserverResult.push('first');calls++;
        const entries=list.getEntries();
        if(calls===1) {
            check(options.droppedEntriesCount===0,'first callback dropped count');
            check(entries.length===2 && entries[0].name==='earlier' && entries[1].name==='later','list is chronologically sorted');
            check(list.getEntriesByName('later','mark').length===1 && list.getEntriesByType('measure').length===0,'entry-list filtering');
            entries.length=0;check(list.getEntries().length===2,'entry-list result copy');
            retained=list;
            performance.mark('inside',{startTime:9});
        } else {
            check(!Object.hasOwn(options,'droppedEntriesCount'),'later callback omitted dropped count');
            check(entries.length===1 && entries[0].name==='inside','new entries form a later delivery');
            first.disconnect();
        }
    });
    const second=new PerformanceObserver(function(list){
        performanceObserverResult.push('second');
        check(list.getEntries().length===3,'each observer buffer is copied when its callback is reached');
        second.disconnect();
    });
    first.observe({entryTypes:['mark','unsupported']});second.observe({type:'mark'});
    performance.mark('later',{startTime:6});performance.mark('earlier',{startTime:2});
    // Performance tasks are not author timer ids; neither replacing timer APIs
    // nor cancellation may suppress them.
    const oldTimeout=setTimeout,oldClear=clearTimeout;
    for(let i=0;i<16;i++)clearTimeout(i);
    globalThis.setTimeout=()=>{throw Error('observer used author timer');};
    Promise.resolve().then(()=>performanceObserverResult.push('microtask'));
    globalThis.performanceObserverCleanup=function(){
        globalThis.setTimeout=oldTimeout;globalThis.clearTimeout=oldClear;
        check(calls===2 && retained.getEntries().length===2,'retained callback list remains a snapshot');
        first.disconnect();second.disconnect();performance.clearMarks();performance.clearMeasures();
        check(!__trust.hasPerformanceTask(),'notification queue drains');
        delete globalThis.performanceObserverCleanup;
    };
})();
