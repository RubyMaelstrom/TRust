// HTML #dedicated-workers-and-the-worker-interface (local whatwg/html@e5071a2):
// Worker is [Exposed=(Window,DedicatedWorker,SharedWorker)], SharedWorker is
// [Exposed=Window]. A dedicated worker creates classic and module workers it
// owns; nested classic workers can importScripts(); an unhandled nested error
// reaches the nested Worker object, then the owner's global, then the page;
// and #the-worker's-lifetime closes nested workers when their owner ends.
globalThis.nestedLog = [];
globalThis.nestedTicks = 0;
const ticks = new BroadcastChannel("nested-worker-ticks");
ticks.onmessage = () => { nestedTicks++; };
globalThis.nestedTickChannel = ticks;
const outerSource = `
    const check = (condition, message) => { if (!condition) postMessage("FAIL " + message); };
    check(typeof Worker === "function" && Worker.length === 1, "Worker in DedicatedWorkerGlobalScope");
    check(Object.getPrototypeOf(Worker) === EventTarget, "Worker inherits EventTarget");
    check(Object.prototype.toString.call(Worker.prototype) === "[object Worker]", "Worker tag");
    check(typeof SharedWorker === "undefined", "SharedWorker is [Exposed=Window]");
    try { new Worker("https://[bad"); check(false, "invalid URL accepted"); }
    catch (error) { check(error.name === "SyntaxError", "invalid URL " + error.name); }
    try { new Worker("inner.js", { type: "shared" }); check(false, "invalid type accepted"); }
    catch (error) { check(error.name === "TypeError", "invalid type " + error.name); }
    try { Worker.prototype.postMessage.call({}, 1); check(false, "postMessage brand"); }
    catch (error) { check(error.name === "TypeError", "postMessage brand " + error.name); }
    const classicSource = \`
        const helper = URL.createObjectURL(new Blob(["self.helperValue = 40 + 2;"], { type: "text/javascript" }));
        importScripts(helper);
        const ticks = new BroadcastChannel("nested-worker-ticks");
        setInterval(() => ticks.postMessage("tick"), 5);
        onmessage = (event) => postMessage([event.data, self.helperValue, self instanceof DedicatedWorkerGlobalScope,
            self.name, typeof importScripts, location.protocol, self.origin].join());
    \`;
    const classic = new Worker(URL.createObjectURL(new Blob([classicSource], { type: "text/javascript" })), { name: "inner" });
    const moduleSource = \`
        let importResult;
        try { importScripts(); importResult = "allowed"; } catch (error) { importResult = error.name; }
        onmessage = (event) => postMessage("module:" + event.data + ":" + importResult + ":" + (typeof Worker));
    \`;
    const module = new Worker(URL.createObjectURL(new Blob([moduleSource], { type: "text/javascript" })), { type: "module" });
    const failing = new Worker(URL.createObjectURL(new Blob(['throw new Error("nested failure");'], { type: "text/javascript" })));
    failing.onerror = (event) => postMessage("nested-error:" + event.message.includes("nested failure") + ":" +
        (event instanceof ErrorEvent) + ":" + event.isTrusted + ":" + (event.error === null) + ":" + event.cancelable);
    classic.onmessage = (event) => postMessage("classic:" + event.data);
    module.onmessage = (event) => postMessage(event.data);
    onmessage = (event) => {
        classic.postMessage(event.data);
        module.postMessage(event.data);
    };
    postMessage("ready");
`;
const outer = new Worker(URL.createObjectURL(new Blob([outerSource], { type: "text/javascript" })), { name: "outer" });
outer.onmessage = (event) => {
    nestedLog.push(event.data);
    if (event.data === "ready") outer.postMessage("ping");
};
outer.onerror = (event) => {
    nestedLog.push("outer-error:" + event.message.includes("nested failure"));
    event.preventDefault();
};
globalThis.nestedOuter = outer;
