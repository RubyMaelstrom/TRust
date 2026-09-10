(() => {
    function check(value, message) { if (!value) throw new Error(message); }
    const c = console, logs = __trust.logs;
    logs.length = 0;
    for (const name of ['log','info','warn','error','debug','trace','dir','dirxml',
        'clear','group','groupCollapsed','groupEnd','table','time','timeLog','timeEnd',
        'count','countReset','assert']) {
        check(typeof c[name] === 'function', name + ' is callable');
        check(c[name].length === 0, name + ' optional arguments');
    }
    const proto = Object.getPrototypeOf(c);
    check(proto !== Object.prototype && Object.getPrototypeOf(proto) === Object.prototype &&
        Reflect.ownKeys(proto).length === 0, 'console namespace prototype');
    const collapsed = c.groupCollapsed, end = c.groupEnd;
    check(collapsed('outer') === undefined, 'unbound groupCollapsed returns undefined');
    c.log('one'); c.group('inner'); c.log('two'); end(); c.log('three');
    c.clear(); c.log('four'); end(); c.log('five');
    check(logs.join('|') === 'groupCollapsed: outer|log:   one|group:   inner|log:     two|log:   three|log: four|log: five', 'group nesting, clear, underflow');
    logs.length = 0;
    c.count(); c.count(undefined); c.countReset(); c.count(); c.count(null);
    c.countReset('missing');
    check(logs.slice(0,5).join('|') === 'count: default: 1|count: default: 2|count: default: 1|count: null: 1|countReset: Count for \'missing\' does not exist', 'per-label counts');
    for (const name of ['count','countReset','time','timeLog','timeEnd']) {
        let threw=false;
        try { c[name](Symbol()); } catch(e) { threw=e instanceof TypeError; }
        check(threw, name + ' DOMString conversion rejects Symbols');
    }
    logs.length = 0;
    check(c.time('elapsed') === undefined, 'time returns undefined');
    c.timeLog('elapsed','midpoint'); c.time('elapsed'); c.timeEnd('elapsed'); c.timeEnd('elapsed');
    check(/^timeLog: elapsed: [0-9.]+ ms midpoint$/.test(logs[0]), 'timeLog data');
    check(logs[1] === "warn: Timer 'elapsed' already exists", 'duplicate timer');
    check(/^timeEnd: elapsed: [0-9.]+ ms$/.test(logs[2]), 'timeEnd data');
    check(logs[3] === "warn: Timer 'elapsed' does not exist", 'timeEnd removes timer');
    logs.length = 0;
    c.assert(true,'not logged'); c.assert(); c.assert(false,'detail'); c.assert(false,3);
    check(logs.join('|') === 'assert: Assertion failed|assert: Assertion failed: detail|assert: Assertion failed 3', 'assert logging');
    c.clear(); c.countReset('default');
    const html = document.createElement('html'), body = document.createElement('body');
    document.appendChild(html); html.appendChild(body);
    const frame = document.createElement('iframe'); body.appendChild(frame);
    const child=frame.contentWindow;
    c.count(); child.console.count(); child.console.groupCollapsed('child'); c.log('parent');
    check(logs.at(-1) === 'log: parent', 'child grouping does not affect parent');
    check(child.__trust.logs.includes('count: default: 1'), 'independent child counter');
    frame.remove();
    return 'console-state-ok';
})()
