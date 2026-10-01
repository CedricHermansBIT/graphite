import test from 'node:test';
import assert from 'node:assert/strict';
import { loadGfaFromUrl, MAX_GFA_BYTES } from '../../web/url-loader.mjs';

function app() {
  return {
    loads: [], statuses: [],
    load_gfa(name, bytes) { this.loads.push({ name, bytes }); },
    set_load_status(message, error) { this.statuses.push({ message, error }); },
  };
}
const page = source => `https://viewer.example/graphite/?gfa=${encodeURIComponent(source)}`;

test('encoded dataset URL preserves its query and streams bytes to the loader', async () => {
  const viewer = app();
  const source = 'https://data.example/my%20graph.gfa.gz?token=a+b&part=2';
  await loadGfaFromUrl(viewer, page(source), async (url, options) => {
    assert.equal(url, source);
    assert.equal(options.credentials, 'omit');
    assert.equal(options.mode, 'cors');
    return new Response(new ReadableStream({ start(c) {
      c.enqueue(new Uint8Array([1, 2]));
      c.enqueue(new Uint8Array([3]));
      c.close();
    } }));
  });
  assert.deepEqual(viewer.loads, [{ name: 'my graph.gfa.gz', bytes: new Uint8Array([1, 2, 3]) }]);
  assert.equal(viewer.statuses[0].error, false);
});

test('relative URLs resolve against the viewer URL', async () => {
  const viewer = app();
  await loadGfaFromUrl(viewer, page('./sample.gfa'), async url => {
    assert.equal(url, 'https://viewer.example/graphite/sample.gfa');
    return new Response('S\t1\tA\n');
  });
  assert.equal(viewer.loads.length, 1);
});

test('private download links can be supplied in the URL fragment', async () => {
  const viewer = app();
  const source = 'https://data.example/private.gfa?signature=a+b&expires=123';
  const url = new URL('https://viewer.example/graphite/');
  url.hash = new URLSearchParams({ gfa: source }).toString();
  await loadGfaFromUrl(viewer, url.href, async target => {
    assert.equal(target, source);
    return new Response('S\t1\tA\n');
  });
  assert.equal(viewer.loads.length, 1);
  assert.equal(viewer.loads[0].name, 'private.gfa');
});

test('no parameter leaves the ordinary local-file startup intact', async () => {
  const viewer = app();
  await loadGfaFromUrl(viewer, 'https://viewer.example/', () => { throw new Error('unexpected fetch'); });
  assert.deepEqual(viewer.statuses, []);
  assert.deepEqual(viewer.loads, []);
});

for (const query of ['gfa=', 'gfa=https://data.example/a&gfa=https://data.example/b',
  'gfa=https://data.example/a#gfa=https://data.example/b',
  'gfa=javascript:alert(1)', 'gfa=file:///tmp/a.gfa', 'gfa=https://user:pass@data.example/a']) {
  test(`rejects invalid link ${query}`, async () => {
    const viewer = app();
    await loadGfaFromUrl(viewer, `https://viewer.example/?${query}`, () => { throw new Error('unexpected fetch'); });
    assert.equal(viewer.loads.length, 0);
    assert.equal(viewer.statuses.at(-1).error, true);
  });
}

test('oversized Content-Length is rejected before reading the body', async () => {
  const viewer = app();
  let cancelled = false;
  await loadGfaFromUrl(viewer, page('https://data.example/a.gfa'), async () => ({
    ok: true, headers: new Headers({ 'Content-Length': String(MAX_GFA_BYTES + 1) }),
    body: { async cancel() { cancelled = true; }, getReader() { assert.fail('body should not be read'); } },
  }));
  assert.equal(cancelled, true);
  assert.match(viewer.statuses.at(-1).message, /256 MiB/);
  assert.equal(viewer.loads.length, 0);
});

test('streamed limit is enforced even without Content-Length', async () => {
  const viewer = app();
  let cancelled = false;
  let released = false;
  await loadGfaFromUrl(viewer, page('https://data.example/a.gfa'), async () => ({
    ok: true, headers: new Headers(),
    body: { getReader() { return {
      async read() { return { done: false, value: { byteLength: MAX_GFA_BYTES + 1 } }; },
      async cancel() { cancelled = true; }, releaseLock() { released = true; },
    }; } },
  }));
  assert.equal(cancelled, true);
  assert.equal(released, true);
  assert.match(viewer.statuses.at(-1).message, /256 MiB/);
  assert.equal(viewer.loads.length, 0);
});

for (const [label, response, message] of [
  ['HTTP failure', () => new Response('not found', { status: 404 }), /HTTP 404/],
  ['empty file', () => new Response(''), /empty file/],
  ['CORS failure', () => { throw new TypeError('Failed to fetch'); }, /CORS/],
]) {
  test(`${label} is shown in the existing input error UI`, async () => {
    const viewer = app();
    await loadGfaFromUrl(viewer, page('https://data.example/a.gfa'), async () => response());
    assert.equal(viewer.loads.length, 0);
    assert.equal(viewer.statuses.at(-1).error, true);
    assert.match(viewer.statuses.at(-1).message, message);
  });
}
