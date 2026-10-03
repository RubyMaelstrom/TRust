// Original Web Audio 1.1 (local WebAudio/web-audio-api@2047f16) regressions:
// OfflineAudioContext rendering, AudioParam automation, OscillatorNode,
// GainNode, DynamicsCompressorNode, AudioBufferSourceNode and AudioBuffer.
(async function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    function throws(name, fn) {
        try { fn(); } catch (e) { check(e.name === name, 'wrong error ' + e.name + ' for ' + fn); return e; }
        throw Error('missing ' + name + ' for ' + fn);
    }
    async function rejects(name, promise) {
        check(promise instanceof Promise, 'operation returns a Promise');
        try { await promise; } catch (e) { check(e.name === name, 'wrong rejection ' + e.name + ': ' + e.message); return e; }
        throw Error('missing rejection ' + name);
    }
    const near = (a, b, epsilon = 2e-5) => Math.abs(a - b) <= epsilon;

    // Interface shapes.
    const interfaces = [[OfflineAudioContext, BaseAudioContext, 1], [AudioBuffer, Function.prototype, 1],
        [AudioParam, Function.prototype, 0], [AudioScheduledSourceNode, AudioNode, 0],
        [OscillatorNode, AudioScheduledSourceNode, 1], [GainNode, AudioNode, 1],
        [DynamicsCompressorNode, AudioNode, 1], [AudioBufferSourceNode, AudioScheduledSourceNode, 1],
        [PeriodicWave, Function.prototype, 1], [OfflineAudioCompletionEvent, Event, 2]];
    for (const [C, parent, length] of interfaces) {
        check(Object.getPrototypeOf(C) === parent, 'inheritance ' + C.name);
        check(C.length === length, 'length ' + C.name + ' ' + C.length);
        check(Object.prototype.toString.call(C.prototype) === '[object ' + C.name + ']', 'tag ' + C.name);
        const own = Object.getOwnPropertyDescriptor(globalThis, C.name);
        check(own && !own.enumerable && own.writable, 'global ' + C.name);
        for (const key of Object.getOwnPropertyNames(C.prototype))
            if (key !== 'constructor') check(Object.getOwnPropertyDescriptor(C.prototype, key).enumerable, 'enumerable ' + C.name + '.' + key);
    }
    throws('TypeError', () => new AudioParam());
    throws('TypeError', () => new AudioScheduledSourceNode());
    throws('TypeError', () => new OfflineAudioContext());
    throws('TypeError', () => new OfflineAudioContext(1, 128));
    throws('TypeError', () => new OfflineAudioContext({length: 128}));
    throws('NotSupportedError', () => new OfflineAudioContext(0, 128, 44100));
    throws('NotSupportedError', () => new OfflineAudioContext(33, 128, 44100));
    throws('NotSupportedError', () => new OfflineAudioContext(1, 0, 44100));
    throws('NotSupportedError', () => new OfflineAudioContext(1, 128, 2999));
    throws('NotSupportedError', () => new OfflineAudioContext(1, -1, 44100));
    throws('TypeError', () => new OfflineAudioContext(1, 128, NaN));
    const reads = [];
    new OfflineAudioContext({get sampleRate() { reads.push('rate'); return 8000; },
        get numberOfChannels() { reads.push('channels'); return 2; },
        get length() { reads.push('length'); return 64; },
        get renderSizeHint() { reads.push('size'); return 64; }});
    check(reads.join() === 'length,channels,size,rate', 'options read in Web IDL order: ' + reads.join());

    const context = new OfflineAudioContext(2, 300, 44100);
    check(context instanceof BaseAudioContext && context instanceof EventTarget, 'context inheritance');
    check(context.length === 300 && context.sampleRate === 44100 && context.state === 'suspended' &&
        context.currentTime === 0 && context.renderQuantumSize === 128, 'initial context attributes');
    check(context.destination.channelCount === 2 && context.destination.maxChannelCount === 2 &&
        context.destination.channelCountMode === 'explicit', 'offline destination channels');
    throws('InvalidStateError', () => { context.destination.channelCount = 1; });
    throws('InvalidStateError', () => { context.destination.channelCountMode = 'max'; });
    check(new OfflineAudioContext({sampleRate: 22050, length: null}).length === null, 'undefined-length rendering');

    // AudioBuffer.
    throws('TypeError', () => new AudioBuffer());
    throws('TypeError', () => new AudioBuffer({sampleRate: 44100}));
    throws('NotSupportedError', () => new AudioBuffer({length: 0, sampleRate: 44100}));
    throws('NotSupportedError', () => context.createBuffer(0, 1, 44100));
    const buffer = new AudioBuffer({length: 4, numberOfChannels: 2, sampleRate: 8000});
    check(buffer.length === 4 && buffer.numberOfChannels === 2 && buffer.sampleRate === 8000 &&
        buffer.duration === 4 / 8000, 'AudioBuffer attributes');
    const data = buffer.getChannelData(0);
    check(data instanceof Float32Array && data === buffer.getChannelData(0) && data.length === 4, 'channel data identity');
    throws('IndexSizeError', () => buffer.getChannelData(2));
    buffer.copyToChannel(new Float32Array([0.25, 0.5, -0.75]), 0, 1);
    check(data.join() === '0,0.25,0.5,-0.75', 'copyToChannel offset');
    const copied = new Float32Array(3);
    buffer.copyFromChannel(copied, 0, 2);
    check(copied.join() === '0.5,-0.75,0', 'copyFromChannel partial');
    throws('TypeError', () => buffer.copyFromChannel([1], 0));
    throws('IndexSizeError', () => buffer.copyToChannel(new Float32Array(1), 5));
    check(Object.getOwnPropertyNames(buffer).length === 0, 'no own AudioBuffer properties');

    // AudioParam attributes, automation validation and fixed rates.
    const gain = context.createGain();
    const param = gain.gain;
    check(param instanceof AudioParam && param === gain.gain, 'SameObject AudioParam');
    check(param.value === 1 && param.defaultValue === 1 && param.automationRate === 'a-rate' &&
        param.maxValue === 3.4028234663852886e38 && param.minValue === -3.4028234663852886e38, 'gain param');
    throws('TypeError', () => { param.value = NaN; });
    throws('RangeError', () => param.setValueAtTime(1, -1));
    throws('TypeError', () => param.setValueAtTime(1, Infinity));
    throws('RangeError', () => param.exponentialRampToValueAtTime(0, 1));
    throws('RangeError', () => param.setTargetAtTime(1, 0, -1));
    throws('InvalidStateError', () => param.setValueCurveAtTime([1], 0, 1));
    throws('RangeError', () => param.setValueCurveAtTime([1, 2], 0, 0));
    check(param.setValueCurveAtTime([1, 2], 10, 1) === param, 'automation methods chain');
    throws('NotSupportedError', () => param.setValueAtTime(3, 10.5));
    throws('NotSupportedError', () => param.setValueCurveAtTime([0, 1], 9.5, 1));
    param.cancelScheduledValues(0);
    param.automationRate = 'invalid';
    check(param.automationRate === 'a-rate', 'invalid enumeration ignored');
    param.automationRate = 'k-rate';
    check(param.automationRate === 'k-rate', 'gain automation rate is not fixed');
    param.automationRate = 'a-rate';
    const compressor = new DynamicsCompressorNode(context, {threshold: -50, knee: 40, ratio: 12, attack: 0, release: 0.25});
    check(compressor.threshold.value === -50 && compressor.knee.value === 40 && compressor.ratio.value === 12 &&
        compressor.attack.value === 0 && compressor.release.value === 0.25 && compressor.reduction === 0,
        'compressor options become AudioParam values');
    check(compressor.channelCountMode === 'clamped-max' && compressor.channelCount === 2, 'compressor channels');
    throws('InvalidStateError', () => { compressor.ratio.automationRate = 'a-rate'; });
    throws('NotSupportedError', () => { compressor.channelCount = 3; });
    throws('NotSupportedError', () => { compressor.channelCountMode = 'max'; });
    check(compressor.threshold.minValue === -100 && compressor.threshold.maxValue === 0, 'threshold range');
    throws('TypeError', () => new GainNode({}));
    throws('TypeError', () => new GainNode(context, {gain: NaN}));
    throws('NotSupportedError', () => new GainNode(context, {channelCount: 0}));

    // OscillatorNode, PeriodicWave and scheduling errors.
    const oscillator = new OscillatorNode(context, {frequency: 1000});
    check(oscillator.type === 'sine' && oscillator.frequency.value === 1000 && oscillator.detune.value === 0 &&
        oscillator.numberOfInputs === 0 && oscillator.numberOfOutputs === 1, 'oscillator defaults');
    check(oscillator.frequency.maxValue === 22050 && oscillator.frequency.minValue === -22050, 'frequency nominal range');
    throws('InvalidStateError', () => { oscillator.type = 'custom'; });
    oscillator.type = 'bogus';
    check(oscillator.type === 'sine', 'invalid oscillator type ignored');
    throws('InvalidStateError', () => new OscillatorNode(context, {type: 'custom'}));
    throws('InvalidStateError', () => oscillator.stop());
    throws('RangeError', () => oscillator.start(-1));
    const wave = new PeriodicWave(context, {real: [0, 0], imag: [0, 1]});
    throws('IndexSizeError', () => new PeriodicWave(context, {real: [0, 1, 2], imag: [0, 1]}));
    throws('IndexSizeError', () => context.createPeriodicWave([0], [0]));
    const custom = context.createOscillator();
    custom.setPeriodicWave(wave);
    check(custom.type === 'custom', 'setPeriodicWave selects custom');
    check(new OscillatorNode(context, {type: 'square', periodicWave: wave}).type === 'custom', 'periodicWave option wins');

    // Render: a 1 kHz sine into stereo through a gain ramp, and a buffer source.
    oscillator.connect(gain).connect(context.destination);
    gain.gain.setValueAtTime(0.5, 0);
    gain.gain.linearRampToValueAtTime(1, 256 / 44100);
    oscillator.start(0);
    oscillator.stop(280 / 44100);
    const source = context.createBufferSource();
    const pulse = context.createBuffer(1, 2, 44100);
    pulse.getChannelData(0).set([0.125, -0.125]);
    source.buffer = pulse;
    throws('InvalidStateError', () => { source.buffer = pulse; });
    source.connect(context.destination);
    source.start(290 / 44100);
    check(pulse.getChannelData(0).join() === '0.125,-0.125', 'acquired buffers expose copies');
    const states = [], order = [];
    context.onstatechange = () => states.push(context.state);
    let ended = 0;
    oscillator.onended = event => { ended++; check(event.isTrusted && event.target === oscillator, 'trusted ended'); };
    context.oncomplete = event => order.push('complete:' + (event instanceof OfflineAudioCompletionEvent) + ':' + event.isTrusted);
    const promise = context.startRendering();
    await rejects('InvalidStateError', context.startRendering());
    const rendered = await promise;
    order.push('resolved');
    check(rendered instanceof AudioBuffer && rendered.length === 300 && rendered.numberOfChannels === 2 &&
        rendered.sampleRate === 44100, 'rendered buffer shape');
    const left = rendered.getChannelData(0), right = rendered.getChannelData(1);
    for (let i = 0; i < 280; i++) {
        const expected = Math.sin(2 * Math.PI * 1000 * i / 44100) * (i < 256 ? 0.5 + 0.5 * i / 256 : 1);
        check(near(left[i], expected) && left[i] === right[i], 'sine through gain ramp at ' + i + ': ' + left[i] + ' vs ' + expected);
    }
    for (let i = 280; i < 290; i++) check(left[i] === 0, 'silence after stop at ' + i);
    check(left[290] === 0.125 && left[291] === -0.125 && left[292] === 0 && right[290] === 0.125, 'buffer playback');
    check(context.currentTime === 384 / 44100, 'currentTime advances by whole render quanta');
    await new Promise(resolve => context.addEventListener('statechange', () => context.state === 'closed' && resolve()));
    check(states.join() === 'running,closed' && context.state === 'closed', 'state changes: ' + states.join());
    check(order.join() === 'resolved,complete:true:true' && ended === 1, 'completion order: ' + order.join());
    await rejects('InvalidStateError', context.startRendering());
    throws('TypeError', () => new OfflineAudioCompletionEvent('complete', {}));
    const event = new OfflineAudioCompletionEvent('complete', {renderedBuffer: rendered});
    check(event.renderedBuffer === rendered && !event.isTrusted, 'constructed completion event');

    // suspend()/resume() at render quantum boundaries.
    const paused = new OfflineAudioContext(1, 512, 8000);
    await rejects('InvalidStateError', paused.suspend(0));
    await rejects('InvalidStateError', paused.suspend(1));
    await rejects('InvalidStateError', paused.resume());
    let suspendedAt = -1;
    paused.suspend(200 / 8000).then(() => { suspendedAt = paused.currentTime; paused.resume(); });
    const constant = paused.createBufferSource();
    const ones = paused.createBuffer(1, 512, 8000);
    ones.getChannelData(0).fill(1);
    constant.buffer = ones;
    constant.connect(paused.destination);
    constant.start();
    const pausedBuffer = await paused.startRendering();
    check(suspendedAt === 256 / 8000, 'suspension rounds up to a quantum: ' + suspendedAt);
    check(pausedBuffer.getChannelData(0).every(value => value === 1), 'resumed rendering continues');
    return 'offline-audio-ok';
})().then(value => globalThis.offlineAudioResult = value,
          error => globalThis.offlineAudioResult = 'ERROR:' + error.name + ':' + error.message);
