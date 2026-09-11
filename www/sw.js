// Offline cache for the app shell. CACHE_VERSION is stamped by the deploy workflow.
const CACHE_VERSION = 'dev';
const CACHE = `pdf-editor-${CACHE_VERSION}`;
const ASSETS = [
  './',
  './index.html',
  './style.css',
  './app.js',
  './worker.js',
  './manifest.webmanifest',
  './icon.svg',
  './pkg/pdf_editor.js',
  './pkg/pdf_editor_bg.wasm',
];

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((c) => c.addAll(ASSETS)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

// Cache-first for our own assets; the network is only consulted when something
// is missing (e.g. a hard reload after a deploy).
self.addEventListener('fetch', (event) => {
  const { request } = event;
  if (request.method !== 'GET' || new URL(request.url).origin !== location.origin) return;
  event.respondWith(
    caches.match(request, { ignoreSearch: true }).then(
      (hit) =>
        hit ||
        fetch(request).then((res) => {
          if (res.ok) {
            const copy = res.clone();
            caches.open(CACHE).then((c) => c.put(request, copy));
          }
          return res;
        }),
    ),
  );
});
