// Headless harness for an Emscripten em-dosbox build (archive.org's DOS emulation: Emscripten
// 1.37.3 asm.js, `dosbox-sync.js` and its `.mem` file). Runs the emulator's own JavaScript with
// stub DOM/canvas/audio objects under node or lumen-cli, so engine changes can be timed without
// a browser. See DIAGNOSTICS.md, "DOS emulation benchmark".
//
//   DOSBOX_JS=dosbox-sync.js DOSBOX_MEM=dosbox-sync.mem GAME_DIR=./c [EXIT_ON_GFX=1] \
//   [FRAME_PNG=out.png] node tools/em_dosbox_harness.js [seconds] [dosbox args...]
//
// DOSBOX_CYCLES sets DOSBox's [cpu] cycles (default `max`). `max` adapts the emulated CPU to
// host timing, so one slow host tick (a collection, a large compile) can throttle the guest for
// many seconds and the work to reach a screen varies run to run. For engine A/B comparisons use a
// fixed count the host cannot keep up with (`fixed 100000`): every tick then runs the same guest
// work, and the host's user cycles to the title measure the engine.
//
// GAME_DIR is mounted as drive C:. The default program is DOOM.EXE; other arguments are passed
// to DOSBox. The first frame with at least 40 distinct sampled colors (graphics mode) is
// reported as `graphics ... first seen at t=...`. EXIT_ON_GFX ends the run there, or 4 s later
// with FRAME_PNG so the frame written is the settled title screen.
'use strict';
var G = globalThis; G.require = require;
var fs = require('fs');
var path = require('path');
var DOSBOX_JS = process.env.DOSBOX_JS, DOSBOX_MEM = process.env.DOSBOX_MEM, GAME_DIR = process.env.GAME_DIR;
if (!DOSBOX_JS || !DOSBOX_MEM || !GAME_DIR) throw new Error('set DOSBOX_JS, DOSBOX_MEM and GAME_DIR');
var seconds = +(process.argv[2] || 60);
var extra = process.argv.slice(3);
var t0 = Date.now();
function now() { return Date.now() - t0; }

// ---- browser stubs ----
var frames = 0, lastFrames = 0, lastImg = null, gfxAt = 0;
function detectGfx(img) {
  var d = new Uint32Array(img.data.buffer, img.data.byteOffset, img.data.length >> 2), seen = {}, n = 0;
  for (var i = 0; i < d.length; i += 997) { var c = d[i]; if (!seen[c]) { seen[c] = 1; n++; } }
  if (n >= 40) {
    gfxAt = now(); console.log('[harness] graphics (distinct colors ' + n + ') first seen at t=' + (gfxAt / 1000).toFixed(1) + 's frames=' + frames);
    // End at once, so instruction counts stop at the title rather than include its idle loop.
    if (process.env.EXIT_ON_GFX && !process.env.FRAME_PNG) { console.log('[harness] done frames=' + frames); process.exit(0); }
  }
}
var evh = function () {};
function El(tag) {
  var e = { tagName: String(tag).toUpperCase(), style: { removeProperty: function () {}, setProperty: function () {} }, children: [], width: 640, height: 400,
    addEventListener: evh, removeEventListener: evh, setAttribute: function () {}, getAttribute: function () { return null; },
    appendChild: function (c) { this.children.push(c); return c; }, removeChild: function () {}, parentNode: null,
    getBoundingClientRect: function () { return { left: 0, top: 0, width: 640, height: 400, right: 640, bottom: 400 }; },
    requestPointerLock: function () {}, classList: { add: function () {}, remove: function () {} } };
  if (tag === 'canvas') e.getContext = function (t) {
    if (t !== '2d') return null;
    var self = e;
    return { canvas: self, createImageData: function (w, h) { return { width: w, height: h, data: new Uint8ClampedArray(w * h * 4) }; },
      putImageData: function (img) { frames++; lastImg = img; if (!gfxAt) detectGfx(img); }, drawImage: function () {}, fillRect: function () {}, clearRect: function () {},
      getImageData: function (x, y, w, h) { return { width: w, height: h, data: new Uint8ClampedArray(w * h * 4) }; },
      save: function () {}, restore: function () {}, fillStyle: '', imageSmoothingEnabled: false };
  };
  return e;
}
var canvas = El('canvas');
G.window = G;
G.document = { createElement: El, getElementById: function () { return canvas; }, addEventListener: evh, removeEventListener: evh,
  body: El('body'), documentElement: El('html'), querySelector: function () { return canvas; }, hidden: false, readyState: 'complete',
  mozFullScreenElement: null, fullscreenElement: null, location: { href: 'http://x/' }, title: '' };
