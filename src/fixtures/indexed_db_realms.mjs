// Indexed Database 3 (local w3c/IndexedDB@f704918) in a Window and in a
// dedicated worker ([Exposed=(Window,Worker)]): the interfaces, request
// event paths (#request-construct get-the-parent), version change events,
// transaction scheduling and cursor updates.
const promised = (request) => new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
});
const done = (transaction) => new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onabort = () => reject(transaction.error);
});
check(typeof indexedDB === "object" && indexedDB instanceof IDBFactory, "indexedDB is an IDBFactory");
const factoryDescriptor = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
check(typeof factoryDescriptor.get === "function" && factoryDescriptor.set === undefined && indexedDB === indexedDB,
    "[SameObject] readonly attribute indexedDB");
for (const name of ["IDBRequest", "IDBOpenDBRequest", "IDBDatabase", "IDBTransaction", "IDBObjectStore", "IDBIndex",
    "IDBCursor", "IDBCursorWithValue", "IDBKeyRange", "IDBVersionChangeEvent", "IDBFactory"])
    check(typeof globalThis[name] === "function" &&
        Object.prototype.toString.call(globalThis[name].prototype) === "[object " + name + "]", name + " interface");
throws(() => new IDBFactory(), "TypeError", "IDBFactory is not constructible");

// IDBVersionChangeEvent and IDBKeyRange attributes live on their prototypes.
const versionEvent = new IDBVersionChangeEvent("versionchange", { oldVersion: 3, newVersion: 5 });
check(versionEvent.oldVersion === 3 && versionEvent.newVersion === 5 && !Object.hasOwn(versionEvent, "oldVersion"),
    "IDBVersionChangeEvent attributes");
check(new IDBVersionChangeEvent("x").newVersion === null, "newVersion defaults to null");
const range = IDBKeyRange.bound(1, 5, true, false);
check(range.lower === 1 && range.upper === 5 && range.lowerOpen && !range.upperOpen && !Object.hasOwn(range, "lowerOpen"),
    "IDBKeyRange attributes");
check(range.includes(5) && !range.includes(1), "IDBKeyRange.includes");
throws(() => IDBKeyRange.bound(2, 2, true), "DataError", "an empty bound range");
check(indexedDB.cmp([1, "a"], [1, "b"]) === -1 && indexedDB.cmp(new Date(5), 5) === 1, "cmp orders keys");

const name = "realm-conformance-" + (typeof window === "undefined" ? "worker" : "window");
await promised(indexedDB.deleteDatabase(name));
const opening = indexedDB.open(name, 1);
opening.onupgradeneeded = (event) => {
    check(event instanceof IDBVersionChangeEvent && event.oldVersion === 0 && event.newVersion === 1, "upgradeneeded versions");
    const db = opening.result;
    check(opening.transaction.mode === "versionchange", "upgrade transaction mode");
    const store = db.createObjectStore("people", { keyPath: "id" });
    store.createIndex("byName", "name", { unique: true });
    db.createObjectStore("plain", { autoIncrement: true });
};
const db = await promised(opening);
check(db.version === 1 && db.objectStoreNames instanceof DOMStringList && db.objectStoreNames.length === 2 &&
    db.objectStoreNames[0] === "people" && db.objectStoreNames.contains("plain"), "objectStoreNames is a DOMStringList");

const write = db.transaction(["people", "plain"], "readwrite");
const people = write.objectStore("people");
people.put({ id: 1, name: "ada" });
people.put({ id: 2, name: "bob" });
const autoKey = write.objectStore("plain").add({ any: "value" });
// #request-construct: request -> transaction -> connection. A capture
// listener on the connection runs first; the error event bubbles back up.
const path = [];
db.addEventListener("error", (event) => path.push("db-capture:" + event.eventPhase + ":" + (event.target === duplicate)), true);
write.addEventListener("error", (event) => path.push("tx-bubble:" + event.eventPhase + ":" + event.composedPath().length));
db.addEventListener("error", (event) => { path.push("db-bubble:" + event.eventPhase); event.preventDefault(); });
const duplicate = people.add({ id: 1, name: "copy" });
duplicate.onerror = (event) => path.push("request:" + event.eventPhase + ":" + duplicate.error.name);
await done(write);
check(path.join() === "db-capture:1:true,request:2:ConstraintError,tx-bubble:3:3,db-bubble:3",
    "request error event path " + path.join());
