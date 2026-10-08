// Streams #transferrable-streams across agents: a Window transfers a
// ReadableStream, a WritableStream and a TransformStream to a dedicated
// worker, which reads, writes, and transfers a stream of its own, an errored
// stream and the received TransformStream (a second hop) back.
globalThis.streamLog = [];
const workerSource = `
    onmessage = async (event) => {
        const { readable, writable, transform } = event.data;
        const reader = readable.getReader(), got = [];
        for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            got.push(value);
        }
        const writer = writable.getWriter();
        await writer.write("from-worker");
        await writer.write(new Uint8Array([7, 8]));
        await writer.close();
        const answer = new ReadableStream({ start(controller) {
            controller.enqueue(got.join("+"));
            controller.enqueue(self instanceof DedicatedWorkerGlobalScope);
            controller.close();
        } });
        const failing = new ReadableStream({ start(controller) { controller.error(new RangeError("worker failure")); } });
        postMessage({ answer, failing, transform }, [answer, failing, transform]);
    };
`;
const worker = new Worker(URL.createObjectURL(new Blob([workerSource], { type: "text/javascript" })));
const sinkChunks = [];
let sinkClosed;
const sinkDone = new Promise((resolve) => { sinkClosed = resolve; });
const readable = new ReadableStream({ start(controller) { controller.enqueue("one"); controller.enqueue("two"); controller.close(); } });
const writable = new WritableStream({
    write(chunk) { sinkChunks.push(chunk instanceof Uint8Array ? "bytes:" + Array.from(chunk).join(":") : chunk); },
    close() { sinkClosed(); },
});
const transform = new TransformStream({ transform(chunk, controller) { controller.enqueue(`${chunk}`.toUpperCase()); } });
worker.onerror = (event) => streamLog.push("error:" + event.message);
worker.onmessage = async (event) => {
    try {
        const { answer, failing, transform: back } = event.data;
        const values = [], answerReader = answer.getReader();
        for (;;) {
            const { value, done } = await answerReader.read();
            if (done) break;
            values.push(`${value}`);
        }
        streamLog.push("answer:" + values.join(","));
        try {
            await failing.getReader().read();
            streamLog.push("failing:resolved");
        } catch (error) {
            streamLog.push("failing:" + error.name + ":" + error.message + ":" + (error instanceof RangeError));
        }
        const backWriter = back.writable.getWriter();
        backWriter.write("round");
        backWriter.write("trip");
        backWriter.close();
        const backReader = back.readable.getReader(), transformed = [];
        for (;;) {
            const { value, done } = await backReader.read();
            if (done) break;
            transformed.push(value);
        }
        streamLog.push("transform:" + transformed.join(","));
        await sinkDone;
        streamLog.push("sink:" + sinkChunks.join(","));
        streamLog.push("done");
    } catch (error) {
        streamLog.push("threw:" + error.name + ":" + error.message);
    }
};
worker.postMessage({ readable, writable, transform }, [readable, writable, transform]);
streamLog.push("locked:" + [readable.locked, writable.locked, transform.readable.locked, transform.writable.locked].join());
globalThis.streamWorker = worker;
