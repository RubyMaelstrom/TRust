// Indexed Database 3 across agents (local w3c/IndexedDB@f704918): a Window and
// a dedicated worker of one origin share its databases (one storage key).
// versionchange and blocked cross agents (#opening, #deleting-a-database),
// transactions are scheduled in creation order across agents
// (#transaction-scheduling), concurrent read/write transactions with disjoint
// scopes both commit, and terminating a worker closes its connections.
globalThis.idbLog = [];
globalThis.idbDone = false;
const DB = "cross-agent";
const log = (entry) => idbLog.push(entry);
const promised = (request) => new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
});
const done = (transaction) => new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onabort = () => reject(transaction.error);
});
const workerSource = `
    const DB = ${JSON.stringify(DB)};
    let connection = null;
    onmessage = (event) => {
        const command = event.data;
        if (command === "upgrade") {
            const request = indexedDB.open(DB, 2);
            let pageValue;
            request.onblocked = (e) => postMessage({ type: "blocked", old: e.oldVersion, new: e.newVersion });
            request.onupgradeneeded = (e) => {
                request.result.createObjectStore("worker").put("upgrade", "w");
                request.transaction.objectStore("items").get("page").onsuccess = (g) => { pageValue = g.target.result; };
            };
            request.onsuccess = () => {
                connection = request.result;
                connection.onversionchange = (e) => {
                    postMessage({ type: "worker-versionchange", old: e.oldVersion, new: e.newVersion });
                    connection.close();
                };
                postMessage({ type: "upgraded", version: connection.version,
                    names: Array.from(connection.objectStoreNames).join("+"), pageValue });
            };
            request.onerror = () => postMessage({ type: "error", message: "upgrade " + request.error.name });
        } else if (command === "read-counter") {
            const transaction = connection.transaction("items", "readwrite");
            const get = transaction.objectStore("items").get("counter");
            get.onsuccess = () => postMessage({ type: "counter", value: get.result });
            postMessage({ type: "created" });
        } else if (command === "fill") {
            const transaction = connection.transaction("worker", "readwrite");
            for (let i = 0; i < 20; i++) transaction.objectStore("worker").put("value" + i, "w" + i);
            transaction.oncomplete = () => postMessage({ type: "filled" });
            transaction.onabort = () => postMessage({ type: "error", message: "fill " + transaction.error });
        } else if (command === "hold") {
            const request = indexedDB.open(DB, 1);
            request.onupgradeneeded = () => request.result.createObjectStore("held");
            request.onsuccess = () => { self.held = request.result; postMessage({ type: "holding", version: request.result.version }); };
            request.onerror = () => postMessage({ type: "error", message: "hold " + request.error.name });
        }
    };
    postMessage({ type: "ready" });
`;
const worker = new Worker(URL.createObjectURL(new Blob([workerSource], { type: "text/javascript" })));
const inbox = [], waiters = [];
const deliver = (message) => {
    const index = waiters.findIndex((waiter) => waiter.type === message.type);
    if (index >= 0) waiters.splice(index, 1)[0].resolve(message);
    else inbox.push(message);
};
const expect = (type) => new Promise((resolve) => {
    const index = inbox.findIndex((message) => message.type === type);
    if (index >= 0) resolve(inbox.splice(index, 1)[0]);
    else waiters.push({ type, resolve });
});
const onBlocked = [];
worker.onmessage = (event) => {
    const message = event.data;
    if (message.type === "error") log("worker-error:" + message.message);
    if (message.type === "blocked") { log("worker-blocked:" + message.old + ">" + message.new); onBlocked.splice(0).forEach((fn) => fn()); }
    if (message.type === "worker-versionchange") log("worker-versionchange:" + message.old + ">" + message.new);
    deliver(message);
};
worker.onerror = (event) => log("worker-failure:" + event.message);
globalThis.idbWorker = worker;

(async () => {
    await expect("ready");
    await promised(indexedDB.deleteDatabase(DB));
    const opening = indexedDB.open(DB, 1);
    opening.onupgradeneeded = () => opening.result.createObjectStore("items").put("v1", "page");
    const first = await promised(opening);
    first.onversionchange = (event) => { log("page-versionchange:" + event.oldVersion + ">" + event.newVersion); first.close(); };
    const stubborn = await promised(indexedDB.open(DB));
    stubborn.onversionchange = (event) => log("page-stubborn-versionchange:" + event.oldVersion + ">" + event.newVersion);
    // The worker's upgrade reaches both Window connections; one stays open,
    // so the worker sees blocked until it closes.
    onBlocked.push(() => stubborn.close());
    worker.postMessage("upgrade");
    const upgraded = await expect("upgraded");
    log("worker-upgraded:" + upgraded.version + ":" + upgraded.names + ":" + upgraded.pageValue);

    // A read/write transaction created in the worker after this Window's
    // overlapping one starts only once it finished, and sees its writes.
    const db = await promised(indexedDB.open(DB));
    const names = Array.from(db.objectStoreNames).join("+");
    log("page-reopened:" + db.version + ":" + names);
    let released = false, counter = 0;
    const holding = db.transaction("items", "readwrite");
    const store = holding.objectStore("items");
    const keepAlive = () => {
        store.put(++counter, "counter").onsuccess = () => { if (!released) keepAlive(); };
    };
    keepAlive();
    holding.oncomplete = () => log("page-complete");
    worker.postMessage("read-counter");
    await expect("created");
    released = true;
    const read = await expect("counter");
    log("worker-counter-matches:" + (read.value === counter));

    // Disjoint scopes commit concurrently without losing either's records.
    worker.postMessage("fill");
    const filling = db.transaction("items", "readwrite");
    for (let i = 0; i < 20; i++) filling.objectStore("items").put("value" + i, "p" + i);
    await done(filling);
    await expect("filled");
    const counting = db.transaction(["items", "worker"]);
    const [pageCount, workerCount] = await Promise.all([
        promised(counting.objectStore("items").count()), promised(counting.objectStore("worker").count())]);
    log("counts:" + pageCount + ":" + workerCount);

    // deleteDatabase fires versionchange at the worker's connection.
    db.close();
    const removed = indexedDB.deleteDatabase(DB);
    const removal = await new Promise((resolve) => { removed.onsuccess = resolve; });
    await expect("worker-versionchange");
    log("deleted:" + removal.oldVersion + ":" + removal.newVersion);

    // A terminated worker's connections are closed: the blocked upgrade proceeds.
    worker.postMessage("hold");
    const held = await expect("holding");
    log("worker-holding:" + held.version);
    const after = indexedDB.open(DB, 2);
    after.onblocked = (event) => { log("page-blocked:" + event.oldVersion + ">" + event.newVersion); worker.terminate(); };
    after.onupgradeneeded = (event) => log("page-upgrade:" + event.oldVersion + ">" + event.newVersion);
    const final = await promised(after);
    log("page-final:" + final.version);
    final.close();
})().catch((error) => log("threw:" + error.name + ":" + error.message)).finally(() => { idbDone = true; });
