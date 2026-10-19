import {message} from './message.js';

addEventListener('activate', event => event.waitUntil((async () => {
  await registration.navigationPreload.enable();
  await registration.navigationPreload.setHeaderValue('servo-acceptance');
  await clients.claim();
})()));

addEventListener('message', event => event.waitUntil((async () => {
  const client = (await clients.matchAll()).find(client => client.url === event.data);
  if (!client) throw Error('Navigated frame client is missing');
  event.ports[0].postMessage({id:client.id, frameType:client.frameType});
  event.ports[0].close();
})()));

addEventListener('fetch', event => {
  const path = new URL(event.request.url).pathname.split('/').pop();
  if (path === 'virtual') event.respondWith(new Response(message));
  else if (path === 'echo') event.respondWith(event.request.text().then(text => new Response(text)));
  else if (path === 'stream') {
    event.respondWith(new Response(new ReadableStream({start(controller) {
      controller.enqueue(new TextEncoder().encode('first '));
      controller.enqueue(new TextEncoder().encode('second'));
      controller.close();
    }})));
  } else if (path === 'child.js') {
    event.respondWith(new Response("fetch('virtual').then(r=>r.text()).then(value=>postMessage(value))",
      {headers:{'content-type':'text/javascript'}}));
  } else if (path === 'frame') {
    event.respondWith(new Response(`<!doctype html><body>Controlled frame<script>parent.postMessage(location.href,'*')<\/script>`,
      {headers:{'content-type':'text/html'}}));
  } else if (path === 'navigation') {
    event.respondWith((async () => {
      const preload = await event.preloadResponse;
      const text = await preload.text();
      const cache = await caches.open('servo-acceptance');
      const saved = await (await cache.match('results')).text();
      const pass = text.trim() === 'network preload' && event.replacesClientId && event.resultingClientId;
      return new Response(`<!doctype html><title>${pass ? 'PASS' : 'FAIL'} navigation</title><h1>${pass ? 'PASS' : 'FAIL'} navigation preload</h1><p>${saved}</p><p>Navigation committed with a new client.</p>`,
        {headers:{'content-type':'text/html'}});
    })());
  }
});
