// Capture actions: screenshots (annotated, change-detected), PDF, screencast recording, CPU profiles,
// performance metrics and Core Web Vitals. Arguments are validated by capture.rs before this runs.
// Inlined into the driver by driver_source as `captureModule`; relies on the driver's `clip`.
// Functions handed to page.evaluate run in the page realm and must stay self-contained.
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const MAX_SCREENSHOT_BYTES = 5 * 1024 * 1024;
const MAX_FULL_PAGE_HEIGHT = 12000;
const MAX_ANNOTATIONS = 60;
const MAX_METRICS = 200;
const RECORD_QUALITY = 80;
const RECORD_EVERY_NTH_FRAME = 2;
const CAPTURE_ACTIONS = new Set(['screenshot', 'locate', 'pdf', 'record_start', 'record_stop', 'profile_start', 'profile_stop', 'metrics', 'vitals']);

const fail = message => { throw new Error(message); };
const requireCdp = tab => tab.cdp || fail('CDP session is unavailable for this tab');

function handles(request) {
  return CAPTURE_ACTIONS.has(request.action);
}

async function run(request, ctx) {
  const base = { ...ctx, request, id: request.tab };
  switch (request.action) {
    case 'screenshot': return screenshot(base);
    case 'locate': return locateCapture(base);
    case 'pdf': return pdf(base);
    case 'record_start': return recordStart(base);
    case 'record_stop': return recordStop(base);
    case 'profile_start': return profileStart(base);
    case 'profile_stop': return profileStop(base);
    case 'metrics': return metrics(base);
    case 'vitals': return vitals(base);
  }
  fail(`unsupported capture action ${request.action}`);
}

async function screenshot(ctx) {
  const { request, page, tab } = ctx;
  const format = request.format ?? 'png';
  const fullPage = !!request.full_page;
  if (fullPage) {
    const height = await page.evaluate(() => Math.max(document.body?.scrollHeight || 0, document.documentElement.scrollHeight));
    if (height > MAX_FULL_PAGE_HEIGHT) fail(`full-page screenshot exceeds ${MAX_FULL_PAGE_HEIGHT}px height; use viewport screenshot`);
  }
  let annotations = null;
  if (request.annotate) {
    annotations = await page.evaluate(collectAnnotationTargets, { limit: MAX_ANNOTATIONS, fullPage });
    if (annotations.length) await page.evaluate(paintAnnotations, annotations);
  }
  let bytes;
  try {
    bytes = await page.screenshot({
      type: format, fullPage, timeout: request.timeout_ms,
      ...(request.quality !== undefined ? { quality: request.quality } : {}),
    });
  } finally {
    if (annotations?.length) await page.evaluate(removeAnnotations).catch(() => {});
  }
  if (bytes.length > MAX_SCREENSHOT_BYTES) fail('screenshot exceeds the provider attachment limit of 5 MiB; use a viewport screenshot');
  const result = {
    tab: ctx.id, url: clip(page.url()),
    ...(annotations ? { annotations: annotations.map(({ label, role, name, selector }) => ({ label, role, name, selector })) } : {}),
  };
  // if_changed compares the encoded image with the previous capture of the same tab and options.
  // Only a digest is kept; an identical capture returns no image.
  const data = bytes.toString('base64');
  const digest = crypto.createHash('sha256').update(data).digest('hex');
  const key = JSON.stringify([fullPage, format, request.quality ?? null, !!request.annotate]);
  tab.shots = tab.shots || new Map();
  const previous = tab.shots.get(key);
  tab.shots.set(key, digest);
  if (request.if_changed === true) {
    result.changed = previous !== digest;
    if (!result.changed) return result;
  }
  result.artifact = { media_type: format === 'jpeg' ? 'image/jpeg' : 'image/png', encoding: 'base64', data };
  return result;
}

