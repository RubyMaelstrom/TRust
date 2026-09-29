// Web Audio 1.1 local snapshot 2047f16: BaseAudioContext factory methods,
// AudioNode connections/channel constraints, ScriptProcessorNode and suspend().
function checkAudio(ok, message) { if (!ok) throw Error(message); }
function throwsAudio(name, fn) {
    try { fn(); } catch (error) {
        checkAudio(error.name === name, name + ': ' + error);
        return;
    }
    throw Error('Expected ' + name);
}
var silentContext = new AudioContext();
var destination = silentContext.destination;
var processor = silentContext.createScriptProcessor(1024, 0, 2);
var processCalls = 0;
processor.onaudioprocess = () => processCalls++;
checkAudio(processor instanceof ScriptProcessorNode && processor instanceof AudioNode &&
    processor instanceof EventTarget, 'processor inheritance');
checkAudio(destination instanceof AudioDestinationNode && destination instanceof AudioNode,
    'destination inheritance');
checkAudio(Object.prototype.toString.call(processor) === '[object ScriptProcessorNode]', 'processor tag');
checkAudio(processor.context === silentContext && destination.context === silentContext, 'owning context');
checkAudio(destination === silentContext.destination, 'stable destination');
checkAudio(processor.bufferSize === 1024 && processor.channelCount === 0 &&
    processor.numberOfInputs === 1 && processor.numberOfOutputs === 1, 'zero input channels retain a port');
checkAudio(destination.numberOfInputs === 1 && destination.numberOfOutputs === 1 &&
    destination.channelCount === 2 && destination.maxChannelCount === 2, 'destination ports and channels');
checkAudio(processor.channelCountMode === 'explicit' && processor.channelInterpretation === 'speakers', 'channel defaults');
for (const ctor of [AudioNode, AudioDestinationNode, ScriptProcessorNode]) {
    throwsAudio('TypeError', () => new ctor(silentContext));
    throwsAudio('TypeError', () => ctor());
}
for (const size of [256, 512, 1024, 2048, 4096, 8192, 16384]) {
    checkAudio(silentContext.createScriptProcessor(size, 32, 32).bufferSize === size, 'supported buffer size');
}
for (const size of [1, 255, 257, 16385, 32768, -1])
    throwsAudio('IndexSizeError', () => silentContext.createScriptProcessor(size));
for (const channels of [[0, 0], [-1, 1], [1, -1], [33, 1], [1, 33]])
    throwsAudio('IndexSizeError', () => silentContext.createScriptProcessor(256, ...channels));
for (const value of [1n, Symbol()]) {
    throwsAudio('TypeError', () => silentContext.createScriptProcessor(value));
    throwsAudio('TypeError', () => silentContext.createScriptProcessor(256, value));
    throwsAudio('TypeError', () => silentContext.createScriptProcessor(256, 1, value));
}
const auto = silentContext.createScriptProcessor();
checkAudio(auto.bufferSize >= 256 && auto.bufferSize <= 16384 &&
    !(auto.bufferSize & (auto.bufferSize - 1)) && auto.channelCount === 2, 'automatic buffer size');
checkAudio(silentContext.createScriptProcessor(NaN).bufferSize === auto.bufferSize, 'unsigned long NaN');
checkAudio(silentContext.createScriptProcessor(4294967552, 1, 0).bufferSize === 256, 'unsigned long modulo');
checkAudio(silentContext.createScriptProcessor('256.9', null, 1).channelCount === 0, 'IDL truncation and null');
const converted = [];
throwsAudio('IndexSizeError', () => silentContext.createScriptProcessor(
    {valueOf() { converted.push('size'); return 1; }},
    {valueOf() { converted.push('inputs'); return 1; }},
    {valueOf() { converted.push('outputs'); return 1; }}));
