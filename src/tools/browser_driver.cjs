'use strict';
// Trusted adapter; expressions execute only in Chromium's page realm, never in Node.
const readline = require('node:readline');
const { chromium } = require(process.env.DIAL_PLAYWRIGHT);
const MAX_PENDING = 16, MAX_PENDING_BYTES = 2 * 1024 * 1024, MAX_STDOUT_BYTES = 8 * 1024 * 1024;
function send(value) {
  const line = JSON.stringify(value) + '\n';
  const size = Buffer.byteLength(line);
  if (size > MAX_STDOUT_BYTES || process.stdout.writableNeedDrain || process.stdout.writableLength + size > MAX_STDOUT_BYTES) {
    return Promise.reject(new Error('browser protocol backpressure/buffer budget exceeded'));
  }
  return new Promise((resolve, reject) => process.stdout.write(line, error => error ? reject(error) : resolve()));
}
const callbacks = new Map();
let callbackId = 0, browser, context, nextTab = 1, operationId = 0, pendingBytes = 0;
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
}
async function settleRequests(deadline) {
  const end = Math.min(deadline,Date.now()+5000);
  let quietSince = 0;
  while (Date.now() < end) {
    if (callbacks.size === 0) {
      if (!quietSince) quietSince=Date.now();
      if (Date.now()-quietSince >= 100) return;
    } else quietSince=0;
    await new Promise(resolve=>setTimeout(resolve,10));
  }
  cancelPending('browser network settlement deadline exceeded');
}
const tabs = new Map();
const clip = value => String(value).slice(0, 2000);
function record(list, value) { list.push(value); if (list.length > 100) list.shift(); }
function http(request) {
  if (!operationId) return Promise.reject(new Error('browser networking is paused between approved operations'));
  if (callbacks.size >= MAX_PENDING) return Promise.reject(new Error('browser pending request budget exceeded'));
  const body = request.postDataBuffer();
  if (body && body.length > 1024 * 1024) return Promise.reject(new Error('request body exceeds 1 MiB'));
  const id = ++callbackId;
  const event = { callback:'http', callback_id:id, operation_id:operationId, expires_at_ms:Date.now()+5000, url:request.url(), method:request.method(), headers:request.headers(), body:body ? body.toString('base64') : '' };
  const bytes = Buffer.byteLength(JSON.stringify(event));
  if (pendingBytes + bytes > MAX_PENDING_BYTES) return Promise.reject(new Error('browser pending request byte budget exceeded'));
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => finishCallback(id,new Error('browser network callback expired')),5000);
    callbacks.set(id, {resolve,reject,bytes,timer});
    pendingBytes += bytes;
    send(event).catch(error => finishCallback(id,error));
  });
}
function register(page) {
  for (const [id, tab] of tabs) if (tab.page === page) return id;
  if (tabs.size >= 16) { page.close().catch(() => {}); throw new Error('maximum 16 browser tabs'); }
  const id = nextTab++;
  const tab = { page, console: [], network: [] };
  tabs.set(id, tab);
  page.on('console', message => { const location=message.location(); record(tab.console, { type:message.type(), text:clip(message.text()), location:{...location,url:clip(location.url)} }); });
  page.on('pageerror', error => record(tab.console, { type: 'pageerror', text: clip(error.message) }));
  page.on('request', request => record(tab.network, { kind: 'request', method: request.method(), url: clip(request.url()), resource: request.resourceType() }));
  page.on('response', response => record(tab.network, { kind: 'response', status: response.status(), url: clip(response.url()) }));
  page.on('requestfailed', request => record(tab.network, { kind:'failed', url:clip(request.url()), error:clip(request.failure()?.errorText || '') }));
  page.on('close', () => tabs.delete(id));
  page.on('download', download => { download.cancel().catch(() => {}); record(tab.console, {type:'blocked-download',text:clip(download.suggestedFilename())}); });
  // CDP Fetch reports every redirect hop. Playwright context.route only intercepts the initial URL.
  tab.ready = context.newCDPSession(page).then(async session => {
    tab.cdp = session;
    session.on('Fetch.requestPaused', event => {
      (async () => {
        try {
          const request=event.request;
          const body=request.postDataEntries?.length
            ? Buffer.concat(request.postDataEntries.map(entry=>Buffer.from(entry.bytes || '','base64')))
            : request.postData === undefined ? null : Buffer.from(request.postData,'utf8');
          const response=await http({postDataBuffer:()=>body,url:()=>request.url,method:()=>request.method,headers:()=>request.headers});
          await session.send('Fetch.fulfillRequest',{requestId:event.requestId,responseCode:response.status,responseHeaders:Object.entries(response.headers).map(([name,value])=>({name,value})),body:response.body});
        } catch (error) {
          record(tab.network,{kind:'blocked',operation_id:operationId,url:clip(event.request.url),error:clip(error.message)});
          await session.send('Fetch.failRequest',{requestId:event.requestId,errorReason:'BlockedByClient'}).catch(()=>{});
        }
      })().catch(error=>record(tab.console,{type:'bridge-error',text:clip(error.message)}));
    });
    await session.send('Network.setCacheDisabled',{cacheDisabled:true});
    await session.send('Fetch.enable',{patterns:[{urlPattern:'*',requestStage:'Request'}]});
  });
  tab.ready.catch(error=>record(tab.console,{type:'bridge-initialize-error',text:clip(error.message)}));
  return id;
}
async function initialize(request) {
  if (browser) return { initialized: true };
  browser = await chromium.launch({ headless: true, executablePath: process.env.DIAL_CHROMIUM, timeout: request.timeout_ms || 30000,
    args: ['--use-gl=angle', '--use-angle=swiftshader', '--disable-background-networking', '--disable-component-update', '--disable-sync', '--disable-quic', '--force-webrtc-ip-handling-policy=disable_non_proxied_udp', '--disable-features=WebRtcHideLocalIpsWithMdns', '--proxy-server=http://127.0.0.1:9', '--proxy-bypass-list=<-loopback>'] });
  context = await browser.newContext({ serviceWorkers: 'block', acceptDownloads: false, viewport: { width:1280, height:720 } });
  if (typeof context.routeWebSocket !== 'function') throw new Error('Playwright >=1.48 is required for fail-closed WebSocket interception');
  await context.routeWebSocket('**/*', socket => socket.close({ code:1008, reason:'WebSockets are disabled by the guarded browser adapter' }));
  context.on('page', page => { try { register(page); } catch (_) {} });
  return { initialized:true, engine:'chromium', version:browser.version() };
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
async function run(request) {
  if (request.action === 'initialize') return initialize(request);
  if (!context) throw new Error('browser is not initialized');
  if (request.action === 'tabs') return { tabs:[...tabs].map(([id,t]) => ({tab:id,url:clip(t.page.url())})) };
  if (request.action === 'open') {
    const page = await context.newPage();
    const id = register(page);
    await tabs.get(id).ready;
    const response = await page.goto(required(request,'url'), {waitUntil:'domcontentloaded',timeout:request.timeout_ms});
    return { tab:id, url:clip(page.url()), title:clip(await page.title()), status:response?.status() };
  }
  const tab = tabFor(request), page = tab.page;
  await tab.ready;
  page.setDefaultTimeout(request.timeout_ms);
  page.setDefaultNavigationTimeout(request.timeout_ms);
  switch (request.action) {
    case 'navigate': {
      const response = await page.goto(required(request,'url'), {waitUntil:'domcontentloaded'});
      return {tab:request.tab,url:clip(page.url()),title:clip(await page.title()),status:response?.status()};
    }
    case 'evaluate': {
      return page.evaluate(async expression => {
        const value = await (0,eval)(expression);
        const encoded = JSON.stringify(value === undefined ? null : value);
        return encoded.length <= 20000 ? {value:value === undefined ? null : value} : {value_preview:encoded.slice(0,20000),truncated:true,total_chars:encoded.length};
      },required(request,'expression'));
    }
    case 'click': await page.locator(required(request,'selector')).click(); break;
    case 'fill': await page.locator(required(request,'selector')).fill(required(request,'text')); break;
    case 'press': await page.locator(required(request,'selector')).press(required(request,'key')); break;
    case 'select': await page.locator(required(request,'selector')).selectOption(required(request,'text')); break;
    case 'wait': await page.locator(required(request,'selector')).waitFor({state:request.state || 'visible'}); break;
    case 'screenshot': {
      if (request.full_page) {
        const height = await page.evaluate(() => Math.max(document.body?.scrollHeight || 0, document.documentElement.scrollHeight));
        if (height > 12000) throw new Error('full-page screenshot exceeds 12000px height; use viewport screenshot');
      }
      const bytes = await page.screenshot({type:'png',fullPage:!!request.full_page,timeout:request.timeout_ms});
      if (bytes.length > 5 * 1024 * 1024) throw new Error('screenshot exceeds the provider attachment limit of 5 MiB; use a viewport screenshot');
      return {tab:request.tab,url:clip(page.url()),artifact:{media_type:'image/png',encoding:'base64',data:bytes.toString('base64')}};
    }
    case 'evidence': return {tab:request.tab,url:clip(page.url()),title:clip(await page.title()),console:tab.console,network:tab.network};
    case 'close': await page.close(); return {closed:request.tab};
    default: throw new Error(`unsupported browser action ${request.action}`);
  }
  return {tab:request.tab,url:clip(page.url()),action:request.action,completed:true};
}
function failureMessage(error, id) {
  const evidence = [...tabs].map(([tab,t]) => ({tab,console:t.console.slice(-10),network:t.network.slice(-20)}));
  const failures = evidence.flatMap(tab=>tab.network).filter(event=>event.kind==='blocked' && event.operation_id===id);
  const reason = failures.at(-1)?.error;
  return ((reason ? 'Browser network failure: '+reason+'\n' : '') + String(error.stack || error) + '\nBrowser failure evidence: ' + JSON.stringify(evidence)).slice(0,20000);
}
let queue = Promise.resolve();
readline.createInterface({input:process.stdin}).on('line', line => {
  let request;
  try { request = JSON.parse(line); } catch (error) { process.stderr.write(error.message+'\n'); return; }
  if (request.callback_id) {
    finishCallback(request.callback_id,request.error ? new Error(request.error) : null,request.result);
    return;
  }
  queue = queue.then(async () => {
    operationId = request.id;
    const deadline = Date.now()+(request.timeout_ms || 30000);
    try {
      const result = await run(request);
      if (request.action !== 'initialize') await settleRequests(deadline);
      await send({id:request.id,result});
    }
    catch (error) {
      try { await send({id:request.id,error:failureMessage(error,request.id)}); }
      catch (_) { process.exit(1); }
    } finally {
      operationId = 0;
      cancelPending('approved browser operation finished');
    }
  });
});
async function stop() { if (browser) await browser.close().catch(() => {}); process.exit(0); }
process.on('SIGTERM', stop);
process.on('SIGINT', stop);
process.stdin.on('end', stop);