// Viewport PNG for the host's grounding model, plus the CSS viewport size so device pixels can be
// mapped back to click_at coordinates. It is not shown to the model and leaves if_changed state alone.
async function locateCapture(ctx) {
  const { request, page } = ctx;
  const bytes = await page.screenshot({ type: 'png', timeout: request.timeout_ms });
  if (bytes.length > MAX_SCREENSHOT_BYTES) fail('locate screenshot exceeds 5 MiB');
  const viewport = await page.evaluate(() => ({ width: window.innerWidth, height: window.innerHeight }));
  return { tab: ctx.id, url: clip(page.url()), viewport, image: bytes.toString('base64') };
}

// Self-contained: serialized into the page. Returns visible interactive elements addressed by a CSS
// selector that the other DOM actions accept, so an annotation label can be acted on afterwards.
function collectAnnotationTargets({ limit, fullPage }) {
  const selectorFor = node => {
    const parts = [];
    while (node && node.nodeType === 1 && parts.length < 12) {
      if (node.id) { parts.unshift('#' + CSS.escape(node.id)); break; }
      let nth = 1;
      for (let sibling = node.previousElementSibling; sibling; sibling = sibling.previousElementSibling) if (sibling.tagName === node.tagName) nth++;
      parts.unshift(node.localName + `:nth-of-type(${nth})`);
      node = node.parentElement;
    }
    return parts.join(' > ');
  };
  const candidates = document.querySelectorAll('a[href],button,input:not([type=hidden]),select,textarea,summary,[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],[role=menuitem],[role=option],[role=switch],[role=textbox],[contenteditable=true],[tabindex]:not([tabindex="-1"])');
  const targets = [];
  for (const element of candidates) {
    if (targets.length >= limit) break;
    const rect = element.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) continue;
    if (!fullPage && (rect.bottom < 0 || rect.right < 0 || rect.top > innerHeight || rect.left > innerWidth)) continue;
    const style = getComputedStyle(element);
    if (style.visibility === 'hidden' || style.display === 'none' || element.closest('[aria-hidden="true"],[hidden]')) continue;
    const name = (element.getAttribute('aria-label') || element.getAttribute('alt') || element.getAttribute('title') || element.getAttribute('placeholder') || element.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 120);
    targets.push({ label: targets.length + 1, role: element.getAttribute('role') || element.localName, name, selector: selectorFor(element), x: rect.left, y: rect.top, width: rect.width, height: rect.height });
  }
  return targets;
}

// Self-contained: paints numbered boxes in document coordinates, so fullPage captures include them.
function paintAnnotations(targets) {
  const layer = document.createElement('div');
  layer.setAttribute('data-dial-annotations', '');
  layer.style.cssText = 'position:absolute;left:0;top:0;width:0;height:0;pointer-events:none;z-index:2147483647';
  for (const target of targets) {
    const box = document.createElement('div');
    box.style.cssText = `position:absolute;left:${target.x + scrollX}px;top:${target.y + scrollY}px;width:${target.width}px;height:${target.height}px;outline:2px solid #e11d48;box-sizing:border-box`;
    const badge = document.createElement('span');
    badge.textContent = String(target.label);
    badge.style.cssText = 'position:absolute;left:0;top:0;transform:translate(-50%,-50%);background:#e11d48;color:#fff;font:bold 12px sans-serif;padding:0 4px;border-radius:8px';
    box.appendChild(badge);
    layer.appendChild(box);
  }
  document.documentElement.appendChild(layer);
}

// Self-contained: removes the overlay installed by paintAnnotations.
function removeAnnotations() {
  document.querySelectorAll('[data-dial-annotations]').forEach(node => node.remove());
}

async function pdf(ctx) {
  const file = path.join(artifactsDir(), `page-${Date.now()}.pdf`);
  await ctx.page.pdf({ path: file });
  return { tab: ctx.id, url: clip(ctx.page.url()), path: clip(file) };
}

