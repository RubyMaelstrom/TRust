// Original Credential Management, Clipboard API, Media Capture and Web Speech
// synthesis regressions. TRust has no credential types, capture devices,
// display sources or voices, and reads no clipboard data.
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
    function interfaceShape(C, name, parent) {
        check(typeof C === 'function' && C.name === name, 'interface ' + name);
        check(Object.getPrototypeOf(C) === (parent || Function.prototype), 'interface inheritance ' + name);
        const own = Object.getOwnPropertyDescriptor(globalThis, name);
        check(own && !own.enumerable && own.writable && own.configurable, 'global property ' + name);
        check(Object.prototype.toString.call(C.prototype) === '[object ' + name + ']', 'toStringTag ' + name);
        for (const key of Object.getOwnPropertyNames(C.prototype)) {
            if (key === 'constructor') continue;
            check(Object.getOwnPropertyDescriptor(C.prototype, key).enumerable, 'enumerable ' + name + '.' + key);
        }
    }
    function noInternals(object, label) {
        check(!Reflect.ownKeys(object).some(key => typeof key === 'string' && (key.startsWith('__') || /trust/i.test(key))),
            'no internals on ' + label);
    }
    function navigatorAttribute(key) {
        const descriptor = Object.getOwnPropertyDescriptor(Navigator.prototype, key);
        check(descriptor && descriptor.get && !descriptor.set && descriptor.enumerable && descriptor.configurable,
            'Navigator attribute ' + key);
        throws('TypeError', () => descriptor.get.call({}));
        check(navigator[key] === navigator[key], 'SameObject ' + key);
        check(Object.getOwnPropertyNames(navigator[key]).length === 0, 'no own properties ' + key);
        return navigator[key];
    }

    // Credential Management.
    interfaceShape(Credential, 'Credential');
    interfaceShape(CredentialsContainer, 'CredentialsContainer');
    check(typeof PasswordCredential === 'undefined' && typeof FederatedCredential === 'undefined'
        && typeof PublicKeyCredential === 'undefined', 'no unsupported credential types');
    throws('TypeError', () => new Credential());
    throws('TypeError', () => new CredentialsContainer());
    check(await Credential.isConditionalMediationAvailable() === false, 'no conditional mediation');
    const credentials = navigatorAttribute('credentials');
    check(credentials instanceof CredentialsContainer, 'CredentialsContainer instance');
    await rejects('NotSupportedError', credentials.get());
    await rejects('NotSupportedError', credentials.get({password: true, mediation: 'silent'}));
    await rejects('TypeError', credentials.get({mediation: 'never'}));
    await rejects('TypeError', credentials.get({signal: {}}));
    const aborted = AbortSignal.abort(new Error('stop'));
    check((await rejects('Error', credentials.get({signal: aborted}))) === aborted.reason, 'abort reason');
    await rejects('NotSupportedError', credentials.create({publicKey: {}}));
    await rejects('TypeError', credentials.store());
    await rejects('TypeError', credentials.store({id: 'user', type: 'password'}));
    check(await credentials.preventSilentAccess() === undefined, 'preventSilentAccess resolves');
    await rejects('TypeError', CredentialsContainer.prototype.get.call({}));

    // Clipboard API.
    interfaceShape(Clipboard, 'Clipboard', EventTarget);
    interfaceShape(ClipboardItem, 'ClipboardItem');
    throws('TypeError', () => new Clipboard());
    const clipboard = navigatorAttribute('clipboard');
    check(clipboard instanceof Clipboard && clipboard instanceof EventTarget, 'Clipboard instance');
    check(ClipboardItem.supports('text/plain') && ClipboardItem.supports('text/html') && ClipboardItem.supports('image/png')
        && !ClipboardItem.supports('text/csv') && !ClipboardItem.supports('web text/csv'), 'supported data types');
    throws('TypeError', () => new ClipboardItem({}));
    throws('TypeError', () => new ClipboardItem({'not a type': 'x'}));
    throws('TypeError', () => new ClipboardItem({'text/plain': 'a', 'TEXT/PLAIN': 'b'}));
    throws('TypeError', () => new ClipboardItem({'text/plain': 'a'}, {presentationStyle: 'popup'}));
    const item = new ClipboardItem({'Text/Plain': 'hello', 'web text/csv': Promise.resolve('a,b')},
        {presentationStyle: 'inline'});
    check(item.types === item.types && Object.isFrozen(item.types) && item.types.join() === 'text/plain,web text/csv',
        'serialized frozen types');
    check(item.presentationStyle === 'inline', 'presentation style');
    noInternals(item, 'ClipboardItem');
    const plain = await item.getType('text/plain');
    check(plain instanceof Blob && plain.type === 'text/plain' && await plain.text() === 'hello', 'string representation');
    check(await (await item.getType('web text/csv')).text() === 'a,b', 'custom representation');
    await rejects('NotFoundError', item.getType('text/html'));
    await rejects('TypeError', item.getType('bad'));
    await rejects('NotAllowedError', clipboard.readText());
    await rejects('NotAllowedError', clipboard.read());
    await rejects('NotAllowedError', clipboard.read({unsanitized: ['image/png']}));
    await rejects('NotAllowedError', clipboard.writeText('no gesture'));
    await rejects('NotAllowedError', clipboard.write([item]));
    await rejects('TypeError', clipboard.write([{}]));
    await rejects('TypeError', clipboard.writeText());
    await rejects('TypeError', Clipboard.prototype.readText.call({}));

    // Media Capture and Streams / Screen Capture.
    interfaceShape(MediaDevices, 'MediaDevices', EventTarget);
    throws('TypeError', () => new MediaDevices());
    const media = navigatorAttribute('mediaDevices');
    check(media instanceof MediaDevices, 'MediaDevices instance');
    const devices = await media.enumerateDevices();
    check(Array.isArray(devices) && devices.length === 0, 'no capture devices');
    const supported = media.getSupportedConstraints();
    check(Object.getPrototypeOf(supported) === Object.prototype && Object.keys(supported).length === 0,
        'no constrainable properties');
    await rejects('TypeError', media.getUserMedia());
    await rejects('TypeError', media.getUserMedia({audio: false, video: false}));
    await rejects('TypeError', media.getUserMedia({video: {advanced: 1}}));
    await rejects('NotFoundError', media.getUserMedia({audio: true}));
    await rejects('NotFoundError', media.getUserMedia({video: {width: 640}}));
    await rejects('NotFoundError', media.getUserMedia({audio: null}));
    await rejects('InvalidStateError', media.getDisplayMedia());
    let changes = 0;
    media.ondevicechange = () => changes++;
    media.dispatchEvent(new Event('devicechange'));
    check(changes === 1 && typeof media.ondevicechange === 'function', 'ondevicechange');
    await rejects('TypeError', MediaDevices.prototype.enumerateDevices.call({}));

    // Web Speech API synthesis.
    interfaceShape(SpeechSynthesis, 'SpeechSynthesis', EventTarget);
    interfaceShape(SpeechSynthesisUtterance, 'SpeechSynthesisUtterance', EventTarget);
    interfaceShape(SpeechSynthesisVoice, 'SpeechSynthesisVoice');
    interfaceShape(SpeechSynthesisEvent, 'SpeechSynthesisEvent', Event);
    interfaceShape(SpeechSynthesisErrorEvent, 'SpeechSynthesisErrorEvent', SpeechSynthesisEvent);
    const synth = speechSynthesis;
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'speechSynthesis');
    check(descriptor && descriptor.get && !descriptor.set && descriptor.enumerable, 'Window attribute');
    check(synth === speechSynthesis && synth instanceof SpeechSynthesis, 'SameObject SpeechSynthesis');
    noInternals(synth, 'SpeechSynthesis');
    throws('TypeError', () => new SpeechSynthesis());
    throws('TypeError', () => new SpeechSynthesisVoice());
    const voices = synth.getVoices();
    check(Array.isArray(voices) && voices.length === 0 && voices !== synth.getVoices(), 'no voices');
    check(!synth.pending && !synth.speaking && !synth.paused, 'idle synthesis');
    const utterance = new SpeechSynthesisUtterance('Hello');
    check(SpeechSynthesisUtterance.length === 0 && new SpeechSynthesisUtterance().text === '', 'optional text');
    check(utterance.text === 'Hello' && utterance.lang === '' && utterance.voice === null && utterance.volume === 1
        && utterance.rate === 1 && utterance.pitch === 1, 'utterance defaults');
    utterance.rate = 1.1;
    check(utterance.rate === Math.fround(1.1), 'float attribute');
    throws('TypeError', () => { utterance.pitch = NaN; });
    throws('TypeError', () => { utterance.voice = {}; });
    noInternals(utterance, 'SpeechSynthesisUtterance');
    throws('TypeError', () => synth.speak({}));
    throws('TypeError', () => new SpeechSynthesisEvent('start'));
    throws('TypeError', () => new SpeechSynthesisEvent('start', {}));
    throws('TypeError', () => new SpeechSynthesisErrorEvent('error', {utterance}));
    throws('TypeError', () => new SpeechSynthesisErrorEvent('error', {utterance, error: 'oops'}));
    const constructed = new SpeechSynthesisErrorEvent('error', {utterance, error: 'network', charIndex: 3, name: 'n'});
    check(constructed.utterance === utterance && constructed.error === 'network' && constructed.charIndex === 3
        && constructed.charLength === 0 && constructed.name === 'n' && !constructed.isTrusted, 'constructed event');
    const events = [];
    const failed = new Promise(resolve => {
        utterance.onerror = event => { events.push(event.type + ':' + event.error + ':' + event.isTrusted
            + ':' + (event instanceof SpeechSynthesisErrorEvent) + ':' + (event.utterance === utterance)); resolve(); };
    });
    utterance.onstart = () => events.push('start');
    synth.speak(utterance);
    check(synth.pending && !synth.speaking, 'queued utterance is pending');
    await failed;
    check(events.join() === 'error:synthesis-unavailable:true:true:true' && !synth.pending, 'synthesis unavailable');
    synth.pause();
    const queued = new SpeechSynthesisUtterance('Later');
    const canceled = new Promise(resolve => { queued.onerror = event => resolve(event.error); });
    synth.speak(queued);
    check(synth.paused && synth.pending, 'paused queue keeps the utterance');
    synth.cancel();
    check(!synth.pending && synth.paused, 'cancel empties the queue and keeps paused');
    check(await canceled === 'canceled', 'cancel fails queued utterances');
    synth.resume();
    check(!synth.paused, 'resume');
    let voiceChanges = 0;
    synth.onvoiceschanged = () => voiceChanges++;
    synth.dispatchEvent(new Event('voiceschanged'));
    check(voiceChanges === 1, 'onvoiceschanged');
    for (const C of [Credential, CredentialsContainer, Clipboard, ClipboardItem, MediaDevices, SpeechSynthesis,
        SpeechSynthesisUtterance, SpeechSynthesisEvent])
        noInternals(C.prototype, C.name + '.prototype');
    return 'window-capabilities-ok';
})().then(value => globalThis.capabilityResult = value,
          error => globalThis.capabilityResult = 'ERROR:' + error.name + ':' + error.message);