Object.defineProperty(G, 'navigator', {value: { userAgent: 'harness', language: 'en-US', platform: 'harness', appVersion: 'harness', vendor: '', product: '', getGamepads: function () { return []; }, maxTouchPoints: 0 }, configurable: true, writable: true});
G.screen = { width: 1024, height: 768 };
G.addEventListener = evh; G.removeEventListener = evh;
if (!G.location) G.location = { href: 'http://x/', origin: 'http://x', search: '', hostname: 'x', protocol: 'http:' };
G.requestAnimationFrame = function (cb) { return setTimeout(function () { cb(now()); }, 16); };
G.cancelAnimationFrame = function (id) { clearTimeout(id); };
// Audio: SDL2 uses a ScriptProcessorNode; the harness drives its callback in real time.
var audioNodes = [];
function AudioContextStub() { this.sampleRate = 48000; this.destination = {}; }
AudioContextStub.prototype.createScriptProcessor = function (bufSize, inCh, outCh) {
  var node = { bufferSize: bufSize, outCh: outCh, onaudioprocess: null, connect: function () {}, disconnect: function () {} };
  audioNodes.push(node); return node;
};
AudioContextStub.prototype.createBuffer = function (ch, len, rate) { var chs = []; for (var i = 0; i < ch; i++) chs.push(new Float32Array(len)); return { getChannelData: function (i) { return chs[i]; } }; };
G.AudioContext = AudioContextStub;

var printed = [];
var lines = 0;
var seen = {};
G.Module = {
  noInitialRun: false,
  canvas: canvas,
  arguments: null,
  print: function (t) { console.log('[dosbox] ' + t); },
  printErr: function (t) { console.log('[dosbox] ' + t); },
  locateFile: function (f) { return f === 'dosbox.html.mem' ? DOSBOX_MEM : f; },
  screenIsReadOnly: true,
  ENVIRONMENT: 'NODE',
  preRun: [function () {
    var FS = G.FS;
    // mount the game as /emulator/c (copy into MEMFS)
    FS.mkdir('/emulator'); FS.mkdir('/emulator/c');
    var base = GAME_DIR;
    (function copy(dir, vdir) {
      fs.readdirSync(dir).forEach(function (n) {
        var p = dir + '/' + n, v = vdir + '/' + n, st = fs.statSync(p);
        if (st.isDirectory()) { FS.mkdir(v); copy(p, v); }
        else FS.writeFile(v, new Uint8Array(fs.readFileSync(p)), {encoding: 'binary'});
      });
    })(base, '/emulator/c');
    FS.writeFile('/emulator/dosbox.conf', '[sdl]\nfullscreen=false\n[cpu]\ncore=auto\ncycles=' + (process.env.DOSBOX_CYCLES || 'max') + '\n[mixer]\nnosound=false\n[midi]\nmpu401=none\n');
  }]
};
var args = ['-conf', '/emulator/dosbox.conf', '-c', 'mount c /emulator/c', '-c', 'c:'];
args = args.concat(extra.length ? extra : ['-c', 'DOOM.EXE']);
G.Module.arguments = args;

