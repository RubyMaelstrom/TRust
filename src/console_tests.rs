//! WHATWG Console #logger / #formatter / #printer conformance and our
//! implementation-defined, side-effect-free object display policy.
//! Local snapshot: whatwg/console@a82403e842252f34975f84091ee694aef86dfd37.
use super::tests::{configured_engine_before_prelude, string_value};
use super::*;

fn console_engine(worker: bool) -> trust_js::Engine {
    let mut engine = configured_engine_before_prelude(
        HostState::new(
            Rc::new(RefCell::new(Dom::new())),
            Rc::new(RealmClock::new()),
        ),
        DEFAULT_URL,
    );
    let source = if worker {
        crate::js::worker_prelude()
    } else {
        crate::js::PRELUDE
    };
    eval_test_bootstrap(&mut engine, source, "console prelude", worker).unwrap();
    eval(
        &mut engine,
        if worker {
            "globalThis.consoleLines = __wkr.consoleLines"
        } else {
            "globalThis.consoleLines = __trust.logs"
        },
        "console test diagnostics",
    )
    .unwrap();
    engine
}

#[test]
fn console_printer_does_not_execute_author_object_hooks() {
    for worker in [false, true] {
        let mut engine = console_engine(worker);
        assert_eq!(
            string_value(
                &mut engine,
                r#"(() => {
                    let hooks = 0;
                    const trap = () => { hooks++; throw Error('Console executed author code'); };
                    const object = { toString: trap, valueOf: trap, [Symbol.toPrimitive]: trap };
                    Object.defineProperty(object, Symbol.toStringTag, {get: trap});
                    const proxy = new Proxy({}, {get: trap, ownKeys: trap,
                        getOwnPropertyDescriptor: trap, getPrototypeOf: trap});
                    const error = new Error('captured diagnostic');
                    for (const key of ['name', 'message', 'stack'])
                        Object.defineProperty(error, key, {get: trap});
                    for (const method of ['log', 'info', 'warn', 'error', 'debug', 'dirxml']) {
                        console[method](object, proxy, error);
                        console[method]('%o %O', object, proxy);
                    }
                    console.dir(object, {toString: trap}, object);
                    console.table(proxy);
                    console.group(object); console.groupEnd();
                    console.groupCollapsed(proxy); console.clear();
                    if (hooks !== 0) throw Error('unexpected hooks: ' + hooks);
                    if (!consoleLines[0].includes('[Object] [Object] Error: <accessor message>'))
                        throw Error('missing safe object/error display');
                    if (consoleLines[12] !== 'dir: [Object]') throw Error('dir printed options or extras');
                    return 'safe-printer';
                })()"#,
            ),
            "safe-printer",
            "worker={worker}",
        );
    }
}

#[test]
fn console_formatter_runs_only_required_conversions_in_order() {
    for worker in [false, true] {
        let mut engine = console_engine(worker);
        assert_eq!(
            string_value(
                &mut engine,
                r#"(() => {
                    const calls = [];
                    const value = (name, result) => ({toString() { calls.push(name); return result; }});
                    const spare = value('unused', 'spare');
                    console.log('%s %d %i %f', value('s', 'alpha'), value('d', '12.8'),
                        value('i', '-9px'), value('f', '3.25px'), spare);
                    console.log('%d %i %f %s', Symbol('d'), Symbol('i'), Symbol('f'), Symbol('text'));
                    console.log('%s %s', '%d', 19);
                    console.assert(false, '%s', value('assert', 'reason'));
                    console.log('%s');
                    console.log();
                    if (calls.join(',') !== 's,d,i,f,assert') throw Error('conversion order/unused object');
                    if (consoleLines.join('|') !== [
                        'log: alpha 12 -9 3.25 [Object]', 'log: NaN NaN NaN Symbol(text)',
                        'log: 19 %s', 'assert: Assertion failed: reason', 'log: %s'
                    ].join('|')) throw Error('format output: ' + consoleLines.join('|'));
                    return 'formatter';
                })()"#,
            ),
            "formatter",
            "worker={worker}",
        );
    }
}

#[test]
fn console_formatter_survives_full_buffer_and_printer_keeps_timer_data_unformatted() {
    for worker in [false, true] {
        let mut engine = console_engine(worker);
        assert_eq!(
            string_value(
                &mut engine,
                r#"(() => {
                    let conversions = 0;
                    const object = {toString() { conversions++; return 'converted'; }};
                    console.time('timer%s');
                    console.timeLog('timer%s', '%s', object);
                    if (!consoleLines[0].startsWith('timeLog: timer%s: ') ||
                        !consoleLines[0].endsWith(' ms %s [Object]') || conversions !== 0)
                        throw Error('timeLog ran Formatter');
                    for (let i = 0; i < 120; i++) console.log('filler');
                    console.log('%s', object);
                    if (consoleLines.length !== 100 || conversions !== 1)
                        throw Error('full buffer changed Logger conversions');
                    return 'buffer-and-timer';
                })()"#,
            ),
            "buffer-and-timer",
            "worker={worker}",
        );
    }
}

#[test]
fn console_printer_owns_array_indices_and_property_descriptors() {
    for worker in [false, true] {
        let mut engine = console_engine(worker);
        assert_eq!(
            string_value(
                &mut engine,
                r#"(() => {
                    const define = Object.defineProperty, create = Object.create;
                    const arrayPrototype = Array.prototype, objectPrototype = Object.prototype;
                    const originalLength = arrayPrototype.length;
                    let indices = 0, descriptors = 0, leakedArray = null;
                    const indexed = create(null);
                    indexed.configurable = true;
                    indexed.set = function() {
                        indices++; leakedArray = this; throw 'inherited index setter';
                    };
                    indexed.get = function() { indices++; throw 'inherited index getter'; };
                    const descriptor = create(null);
                    descriptor.configurable = true;
                    descriptor.get = function() { descriptors++; throw 'inherited descriptor getter'; };
                    define(arrayPrototype, '0', indexed);
                    define(arrayPrototype, '1', indexed);
                    define(arrayPrototype, '2', indexed);
                    define(objectPrototype, 'get', descriptor);
                    define(objectPrototype, 'set', descriptor);
                    try {
                        console.log('plain');
                        console.log('%s', 7, 'tail');
                        console.assert(false, {});
                        console.assert(false, '%s', 8);
                        console.assert(false);
                        console.group({}); console.groupEnd();
                        console.time('timer'); console.timeLog('timer', {}); console.timeEnd('timer');
                        console.dir({}); console.table({});
                    } finally {
                        delete objectPrototype.get; delete objectPrototype.set;
                        delete arrayPrototype[0]; delete arrayPrototype[1]; delete arrayPrototype[2];
                        arrayPrototype.length = originalLength;
                    }
                    if (indices !== 0 || descriptors !== 0 || leakedArray !== null)
                        throw Error('console exposed internal arrays/descriptors');
                    if (consoleLines.length !== 10 || consoleLines[0] !== 'log: plain' ||
                        consoleLines[1] !== 'log: 7 tail' || consoleLines[2] !== 'assert: Assertion failed [Object]' ||
                        consoleLines[3] !== 'assert: Assertion failed: 8' || consoleLines[4] !== 'assert: Assertion failed')
                        throw Error('console lost output during inherited hooks');
                    return 'own-console-storage';
                })()"#,
            ),
            "own-console-storage",
            "worker={worker}",
        );
    }
}
