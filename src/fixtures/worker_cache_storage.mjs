// Service Workers §5 Cache Storage (local w3c/ServiceWorker@92aba3b) across
// agents: a Window and a secure dedicated worker of one origin share the
// origin's name to cache map and request response lists
// (#cachestorage-interface), Batch Cache Operations stay atomic when both
// agents write one list at once (#batch-cache-operations-algorithm), and a
// deleted cache remains usable through an existing Cache object.
globalThis.cacheLog = [];
globalThis.cacheDone = false;
const log = (entry) => cacheLog.push(entry);
const workerSource = `
    onmessage = async (event) => {
        try {
            if (event.data === "read") {
                const cache = await caches.open("shared");
                const hit = await cache.match("https://example.com/from-page");
                postMessage({ type: "read", text: hit ? await hit.text() : "missing", secure: isSecureContext,
                    tags: [caches, cache].map((value) => Object.prototype.toString.call(value)).join("+") });
            } else if (event.data === "burst") {
                const cache = await caches.open("shared");
                await Promise.all(Array.from({ length: 20 }, (_, index) =>
                    cache.put("https://example.com/worker/" + index, new Response("w" + index))));
                await cache.put("https://example.com/from-worker", new Response("worker-body"));
                postMessage({ type: "burst" });
            } else if (event.data === "delete") {
                postMessage({ type: "deleted", result: await caches.delete("shared") });
            }
        } catch (error) {
            postMessage({ type: "error", message: error.name + ":" + error.message });
        }
    };
    postMessage({ type: "ready" });
`;
const worker = new Worker(URL.createObjectURL(new Blob([workerSource], { type: "text/javascript" })));
const inbox = [], waiters = [];
const expect = (type) => new Promise((resolve) => {
    const index = inbox.findIndex((message) => message.type === type);
    if (index >= 0) resolve(inbox.splice(index, 1)[0]);
    else waiters.push({ type, resolve });
});
worker.onmessage = (event) => {
    const message = event.data;
    if (message.type === "error") log("worker-error:" + message.message);
    const index = waiters.findIndex((waiter) => waiter.type === message.type);
    if (index >= 0) waiters.splice(index, 1)[0].resolve(message);
    else inbox.push(message);
};
worker.onerror = (event) => log("worker-failure:" + event.message);
globalThis.cacheWorker = worker;

(async () => {
    await expect("ready");
    const cache = await caches.open("shared");
    await cache.put("https://example.com/from-page", new Response("page-body"));
    worker.postMessage("read");
    const read = await expect("read");
    log("worker-read:" + read.text + ":" + read.secure + ":" + read.tags);
    worker.postMessage("burst");
    await Promise.all(Array.from({ length: 20 }, (_, index) =>
        cache.put("https://example.com/page/" + index, new Response("p" + index))));
    await expect("burst");
    log("keys:" + (await cache.keys()).length);
    const fromWorker = await caches.match("https://example.com/from-worker");
    log("page-read:" + (fromWorker ? await fromWorker.text() : "missing"));
    log("names:" + (await caches.keys()).join());
    worker.postMessage("delete");
    log("worker-deleted:" + (await expect("deleted")).result);
    log("has-after:" + await caches.has("shared"));
    log("retained:" + (await cache.keys()).length);
    worker.terminate();
})().catch((error) => log("threw:" + error.name + ":" + error.message)).finally(() => { cacheDone = true; });