// audio pump: ~21 callbacks per second like the browser
var AUD_FRAME_MS = 2048 / 48000 * 1000;
var pump = function () {
  for (var i = 0; i < audioNodes.length; i++) {
    var n = audioNodes[i];
    if (n.onaudioprocess) {
      var chans = [];
      for (var c = 0; c < n.outCh; c++) chans.push(new Float32Array(n.bufferSize));
      n.onaudioprocess({ outputBuffer: { getChannelData: function (c) { return chans[c]; }, length: n.bufferSize, numberOfChannels: n.outCh, sampleRate: 48000 } });
    }
  }
  setTimeout(pump, AUD_FRAME_MS);
};
setTimeout(pump, 500);

function dumpPng(file) {
  if (!file || !lastImg) return;
  var w = lastImg.width, h = lastImg.height, d = lastImg.data;
  var rawLen = (w * 4 + 1) * h, raw = new Uint8Array(rawLen);
  for (var y = 0; y < h; y++) { raw[y * (w * 4 + 1)] = 0; raw.set(d.subarray(y * w * 4, (y + 1) * w * 4), y * (w * 4 + 1) + 1); }
  var tbl = []; for (var n = 0; n < 256; n++) { var c = n; for (var k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; tbl[n] = c >>> 0; }
  function crc32(bufs) { var crc = 0xffffffff; for (var j = 0; j < bufs.length; j++) { var buf = bufs[j]; for (var i = 0; i < buf.length; i++) crc = tbl[(crc ^ buf[i]) & 0xff] ^ (crc >>> 8); } return (crc ^ 0xffffffff) >>> 0; }
  function u32(v) { return new Uint8Array([(v >>> 24) & 255, (v >>> 16) & 255, (v >>> 8) & 255, v & 255]); }
  function chunk(type, data) { var t = new Uint8Array([type.charCodeAt(0), type.charCodeAt(1), type.charCodeAt(2), type.charCodeAt(3)]); return [u32(data.length), t, data, u32(crc32([t, data]))]; }
  // stored deflate blocks
  var blocks = [new Uint8Array([0x78, 0x01])], a1 = 1, b1 = 0;
  for (var i = 0; i < raw.length; i++) { a1 = (a1 + raw[i]) % 65521; b1 = (b1 + a1) % 65521; }
  for (var off = 0; off < raw.length; off += 65535) {
    var len = Math.min(65535, raw.length - off), last = off + len >= raw.length ? 1 : 0;
    blocks.push(new Uint8Array([last, len & 255, len >> 8, (~len) & 255, ((~len) >> 8) & 255])); blocks.push(raw.subarray(off, off + len));
  }
  blocks.push(u32(((b1 << 16) | a1) >>> 0));
  var idat = 0; blocks.forEach(function (x) { idat += x.length; }); var z = new Uint8Array(idat), p = 0; blocks.forEach(function (x) { z.set(x, p); p += x.length; });
  var ihdr = new Uint8Array(13); ihdr.set(u32(w), 0); ihdr.set(u32(h), 4); ihdr[8] = 8; ihdr[9] = 6;
  var parts = [new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10])].concat(chunk('IHDR', ihdr), chunk('IDAT', z), chunk('IEND', new Uint8Array(0)));
  var total = 0; parts.forEach(function (x) { total += x.length; }); var out = new Uint8Array(total); p = 0; parts.forEach(function (x) { out.set(x, p); p += x.length; });
  fs.writeFileSync(file, out);
}
var tick = function () {
  var f = frames, now_ = now();
  if (Math.floor(now_ / 5000) !== Math.floor((now_ - 1000) / 5000)) console.log('[harness] t=' + (now_ / 1000).toFixed(1) + 's frames=' + f + ' (+' + (f - lastFrames) + ')');
  lastFrames = f;
  if (now_ / 1000 >= seconds || (gfxAt && now_ - gfxAt > 4000 && process.env.EXIT_ON_GFX)) { console.log('[harness] done frames=' + frames); dumpPng(process.env.FRAME_PNG); process.exit(0); }
  setTimeout(tick, 1000);
};
setTimeout(tick, 1000);

var src = fs.readFileSync(DOSBOX_JS, 'utf8');
var tl = Date.now();
(0, eval)(src);
console.log('[harness] script evaluated in ' + (Date.now() - tl) + ' ms');
