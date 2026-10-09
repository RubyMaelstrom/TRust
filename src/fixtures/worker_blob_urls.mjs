// File API (local w3c/FileAPI@1341d26) #url-model: one blob URL store for a
// page and its agents. A Window and its workers resolve each other's blob
// URLs for Worker and SharedWorker construction, importScripts(), fetch()
// and XMLHttpRequest (the page actor test covers <img>, <script>, <link>
// and <iframe>); revocation works across
// agents (#dfn-revokeObjectURL); a URL's entry is resolved when the URL is
// parsed, so a later revocation does not fail a constructed Request, an
// opened XHR or a constructed worker (URL #concept-url-parser, Fetch
// #scheme-fetch); and another partition (an opaque-origin data: worker)
// neither obtains nor shares entries (#partitioningOfBlobUrls).
globalThis.blobLog = [];
globalThis.blobDone = false;
const log = (entry) => blobLog.push(entry);
const workerSource = `
    const reply = (message) => postMessage(message);
    onmessage = async (event) => {
        const [command, arg] = event.data;
        try {
            if (command === "use") {
                const viaFetch = await (await fetch(arg.text)).text();
                const xhr = new XMLHttpRequest();
                xhr.open("GET", arg.text, false);
                xhr.send();
                importScripts(arg.script);
                const nested = await new Promise((resolve) => {
                    const worker = new Worker(arg.nested);
                    worker.onmessage = (e) => resolve(e.data);
                    worker.onerror = () => resolve("nested-error");
                });
                reply({ type: "used", fetch: viaFetch, xhr: xhr.responseText, imported: self.importedValue, nested });
            } else if (command === "make") {
                const text = URL.createObjectURL(new Blob(["from-worker"], { type: "text/plain" }));
                const worker = URL.createObjectURL(new Blob(["postMessage('worker-made-worker');"], { type: "text/javascript" }));
                reply({ type: "made", text, worker, origin: new URL(text).origin });
            } else if (command === "revoke") {
                URL.revokeObjectURL(arg);
                reply({ type: "revoked" });
            } else if (command === "fetch") {
                try { reply({ type: "fetched", result: await (await fetch(arg)).text() }); }
                catch (error) { reply({ type: "fetched", result: error.name }); }
            } else if (command === "hold") {
                reply({ type: "held", url: URL.createObjectURL(new Blob(["held"])) });
            }
        } catch (error) {
            reply({ type: "error", message: command + ": " + error.name + ": " + error.message });
        }
    };
    reply({ type: "ready" });
`;
const inbox = [], waiters = [];
const expect = (type) => new Promise((resolve) => {
    const index = inbox.findIndex((message) => message.type === type);
    if (index >= 0) resolve(inbox.splice(index, 1)[0]);
    else waiters.push({ type, resolve });
});
const deliver = (message) => {
    if (message.type === "error") log("worker-error:" + message.message);
    const index = waiters.findIndex((waiter) => waiter.type === message.type);
    if (index >= 0) waiters.splice(index, 1)[0].resolve(message);
    else inbox.push(message);
};
const text = (blob) => URL.createObjectURL(new Blob([blob], { type: "text/plain" }));
const fetchText = async (url) => {
    try { return await (await fetch(url)).text(); }
    catch (error) { return error.name; }
};
const xhrText = (url, beforeSend) => new Promise((resolve) => {
    const xhr = new XMLHttpRequest();
    xhr.open("GET", url);
    if (beforeSend) beforeSend();
    xhr.onload = () => resolve(xhr.responseText);
    xhr.onerror = () => resolve("error");
    xhr.send();
});

