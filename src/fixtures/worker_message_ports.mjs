// Original conformance fixture: HTML #transferMessagePort, #message-port-post-message-steps,
// #dom-worker-postmessage and #structuredserializewithtransfer (2026-09-06 snapshot).
globalThis.portChecks = [];
globalThis.portFailures = [];
function check(value, label) { if (!value) portFailures.push(label); }
const workerSource = `
    const channel = new MessageChannel();
    channel.port1.onmessage = event => {
        event.ports[0].postMessage('nested');
        event.ports[0].close();
    };
    postMessage({kind:'created', port:channel.port2}, [channel.port2]);
    onmessage = async event => {
        if (event.data.kind === 'bitmap') {
            const input = event.data.bitmap;
            const valid = input instanceof ImageBitmap && input.width === 1;
            const output = await createImageBitmap(input, {resizeWidth:2, resizeQuality:'pixelated'});
            input.close();
            postMessage({kind:'bitmap', valid, bitmap:output}, [output]);
            postMessage({kind:'bitmap-detached', valid:output.width === 0});
            return;
        }
        const port = event.data.port;
        postMessage({kind:'attached', valid:
            port instanceof MessagePort && port instanceof EventTarget &&
            event.ports.length === 2 && event.ports[0] === port &&
            event.ports[1] instanceof MessagePort && Object.isFrozen(event.ports) &&
            event.isTrusted && event.origin === '' && event.source === null,
            bytes:event.data.bytes}, {transfer:[event.data.bytes]});
        let started = false;
        const onmessage = e => {
            if (!started) { port.postMessage('BAD:port started early'); return; }
            if (e.data === 'return') {
                postMessage({kind:'returned', port}, [port]);
                let detached = false;
                try { postMessage(port, [port]); } catch (error) { detached = error.name === 'DataCloneError'; }
                postMessage({kind:'detached', valid:detached});
            } else {
                port.postMessage(e.data);
                Promise.resolve().then(() => port.postMessage('micro:' + e.data));
            }
        };
        port.addEventListener('message', onmessage);
        // A transferred port's queue stays disabled until explicitly started.
        setTimeout(() => { started = true; postMessage({kind:'starting'}); port.start(); }, 0);
    };
`;
const worker = new Worker(URL.createObjectURL(new Blob([workerSource], {type:'text/javascript'})));
globalThis.portWorker = worker;
worker.onerror = event => portFailures.push('worker: ' + event.message);
createImageBitmap(new ImageData(new Uint8ClampedArray([255,0,0,255]),1,1)).then(bitmap => {
    worker.postMessage({kind:'bitmap', bitmap}, [bitmap]);
    check(bitmap.width === 0, 'bitmap detached on worker send');
});
const channel = new MessageChannel();
const extra = new MessageChannel();
const bytes = new Uint8Array([3, 5, 8]).buffer;
let getterError = false;
try {
    worker.postMessage({get bad() { throw new Error('getter'); }}, [channel.port2, bytes]);
} catch (error) { getterError = error.message === 'getter'; }
check(getterError && bytes.byteLength === 3, 'serialization failure preserves transfers');
for (const transfer of [[channel.port2, channel.port2], [bytes, bytes], [{}]]) {
    let rejected = false;
    try { worker.postMessage(null, transfer); } catch (error) { rejected = error.name === 'DataCloneError'; }
    check(rejected, 'invalid transfer is DataCloneError');
}
let iteratorReads = 0;
const transfers = {
    get [Symbol.iterator]() {
        iteratorReads++;
        return function* () { yield channel.port2; yield bytes; yield extra.port2; };
    }
};
channel.port1.postMessage('first');
channel.port1.postMessage('second');
worker.postMessage({port:channel.port2, bytes}, transfers);
check(iteratorReads === 1 && bytes.byteLength === 0, 'overload conversion and buffer detachment');
let detached = false;
try { structuredClone(channel.port2, {transfer:[channel.port2]}); }
catch (error) { detached = error.name === 'DataCloneError'; }
check(detached, 'sender port detached');
channel.port1.onmessage = event => {
    check(!event.data.startsWith('BAD:'), 'receiving port starts explicitly');
    check(event.isTrusted && event.target === channel.port1, 'port event target/trust');
    portChecks.push(event.data);
    if (event.data === 'micro:second') channel.port1.postMessage('return');
};
worker.onmessage = event => {
    const data = event.data;
    if (data.kind === 'created') {
        const nested = new MessageChannel();
        nested.port1.onmessage = event => portChecks.push(event.data);
        check(data.port === event.ports[0], 'worker-created port identity');
        data.port.postMessage(null, {transfer:[nested.port2]});
    } else if (data.kind === 'attached') {
        check(data.valid && new Uint8Array(data.bytes).join() === '3,5,8', 'worker receive and return buffer');
        check(event.ports.length === 0, 'ArrayBuffer excluded from event.ports');
        portChecks.push('attached');
    } else if (data.kind === 'returned') {
        check(data.port === event.ports[0], 'returned endpoint identity');
        data.port.onmessage = event => portChecks.push('return:' + event.data);
        channel.port1.postMessage('again');
    } else if (data.kind === 'detached') {
        check(data.valid, 'worker sender detached');
        portChecks.push('detached');
    } else if (data.kind === 'bitmap') {
        check(data.valid && data.bitmap instanceof ImageBitmap && data.bitmap.width === 2, 'worker bitmap round trip');
        const canvas = document.createElement('canvas'), context = canvas.getContext('2d');
        context.drawImage(data.bitmap,0,0);
        check(Array.from(context.getImageData(0,0,1,1).data).join() === '255,0,0,255', 'worker bitmap pixels');
        check(event.ports.length === 0, 'ImageBitmap excluded from event.ports');
        data.bitmap.close(); portChecks.push('bitmap');
    } else if (data.kind === 'bitmap-detached') {
        check(data.valid, 'bitmap detached on worker return');
        portChecks.push('bitmap-detached');
    } else {
        portChecks.push(data.kind);
    }
};
