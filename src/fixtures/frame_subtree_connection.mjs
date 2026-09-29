// DOM #concept-node-insert; HTML iframe post-connection steps. Do not read
// contentWindow/contentDocument: a getter must not be what starts navigation.
document.appendChild(document.createElement('body'));
globalThis.frameConnections = [];
globalThis.frameConnectionLoads = [];
globalThis.frameConnectionWrappers = [];
function connectionMarkup(name) {
    return '<body><script>parent.frameConnections.push("' + name + '");<\/script>';
}
function connectionWrapper(name) {
    const wrapper = document.createElement('section');
    const frame = document.createElement('iframe');
    frame.src = 'https://unused.example.test/ignored-because-srcdoc-is-present';
    frame.srcdoc = connectionMarkup(name);
    frame.onload = () => frameConnectionLoads.push(name);
    wrapper.appendChild(frame);
    frameConnectionWrappers.push(wrapper);
    return wrapper;
}
const byAppend = connectionWrapper('append');
const byInsert = connectionWrapper('insert');
const byReplace = connectionWrapper('replace');
const parsed = document.createElement('section');
parsed.innerHTML = '<iframe srcdoc="' + connectionMarkup('clone').replaceAll('"', '&quot;') + '"></iframe>';
const cloned = parsed.cloneNode(true);
const shadowHost = document.createElement('div');
const shadow = shadowHost.attachShadow({mode:'closed'});
shadow.appendChild(connectionWrapper('shadow'));
const fragment = document.createDocumentFragment();
fragment.appendChild(connectionWrapper('fragment'));
while (__trust.hasPlatformTask()) __trust.runPlatformTask();
if (frameConnections.length || frameConnectionLoads.length)
    throw new Error('A detached subtree must not start frame navigation');
const marker = document.createElement('p');
document.body.appendChild(marker);
document.body.appendChild(byAppend);
document.body.insertBefore(byInsert, marker);
document.body.replaceChild(byReplace, marker);
document.body.appendChild(cloned);
document.body.appendChild(shadowHost);
document.body.appendChild(fragment);
const removed = connectionWrapper('removed-before-navigation');
document.body.appendChild(removed);
removed.remove();
'frame-connections-pending'
