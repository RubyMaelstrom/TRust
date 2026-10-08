// HTML #media-elements and #timed-text-tracks with WebVTT's VTTCue and
// VTTRegion, for platform_fixture_in_window (`check` and `throws` come
// from the fixture wrapper). TRust has no inline decoder, so readyState stays
// HAVE_NOTHING and the current playback position stays 0.
const tasks = async () => { for (let i = 0; i < 4; i++) await new Promise(resolve => setTimeout(resolve, 0)); };
const video = document.createElement('video');
const events = [];
for (const type of ['volumechange', 'ratechange', 'loadstart', 'error', 'emptied', 'abort'])
    video.addEventListener(type, () => events.push(type));

// Interfaces.
for (const name of ['TextTrack', 'TextTrackList', 'TextTrackCue', 'TextTrackCueList', 'VTTCue', 'VTTRegion',
    'TrackEvent', 'AudioTrack', 'AudioTrackList', 'VideoTrack', 'VideoTrackList'])
    check(typeof window[name] === 'function', name + ' is exposed');
throws(() => new TextTrackCue(), 'TypeError', 'TextTrackCue is abstract');
throws(() => new TextTrack(), 'TypeError', 'TextTrack is not constructible');
check(VTTCue.length === 3 && TrackEvent.length === 1, 'constructor lengths');
throws(() => HTMLMediaElement.prototype.pause.call(document.body), 'TypeError', 'media brand check');

// Playback state (#dom-media-paused is readonly).
check(Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'paused').set === undefined && video.paused, 'paused is readonly');
check(video.readyState === 0 && video.duration !== video.duration && video.preservesPitch === true, 'initial state');
check(isNaN(video.getStartDate().getTime()), 'getStartDate is NaN');

// #dom-media-volume and #dom-media-muted
throws(() => { video.volume = 1.5; }, 'IndexSizeError', 'volume above 1');
throws(() => { video.volume = NaN; }, 'TypeError', 'volume is a double');
video.volume = 0.25;
video.volume = 0.25;
check(video.volume === 0.25, 'volume');
check(!video.muted && !video.defaultMuted, 'not muted');
video.setAttribute('muted', '');
check(video.muted && video.defaultMuted, 'the default muted state follows the attribute');
video.muted = false;
check(!video.muted && video.hasAttribute('muted'), 'muted setter overrides the attribute');

// #dom-media-playbackrate and #dom-media-defaultplaybackrate
throws(() => { video.playbackRate = Infinity; }, 'TypeError', 'playbackRate is a double');
video.playbackRate = 2;
video.defaultPlaybackRate = 0.5;
video.defaultPlaybackRate = 0.5;
check(video.playbackRate === 2 && video.defaultPlaybackRate === 0.5, 'rates');
await tasks();
check(events.join() === 'volumechange,volumechange,ratechange,ratechange', 'volume and rate events: ' + events.join());

// #dom-media-currenttime at HAVE_NOTHING and #dom-media-fastseek
video.currentTime = 5;
video.fastSeek(3);
check(video.currentTime === 5, 'default playback start position');
throws(() => video.fastSeek(), 'TypeError', 'fastSeek requires a time');

// Reflected attributes.
check(video.preload === 'metadata' && video.crossOrigin === null && video.loading === 'eager', 'attribute defaults');
video.preload = ''; check(video.preload === 'auto', 'preload empty is auto');
video.preload = 'NONE'; check(video.preload === 'none', 'preload is ASCII case-insensitive');
video.preload = 'bogus'; check(video.preload === 'metadata' && video.getAttribute('preload') === 'bogus', 'invalid preload');
video.crossOrigin = 'USE-credentials'; check(video.crossOrigin === 'use-credentials', 'crossOrigin known value');
video.crossOrigin = 'x'; check(video.crossOrigin === 'anonymous', 'crossOrigin invalid value');
video.crossOrigin = null; check(!video.hasAttribute('crossorigin'), 'crossOrigin null removes');
video.loop = true; video.autoplay = 1; video.controls = true;
check(video.hasAttribute('loop') && video.hasAttribute('autoplay') && video.controls, 'boolean reflection');

