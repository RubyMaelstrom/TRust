// Web Audio 1.1 #AudioContext / #sample-rates and Web IDL #js-to-dictionary.
// A missing renderer must not abort a module that constructs its context
// before creating UI. It must also never pretend to play or advance time.
function assertAudio(condition, message) {
    if (!condition) throw new Error(message);
}
function audioThrows(name, action) {
    try { action(); } catch (error) {
        assertAudio(error.name === name, name + ': ' + error);
        return;
    }
    throw new Error('Expected ' + name);
}
var audioEvents = [];
var audioContext = new AudioContext({latencyHint: 0.03});
assertAudio(audioContext instanceof AudioContext && audioContext instanceof BaseAudioContext &&
    audioContext instanceof EventTarget, 'context interface inheritance');
assertAudio(Object.prototype.toString.call(audioContext) === '[object AudioContext]', 'interface tag');
assertAudio(audioContext.state === 'suspended' && audioContext.currentTime === 0, 'initial state and clock');
assertAudio(audioContext.sampleRate === 48000 && audioContext.renderQuantumSize === 128, 'default settings');
assertAudio(audioContext.onerror === null && audioContext.onstatechange === null, 'initial event handlers');
assertAudio(audioContext.baseLatency === 0 && audioContext.outputLatency === 0, 'no acquired renderer latency');
const stamp = audioContext.getOutputTimestamp();
assertAudio(stamp.contextTime === 0 && stamp.performanceTime === 0 &&
    stamp !== audioContext.getOutputTimestamp(), 'fresh zero timestamp before processing');
audioThrows('TypeError', () => new BaseAudioContext());
audioThrows('TypeError', () => AudioContext());
audioThrows('TypeError', () => Object.getOwnPropertyDescriptor(BaseAudioContext.prototype, 'state').get.call({}));
audioThrows('TypeError', () => AudioContext.prototype.getOutputTimestamp.call({}));
assertAudio(typeof __audio_context_binding === 'undefined', 'private binding removed');

for (const options of [false, 1, '', Symbol(), 1n])
    audioThrows('TypeError', () => new AudioContext(options));
for (const rate of [NaN, Infinity, -Infinity, 1e100, Symbol(), 1n])
    audioThrows('TypeError', () => new AudioContext({sampleRate: rate}));
for (const rate of [0, -1, 2999, 768001])
    audioThrows('NotSupportedError', () => new AudioContext({sampleRate: rate}));
for (const latency of [NaN, Infinity, '0.03', 'invalid', null, Symbol()])
    audioThrows('TypeError', () => new AudioContext({latencyHint: latency}));
for (const quantum of [0, -1, NaN, Infinity, 288001])
    audioThrows('NotSupportedError', () => new AudioContext({renderSizeHint: quantum}));
audioThrows('TypeError', () => new AudioContext({renderSizeHint: '256'}));
audioThrows('TypeError', () => new AudioContext({sinkId: null}));
audioThrows('TypeError', () => new AudioContext({sinkId: {type: 'invalid'}}));
audioThrows('TypeError', () => new AudioContext({sinkId: Symbol()}));
for (const rate of [3000, 44100.1, 768000]) {
    const context = new AudioContext({sampleRate: rate});
    assertAudio(context.sampleRate === Math.fround(rate), 'float sample rate');
    context.close();
}
for (const options of [undefined, null, {}, {latencyHint: 'interactive'},
    {latencyHint: 'balanced'}, {latencyHint: 'playback'}, {renderSizeHint: 'hardware'},
    {renderSizeHint: 256}, {sinkId: {type: 'none'}}]) new AudioContext(options).close();
const reads = [];
new AudioContext({
    get sinkId() { reads.push('sink'); return ''; },
    get sampleRate() { reads.push('rate'); return {valueOf() { reads.push('number'); return 44100; }}; },
    get renderSizeHint() { reads.push('quantum'); return 256; },
    get latencyHint() { reads.push('latency'); return 'interactive'; }
}).close();
assertAudio(reads.join(',') === 'latency,quantum,rate,number,sink', 'dictionary conversion ordering');

audioContext.addEventListener('error', event => {
    assertAudio(event.isTrusted && !event.bubbles && !event.cancelable &&
        event.target === audioContext, 'trusted acquisition failure event');
    audioEvents.push('error');
});
audioContext.onerror = () => audioEvents.push('old-handler');
audioContext.addEventListener('error', () => audioEvents.push('after-handler'));
audioContext.onerror = () => audioEvents.push('new-handler');
audioContext.onstatechange = () => audioEvents.push('unexpected-statechange');
assertAudio(audioEvents.length === 0, 'construction does not dispatch synchronously');
var audioResumeResult = 'pending', audioSuspendResult = 'pending', audioBrandResult = 'pending';
audioContext.resume().then(() => audioResumeResult = 'incorrectly running', error => audioResumeResult = error.name);
audioContext.suspend().then(() => audioSuspendResult = 'suspended');
AudioContext.prototype.resume.call({}).catch(error => audioBrandResult = error.name);
assertAudio(audioResumeResult === 'pending', 'resume failure is asynchronous');
'audio-context-pending'
