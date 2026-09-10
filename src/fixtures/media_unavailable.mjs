function assertMedia(condition, message) {
    if (!condition) throw new Error(message);
}
var media = document.createElement('video');
var mediaEvents = [];
assertMedia(media.networkState === media.NETWORK_EMPTY && media.error === null, 'initial media state');
for (const type of ['video/mp4', 'video/webm;codecs="vp9"', 'audio/mpeg',
    'application/octet-stream', 'application/vnd.apple.mpegurl', '']) {
    assertMedia(media.canPlayType(type) === '', 'no inline codec: ' + type);
}
var mediaTypeConversions = 0;
media.canPlayType({toString() { mediaTypeConversions++; return 'video/mp4'; }});
assertMedia(mediaTypeConversions === 1, 'DOMString conversion');
for (const type of ['loadstart', 'error', 'abort', 'emptied']) {
    media.addEventListener(type, event => {
        assertMedia(event.isTrusted && !event.bubbles && !event.cancelable, 'UA media event');
        mediaEvents.push(type);
    });
}
media.src = '/old.mp4';
media.src = '/new.webm';
media.load();
assertMedia(mediaEvents.length === 0 && media.error === null, 'load must not fire synchronously');
var sourceMedia = document.createElement('audio');
var sourceErrors = [];
for (const name of ['one', 'two']) {
    const source = document.createElement('source');
    source.src = name + '.mp3';
    source.type = 'audio/mpeg';
    source.addEventListener('error', event => sourceErrors.push(name + ':' + event.isTrusted));
    sourceMedia.appendChild(source);
}
sourceMedia.addEventListener('error', () => { throw new Error('source error must not bubble'); });
sourceMedia.load();
'media-unavailable-pending'
