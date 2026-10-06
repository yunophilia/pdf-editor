// Offline cache for the app shell. CACHE_VERSION is stamped by the deploy workflow.
const CACHE_VERSION = 'dev';
const CACHE = `pdf-editor-${CACHE_VERSION}`;
const ASSETS = [
  './',
  './index.html',
  './worker.js',
  './manifest.webmanifest',
  './icon.svg',
  './pkg/pdf_editor_ui.js',
  './pkg/pdf_editor_ui_bg.wasm',
  './worker-pkg/pdf_editor_worker.js',
  './worker-pkg/pdf_editor_worker_bg.wasm',
];

self.addEventListener('install', (event) => {
  // Fetch with cache: 'reload' so the HTTP cache cannot hand us the previous
  // deploy's files. Asset names are not content-hashed, so a plain addAll()
  // will happily populate a brand-new cache with stale bytes while the hosting
  // layer's max-age is still in force -- producing a new shell wired to old
  // scripts.
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) =>
        Promise.all(
          ASSETS.map((url) =>
            fetch(new Request(url, { cache: 'reload' })).then((res) => {
              if (!res.ok) throw new Error(`${url}: ${res.status}`);
              return cache.put(url, res);
            }),
          ),
        ),
      )
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

// Navigations go to the network first, so a deploy takes effect on the next
// load rather than after an extra reload: a previously-installed worker would
// otherwise answer the navigation from its cache before the new worker has
// activated, serving the old shell. The cache remains the offline fallback.
//
// Everything else is cache-first. Asset names are not content-hashed, but each
// deploy gets its own cache (CACHE_VERSION is the commit) and stale caches are
// dropped on activate, so a cache hit is always from the running version.
self.addEventListener('fetch', (event) => {
  const { request } = event;
  if (request.method !== 'GET' || new URL(request.url).origin !== location.origin) return;

  if (request.mode === 'navigate') {
    event.respondWith(
      fetch(request)
        .then((res) => {
          if (res.ok) {
            const copy = res.clone();
            caches.open(CACHE).then((c) => c.put(request, copy));
          }
          return res;
        })
        .catch(() =>
          caches
            .match(request, { ignoreSearch: true })
            .then((hit) => hit || caches.match('./')),
        ),
    );
    return;
  }

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