(async () => {
    // A worker constructed from a blob URL keeps the entry its URL parse
    // resolved, though the URL is revoked at once.
    const workerURL = URL.createObjectURL(new Blob([workerSource], { type: "text/javascript" }));
    const worker = new Worker(workerURL);
    URL.revokeObjectURL(workerURL);
    worker.onmessage = (event) => deliver(event.data);
    worker.onerror = (event) => log("worker-failure:" + event.message);
    globalThis.blobWorker = worker;
    await expect("ready");
    log("worker-from-revoked-url");

    // The worker uses the Window's URLs: fetch, XHR, importScripts, a nested worker.
    const windowText = text("from-window");
    const scriptURL = URL.createObjectURL(new Blob(["self.importedValue = 7;"], { type: "text/javascript" }));
    const nestedURL = URL.createObjectURL(new Blob(["postMessage('nested-ok');"], { type: "text/javascript" }));
    globalThis.keptURL = nestedURL;
    worker.postMessage(["use", { text: windowText, script: scriptURL, nested: nestedURL }]);
    const used = await expect("used");
    log("worker-used:" + [used.fetch, used.xhr, used.imported, used.nested].join(","));

    // The Window uses the worker's URLs.
    worker.postMessage(["make"]);
    const made = await expect("made");
    log("worker-url-origin:" + (made.origin === location.origin) + ":" + made.text.startsWith("blob:" + location.origin + "/"));
    log("window-fetch:" + await fetchText(made.text) + ":" + await xhrText(made.text));
    const fromWorkerURL = new Worker(made.worker);
    log("window-worker:" + await new Promise((resolve) => {
        fromWorkerURL.onmessage = (event) => resolve(event.data);
        fromWorkerURL.onerror = () => resolve("error");
    }));

    // Revocation crosses agents.
    URL.revokeObjectURL(made.text);
    worker.postMessage(["fetch", made.text]);
    log("worker-after-window-revoke:" + (await expect("fetched")).result);
    worker.postMessage(["revoke", windowText]);
    await expect("revoked");
    log("window-after-worker-revoke:" + await fetchText(windowText));

    // An entry is resolved when its URL is parsed.
    const raced = text("race-request");
    const request = new Request(raced);
    URL.revokeObjectURL(raced);
    const racedXHR = text("race-xhr");
    const racedFetch = text("race-fetch");
    const pending = fetch(racedFetch);
    URL.revokeObjectURL(racedFetch);
    log("races:" + await (await fetch(request)).text() + "," +
        await xhrText(racedXHR, () => URL.revokeObjectURL(racedXHR)) + "," + await (await pending).text() + "," +
        await fetchText(raced));

    // A SharedWorker from a blob URL revoked right after construction.
    const sharedURL = URL.createObjectURL(new Blob(["onconnect = (e) => e.ports[0].postMessage('shared-ok');"], { type: "text/javascript" }));
    const shared = new SharedWorker(sharedURL);
    URL.revokeObjectURL(sharedURL);
    log("shared:" + await new Promise((resolve) => {
        shared.port.onmessage = (event) => resolve(event.data);
        shared.onerror = () => resolve("error");
    }));

    // Another partition: a data: worker has an opaque origin.
    const reachable = text("partitioned");
    const opaque = new Worker("data:text/javascript," + encodeURIComponent(`
        onmessage = async (event) => {
            let result;
            try { result = await (await fetch(event.data)).text(); } catch (error) { result = error.name; }
            postMessage([result, URL.createObjectURL(new Blob(["opaque"]))]);
        };`));
    const [opaqueResult, opaqueURL] = await new Promise((resolve) => {
        opaque.onmessage = (event) => resolve(event.data);
        opaque.postMessage(reachable);
    });
    log("partition:" + opaqueResult + ":" + opaqueURL.startsWith("blob:null/") + ":" + await fetchText(opaqueURL));
    opaque.terminate();

    // A URL the worker holds; the test ends the worker and checks its removal.
    worker.postMessage(["hold"]);
    globalThis.heldURL = (await expect("held")).url;
    log("held:" + await fetchText(heldURL));
})().catch((error) => log("threw:" + error.name + ":" + error.message)).finally(() => { blobDone = true; });