// The load algorithm resets playbackRate and runs resource selection.
events.length = 0;
video.srcObject = new Blob(['x']);
check(video.srcObject instanceof Blob && video.networkState === 3, 'srcObject starts loading');
throws(() => { video.srcObject = {}; }, 'TypeError', 'srcObject is a MediaProvider');
await tasks();
check(events.join() === 'ratechange,loadstart,error' && video.playbackRate === 0.5 && video.error.code === 4 &&
    video.currentSrc === '', 'load from a media provider: ' + events.join());
const empty = document.createElement('audio');
empty.pause();
check(empty.networkState === 3, 'pause() on an empty element selects a resource');
await tasks();
check(empty.networkState === 0, 'no resource leaves NETWORK_EMPTY');

// #text-track-api
const list = video.textTracks;
check(list === video.textTracks && list.length === 0 && video.audioTracks.length === 0 &&
    video.videoTracks.selectedIndex === -1, 'track lists');
throws(() => video.addTextTrack('bogus'), 'TypeError', 'TextTrackKind');
const added = [];
list.onaddtrack = event => added.push(event instanceof TrackEvent && event.track);
const track = video.addTextTrack('subtitles', 'English', 'en');
check(track instanceof TextTrack && track.kind === 'subtitles' && track.label === 'English' && track.language === 'en' &&
    track.id === '' && track.mode === 'hidden' && track.inBandMetadataTrackDispatchType === '', 'addTextTrack');
check(list.length === 1 && list[0] === track && list.getTrackById('') === track && list.getTrackById('x') === null,
    'TextTrackList indexed access');
check(Object.keys(list).join() === '0' && [...list][0] === track, 'TextTrackList own keys and iteration');
throws(() => Object.defineProperty(list, '0', {value: 1}), 'TypeError', 'indexed properties are read-only');
await tasks();
check(added.length === 1 && added[0] === track, 'addtrack event');

// TextTrack mode changes notify the list once per task.
let changes = 0;
list.onchange = () => changes++;
track.mode = 'bogus';
track.mode = 'showing';
track.mode = 'hidden';
await tasks();
check(track.mode === 'hidden' && changes === 1, 'change event');

// VTTCue (#dom-vttcue-vttcue) and its attributes.
throws(() => new VTTCue(0, 1), 'TypeError', 'VTTCue requires three arguments');
throws(() => new VTTCue(0, NaN, ''), 'TypeError', 'endTime NaN');
throws(() => new VTTCue(Infinity, 1, ''), 'TypeError', 'startTime is a double');
const cue = new VTTCue(1, 2, 'hi');
check(cue.id === '' && cue.track === null && !cue.pauseOnExit && cue.region === null && cue.vertical === '' &&
    cue.snapToLines && cue.line === 'auto' && cue.lineAlign === 'start' && cue.position === 'auto' &&
    cue.positionAlign === 'auto' && cue.size === 100 && cue.align === 'center' && cue.text === 'hi', 'VTTCue defaults');
cue.vertical = 'x'; cue.align = 'left'; cue.line = -2; cue.position = 50;
check(cue.vertical === '' && cue.align === 'left' && cue.line === -2 && cue.position === 50, 'VTTCue setters');
throws(() => { cue.line = 'top'; }, 'TypeError', 'line is a double or "auto"');
throws(() => { cue.position = 101; }, 'IndexSizeError', 'position range');
throws(() => { cue.size = -1; }, 'IndexSizeError', 'size range');
throws(() => { cue.region = {}; }, 'TypeError', 'region is a VTTRegion');
throws(() => { cue.endTime = -Infinity; }, 'TypeError', 'endTime -Infinity');
cue.endTime = Infinity;
check(cue.endTime === Infinity, 'unbounded cue');
cue.endTime = 2;
const region = new VTTRegion();
check(region.width === 100 && region.lines === 3 && region.regionAnchorX === 0 && region.regionAnchorY === 100 &&
    region.viewportAnchorX === 0 && region.viewportAnchorY === 100 && region.scroll === '' && region.id === '', 'VTTRegion defaults');