checkAudio(converted.join() === 'size,inputs,outputs', 'convert all arguments before validation');
throwsAudio('TypeError', () => BaseAudioContext.prototype.createScriptProcessor.call(processor));
throwsAudio('TypeError', () => AudioNode.prototype.connect.call(silentContext, destination));
throwsAudio('TypeError', () => Object.getOwnPropertyDescriptor(ScriptProcessorNode.prototype, 'bufferSize').get.call(destination));
throwsAudio('TypeError', () => Object.getOwnPropertyDescriptor(AudioDestinationNode.prototype, 'maxChannelCount').get.call(processor));
throwsAudio('TypeError', () => processor.connect(Object.create(AudioNode.prototype)));
processor.channelCount = 0;
processor.channelCountMode = 'explicit';
throwsAudio('NotSupportedError', () => { processor.channelCount = 1; });
throwsAudio('NotSupportedError', () => { processor.channelCountMode = 'max'; });
throwsAudio('TypeError', () => { processor.channelCountMode = 'invalid'; });
throwsAudio('TypeError', () => { processor.channelInterpretation = Symbol(); });
processor.channelInterpretation = 'discrete';
checkAudio(processor.channelInterpretation === 'discrete', 'channel interpretation setter');
destination.channelCount = 1;
throwsAudio('IndexSizeError', () => { destination.channelCount = 0; });
throwsAudio('IndexSizeError', () => { destination.channelCount = 3; });
destination.channelCount = 2;
checkAudio(processor.connect(destination) === destination, 'SDL connection and return');
processor.connect(destination); // A duplicate must not leave a second edge.
processor.disconnect(destination);
throwsAudio('InvalidAccessError', () => processor.disconnect(destination));
throwsAudio('IndexSizeError', () => processor.connect(destination, 1));
throwsAudio('IndexSizeError', () => processor.connect(destination, 0, 1));
throwsAudio('TypeError', () => processor.connect());
throwsAudio('TypeError', () => processor.disconnect({}, 0));
const secondContext = new AudioContext();
throwsAudio('InvalidAccessError', () => processor.connect(secondContext.destination));
throwsAudio('InvalidAccessError', () => processor.disconnect(secondContext.destination));
// Cycles and fan-out; clearing the destination's outgoing edges must retain
// the incoming edge. Zero output channels still leave the processor's port.
const sink = silentContext.createScriptProcessor(256, 1, 0);
processor.connect(destination);
processor.connect(sink);
sink.connect(processor);
processor.disconnect();
sink.disconnect(processor);
throwsAudio('InvalidAccessError', () => processor.disconnect(destination));
processor.connect(destination);
destination.disconnect();
processor.disconnect(destination, 0, 0);
for (const args of [[], [0], [undefined], [null], [{valueOf() { return 0; }}]]) {
    processor.connect(destination);
    processor.disconnect(...args);
    throwsAudio('InvalidAccessError', () => processor.disconnect(destination));
}
processor.connect(destination);
throwsAudio('IndexSizeError', () => processor.disconnect(destination, 0, 1));
processor.disconnect(destination, 0);
throwsAudio('InvalidAccessError', () => processor.disconnect(destination, 0));
throwsAudio('IndexSizeError', () => processor.disconnect(1));
// EventTarget behavior is observable even without a renderer.
const events = [];
processor.onaudioprocess = function() { checkAudio(this === processor, 'handler receiver'); events.push('old'); };
processor.addEventListener('audioprocess', () => events.push('listener'));
processor.onaudioprocess = function() { checkAudio(this === processor, 'replacement receiver'); events.push('new'); return false; };
checkAudio(!processor.dispatchEvent(new Event('audioprocess', {cancelable:true})), 'handler cancellation');
checkAudio(events.join() === 'new,listener', 'replacement retains event ordering');
processor.onaudioprocess = 12;
checkAudio(processor.onaudioprocess === null, 'non-object handler clears');
const handlerObject = {};
processor.onaudioprocess = handlerObject;
checkAudio(processor.onaudioprocess === handlerObject, 'non-callable object retained until invocation');
processor.onaudioprocess = () => processCalls++;
processor.connect(destination);
var silentResume = 'pending';
silentContext.resume().catch(error => { silentResume = error.name; });
secondContext.close();
'audio-script-processor-ready'
