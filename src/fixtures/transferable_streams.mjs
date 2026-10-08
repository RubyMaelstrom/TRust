// Streams #transferrable-streams (local whatwg/streams@b9ba9f4) with HTML
// #structuredserializewithtransfer and #structureddeserializewithtransfer
// (local whatwg/html@e5071a2), within one realm: structuredClone() and a
// MessageChannel. Runs in a Window and in a dedicated worker.
const collect = async (stream) => {
    const reader = stream.getReader(), chunks = [];
    for (;;) {
        const { value, done } = await reader.read();
        if (done) return chunks;
        chunks.push(value);
    }
};

// #rs-transfer: the original is piped into a cross-realm writable, so it is
// locked; the transferred stream carries every chunk, then closes.
let pulls = 0;
const source = new ReadableStream({
    start(controller) { controller.enqueue("a"); controller.enqueue({ b: 2 }); },
    pull(controller) {
        if (++pulls === 1) { controller.enqueue(new Uint8Array([1, 2])); controller.close(); }
    },
});
const graph = structuredClone({ stream: source, again: source }, { transfer: [source] });
const moved = graph.stream;
check(moved instanceof ReadableStream && moved !== source, "a transferred ReadableStream is a new stream");
check(graph.again === moved, "graph references resolve to the transferred stream");
check(source.locked, "the transferred original is locked by its pipe");
throws(() => structuredClone(source, { transfer: [source] }), "DataCloneError", "a detached ReadableStream");
const chunks = await collect(moved);
check(chunks.length === 3 && chunks[0] === "a" && chunks[1].b === 2 &&
    chunks[2] instanceof Uint8Array && chunks[2].join() === "1,2", "readable chunks " + JSON.stringify(chunks));

const locked = new ReadableStream();
locked.getReader();
throws(() => structuredClone(locked, { transfer: [locked] }), "DataCloneError", "a locked ReadableStream");
throws(() => structuredClone(new ReadableStream()), "DataCloneError", "an untransferred ReadableStream");
throws(() => structuredClone({ writer: new WritableStream().getWriter() }), "DataCloneError", "a stream writer");
throws(() => structuredClone(new CountQueuingStrategy({ highWaterMark: 1 })), "DataCloneError", "a queuing strategy");

// #ws-transfer: writes through the transferred writable reach the original
// sink in order, followed by its close.
const written = [];
let sinkClosed;
const sinkDone = new Promise((resolve) => { sinkClosed = resolve; });
const sink = new WritableStream({ write(chunk) { written.push(chunk); }, close() { sinkClosed(); } });
const sinkCopy = structuredClone(sink, { transfer: [sink] });
check(sinkCopy instanceof WritableStream && sink.locked, "WritableStream transfer locks the original");
throws(() => structuredClone(sink, { transfer: [sink] }), "DataCloneError", "a detached WritableStream");
const writer = sinkCopy.getWriter();
await writer.write("x");
await writer.write(["y"]);
await writer.close();
await sinkDone;
check(written.length === 2 && written[0] === "x" && written[1][0] === "y", "writable chunks " + JSON.stringify(written));
const lockedSink = new WritableStream();
lockedSink.getWriter();
throws(() => structuredClone(lockedSink, { transfer: [lockedSink] }), "DataCloneError", "a locked WritableStream");

// #ts-transfer: both sides are transferred; the transformer stays here.
const upper = new TransformStream({ transform(chunk, controller) { controller.enqueue(`${chunk}`.toUpperCase()); } });
const transform = structuredClone(upper, { transfer: [upper] });
check(transform instanceof TransformStream && transform !== upper, "a transferred TransformStream");
check(transform.readable instanceof ReadableStream && transform.writable instanceof WritableStream, "transferred sides");
check(upper.readable.locked && upper.writable.locked, "TransformStream transfer locks both original sides");
throws(() => structuredClone(upper, { transfer: [upper] }), "DataCloneError", "a detached TransformStream");
const transformWriter = transform.writable.getWriter();
transformWriter.write("hi");
transformWriter.write("there");
transformWriter.close();
check((await collect(transform.readable)).join() === "HI,THERE", "transferred TransformStream output");
const halfLocked = new TransformStream();
halfLocked.readable.getReader();
throws(() => structuredClone(halfLocked, { transfer: [halfLocked] }), "DataCloneError", "a TransformStream with a locked side");

// Error propagation (#set-up-cross-realm-transform-writable abort algorithm):
// a source error reaches the transferred reader, cloned.
let failingController;
const failing = new ReadableStream({ start(controller) { failingController = controller; controller.enqueue(1); } });
const failingCopy = structuredClone(failing, { transfer: [failing] });
const failingReader = failingCopy.getReader();
check((await failingReader.read()).value === 1, "the chunk before the error");
failingController.error(new TypeError("boom"));
await failingReader.read().then(() => fails.push("a source error did not propagate"), (error) => {
    check(error instanceof TypeError && error.message === "boom", "propagated error " + error);
});

// Cancel propagation: the transferred reader's cancel reason reaches the
// original source through the pipe.
let cancelReason, cancelled;
const cancelDone = new Promise((resolve) => { cancelled = resolve; });
const cancellable = new ReadableStream({ cancel(reason) { cancelReason = reason; cancelled(); } });
await structuredClone(cancellable, { transfer: [cancellable] }).cancel("stop");
await cancelDone;
check(cancelReason === "stop", "cancel reason " + cancelReason);

// Abort propagation: the transferred writer's abort reason reaches the sink.
let abortReason, aborted;
const abortDone = new Promise((resolve) => { aborted = resolve; });
const abortable = new WritableStream({ abort(reason) { abortReason = reason; aborted(); } });
await structuredClone(abortable, { transfer: [abortable] }).abort("halt");
await abortDone;
check(abortReason === "halt", "abort reason " + abortReason);

// PackAndPostMessageHandlingError: an uncloneable chunk rejects the write
// with DataCloneError and errors the transferred stream.
const unclonable = new WritableStream();
const unclonableWriter = structuredClone(unclonable, { transfer: [unclonable] }).getWriter();
await rejects(unclonableWriter.write(() => 1), "DataCloneError", "an uncloneable chunk");
await rejects(unclonableWriter.closed, "DataCloneError", "the writer after an uncloneable chunk");

// MessagePort post message steps transfer a stream inside a message.
const channel = new MessageChannel();
const delivered = new Promise((resolve) => { channel.port2.onmessage = (event) => resolve(event.data); });
const portSource = new ReadableStream({ start(controller) { controller.enqueue("port"); controller.close(); } });
channel.port1.postMessage({ stream: portSource }, [portSource]);
const message = await delivered;
check(message.stream instanceof ReadableStream, "a stream transferred through a MessagePort");
check((await collect(message.stream)).join() === "port", "MessagePort stream chunks");
throws(() => channel.port1.postMessage(portSource, [portSource]), "DataCloneError", "posting a detached stream");
channel.port1.close();