throws(() => { region.width = 101; }, 'IndexSizeError', 'region width range');
region.scroll = 'down'; region.lines = -1; region.scroll = 'up';
check(region.scroll === 'up' && region.lines === 4294967295, 'VTTRegion setters');
cue.region = region;
check(cue.region === region, 'cue region');

// Cue lists in text track cue order.
const early = new VTTCue(0, 5, 'a'), late = new VTTCue(1, 3, 'b'), past = new VTTCue(-5, -1, 'c');
early.id = 'early';
track.addCue(cue);
track.addCue(late);
track.addCue(early);
track.addCue(past);
const cues = track.cues;
check(cues === track.cues && cue.track === track && cues.length === 3 && cues[0] === early && cues[1] === late &&
    cues[2] === cue && cues.getCueById('early') === early && cues.getCueById('') === null, 'text track cue order');
const other = video.addTextTrack('metadata');
other.addCue(late);
check(late.track === other && cues.length === 2, 'addCue moves a cue');
throws(() => track.removeCue(late), 'NotFoundError', 'removeCue of a foreign cue');
throws(() => track.addCue({}), 'TypeError', 'addCue requires a TextTrackCue');
track.removeCue(past);
check(past.track === null, 'removeCue');
track.mode = 'disabled';
check(track.cues === null && track.activeCues === null, 'disabled tracks expose no cues');
track.mode = 'hidden';
await tasks();

// Time marches on once the show poster flag is unset (fastSeek runs the seek
// algorithm's first step): the current playback position 0 is inside early.
const cueEvents = [];
for (const each of [early, cue]) {
    each.onenter = () => cueEvents.push('enter:' + each.text);
    each.onexit = () => cueEvents.push('exit:' + each.text);
}
track.oncuechange = () => cueEvents.push('cuechange');
check(track.activeCues.length === 0, 'no active cues before time marches on');
video.fastSeek(0);
track.mode = 'showing';
check(track.activeCues.length === 0, 'activeCues reflects the start of the script');
await tasks();
check(cueEvents.join() === 'enter:a,cuechange' && track.activeCues.length === 1 && track.activeCues[0] === early,
    'enter: ' + cueEvents.join());
early.endTime = 0;
await tasks();
check(cueEvents.join() === 'enter:a,cuechange,exit:a,cuechange' && track.activeCues.length === 0,
    'exit: ' + cueEvents.join());

// #dom-vttcue-getcueashtml (WebVTT cue text parsing and DOM construction).
const html = new VTTCue(0, 1, '<c.yellow.big>a</c> &amp; <i>b<b>c</b></i><v Roger \t Bingham>hi</v>' +
    '<lang en>x</lang><ruby>r<rt>t</rt></ruby><00:01.500>z &lt;&gt; &notin; &#65;&#x42;&#0;&bogus; <x>y</x><1:2>');
const fragment = html.getCueAsHTML();
const holder = document.createElement('div');
holder.appendChild(fragment);
check(fragment instanceof DocumentFragment, 'getCueAsHTML returns a DocumentFragment');
const timestamp = holder.childNodes[6];
check(timestamp.nodeType === 7 && timestamp.target === 'timestamp' && timestamp.data === '00:00:01.500',
    'WebVTT Timestamp Object');
holder.removeChild(timestamp);
check(holder.innerHTML === '<span class="yellow big">a</span> &amp; <i>b<b>c</b></i><span title="Roger Bingham">hi</span>' +
    '<span lang="en">x</span><ruby>r<rt>t</rt></ruby>z &lt;&gt; \u2209 AB\ufffd&amp;bogus; y',
    'cue text DOM: ' + holder.innerHTML);

// #the-trackevent-interface
const trackEvent = new TrackEvent('addtrack', {track});
check(trackEvent.track === track && new TrackEvent('x').track === null, 'TrackEvent');
throws(() => new TrackEvent('x', {track: {}}), 'TypeError', 'TrackEvent track type');
