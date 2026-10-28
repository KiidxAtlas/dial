'use strict';
// Trusted adapter; expressions execute only in Chromium's page realm, never in Node.
const readline = require('node:readline');
const { chromium } = require(process.env.DIAL_PLAYWRIGHT);
const fs = require('node:fs');
const path = require('node:path');
// Dev-server ESM graphs routinely enqueue thousands of modules. Bound waiting metadata/body
// bytes independently of active bridge jobs instead of rejecting an ordinary module fan-out.
const MAX_PENDING = 16, MAX_QUEUED_HTTP = 4096, MAX_QUEUED_BYTES = 8 * 1024 * 1024;
const MAX_PENDING_BYTES = 2 * 1024 * 1024, MAX_STDOUT_BYTES = 8 * 1024 * 1024;
const READABLE_LIMIT = 64 * 1024;
const CALLBACK_TTL_MS = 30 * 1000;
function send(value) {
  const line = JSON.stringify(value) + '\n';
  const size = Buffer.byteLength(line);
  if (size > MAX_STDOUT_BYTES || process.stdout.writableNeedDrain || process.stdout.writableLength + size > MAX_STDOUT_BYTES) {
    return Promise.reject(new Error('browser protocol backpressure/buffer budget exceeded'));
  }
  return new Promise((resolve, reject) => process.stdout.write(line, error => error ? reject(error) : resolve()));
}
const callbacks = new Map();
let callbackId = 0, browser, context, nextTab = 1, operationId = 0, operationDeadline = null, pendingBytes = 0, activeHttp = 0;
let attached = false, tracing = false, wsEndpoints = [];
const httpQueue = [];
let queuedHttpBytes = 0;
let activeRoutes = 0;
function finishCallback(id, error, result) {
  const callback = callbacks.get(id);
  if (!callback) return;
  callbacks.delete(id);
  pendingBytes -= callback.bytes;
  clearTimeout(callback.timer);
  error ? callback.reject(error) : callback.resolve(result);
}
function cancelPending(reason) {
  for (const id of callbacks.keys()) finishCallback(id, new Error(reason));
  for (const queued of httpQueue.splice(0)) queued.reject(new Error(reason));
  queuedHttpBytes = 0;
}
async function settleRequests(deadline, budgetMs = 1000, quietMs = 100) {
  const end = Math.min(deadline ?? Infinity, Date.now() + budgetMs);
  let quietSince = 0;
  while (Date.now() < end) {
    if (activeHttp === 0 && httpQueue.length === 0 && activeRoutes === 0) {
      if (!quietSince) quietSince=Date.now();
      if (Date.now()-quietSince >= quietMs) return {settled:true};
    } else quietSince=0;
    await new Promise(resolve=>setTimeout(resolve,10));
  }
  const pending = Math.max(activeHttp + httpQueue.length, activeRoutes);
  cancelPending('browser post-action network settlement budget exhausted');
  return {settled:false, pending, note:'Action completed; background network did not settle within its separate budget. This is not proof of application success. Inspect the page before retrying any action.'};
}
const tabs = new Map();
// UTF-16 slicing that never leaves a lone high surrogate: Rust rejects lone surrogates in JSON strings.
const cut = (value, limit) => { const text = String(value).slice(0, limit); return /[\uD800-\uDBFF]$/.test(text) ? text.slice(0, -1) : text; };
const clip = value => cut(value, 2000);
// Every tab event gets a per-tab sequence number so `events` can return only what is new.
const EVENT_CHANNELS = ['console','network','dialogs','downloads','navigation'];
function record(tab, channel, value) {
  value.seq = ++tab.seq;
  const list = tab[channel]; list.push(value); if (list.length > 100) list.shift();
  // Keep causes even when an asset burst evicts them from the general network ring.
  if (channel === 'network' && value.kind === 'blocked') {
    const failures = tab.networkFailures ||= [];
    failures.push(value); if (failures.length > 100) failures.shift();
  }
}
function tabEvents(tab, after) {
  if (after !== undefined && after !== null && (!Number.isInteger(after) || after < 0)) throw new Error('after must be a non-negative integer');
  const since = after ?? 0;
  const events = EVENT_CHANNELS.flatMap(channel => tab[channel].filter(event => event.seq > since).map(event => ({channel, ...event})));
  return {events: events.sort((a,b) => a.seq - b.seq), network_failures:(tab.networkFailures || []).filter(event=>event.seq > since), last_seq: tab.seq};
}
// Request log, HAR and routes. Rust owns the allowed-host guard, header redaction and output shaping;
// route verdicts come from the guarded Rust bridge, and this adapter acts on them.
const REQUEST_LOG_MAX = 200, ROUTES_MAX = 32, REQUEST_BODY_TRANSFER_MAX = 64 * 1024, HAR_BODY_BUDGET = 2 * 1024 * 1024;
let initScriptSeq = 0;
const initScripts = new Map();
function isTextualType(contentType) {
  const kind = String(contentType || '').split(';')[0].trim().toLowerCase();
  return kind.startsWith('text/') || kind.endsWith('+json') || kind.endsWith('+xml')
    || ['application/json','application/javascript','application/xml','application/x-www-form-urlencoded','image/svg+xml'].includes(kind);
}
function publicRecord(record) {
  const {_request, started_ms, finished, ...rest} = record;
  return rest;
}
function routeSummary(route) {
  return {id:route.id, pattern:route.pattern, resource_type:route.resource_type, kind:route.kind};
}
function logRequest(tab, record) {
  tab.requestLog.push(record);
  tab.byRequest.set(record._request, record);
  if (tab.requestLog.length > REQUEST_LOG_MAX) tab.requestLog.shift();
}
async function responseBody(record) {
  if (!record.finished) return null;
  const response = await record._request.response().catch(() => null);
  return response ? response.body().catch(() => null) : null;
}
function fulfillment(route, requestId) {
  const spec = route.response || {};
  const headers = {...(spec.headers || {})};
  if (spec.content_type) headers['content-type'] = spec.content_type;
  return {requestId, responseCode:spec.status ?? 200, responseHeaders:Object.entries(headers).map(([name, value]) => ({name, value:String(value)})), body:Buffer.from(spec.body ?? '', 'utf8').toString('base64')};
}
async function acquireHttp(bytes) {
  if (activeHttp < MAX_PENDING) { activeHttp++; return; }
  if (httpQueue.length >= MAX_QUEUED_HTTP) throw new Error('browser HTTP request queue budget exceeded (4096 waiting requests)');
  if (queuedHttpBytes + bytes > MAX_QUEUED_BYTES) throw new Error('browser HTTP request queue byte budget exceeded (8 MiB)');
  queuedHttpBytes += bytes;
  await new Promise((resolve, reject) => httpQueue.push({resolve, reject, bytes}));
}
function releaseHttp() {
  const next = httpQueue.shift();
  if (next) { queuedHttpBytes -= next.bytes; next.resolve(); }
  else activeHttp--;
}
async function http(request, routing) {
  if (!operationId) throw new Error('browser networking is paused between approved operations');
  const owner = operationId;
  const event = httpEvent(request);
  if (routing) Object.assign(event, routing);
  await acquireHttp(Buffer.byteLength(JSON.stringify(event)));
  try {
    if (operationId !== owner) throw new Error('approved browser operation finished');
    const started = await requestCallback(event);
    while (operationId === owner) {
      const reply = await requestCallback({callback:'http_poll', job:started.job});
      if (reply.done) {
        if (reply.error) throw new Error(reply.error);
        return reply.value;
      }
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    throw new Error('approved browser operation finished');
  } finally { releaseHttp(); }
}
function httpEvent(request) {
  const body = request.postDataBuffer();
  if (body && body.length > 1024 * 1024) throw new Error('request body exceeds 1 MiB');
  return {callback:'http', url:request.url(), method:request.method(), headers:request.headers(), body:body ? body.toString('base64') : ''};
}
function requestCallback(fields) {
  if (!operationId) return Promise.reject(new Error('browser networking is paused between approved operations'));
  const id = ++callbackId;
  const expiresAt = operationDeadline ?? Date.now() + CALLBACK_TTL_MS;
  const event = {...fields, callback_id:id, operation_id:operationId, expires_at_ms:expiresAt};
  const bytes = Buffer.byteLength(JSON.stringify(event));
  if (pendingBytes + bytes > MAX_PENDING_BYTES) return Promise.reject(new Error('browser pending request byte budget exceeded'));
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => finishCallback(id,new Error('browser network callback expired')),Math.max(0,expiresAt-Date.now()));
    timer.unref?.();
    callbacks.set(id, {resolve,reject,bytes,timer});
    pendingBytes += bytes;
    send(event).catch(error => finishCallback(id,error));
  });
}
// Only exact loopback endpoints approved by the Rust policy connect; everything else is closed.
// Used for context-wide routing (launched browsers) and per-page routing (attached browsers).
function guardSocket(socket) {
  let url;
  try { url = new URL(socket.url()); } catch { socket.close({ code:1008, reason:'WebSocket endpoint is not approved for this browser session' }); return; }
  const port = url.port || (url.protocol === 'wss:' ? '443' : '80');
  if (wsEndpoints.includes(`${url.hostname}:${port}`)) socket.connectToServer();
  else socket.close({ code:1008, reason:'WebSocket endpoint is not approved for this browser session' });
}
function register(page) {
  for (const [id, tab] of tabs) if (tab.page === page) return id;
  if (tabs.size >= 16) { page.close().catch(() => {}); throw new Error('maximum 16 browser tabs'); }
  const id = nextTab++;
  const tab = { page, seq: 0, console: [], network: [], dialogs: [], downloads: [], navigation: [], frames: new Map(), nextFrame: 1, dialogPolicy: null, downloadArmed: false, requestLog: [], requestSeq: 0, byRequest: new WeakMap(), routes: [], routeSeq: 0, har: null };
  tabs.set(id, tab);
  page.on('console', message => { const location=message.location(); record(tab, 'console', { time:Date.now(), operation_id:operationId, type:message.type(), text:clip(message.text()), location:{...location,url:clip(location.url)} }); });
  page.on('pageerror', error => record(tab, 'console', { time:Date.now(), operation_id:operationId, type: 'pageerror', text: clip(error.message) }));
  page.on('request', request => record(tab, 'network', { time:Date.now(), operation_id:operationId, kind: 'request', method: request.method(), url: clip(request.url()), resource: request.resourceType() }));
  page.on('response', response => record(tab, 'network', { time:Date.now(), operation_id:operationId, kind: 'response', status: response.status(), url: clip(response.url()) }));
  page.on('requestfailed', request => record(tab, 'network', { time:Date.now(), operation_id:operationId, kind:'failed', url:clip(request.url()), error:clip(request.failure()?.errorText || '') }));
  page.on('framenavigated', frame => { if (frame === page.mainFrame()) record(tab, 'navigation', { time:Date.now(), operation_id:operationId, kind:'navigated', url:clip(frame.url()) }); });
  page.on('load', () => record(tab, 'navigation', { time:Date.now(), operation_id:operationId, kind:'load', url:clip(page.url()) }));
  page.on('close', () => tabs.delete(id));
  page.on('dialog', dialog => {
    const policy = tab.dialogPolicy || {mode:'dismiss'};
    if (policy.once) tab.dialogPolicy = null;
    const event = {time:Date.now(), operation_id:operationId, type:dialog.type(), message:clip(dialog.message()), default_value:clip(dialog.defaultValue()), handling:policy.mode};
    record(tab,'dialogs',event);
    (policy.mode === 'accept' ? dialog.accept(policy.text) : dialog.dismiss())
      .then(()=>{event.handled=true;},error=>{event.error=clip(error.message);});
  });
  page.on('download', download => {
    const armed = tab.downloadArmed;
    tab.downloadArmed = false;
    record(tab,'downloads',{time:Date.now(), operation_id:operationId, url:clip(download.url()), filename:clip(download.suggestedFilename()), status:armed ? 'approved-transfer' : 'cancelled-unarmed'});
    if (!armed) download.cancel().then(()=>download.delete()).catch(error=>record(tab,'console',{type:'download-cleanup-error',text:clip(error.message)}));
  });
  page.on('framedetached', frame => tab.frames.delete(frame));
  page.on('request', request => logRequest(tab, {id:++tab.requestSeq, method:request.method(), url:clip(request.url()), resource_type:request.resourceType(), started:new Date().toISOString(), started_ms:Date.now(), request_headers:request.headers(), status:null, failure:null, duration_ms:null, response_headers:null, response_bytes:null, finished:false, _request:request}));
  page.on('response', response => { const record = tab.byRequest.get(response.request()); if (record) { record.status = response.status(); record.response_headers = response.headers(); } });
  page.on('requestfinished', request => { const record = tab.byRequest.get(request); if (!record) return; record.finished = true; record.duration_ms = Date.now() - record.started_ms; request.sizes().then(sizes => { record.response_bytes = sizes.responseBodySize; }, () => {}); });
  page.on('requestfailed', request => { const record = tab.byRequest.get(request); if (!record) return; record.finished = true; record.failure = clip(request.failure()?.errorText || 'unknown'); record.duration_ms = Date.now() - record.started_ms; });
  // CDP Fetch reports every redirect hop. Playwright context.route only intercepts the initial URL.
  tab.ready = (attached ? page.routeWebSocket('**/*', guardSocket) : Promise.resolve()).then(() => context.newCDPSession(page)).then(async session => {
    if (!session) return;
    tab.cdp = session;
    session.on('Fetch.requestPaused', event => {
      activeRoutes++;
      const owner = operationId;
      (async () => {
        try {
          const request=event.request;
          const body=request.postDataEntries?.length
            ? Buffer.concat(request.postDataEntries.map(entry=>Buffer.from(entry.bytes || '','base64')))
            : request.postData === undefined ? null : Buffer.from(request.postData,'utf8');
          const routing = tab.routes.length ? {routes:tab.routes.map(routeSummary), resource_type:String(event.resourceType || '').toLowerCase()} : undefined;
          const response=await http({postDataBuffer:()=>body,url:()=>request.url,method:()=>request.method,headers:()=>request.headers}, routing);
          if (response.route) {
            const route = tab.routes.find(entry => entry.id === response.route.id);
            if (!route) throw new Error(`route ${response.route.id} was removed while the request was pending`);
            if (response.route.kind === 'abort') throw new Error(`aborted by route ${response.route.id}`);
            await session.send('Fetch.fulfillRequest', fulfillment(route, event.requestId));
            return;
          }
          await session.send('Fetch.fulfillRequest',{requestId:event.requestId,responseCode:response.status,responseHeaders:Object.entries(response.headers).flatMap(([name,value])=>(Array.isArray(value)?value:[value]).map(item=>({name,value:String(item)}))),body:response.body});
        } catch (error) {
          record(tab,'network',{kind:'blocked',operation_id:owner,url:clip(event.request.url),error:clip(error.message)});
          await session.send('Fetch.failRequest',{requestId:event.requestId,errorReason:'BlockedByClient'}).catch(()=>{});
        } finally {
          activeRoutes--;
        }
      })().catch(error=>record(tab,'console',{type:'bridge-error',text:clip(error.message)}));
    });
    await session.send('Network.setCacheDisabled',{cacheDisabled:true});
    await session.send('Fetch.enable',{patterns:[{urlPattern:'*',requestStage:'Request'}]});
  });
  tab.ready.catch(error=>record(tab,'console',{type:'bridge-initialize-error',text:clip(error.message)}));
  return id;
}
async function initialize(request) {
  if (browser) return { initialized: true };
  attached = !!process.env.DIAL_CDP_URL;
  if (attached && request.ignore_https_errors === true) throw new Error('ignore_https_errors cannot be applied to an attached browser (DIAL_CDP_URL); the operator browser owns its TLS settings');
  const viewport = { width:1280, height:720 };
  try {
    if (attached) {
      // Operator-started browser: external contexts keep their own state and are never closed by Dial.
      browser = await chromium.connectOverCDP(process.env.DIAL_CDP_URL, { timeout: request.timeout_ms ?? 0 });
      context = browser.contexts()[0] || await browser.newContext({ viewport });
    } else {
      browser = await chromium.launch({ headless: request.headless !== false, executablePath: process.env.DIAL_CHROMIUM, timeout: request.timeout_ms ?? 0,
        args: ['--use-gl=angle', '--use-angle=swiftshader', '--disable-background-networking', '--disable-component-update', '--disable-sync', '--disable-quic', '--force-webrtc-ip-handling-policy=disable_non_proxied_udp', '--disable-features=WebRtcHideLocalIpsWithMdns', '--proxy-server=http://127.0.0.1:9', '--proxy-bypass-list=<-loopback>', ...(request.stealth?.args ?? [])],
        ...(request.stealth ? { ignoreDefaultArgs: request.stealth.ignoreDefaultArgs } : {}) });
      context = await browser.newContext({ serviceWorkers: 'block', acceptDownloads: true, viewport, ignoreHTTPSErrors: request.ignore_https_errors === true,
        storageState: request.storage_state || undefined,
        recordVideo: request.record_video ? { dir: artifactsDir() } : undefined });
      if (request.stealth?.script) await context.addInitScript(request.stealth.script);
      if (typeof context.routeWebSocket !== 'function') throw new Error('Playwright >=1.48 is required for fail-closed WebSocket interception');
      // Only exact loopback endpoints approved by the Rust policy connect; everything else is closed.
      await context.routeWebSocket('**/*', guardSocket);
    }
    await context.addInitScript(webmcpModule.installHook, webmcpModule.KEY);
    // Attached browsers already own pages; register them so tabs/tab-less actions see them.
    for (const page of context.pages()) { try { register(page); } catch (_) {} }
    context.on('page', page => { try { register(page); } catch (_) {} });
    return { initialized:true, engine:'chromium', version:browser.version(), attached };
  } catch (error) {
    // A failed setup must not leave a half-initialized browser that later calls report as ready.
    const failed = browser;
    browser = null;
    context = null;
    if (failed && !attached) await failed.close().catch(() => {});
    throw error;
  }
}
function tabFor(request) {
  if (request.tab === undefined) {
    if (tabs.size === 1) request.tab = tabs.keys().next().value;
    else if (tabs.size === 0) throw new Error('No browser tab is open. Use action=open with the actual HTTP(S) URL first, then use the returned tab id.');
    else throw new Error('Multiple browser tabs are open. Use action=tabs and pass an explicit tab id.');
  }
  const id = request.tab;
  const tab = tabs.get(id);
  if (!tab) throw new Error(`Browser tab ${id} is not open. Use action=tabs to find an open tab or action=open to create one.`);
  return tab;
}
function required(request, key) {
  if (typeof request[key] !== 'string') throw new Error(`${key} must be a string`);
  return request[key];
}
function frameEntries(tab) {
  const frames = tab.page.frames();
  if (frames.length > 64) throw new Error('frame inspection exceeds 64 frames');
  for (const frame of frames) if (!tab.frames.has(frame)) tab.frames.set(frame,tab.nextFrame++);
  for (const frame of tab.frames.keys()) if (!frames.includes(frame)) tab.frames.delete(frame);
  return frames.map(frame=>({frame:tab.frames.get(frame),parent:frame.parentFrame() ? tab.frames.get(frame.parentFrame()) : null,name:clip(frame.name()),url:clip(frame.url()),main:frame===tab.page.mainFrame()}));
}
function scopeFor(tab,request) {
  frameEntries(tab);
  if (request.frame === undefined) return tab.page.mainFrame();
  for (const [frame,id] of tab.frames) if (id===request.frame) return frame;
  throw new Error(`Frame ${request.frame} is not attached; use action=frames to discover current frame ids`);
}
async function snapshot(scope,request) {
  const maxNodes=request.max_nodes ?? 300;
  if (!Number.isInteger(maxNodes) || maxNodes<1 || maxNodes>1000) throw new Error('max_nodes must be 1..1000');
  return scope.locator(request.selector || 'body').evaluate((root,maxNodes)=>{
    const nodes=[], walker=document.createTreeWalker(root,NodeFilter.SHOW_ELEMENT);
    let element=root, visited=0, chars=0, truncated=false;
    const trim=value=>String(value || '').replace(/\s+/g,' ').trim().slice(0,300);
    const selectorFor=node=>{
      const parts=[];
      while (node && node.nodeType===1 && parts.length<12) {
        if (node.id) { parts.unshift('#'+CSS.escape(node.id)); break; }
        let nth=1;
        for (let sibling=node.previousElementSibling;sibling;sibling=sibling.previousElementSibling) if (sibling.tagName===node.tagName) nth++;
        parts.unshift(node.localName+`:nth-of-type(${nth})`);
        node=node.parentElement;
      }
      return parts.join(' > ');
    };
    const implicitRole=node=>{
      const tag=node.localName;
      if (tag==='input') return ({checkbox:'checkbox',radio:'radio',range:'slider',number:'spinbutton',button:'button',submit:'button',reset:'button',file:'button',hidden:null})[node.type] || (node.type==='hidden' ? null : 'textbox');
      return ({a:node.hasAttribute('href') ? 'link' : null,button:'button',select:node.multiple ? 'listbox' : 'combobox',textarea:'textbox',img:'img',h1:'heading',h2:'heading',h3:'heading',h4:'heading',h5:'heading',h6:'heading',ul:'list',ol:'list',li:'listitem',table:'table',tr:'row',th:'columnheader',td:'cell',nav:'navigation',main:'main',form:'form',dialog:'dialog',iframe:'iframe'})[tag] || null;
    };
    while (element) {
      if (++visited>5000) { truncated=true; break; }
      const style=getComputedStyle(element);
      if (style.display!=='none' && style.visibility!=='hidden' && element.getClientRects().length && element.getAttribute('aria-hidden')!=='true' && !element.closest('[aria-hidden="true"],[hidden]')) {
        const tag=element.localName, role=element.getAttribute('role') || implicitRole(element);
        const password=tag==='input' && element.type==='password';
        const directText=password ? '' : trim([...element.childNodes].filter(n=>n.nodeType===3).map(n=>n.textContent).join(' '));
        if (role || directText || element.hasAttribute('tabindex')) {
          const labelled=(element.getAttribute('aria-labelledby') || '').split(/\s+/).slice(0,8).map(id=>document.getElementById(id)?.textContent || '').join(' ');
          const name=trim(labelled || element.getAttribute('aria-label') || [...(element.labels || [])].map(label=>label.textContent).join(' ') || element.getAttribute('alt') || element.getAttribute('title') || (role==='button' || role==='link' || role==='heading' ? element.textContent : ''));
          const node={selector:selectorFor(element),tag,...(role ? {role} : {}),...(name ? {name} : {}),...(directText ? {text:directText} : {})};
          if (['input','textarea','select'].includes(tag) && !password && element.type!=='file') node.value=trim(element.value);
          if (password) node.redacted='password';
          if ('disabled' in element) node.disabled=element.disabled;
          if ('checked' in element && ['checkbox','radio'].includes(element.type)) node.checked=element.checked;
          for (const attribute of ['aria-expanded','aria-checked','aria-selected','aria-required','aria-invalid','placeholder','href']) if (element.hasAttribute(attribute)) node[attribute]=trim(element.getAttribute(attribute));
          if (element===document.activeElement) node.focused=true;
          const size=JSON.stringify(node).length;
          if (nodes.length>=maxNodes || chars+size>17000) { truncated=true; break; }
          chars+=size; nodes.push(node);
        }
      }
      element=walker.nextNode();
    }
    return {source:'dom-semantic',url:location.href.slice(0,2000),nodes,truncated,visited_elements:Math.min(visited,5000),limits:{nodes:maxNodes,characters:20000,visited_elements:5000}};
  },maxNodes);
}
async function transferDownload(tab,scope,request) {
  const selector=required(request,'selector');
  let download, timer, onDownload, onClose, evidence;
  const arrived=new Promise((resolve,reject)=>{
    onDownload=value=>{
      if (download) return;
      download=value;
      evidence=tab.downloads.at(-1);
      resolve(value);
    };
    tab.page.on('download',onDownload);
    onClose=()=>reject(new Error('tab closed before download arrived'));
    tab.page.once('close',onClose);
    if (request.timeout_ms) timer=setTimeout(()=>reject(new Error('download event deadline exceeded')),request.timeout_ms);
  });
  tab.downloadArmed=true;
  try {
    await Promise.all([arrived,scope.locator(selector).click()]);
    const stream=await download.createReadStream();
    if (!stream) throw new Error(await download.failure() || 'download has no readable stream');
    const chunks=[];
    let size=0;
    for await (const chunk of stream) {
      size+=chunk.length;
      if (size>4*1024*1024) {
        stream.destroy();
        await download.cancel().catch(()=>{});
        throw new Error('download exceeds 4 MiB; nothing written to workspace');
      }
      chunks.push(chunk);
    }
    const failure=await download.failure();
    if (failure) throw new Error(`download failed: ${failure}`);
    if (evidence) { evidence.status='transferred'; evidence.bytes=size; }
    return {tab:request.tab,filename:clip(download.suggestedFilename()),url:clip(download.url()),download_body:Buffer.concat(chunks,size).toString('base64')};
  } catch (error) {
    if (evidence) { evidence.status='failed'; evidence.error=clip(error.message); }
    throw error;
  } finally {
    tab.downloadArmed=false;
    clearTimeout(timer);
    tab.page.off('download',onDownload);
    tab.page.off('close',onClose);
    if (download) {
      await download.cancel().catch(()=>{});
      await download.delete().catch(()=>{});
    }
  }
}
async function run(request) {
  wsEndpoints = Array.isArray(request.websocket_endpoints) ? request.websocket_endpoints.map(String) : wsEndpoints;
  if (request.action === 'initialize') return initialize(request);
  if (!context) throw new Error('browser is not initialized');
  if (siteModule.ACTIONS.has(request.action)) return siteModule.run(request, { context, tabFor });
  if (request.action === 'storage_state') return {storage_state: await context.storageState()};
  if (request.action === 'add_init_script') {
    if (typeof request.source !== 'string' || !request.source) throw new Error('add_init_script requires source');
    const disposable = await context.addInitScript({ content: request.source });
    const id = `init-${++initScriptSeq}`;
    const bytes = Buffer.byteLength(request.source);
    initScripts.set(id, { disposable, bytes });
    return { id, bytes };
  }
  if (request.action === 'remove_init_script') {
    const entry = initScripts.get(request.script_id);
    if (!entry) throw new Error(`No init script with id ${request.script_id}`);
    await entry.disposable.dispose();
    initScripts.delete(request.script_id);
    return { removed: request.script_id };
  }
  if (request.action === 'init_scripts') return { scripts: [...initScripts].map(([id, entry]) => ({ id, bytes: entry.bytes })) };
  if (request.action === 'webmcp_list') {
    const raw = await tabFor(request).page.evaluate(webmcpModule.listInPage, { key: webmcpModule.KEY, name: request.name ?? null });
    return { tab: request.tab, ...raw };
  }
  if (request.action === 'webmcp_invoke') {
    return tabFor(request).page.evaluate(webmcpModule.invokeInPage, { key: webmcpModule.KEY, name: request.name, params: request.params ?? {} });
  }
  if (request.action === 'webmcp_events') {
    return tabFor(request).page.evaluate(webmcpModule.eventsInPage, { key: webmcpModule.KEY, since: request.since ?? 0, clear: request.clear === true });
  }
  if (request.action === 'trace_start') {
    if (tracing) throw new Error('tracing is already running');
    await context.tracing.start({screenshots:true,snapshots:true});
    tracing = true;
    return {tracing:true};
  }
  if (request.action === 'trace_stop') {
    if (!tracing) throw new Error('tracing is not running');
    const trace = path.join(artifactsDir(), `trace-${Date.now()}.zip`);
    await context.tracing.stop({path:trace});
    tracing = false;
    return {trace:clip(trace)};
  }
  if (request.action === 'tabs') return { tabs:[...tabs].map(([id,t]) => ({tab:id,url:clip(t.page.url())})) };
  if (request.action === 'open') {
    const page = await context.newPage();
    const id = register(page);
    await tabs.get(id).ready;
    if (request.emulation) await siteModule.applyTabEmulation(tabs.get(id), request.emulation);
    const response = await page.goto(required(request,'url'), {waitUntil:'domcontentloaded',timeout:request.timeout_ms});
    return { tab:id, url:clip(page.url()), title:clip(await page.title()), status:response?.status() };
  }
  const tab = tabFor(request), page = tab.page;
  await tab.ready;
  page.setDefaultTimeout(request.timeout_ms);
  page.setDefaultNavigationTimeout(request.timeout_ms);
  const scope = scopeFor(tab,request);
  if (pageActions.handles(request)) return pageActions.run(request,{tab,page,scope,frameId:tab.frames.get(scope),cut,clip});
  if (request.frame !== undefined && !['navigate','evaluate','snapshot','aria','audit','click','fill','type','readable','press','select','check','uncheck','hover','focus','wait','upload','download'].includes(request.action)) throw new Error(`frame is not supported for action=${request.action}`);
  if (captureModule.handles(request)) return captureModule.run(request,{tab,page});
  switch (request.action) {
    case 'navigate': {
      const response = await scope.goto(required(request,'url'), {waitUntil:'domcontentloaded'});
      return {tab:request.tab,frame:tab.frames.get(scope),url:clip(scope.url()),title:clip(await scope.title()),status:response?.status()};
    }
    case 'evaluate': {
      return scope.evaluate(async expression => {
        const value = await (0,eval)(expression);
        const encoded = JSON.stringify(value);
        if (encoded === undefined) return {value:null};
        if (encoded.length > 20000) {
          const preview = encoded.slice(0,20000);
          return {value_preview:/[\uD800-\uDBFF]$/.test(preview) ? preview.slice(0,-1) : preview,truncated:true,total_chars:encoded.length};
        }
        return {value:JSON.parse(encoded)};
      },required(request,'expression'));
    }
    case 'snapshot': return {tab:request.tab,frame:tab.frames.get(scope),...await snapshot(scope,request)};
    case 'frames': return {tab:request.tab,frames:frameEntries(tab)};
    case 'dialog': {
      if (!['accept','dismiss'].includes(request.dialog_mode)) throw new Error('dialog_mode must be accept or dismiss');
      tab.dialogPolicy={mode:request.dialog_mode,text:request.text,once:request.once !== false};
      return {tab:request.tab,policy:{...tab.dialogPolicy},dialogs:tab.dialogs};
    }
    case 'upload': {
      if (!Array.isArray(request.upload_files) || !request.upload_files.length || request.upload_files.length>8) throw new Error('upload requires approved workspace file payloads');
      const files=request.upload_files.map(file=>({name:file.name,mimeType:file.mimeType,buffer:Buffer.from(file.data,'base64')}));
      if (files.reduce((size,file)=>size+file.buffer.length,0)>1024*1024) throw new Error('upload exceeds 1 MiB');
      await scope.locator(required(request,'selector')).setInputFiles(files);
      return {tab:request.tab,frame:tab.frames.get(scope),uploaded:files.map(file=>({name:file.name,bytes:file.buffer.length}))};
    }
    case 'download': return transferDownload(tab,scope,request);
    case 'downloads': return {tab:request.tab,downloads:tab.downloads};
    case 'click': await scope.locator(required(request,'selector')).click(); break;
    case 'fill': await scope.locator(required(request,'selector')).fill(required(request,'text')); break;
    case 'type': await scope.locator(required(request,'selector')).pressSequentially(required(request,'text')); break;
    case 'readable': {
      await scope.waitForLoadState('load');
      const source = await fs.promises.readFile(required(process.env,'DIAL_READABILITY'),'utf8');
      // Readability runs inside the page realm through a fresh script tag; the global is removed once read.
      await scope.addScriptTag({content:'(function(){var module={exports:{}};' + source + '\n;window.__dialReadability=module.exports;})();'});
      const article = await scope.evaluate(({selector,limit}) => {
        const Readability = window.__dialReadability;
        delete window.__dialReadability;
        if (typeof Readability !== 'function') throw new Error('Readability failed to load in the page');
        const doc = document.cloneNode(true);
        if (selector) {
          const target = doc.querySelector(selector);
          if (!target) throw new Error('readable selector matched no element');
          doc.body.replaceChildren(target);
        }
        const parsed = new Readability(doc).parse();
        if (!parsed) return null;
        const text = (parsed.textContent || '').replace(/[ \t]+\n/g,'\n').trim();
        let shown = text.slice(0,limit);
        if (/[\uD800-\uDBFF]$/.test(shown)) shown = shown.slice(0,-1);
        return {title:parsed.title || '',byline:parsed.byline || null,excerpt:parsed.excerpt || null,lang:parsed.lang || null,length:text.length,text:shown,truncated:text.length>limit};
      },{selector:request.selector ?? null,limit:READABLE_LIMIT});
      if (!article) throw new Error('readable found no article content on this page');
      return {tab:request.tab,frame:tab.frames.get(scope),url:clip(scope.url()),...article};
    }
    case 'press': await scope.locator(required(request,'selector')).press(required(request,'key')); break;
    case 'select': {
      const values=request.values ?? required(request,'text');
      if (Array.isArray(values) && (values.length>32 || values.some(value=>typeof value!=='string'))) throw new Error('select values must contain at most 32 strings');
      await scope.locator(required(request,'selector')).selectOption(values); break;
    }
    case 'check': await scope.locator(required(request,'selector')).check(); break;
    case 'uncheck': await scope.locator(required(request,'selector')).uncheck(); break;
    case 'hover': await scope.locator(required(request,'selector')).hover(); break;
    case 'focus': await scope.locator(required(request,'selector')).focus(); break;
    case 'wait': await scope.locator(required(request,'selector')).waitFor({state:request.state || 'visible'}); break;
    case 'evidence': return {tab:request.tab,url:clip(page.url()),title:clip(await page.title()),console:tab.console,network:tab.network,dialogs:tab.dialogs,downloads:tab.downloads,navigation:tab.navigation,frames:frameEntries(tab)};
    case 'events': return {tab:request.tab,...tabEvents(tab,request.after)};
    case 'requests': {
      let list = tab.requestLog;
      if (request.resource_type) list = list.filter(record => record.resource_type === request.resource_type);
      if (request.filter) list = list.filter(record => record.url.includes(request.filter));
      return {tab:request.tab, records: list.slice(-(request.limit ?? 50)).reverse().map(publicRecord)};
    }
    case 'request': {
      const record = tab.requestLog.find(entry => entry.id === request.request_id);
      if (!record) throw new Error(`no request ${request.request_id} in this tab's log; the log keeps the most recent ${REQUEST_LOG_MAX} requests`);
      const contentType = (record.response_headers || {})['content-type'] || '';
      const buffer = await responseBody(record);
      const out = {record: publicRecord(record), content_type: clip(contentType)};
      if (buffer) { out.body_bytes = buffer.length; out.body = isTextualType(contentType) ? buffer.subarray(0, REQUEST_BODY_TRANSFER_MAX).toString('utf8') : ''; }
      return out;
    }
    case 'har_start': {
      if (tab.har) throw new Error('a HAR recording is already running for this tab; call har_stop first');
      tab.har = {startSeq: tab.requestSeq + 1, content: request.content ?? 'text'};
      return {tab:request.tab, recording:true, content:tab.har.content};
    }
    case 'har_stop': {
      if (!tab.har) throw new Error('no HAR recording is running for this tab');
      const {startSeq, content} = tab.har;
      tab.har = null;
      const firstKept = tab.requestLog.length ? tab.requestLog[0].id : tab.requestSeq + 1;
      let budget = HAR_BODY_BUDGET, bodiesOmitted = 0;
      const entries = [];
      for (const record of tab.requestLog) {
        if (record.id < startSeq) continue;
        const entry = publicRecord(record);
        const contentType = (record.response_headers || {})['content-type'] || '';
        const textual = isTextualType(contentType);
        if (content !== 'none' && (textual || content === 'all')) {
          const buffer = await responseBody(record);
          if (buffer) {
            const chunk = buffer.subarray(0, REQUEST_BODY_TRANSFER_MAX);
            if (chunk.length <= budget) {
              budget -= chunk.length;
              if (textual) entry.body = chunk.toString('utf8'); else entry.body_base64 = chunk.toString('base64');
            } else bodiesOmitted++;
          }
        }
        entries.push(entry);
      }
      return {tab:request.tab, entries, dropped:Math.max(0, firstKept - startSeq), content, bodies_omitted:bodiesOmitted};
    }
    case 'route': {
      if (tab.routes.length >= ROUTES_MAX) throw new Error(`a tab holds at most ${ROUTES_MAX} routes`);
      const route = {id:++tab.routeSeq, pattern:required(request,'pattern'), resource_type:request.resource_type ?? null, kind:request.route_kind, response:request.response ?? null};
      tab.routes.push(route);
      return {tab:request.tab, route:routeSummary(route), routes:tab.routes.length};
    }
    case 'unroute': {
      const before = tab.routes.length;
      tab.routes = request.pattern == null ? [] : tab.routes.filter(route => route.pattern !== request.pattern);
      return {tab:request.tab, removed:before - tab.routes.length, routes:tab.routes.length};
    }
    case 'routes': return {tab:request.tab, routes:tab.routes.map(routeSummary)};
    case 'back': case 'forward': case 'reload': {
      const options = {waitUntil:'domcontentloaded'};
      const response = await (request.action === 'back' ? page.goBack(options) : request.action === 'forward' ? page.goForward(options) : page.reload(options));
      // null: no history entry in that direction, or a same-document navigation without a response.
      return {tab:request.tab,action:request.action,url:clip(page.url()),title:clip(await page.title()),status:response?.status() ?? null};
    }
    case 'run': return runModule.runCode({page, code: request.code, timeoutMs: request.timeoutMs});
    case 'call': return runModule.runCall({page, chain: request.chain, timeoutMs: request.timeoutMs});
    case 'close': await page.close(); return {closed:request.tab};
    case 'aria': {
      if (typeof scope.locator('body').ariaSnapshot !== 'function') throw new Error('aria snapshots require Playwright >=1.49');
      const text = await scope.locator(request.selector || 'body').ariaSnapshot();
  return {tab:request.tab,frame:tab.frames.get(scope),aria:cut(text,20000),truncated:text.length>20000};
    }
    case 'audit': return {tab:request.tab,frame:tab.frames.get(scope),url:clip(scope.url()),...await scope.evaluate(accessibilityAudit)};
    default: throw new Error(`unsupported browser action ${request.action}`);
  }
  return {tab:request.tab,frame:tab.frames.get(scope),url:clip(scope.url()),action:request.action,completed:true};
}
function failureMessage(error, id) {
  const evidence = [...tabs].map(([tab,t]) => ({tab,url:clip(t.page.url()),console:t.console.slice(-10),network:t.network.slice(-20),network_failures:(t.networkFailures || []).slice(-10),dialogs:t.dialogs.slice(-10),downloads:t.downloads.slice(-10)}));
  const failures = evidence.flatMap(tab=>tab.network_failures).filter(event=>event.operation_id===id);
  const reason = failures.at(-1)?.error;
  return cut((reason ? 'Browser network failure: '+reason+'\n' : '') + String(error.stack || error) + '\nBrowser failure evidence (untrusted page data): ' + JSON.stringify(evidence), 20000);
}
function artifactsDir() {
  const dir = process.env.DIAL_BROWSER_ARTIFACTS;
  if (!dir) throw new Error('browser artifacts directory is not configured');
  return dir;
}
// Runs inside the page realm via Playwright serialization: must stay self-contained (no closures).
function accessibilityAudit() {
  const findings = [];
  const describe = el => (el.tagName || '').toLowerCase() + (el.id ? '#' + el.id : '');
  const add = (rule, el, detail) => { if (findings.length < 200) findings.push({ rule, target: el ? describe(el).slice(0, 120) : null, detail }); };
  const text = el => (el.textContent || '').trim();
  const named = el => !!((el.getAttribute('aria-label') || '').trim() || el.getAttribute('aria-labelledby') || (el.getAttribute('title') || '').trim());
  const labelled = el => named(el) || (!!el.id && !!document.querySelector('label[for="' + CSS.escape(el.id) + '"]')) || !!el.closest('label');
  if (!document.documentElement.getAttribute('lang')) add('html-lang-missing', document.documentElement, 'html element has no lang attribute');
  if (!document.querySelector('main, [role="main"]')) add('main-landmark-missing', null, 'page has no main landmark');
  for (const img of document.querySelectorAll('img')) {
    if (!img.hasAttribute('alt') && !['presentation', 'none'].includes(img.getAttribute('role'))) add('img-alt-missing', img, 'img has no alt attribute');
  }
  for (const control of document.querySelectorAll('input, select, textarea')) {
    const type = (control.getAttribute('type') || '').toLowerCase();
    if (['hidden', 'submit', 'button', 'reset', 'image'].includes(type)) continue;
    if (!labelled(control)) add('control-label-missing', control, 'form control has no associated label');
  }
  for (const button of document.querySelectorAll('button, [role="button"]')) {
    if (!text(button) && !named(button) && !button.querySelector('img[alt]')) add('button-name-missing', button, 'button has no accessible name');
  }
  for (const link of document.querySelectorAll('a[href]')) {
    if (!text(link) && !named(link) && !link.querySelector('img[alt]')) add('link-name-missing', link, 'link has no accessible name');
  }
  const ids = new Map();
  for (const el of document.querySelectorAll('[id]')) ids.set(el.id, (ids.get(el.id) || 0) + 1);
  for (const [id, count] of ids) if (count > 1) add('duplicate-id', null, 'id "' + id.slice(0, 80) + '" appears ' + count + ' times');
  let previous = 0;
  for (const heading of document.querySelectorAll('h1, h2, h3, h4, h5, h6')) {
    const level = Number(heading.tagName.slice(1));
    if (!text(heading) && !named(heading)) add('empty-heading', heading, 'heading has no text');
    if (previous && level > previous + 1) add('heading-level-skip', heading, 'heading jumps from h' + previous + ' to h' + level);
    previous = level;
  }
  const counts = {};
  for (const finding of findings) counts[finding.rule] = (counts[finding.rule] || 0) + 1;
  return { passed: findings.length === 0, counts, findings };
}
let queue = Promise.resolve();
readline.createInterface({input:process.stdin}).on('line', line => {
  let request;
  try { request = JSON.parse(line); } catch (error) { process.stderr.write(error.message+'\n'); return; }
  if (!request || typeof request !== 'object') { process.stderr.write('browser protocol line is not an object\n'); return; }
  if (request.callback_id) {
    finishCallback(request.callback_id,request.error ? new Error(request.error) : null,request.result);
    return;
  }
  queue = queue.then(async () => {
    operationId = request.id;
    const deadline = request.timeout_ms ? Date.now()+request.timeout_ms : null;
    operationDeadline = deadline;
    try {
      const result = await run(request);
      if (request.action !== 'initialize') {
        const navigation = ['open','navigate','reload','back','forward'].includes(request.action);
        const settlement = await settleRequests(deadline, navigation ? 15000 : 1000, navigation ? 500 : 100);
        if (!settlement.settled) result.network_settlement = settlement;
        if (navigation && result.tab) {
          const tab = tabs.get(result.tab);
          const scope = scopeFor(tab, request);
          result.url = clip(scope.url());
          result.title = clip(await scope.title());
          const failures = (tab.networkFailures || []).filter(event=>event.operation_id === request.id);
          if (failures.length) result.network_failures = failures.slice(-10);
        }
      }
      await send({id:request.id,result});
    }
    catch (error) {
      try { await send({id:request.id,result:{browser_error:failureMessage(error,request.id)}}); }
      catch (_) { process.exit(1); }
    } finally {
      operationId = 0;
      operationDeadline = null;
      cancelPending('approved browser operation finished');
    }
  });
});
async function stop() { if (browser && !attached) await browser.close().catch(() => {}); process.exit(0); }
process.on('SIGTERM', stop);
process.on('SIGINT', stop);
process.stdin.on('end', stop);