// Screencast frames are spooled as JPEGs with their capture times. In the concat list each frame is
// shown until the next frame's time, and the last one until the recording ends.
function concatList(frames, endedMs) {
  const quote = file => file.replace(/'/g, "'\\''");
  const lines = [];
  frames.forEach((frame, index) => {
    const next = index + 1 < frames.length ? frames[index + 1].timestamp_ms : endedMs;
    const seconds = Math.max(next - frame.timestamp_ms, 1) / 1000;
    lines.push(`file '${quote(frame.file)}'`, `duration ${seconds.toFixed(6)}`);
  });
  lines.push(`file '${quote(frames[frames.length - 1].file)}'`);
  return lines.join('\n') + '\n';
}

async function recordStart(ctx) {
  const { tab, page, request } = ctx;
  if (tab.recording) {
    if (request.record_restart !== true) fail('recording is already running; pass record_restart to replace it');
    await discardRecording(tab);
  }
  const cdp = requireCdp(tab);
  const stamp = Date.now();
  const artifacts = artifactsDir();
  const dir = path.join(artifacts, `recording-${stamp}-frames`);
  await fs.promises.mkdir(dir, { recursive: true });
  const viewport = page.viewportSize();
  const rec = {
    dir, output: path.join(artifacts, `recording-${stamp}.mp4`), startedMs: stamp,
    frames: [], pending: [], seq: 0, error: null, onFrame: null,
  };
  rec.onFrame = event => {
    const file = path.join(dir, `frame-${String(rec.seq++).padStart(8, '0')}.jpg`);
    const timestamp = event.metadata?.timestamp;
    rec.frames.push({ file, timestamp_ms: Number.isFinite(timestamp) ? Math.round(timestamp * 1000) : Date.now() });
    rec.pending.push(fs.promises.writeFile(file, Buffer.from(event.data, 'base64')).catch(error => { rec.error = rec.error || error.message; }));
    cdp.send('Page.screencastFrameAck', { sessionId: event.sessionId }).catch(() => {});
  };
  cdp.on('Page.screencastFrame', rec.onFrame);
  try {
    await cdp.send('Page.startScreencast', {
      format: 'jpeg', quality: RECORD_QUALITY, everyNthFrame: RECORD_EVERY_NTH_FRAME,
      ...(viewport ? { maxWidth: viewport.width, maxHeight: viewport.height } : {}),
    });
  } catch (error) {
    cdp.removeListener('Page.screencastFrame', rec.onFrame);
    await fs.promises.rm(dir, { recursive: true, force: true });
    throw error;
  }
  tab.recording = rec;
  return { tab: ctx.id, recording: true };
}

async function discardRecording(tab) {
  const rec = tab.recording;
  tab.recording = null;
  tab.cdp.removeListener('Page.screencastFrame', rec.onFrame);
  try {
    await tab.cdp.send('Page.stopScreencast');
  } finally {
    await Promise.all(rec.pending);
    await fs.promises.rm(rec.dir, { recursive: true, force: true });
  }
}

async function recordStop(ctx) {
  const { tab } = ctx;
  const rec = tab.recording;
  if (!rec) fail('recording is not running');
  tab.recording = null;
  requireCdp(tab).removeListener('Page.screencastFrame', rec.onFrame);
  let stopError = null;
  await tab.cdp.send('Page.stopScreencast').catch(error => { stopError = error; });
  await Promise.all(rec.pending);
  if (stopError) throw stopError;
  if (rec.error) fail(`recording frame write failed: ${rec.error}`);
  const endedMs = Date.now();
  let frames = rec.frames;
  if (frames.length === 0) {
    // A page that never repainted still yields one frame, so the output is a real still.
    const file = path.join(rec.dir, 'frame-00000000.jpg');
    await fs.promises.writeFile(file, await ctx.page.screenshot({ type: 'jpeg', quality: RECORD_QUALITY }));
    frames = [{ file, timestamp_ms: rec.startedMs }];
  }
  const concat = path.join(rec.dir, 'concat.txt');
  await fs.promises.writeFile(concat, concatList(frames, endedMs));
  return {
    tab: ctx.id, recording: clip(rec.output), concat: clip(concat), frames: frames.length,
    duration_ms: Math.max(endedMs - frames[0].timestamp_ms, 0),
  };
}

async function profileStart(ctx) {
  const { tab } = ctx;
  if (tab.profiling) fail('profiling is already running');
  const cdp = requireCdp(tab);
  await cdp.send('Profiler.enable');
  try {
    await cdp.send('Profiler.start');
  } catch (error) {
    await cdp.send('Profiler.disable').catch(() => {});
    throw error;
  }
  tab.profiling = true;
  return { tab: ctx.id, profiling: true };
}

async function profileStop(ctx) {
  const { tab } = ctx;
  if (!tab.profiling) fail('profiling is not running');
  const cdp = requireCdp(tab);
  let profile;
  try {
    ({ profile } = await cdp.send('Profiler.stop'));
  } finally {
    tab.profiling = false;
  }
  await cdp.send('Profiler.disable');
  const file = path.join(artifactsDir(), `profile-${Date.now()}.cpuprofile`);
  await fs.promises.writeFile(file, JSON.stringify(profile));
  return { tab: ctx.id, path: clip(file) };
}

async function metrics(ctx) {
  const cdp = requireCdp(ctx.tab);
  await cdp.send('Performance.enable');
  const { metrics: samples } = await cdp.send('Performance.getMetrics');
  await cdp.send('Performance.disable');
  return {
    tab: ctx.id, url: clip(ctx.page.url()),
    metrics: Object.fromEntries(samples.slice(0, MAX_METRICS).map(sample => [sample.name, sample.value])),
  };
}

async function vitals(ctx) {
  return { tab: ctx.id, url: clip(ctx.page.url()), vitals: await ctx.page.evaluate(collectVitals) };
}

// Self-contained: reads the buffered performance timeline, not live observers, so values can differ
// from what a page's own observers reported. INP here is the worst interaction latency recorded,
// which matches the INP definition for pages with fewer than 50 interactions.
function collectVitals() {
  const round = (value, digits) => Math.round(value * 10 ** digits) / 10 ** digits;
  const navigation = performance.getEntriesByType('navigation')[0];
  const paint = performance.getEntriesByName('first-contentful-paint')[0];
  const lcp = performance.getEntriesByType('largest-contentful-paint').at(-1);
  // Session windows (gap <= 1 s, span <= 5 s) as in the Core Web Vitals CLS definition.
  let cls = 0, windowValue = 0, windowStart = 0, lastShift = 0;
  for (const shift of performance.getEntriesByType('layout-shift')) {
    if (shift.hadRecentInput) continue;
    if (windowValue > 0 && (shift.startTime - lastShift > 1000 || shift.startTime - windowStart > 5000)) windowValue = 0;
    if (windowValue === 0) windowStart = shift.startTime;
    windowValue += shift.value;
    lastShift = shift.startTime;
    cls = Math.max(cls, windowValue);
  }
  let inp = 0;
  for (const event of performance.getEntriesByType('event')) if (event.interactionId > 0) inp = Math.max(inp, event.duration);
  const longTasks = performance.getEntriesByType('longtask');
  return {
    ttfb_ms: navigation ? round(navigation.responseStart, 2) : null,
    fcp_ms: paint ? round(paint.startTime, 2) : null,
    lcp_ms: lcp ? round(lcp.startTime, 2) : null,
    cls: round(cls, 4),
    inp_ms: inp ? round(inp, 2) : null,
    dom_content_loaded_ms: navigation ? round(navigation.domContentLoadedEventEnd, 2) : null,
    load_ms: navigation ? round(navigation.loadEventEnd, 2) : null,
    long_tasks: longTasks.length,
    long_task_total_ms: round(longTasks.reduce((sum, task) => sum + task.duration, 0), 2),
  };
}

module.exports = { handles, run };
