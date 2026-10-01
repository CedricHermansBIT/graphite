// Keep the network read bounded before any bytes are copied into Wasm memory.
export const MAX_GFA_BYTES = 256 * 1024 * 1024;

export async function loadGfaFromUrl(app, pageUrl, fetchImpl = fetch) {
  const page = new URL(pageUrl);
  const values = [
    ...page.searchParams.getAll('gfa'),
    ...new URLSearchParams(page.hash.slice(1)).getAll('gfa'),
  ];
  if (values.length === 0) return;

  let timeout;
  let reader;
  try {
    if (values.length !== 1 || !values[0].trim()) {
      throw new Error('Supply exactly one non-empty gfa URL parameter.');
    }
    const source = new URL(values[0], pageUrl);
    if (!['https:', 'http:'].includes(source.protocol) || source.username || source.password) {
      throw new Error('The gfa parameter must be an HTTP or HTTPS file URL without embedded credentials.');
    }
    let name;
    try {
      name = decodeURIComponent(source.pathname.split('/').pop() || 'linked.gfa');
    } catch {
      name = 'linked.gfa';
    }
    app.set_load_status(`Downloading ${name}…`, false);
    const controller = new AbortController();
    timeout = setTimeout(() => controller.abort(), 120_000);
    const response = await fetchImpl(source.href, {
      mode: 'cors',
      credentials: 'omit',
      signal: controller.signal,
    });
    if (!response.ok) throw new Error(`The dataset server returned HTTP ${response.status}.`);
    const declaredSize = Number(response.headers.get('Content-Length'));
    if (declaredSize > MAX_GFA_BYTES) {
      await response.body?.cancel();
      throw new Error('Linked GFA exceeds the browser 256 MiB input limit; use desktop Graphite.');
    }
    if (!response.body) throw new Error('The dataset server returned an empty response.');
    reader = response.body.getReader();
    const chunks = [];
    let length = 0;
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > MAX_GFA_BYTES) {
        throw new Error('Linked GFA exceeds the browser 256 MiB input limit; use desktop Graphite.');
      }
      chunks.push(value);
    }
    if (length === 0) throw new Error('The dataset server returned an empty file.');
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    app.load_gfa(name, bytes);
  } catch (error) {
    await reader?.cancel().catch(() => {});
    const detail = error?.name === 'AbortError'
      ? 'The download timed out after two minutes.'
      : error instanceof TypeError
        ? 'The file could not be fetched. Check the URL and the dataset server’s CORS permissions.'
        : error?.message || String(error);
    app.set_load_status(`Could not load linked GFA: ${detail}`, true);
  } finally {
    clearTimeout(timeout);
    reader?.releaseLock();
  }
}