check(autoKey.result === 1, "the key generator starts at 1");

// A listener exception aborts its transaction with AbortError.
const throwing = db.transaction("people", "readwrite");
throwing.objectStore("people").put({ id: 3, name: "cy" }).onsuccess = () => { throw new Error("listener failure"); };
const abortedWith = await done(throwing).then(() => "committed", (error) => error && error.name);
check(abortedWith === "AbortError", "a throwing listener aborts the transaction: " + abortedWith);

// #transaction-scheduling: overlapping read/write transactions run in creation order.
const order = [];
const first = db.transaction("people", "readwrite"), second = db.transaction("people", "readwrite");
second.objectStore("people").get(1).onsuccess = (event) => order.push("second-read:" + event.target.result.name);
first.objectStore("people").put({ id: 1, name: "ada-2" }).onsuccess = () => order.push("first-write");
first.oncomplete = () => order.push("first-complete");
await done(second);
check(order.join() === "first-write,first-complete,second-read:ada-2", "transaction order " + order.join());

// Index reads, cursors, and IDBCursor.update with in-line keys.
const reading = db.transaction("people", "readwrite");
const byName = reading.objectStore("people").index("byName");
check(byName instanceof IDBIndex && byName.objectStore.name === "people" && byName.unique, "IDBIndex attributes");
const names = [];
const cursorRequest = reading.objectStore("people").openCursor();
cursorRequest.onsuccess = () => {
    const cursor = cursorRequest.result;
    if (!cursor) return;
    check(cursor.source.name === "people" && cursor.direction === "next" && cursor.request === cursorRequest, "IDBCursor attributes");
    names.push(cursor.value.name);
    throws(() => cursor.update({ id: 99, name: "moved" }), "DataError", "an in-line key that differs from the cursor");
    const updated = cursor.update({ id: cursor.primaryKey, name: cursor.value.name.toUpperCase() });
    check(updated.source === cursor, "IDBCursor.update's request source is the cursor");
    cursor.continue();
};
await done(reading);
check(names.join() === "ada-2,bob", "cursor order " + names.join());
const upper = await promised(db.transaction("people").objectStore("people").index("byName").getAll());
check(upper.map((person) => person.name).join() === "ADA-2,BOB", "updated records " + JSON.stringify(upper));

// versionchange at an open connection, blocked while it stays open.
const versionLog = [];
db.onversionchange = (event) => {
    versionLog.push("versionchange:" + event.oldVersion + ":" + event.newVersion);
    setTimeout(() => db.close(), 0);
};
const upgrade = indexedDB.open(name, 2);
upgrade.onblocked = (event) => versionLog.push("blocked:" + event.oldVersion + ":" + event.newVersion);
upgrade.onupgradeneeded = (event) => versionLog.push("upgrade:" + event.oldVersion + ":" + event.newVersion);
const upgraded = await promised(upgrade);
check(versionLog.join() === "versionchange:1:2,blocked:1:2,upgrade:1:2", "version change " + versionLog.join());
const listed = await indexedDB.databases();
check(listed.some((info) => info.name === name && info.version === 2), "databases() lists the database");
upgraded.close();
const removed = indexedDB.deleteDatabase(name);
const removal = await new Promise((resolve) => { removed.onsuccess = (event) => resolve(event); });
check(removal instanceof IDBVersionChangeEvent && removal.oldVersion === 2 && removal.newVersion === null, "deleteDatabase success");
