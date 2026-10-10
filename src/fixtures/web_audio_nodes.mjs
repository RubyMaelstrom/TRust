// Web Audio 1.1 (local WebAudio/web-audio-api@2047f16) processing nodes:
// #ConstantSourceNode, #stereopanner-algorithm, #ChannelSplitterNode,
// #ChannelMergerNode, #DelayNode-processing (and cycles, #rendering-loop),
// #filters-characteristics, #IIRFilterNode-filter-definition,
// #WaveShaperNode, #AnalyserNode, #ConvolverNode, #AudioListener and the
// channel count, mode and interpretation constraints.
(async function () {
    function check(ok, message) { if (!ok) throw Error(message); }
    function throws(name, fn) {
        try { fn(); } catch (e) { check(e.name === name, 'wrong error ' + e.name + ' for ' + fn); return; }
        throw Error('missing ' + name + ' for ' + fn);
    }
    const near = (a, b, epsilon = 1e-4) => Math.abs(a - b) <= epsilon;
    const RATE = 8000;
    const render = async (channels, length, build) => {
        const context = new OfflineAudioContext(channels, length, RATE);
        await build(context);
        const buffer = await context.startRendering();
        return [...Array(channels).keys()].map(c => buffer.getChannelData(c));
    };

    // ConstantSourceNode with automation.
    let [out] = await render(1, 256, context => {
        const source = new ConstantSourceNode(context, {offset: 0.25});
        source.offset.setValueAtTime(0.75, 128 / RATE);
        source.connect(context.destination);
        source.start(16 / RATE);
        source.stop(200 / RATE);
    });
    check(out[15] === 0 && out[16] === 0.25 && out[127] === 0.25 && out[128] === 0.75 && out[199] === 0.75 &&
        out[200] === 0, 'constant source: ' + [out[15], out[16], out[128], out[200]]);

    // StereoPannerNode equal-power panning of mono and stereo input.
    let [left, right] = await render(2, 128, context => {
        const source = new ConstantSourceNode(context, {offset: 1});
        const panner = new StereoPannerNode(context, {pan: 0});
        panner.pan.setValueAtTime(-1, 32 / RATE);
        panner.pan.setValueAtTime(1, 64 / RATE);
        panner.pan.setValueAtTime(0.5, 96 / RATE);
        source.connect(panner).connect(context.destination);
        source.start();
    });
    check(near(left[0], Math.SQRT1_2) && near(right[0], Math.SQRT1_2), 'centered mono');
    check(near(left[32], 1) && near(right[32], 0) && near(left[64], 0) && near(right[64], 1), 'hard pans');
    check(near(left[96], Math.cos(0.75 * Math.PI / 2)) && near(right[96], Math.sin(0.75 * Math.PI / 2)), 'half right');
    [left, right] = await render(2, 128, context => {
        const merger = new ChannelMergerNode(context, {numberOfInputs: 2});
        const a = new ConstantSourceNode(context, {offset: 0.5}), b = new ConstantSourceNode(context, {offset: 0.25});
        a.connect(merger, 0, 0); b.connect(merger, 0, 1);
        const panner = new StereoPannerNode(context, {pan: -0.5});
        merger.connect(panner).connect(context.destination);
        a.start(); b.start();
    });
    // Stereo, pan <= 0: x = pan + 1, L = inL + inR * gainL, R = inR * gainR.
    check(near(left[0], 0.5 + 0.25 * Math.cos(0.5 * Math.PI / 2)) && near(right[0], 0.25 * Math.sin(0.5 * Math.PI / 2)),
        'stereo panning');

    // Splitter and merger: swap two channels.
    [left, right] = await render(2, 128, context => {
        const merger = new ChannelMergerNode(context, {numberOfInputs: 2});
        const a = new ConstantSourceNode(context, {offset: 0.5}), b = new ConstantSourceNode(context, {offset: -0.25});
        a.connect(merger, 0, 0); b.connect(merger, 0, 1);
        const splitter = context.createChannelSplitter(2), swap = context.createChannelMerger(2);
        merger.connect(splitter);
        splitter.connect(swap, 0, 1); splitter.connect(swap, 1, 0);
        swap.connect(context.destination);
        a.start(); b.start();
        check(splitter.numberOfOutputs === 2 && splitter.channelCount === 2 &&
            splitter.channelInterpretation === 'discrete' && swap.numberOfInputs === 2 && swap.channelCount === 1,
            'splitter and merger shapes');
    });
    check(left[5] === -0.25 && right[5] === 0.5, 'swapped channels');

    // DelayNode: exact delays, interpolation, and a feedback cycle.
    [out] = await render(1, 1024, context => {
        const impulse = context.createBuffer(1, 1, RATE);
        impulse.getChannelData(0)[0] = 1;
        const source = new AudioBufferSourceNode(context, {buffer: impulse});
        // 1/16 s is exactly 500 frames (a float delayTime, so binary).
        const delay = new DelayNode(context, {delayTime: 1 / 16, maxDelayTime: 1});
        source.connect(delay).connect(context.destination);
        source.start();
    });
    check(out[499] === 0 && out[500] === 1 && out[501] === 0, 'impulse delayed by 500 frames: ' +
        [out[499], out[500], out[501]]);
    [out] = await render(1, 1024, context => {
        const impulse = context.createBuffer(1, 1, RATE);
        impulse.getChannelData(0)[0] = 1;
        const source = new AudioBufferSourceNode(context, {buffer: impulse});
        // A delay shorter than a render quantum is clamped to one in a cycle.
        const delay = new DelayNode(context, {delayTime: 5 / RATE});
        const feedback = new GainNode(context, {gain: 0.5});
        source.connect(delay);
        delay.connect(feedback).connect(delay);
        delay.connect(context.destination);
        source.start();
    });
    check(out[0] === 0 && out[128] === 1 && out[256] === 0.5 && out[384] === 0.25 && out[129] === 0,
        'feedback echoes every render quantum: ' + [out[128], out[256], out[384]]);
    [out] = await render(1, 256, context => {
        const source = new ConstantSourceNode(context);
        const a = new GainNode(context), b = new GainNode(context);
        source.connect(a).connect(b).connect(a);
        b.connect(context.destination);
        source.start();
    });
    check(out.every(value => value === 0), 'a cycle without a delay is muted');
    throws('NotSupportedError', () => new OfflineAudioContext(1, 1, RATE).createDelay(0));
    throws('NotSupportedError', () => new OfflineAudioContext(1, 1, RATE).createDelay(180));

    // BiquadFilterNode: DC through lowpass and highpass, responses.
    [left, right] = await render(2, 2048, context => {
        const source = new ConstantSourceNode(context);
        const merger = new ChannelMergerNode(context, {numberOfInputs: 2});
        const lowpass = new BiquadFilterNode(context, {frequency: 500});
        const highpass = new BiquadFilterNode(context, {type: 'highpass', frequency: 500});
        source.connect(lowpass).connect(merger, 0, 0);
        source.connect(highpass).connect(merger, 0, 1);
        merger.connect(context.destination);
        source.start();
        check(lowpass.type === 'lowpass' && highpass.type === 'highpass' && lowpass.frequency.value === 500 &&
            lowpass.Q.value === 1 && lowpass.gain.value === 0 && lowpass.frequency.maxValue === RATE / 2, 'biquad attributes');
        const frequencies = new Float32Array([0, 500, RATE, -1]), magnitude = new Float32Array(4), phase = new Float32Array(4);
        lowpass.getFrequencyResponse(frequencies, magnitude, phase);
        // Q is in dB for lowpass: the gain at the cutoff is 10^(1/20).
        check(near(magnitude[0], 1) && near(magnitude[1], Math.pow(10, 1 / 20), 1e-3) && isNaN(magnitude[2]) && isNaN(phase[3]),
            'lowpass response: ' + [...magnitude]);
        throws('InvalidAccessError', () => lowpass.getFrequencyResponse(frequencies, new Float32Array(3), phase));
        throws('TypeError', () => lowpass.getFrequencyResponse([0], magnitude, phase));
        lowpass.type = 'bogus';
        check(lowpass.type === 'lowpass', 'invalid filter type ignored');
    });
    check(near(left[2047], 1, 1e-3) && near(right[2047], 0, 1e-3), 'DC: ' + [left[2047], right[2047]]);

    // IIRFilterNode: y(n) = 0.5 x(n) + 0.5 y(n-1).
    [out] = await render(1, 128, context => {
        const source = new ConstantSourceNode(context);
        const filter = context.createIIRFilter([0.5], [1, -0.5]);
        source.connect(filter).connect(context.destination);
        source.start();
        const magnitude = new Float32Array(1), phase = new Float32Array(1);
        filter.getFrequencyResponse(new Float32Array([0]), magnitude, phase);
        check(near(magnitude[0], 1) && near(phase[0], 0), 'IIR DC response');
        throws('NotSupportedError', () => context.createIIRFilter([], [1]));
        throws('NotSupportedError', () => context.createIIRFilter(new Array(21).fill(1), [1]));
        throws('InvalidStateError', () => context.createIIRFilter([0, 0], [1]));
        throws('InvalidStateError', () => context.createIIRFilter([1], [0, 1]));
    });
    check(out[0] === 0.5 && out[1] === 0.75 && out[2] === 0.875, 'IIR recursion: ' + [out[0], out[1], out[2]]);

    // WaveShaperNode: interpolation, identity, and a curve that makes DC.
    [left, right] = await render(2, 128, context => {
        const source = new ConstantSourceNode(context, {offset: 0.5});
        const merger = new ChannelMergerNode(context, {numberOfInputs: 2});
        const shaper = new WaveShaperNode(context, {curve: [-1, 0, 2]});
        const offset = context.createWaveShaper();
        offset.curve = new Float32Array([0, 1]);
        throws('InvalidStateError', () => { offset.curve = new Float32Array([0, 1]); });
        throws('InvalidStateError', () => { context.createWaveShaper().curve = new Float32Array(1); });
        source.connect(shaper).connect(merger, 0, 0);
        offset.connect(merger, 0, 1);
        merger.connect(context.destination);
        source.start();
        check(shaper.curve instanceof Float32Array && shaper.curve.join() === '-1,0,2' && shaper.oversample === 'none',
            'shaper attributes');
        shaper.oversample = '3x';
        check(shaper.oversample === 'none', 'invalid oversample ignored');
    });
    check(left[0] === 1 && right[0] === 0.5, 'shaped: ' + [left[0], right[0]]);

    // AnalyserNode, read while rendering is suspended.
    {
        const context = new OfflineAudioContext(1, 4096, RATE);
        const source = new ConstantSourceNode(context, {offset: 0.5});
        const analyser = context.createAnalyser();
        analyser.fftSize = 256;
        analyser.smoothingTimeConstant = 0;
        source.connect(analyser);
        source.start();
        throws('IndexSizeError', () => { analyser.fftSize = 100; });
        throws('IndexSizeError', () => { analyser.minDecibels = -10; });
        throws('IndexSizeError', () => { analyser.smoothingTimeConstant = 2; });
        let time, bytes, frequency;
        context.suspend(1024 / RATE).then(() => {
            time = new Float32Array(analyser.fftSize);
            analyser.getFloatTimeDomainData(time);
            bytes = new Uint8Array(4);
            analyser.getByteTimeDomainData(bytes);
            frequency = new Float32Array(analyser.frequencyBinCount);
            analyser.getFloatFrequencyData(frequency);
            context.resume();
        });
        await context.startRendering();
        check(time.every(value => value === 0.5) && bytes.join() === '192,192,192,192', 'time domain: ' + bytes.join());
        // DC concentrates in bin 0 (Blackman window: 0.42 of 0.5).
        check(near(frequency[0], 20 * Math.log10(0.21), 0.05) && frequency[0] > frequency[10],
            'frequency data: ' + frequency[0]);
    }

    // ConvolverNode: exact linear convolution without normalization.
    [out] = await render(1, 256, context => {
        const impulse = context.createBuffer(1, 1, RATE);
        impulse.getChannelData(0)[0] = 1;
        const response = context.createBuffer(1, 3, RATE);
        response.getChannelData(0).set([1, 0.5, -0.25]);
        const convolver = new ConvolverNode(context, {buffer: response, disableNormalization: true});
        const source = new AudioBufferSourceNode(context, {buffer: impulse});
        source.connect(convolver).connect(context.destination);
        source.start(4 / RATE);
        check(convolver.buffer === response && !convolver.normalize, 'convolver attributes');
        throws('NotSupportedError', () => { convolver.buffer = context.createBuffer(3, 1, RATE); });
        throws('NotSupportedError', () => { convolver.buffer = context.createBuffer(1, 1, RATE * 2); });
        throws('NotSupportedError', () => { convolver.channelCount = 3; });
        throws('NotSupportedError', () => { convolver.channelCountMode = 'max'; });
    });
    // FFT convolution: exact up to rounding.
    check(near(out[3], 0, 1e-6) && near(out[4], 1, 1e-6) && near(out[5], 0.5, 1e-6) && near(out[6], -0.25, 1e-6) &&
        near(out[7], 0, 1e-6), 'convolution: ' + [...out.slice(3, 8)]);

    // Channel constraints and AudioListener.
    const context = new OfflineAudioContext(1, 128, RATE);
    throws('InvalidStateError', () => { context.createChannelMerger().channelCount = 2; });
    throws('InvalidStateError', () => { context.createChannelMerger().channelCountMode = 'max'; });
    throws('InvalidStateError', () => { context.createChannelSplitter().channelInterpretation = 'speakers'; });
    throws('InvalidStateError', () => { context.createChannelSplitter(3).channelCount = 2; });
    throws('IndexSizeError', () => context.createChannelSplitter(0));
    throws('IndexSizeError', () => context.createChannelMerger(33));
    throws('NotSupportedError', () => { context.createStereoPanner().channelCount = 3; });
    throws('NotSupportedError', () => { context.createStereoPanner().channelCountMode = 'max'; });
    const listener = context.listener;
    check(listener === context.listener && listener instanceof AudioListener && listener.forwardZ.value === -1 &&
        listener.upY.value === 1, 'listener');
    listener.setPosition(1, 2, 3);
    check(listener.positionX.value === 1 && listener.positionZ.value === 3, 'setPosition');
    throws('TypeError', () => new AudioListener());
    check(typeof context.createPanner === 'undefined' && typeof PannerNode === 'undefined', 'no PannerNode');
    return 'web-audio-nodes-ok';
})().then(value => globalThis.webAudioNodesResult = value,
          error => globalThis.webAudioNodesResult = 'ERROR:' + error.name + ':' + error.message);
