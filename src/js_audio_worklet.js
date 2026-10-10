// Web Audio 1.1 (local WebAudio/web-audio-api@2047f16) #AudioWorkletGlobalScope
// and #AudioWorkletProcessor, on top of the worker platform: the scope runs on
// its BaseAudioContext's rendering thread (`audio_worklet_host`), which calls
// the control methods below. Interfaces a WorkletGlobalScope does not expose
// are removed.
(function (g, portAPI) {
    "use strict";
    const cfg = g.__worker_cfg;
    if (!cfg || !cfg.worklet) return;
    const control = Reflect.apply(WeakMap.prototype.get, __platform_slots("controls", new WeakMap()), [g]);
    const apply = Reflect.apply, construct = Reflect.construct, define = Object.defineProperty;
    const freeze = Object.freeze, isArray = Array.isArray, NumberCtor = Number, F32 = Float32Array;
    const fround = Math.fround, finite = Number.isFinite;
    const Exception = g.DOMException;
    const typedArrayLength = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(F32.prototype), "length").get;
    const rate = fround(cfg.worklet.rate), quantum = cfg.worklet.quantum;
    const MAX_FLOAT = 3.4028234663852886e38;
    // [[node name to processor constructor map]] and the descriptors the
    // control thread has not been told about yet.
    const processors = new Map(), registered = [];
    // AudioWorkletProcessor instances, by AudioWorkletNode id.
    const instances = new Map();
    const internals = new WeakMap();
    // [[pending processor construction data]].
    let pending = null;
    let frame = 0, globalPort = null;

    class AudioWorkletProcessor {
        // #AudioWorketProcessor-constructors
        constructor() {
            if (!pending) throw new TypeError("AudioWorkletProcessor is constructed by an AudioWorkletNode");
            const data = pending;
            pending = null;
            internals.set(this, {node: data.node, port: data.port});
        }
        get port() {
            const state = internals.get(this);
            if (!state) throw new TypeError("Illegal AudioWorkletProcessor invocation");
            return state.port;
        }
    }
    define(AudioWorkletProcessor.prototype, "port", {enumerable: true});
    define(AudioWorkletProcessor.prototype, Symbol.toStringTag, {value: "AudioWorkletProcessor", configurable: true});

    function isConstructor(value) {
        if (typeof value !== "function") return false;
        try { construct(String, [], value); return true; } catch (_) { return false; }
    }
    // Web IDL dictionary AudioParamDescriptor, members in lexicographic order.
    function descriptor(value) {
        if (value === null || (typeof value !== "object" && typeof value !== "function"))
            throw new TypeError("AudioParamDescriptor must be a dictionary");
        const rateValue = value.automationRate;
        const automationRate = rateValue === undefined ? "a-rate" : `${rateValue}`;
        if (automationRate !== "a-rate" && automationRate !== "k-rate")
            throw new TypeError("Invalid AutomationRate");
        const float = (member, fallback) => {
            if (member === undefined) return fallback;
            const number = fround(NumberCtor(member));
            if (!finite(number)) throw new TypeError("AudioParamDescriptor values must be finite floats");
            return number;
        };
        const defaultValue = float(value.defaultValue, 0);
        const maxValue = float(value.maxValue, MAX_FLOAT);
        const minValue = float(value.minValue, -MAX_FLOAT);
        if (value.name === undefined) throw new TypeError("AudioParamDescriptor.name is required");
        const name = `${value.name}`;
        return {name, automationRate, defaultValue, minValue, maxValue};
    }
    // #dom-audioworkletglobalscope-registerprocessor
    function registerProcessor(name, processorCtor) {
        if (arguments.length < 2) throw new TypeError("registerProcessor requires a name and a constructor");
        name = `${name}`;
        if (name === "") throw new Exception("The processor name is empty", "NotSupportedError");
        if (processors.has(name)) throw new Exception("The processor name is already registered", "NotSupportedError");
        if (!isConstructor(processorCtor)) throw new TypeError("The processor is not a constructor");
        const prototype = processorCtor.prototype;
        if (prototype === null || (typeof prototype !== "object" && typeof prototype !== "function"))
            throw new TypeError("The processor's prototype is not an object");
        const value = processorCtor.parameterDescriptors;
        const descriptors = [], names = [];
        if (value !== undefined) {
            if (value === null || (typeof value !== "object" && typeof value !== "function") ||
                typeof value[Symbol.iterator] !== "function")
                throw new TypeError("parameterDescriptors must be a sequence");
            for (const item of value) descriptors.push(descriptor(item));
            for (const item of descriptors) {
                if (names.includes(item.name))
                    throw new Exception("Duplicate AudioParam name " + item.name, "NotSupportedError");
                names.push(item.name);
                if (!(item.minValue <= item.defaultValue && item.defaultValue <= item.maxValue))
                    throw new Exception("defaultValue is outside [minValue, maxValue]", "InvalidStateError");
            }
        }
        processors.set(name, {ctor: processorCtor, descriptors});
        registered.push([name, descriptors]);
    }

    // The global scope's interface (#AudioWorkletGlobalScope-attributes).
    define(g, "registerProcessor", {value: registerProcessor, writable: true, configurable: true});
    define(g, "AudioWorkletProcessor", {value: AudioWorkletProcessor, writable: true, configurable: true});
    define(g, "currentFrame", {configurable: true, enumerable: true, get: function currentFrame() { return frame; }});
    define(g, "currentTime", {configurable: true, enumerable: true, get: function currentTime() { return frame / rate; }});
    define(g, "sampleRate", {configurable: true, enumerable: true, get: function sampleRate() { return rate; }});
    define(g, "renderQuantumSize", {configurable: true, enumerable: true, get: function renderQuantumSize() { return quantum; }});
    define(g, "port", {configurable: true, enumerable: true, get: function port() { return globalPort; }});

    function errorInfo(error) {
        let message = "";
        try { message = error instanceof Error ? `${error.message}` : `${error}`; }
        catch (_) { message = "Uncaught exception"; }
        let filename = "", line = 0, column = 0;
        try {
            const stack = error && typeof error.stack === "string" ? error.stack : "";
            const match = /\((.*):(\d+):(\d+)\)/.exec(stack) || /at (.*):(\d+):(\d+)/.exec(stack);
            if (match) { filename = match[1]; line = +match[2]; column = +match[3]; }
        } catch (_) {}
        return JSON.stringify({message, filename, line, column});
    }
    function shapeOf(text) {
        return text === "" ? [] : text.split(",").map(NumberCtor);
    }
    function frozenChannels(count) {
        const channels = [];
        for (let c = 0; c < count; c++) channels.push(new F32(quantum));
        return freeze(channels);
    }

    // Methods the rendering thread calls (`audio_worklet_host`).
    control.workletTake = function () {
        const list = registered.splice(0);
        return JSON.stringify(list.map(([name, descriptors]) => ({name, descriptors})));
    };
    control.workletTime = function (value) { frame = value; };
    control.workletGlobalPort = function (packet) {
        try { globalPort = portAPI.deserialize(JSON.parse(packet)).ports[0] || null; }
        catch (_) { globalPort = null; }
    };
    // #invoking-processor-constructor
    control.workletCreate = function (node, name, packet) {
        const entry = processors.get(name);
        try {
            if (!entry) throw new Exception("No processor named " + name, "InvalidStateError");
            const {data, ports} = portAPI.deserialize(JSON.parse(packet));
            pending = {node, port: ports[0] || null};
            const processor = construct(entry.ctor, [data]);
            if (processor === null || (typeof processor !== "object" && typeof processor !== "function"))
                throw new TypeError("The processor constructor did not return an object");
            instances.set(node, {processor, descriptors: entry.descriptors, shape: null, arrays: null,
                inputs: null, outputs: null, parameters: null, outputCount: 0});
            return "";
        } catch (error) {
            return errorInfo(error);
        } finally {
            pending = null;
        }
    };
    control.workletRelease = function (node) { instances.delete(node); };
    // The arrays of one call: inputs, then outputs, then parameters,
    // reallocated only when a channel count or a parameter's length changes.
    control.workletPrepare = function (node, inputShape, outputShape, paramShape) {
        const instance = instances.get(node);
        if (!instance) return null;
        const shape = inputShape + "|" + outputShape + "|" + paramShape;
        if (instance.shape !== shape) {
            const arrays = [];
            const inputs = shapeOf(inputShape).map(frozenChannels);
            const outputs = shapeOf(outputShape).map(frozenChannels);
            for (const input of inputs) for (const channel of input) arrays.push(channel);
            for (const output of outputs) for (const channel of output) arrays.push(channel);
            const parameters = Object.create(null);
            shapeOf(paramShape).forEach((length, index) => {
                const values = new F32(length);
                arrays.push(values);
                parameters[instance.descriptors[index].name] = values;
            });
            instance.inputs = freeze(inputs);
            instance.outputs = freeze(outputs);
            instance.parameters = freeze(parameters);
            instance.arrays = arrays;
            instance.shape = shape;
        }
        return instance.arrays;
    };
    // #rendering-loop step 4.5.4: process() with zero-filled outputs; the
    // result is ToBoolean of its return value, or an error description.
    control.workletRun = function (node) {
        const instance = instances.get(node);
        if (!instance) return {error: errorInfo(new TypeError("The processor is gone"))};
        for (const output of instance.outputs) for (const channel of output) channel.fill(0);
        try {
            const process = instance.processor.process;
            if (typeof process !== "function") throw new TypeError("process is not a function");
            return !!apply(process, instance.processor, [instance.inputs, instance.outputs, instance.parameters]);
        } catch (error) {
            return {error: errorInfo(error)};
        }
    };
})(globalThis, __port_api);
/*__AUDIO_WORKLET_SCOPE_END__*/
// After the whole worker platform is installed:
(function (g) {
    "use strict";
    // A WorkletGlobalScope exposes neither worker messaging, timers, network
    // nor storage.
    for (const name of ["postMessage", "close", "importScripts", "onmessage", "onmessageerror", "onerror",
        "onlanguagechange", "onoffline", "ononline", "onrejectionhandled", "onunhandledrejection",
        "setTimeout", "setInterval", "clearTimeout", "clearInterval", "requestAnimationFrame",
        "cancelAnimationFrame", "fetch", "XMLHttpRequest", "XMLHttpRequestEventTarget", "XMLHttpRequestUpload",
        "WebSocket", "EventSource", "Worker", "SharedWorker", "indexedDB", "caches", "navigator", "location",
        "self", "name", "performance", "crypto", "createImageBitmap", "OffscreenCanvas", "Request", "Response",
        "Headers", "FileReader", "FileReaderSync", "WorkerGlobalScope", "DedicatedWorkerGlobalScope",
        "WorkerNavigator", "WorkerLocation", "atob", "btoa", "reportError", "structuredClone", "isSecureContext",
        "origin", "crossOriginIsolated", "scheduler"])
        Reflect.deleteProperty(g, name);
})(globalThis);
