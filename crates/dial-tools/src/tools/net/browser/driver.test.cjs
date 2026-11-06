'use strict';
// Deterministic producer-side protocol tests: make test-drivers
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(require('node:path').join(__dirname,'driver.cjs'),'utf8');
function harness({realTimers = false} = {}) {
  const timers = new Set(), frames = [];
  let onLine;
  const stdout = { writableNeedDrain:false,writableLength:0,write(line,callback) { frames.push(JSON.parse(line));callback();return true; } };
  const context = vm.createContext({ Buffer, setTimeout: realTimers ? setTimeout : (fn) => {timers.add(fn);return fn;},clearTimeout: realTimers ? clearTimeout : (fn) => {timers.delete(fn);},
    require(name) { return name === 'node:readline' ? {createInterface(){return {on(event,callback){if(event === 'line') onLine=callback;}};}} : {chromium:{}}; },
    process:{env:{DIAL_PLAYWRIGHT:'external-operator-module'},stdout,stderr:{write(){}},on(){},stdin:{on(){}},exit(){}},
  });
  vm.runInContext(source,context);
  context.fakeRequest = {postDataBuffer(){return null;},url(){return 'https://example.com/flood';},method(){return 'GET';},headers(){return {};} };
  return {run(code){return vm.runInContext(code,context);},dispatch(request){onLine(JSON.stringify(request));return vm.runInContext('queue',context);},timers,frames,stdout};
}
test('post-action settlement is bounded even with unlimited action time', async () => {
  const h = harness({realTimers:true});
  h.run('activeHttp = 1');
  const start = Date.now();
  const result = await h.run('settleRequests(null)');
  assert.equal(result.settled, false);
  assert.equal(result.pending, 1);
  assert(Date.now() - start < 3000);
});
test('an idle operation reports settled rather than a network failure', async () => {
  const h = harness({realTimers:true});
  assert.equal((await h.run('settleRequests(null)')).settled, true);
});
test('idle pages cannot queue network callbacks',async () => {
  const h=harness();
  await assert.rejects(h.run('http(fakeRequest)'),/paused between approved operations/);
  assert.equal(h.run('callbacks.size'),0);
  assert.equal(h.frames.length,0);
});
test('missing tab selects one existing tab but never guesses among several', () => {
  const h=harness();
  assert.throws(()=>h.run('tabFor({})'),/No browser tab is open \(tabs do not survive a browser restart/);
  assert.throws(()=>h.run('tabFor({tab:3})'),/Browser tab 3 is not open \(no tab is open; tabs do not survive/);
  h.run('tabs.set(4,{page:{title:"preview",url:()=>"http://localhost:3000/"}}); request={}; tabFor(request)');
  assert.equal(h.run('request.tab'),4);
  h.run('tabs.set(5,{page:{title:"other",url:()=>{throw new Error("closed")}}})');
  // Errors list the open tabs so the next call can pick one without action=tabs.
  assert.throws(()=>h.run('tabFor({})'),/Multiple browser tabs are open; pass an explicit tab id \(open tabs: 4 http:\/\/localhost:3000\/, 5 \?\)/);
  assert.throws(()=>h.run('tabFor({tab:999})'),/Browser tab 999 is not open \(open tabs: 4 http:\/\/localhost:3000\/, 5 \?\)/);
  assert.equal(h.run('tabFor({tab:4}).page.title'),'preview');
});
test('pending callbacks and queued requests remain bounded on a flood',async () => {
  const h=harness();
  h.run('operationId = 42');
  const promises=h.run('Array.from({length:5000},()=>http(fakeRequest).catch(error=>error.message))');
  await Promise.resolve();
  assert.equal(h.run('callbacks.size'),16);
  assert(h.run('pendingBytes')<=2*1024*1024);
  assert.equal(h.frames.length,16);
  assert.equal(h.run('httpQueue.length'),4096);
  assert(h.frames.every(frame=>frame.operation_id===42));
  h.run('cancelPending("test cleanup")');
  const errors = await Promise.all(promises);
  assert(errors.some(error=>error.includes('queue budget exceeded')));
  assert.equal(h.run('queuedHttpBytes'),0);
  assert.equal(h.run('callbacks.size'),0);
  assert.equal(h.run('pendingBytes'),0);
});

test('pending callbacks expire and release their slots',async () => {
  const h=harness();
  h.run('operationId = 42');
  const promises=h.run('Array.from({length:16},()=>http(fakeRequest).catch(error=>error.message))');
  await Promise.resolve();
  for (const timer of [...h.timers]) timer();
  const errors = await Promise.all(promises);
  assert(errors.every(error=>error.includes('expired')));
  assert.equal(h.run('callbacks.size'),0);
  assert.equal(h.run('pendingBytes'),0);
  assert.equal(h.run('activeHttp'),0);
});
test('stdout backpressure rejects new frames without buffering',async () => {
  const h=harness();
  h.run('operationId = 1');
  h.stdout.writableNeedDrain=true;
  await assert.rejects(h.run('http(fakeRequest)'),/backpressure/);
  assert.equal(h.frames.length,0);
  assert.equal(h.run('callbacks.size'),0);
  assert.equal(h.run('pendingBytes'),0);
});
test('callback byte budget prevents large concurrent uploads',async () => {
  const h=harness();
  h.run('operationId = 1; fakeRequest.postDataBuffer = () => Buffer.alloc(1024*1024)');
  const promises=h.run('Array.from({length:100},()=>http(fakeRequest).catch(error=>error.message))');
  await Promise.resolve();
  assert.equal(h.run('callbacks.size'),1);
  assert(h.run('pendingBytes')<=2*1024*1024);
  assert(h.run('queuedHttpBytes')<=8*1024*1024);
  h.run('cancelPending("test cleanup")');
  const errors = await Promise.all(promises);
  assert(errors.some(error=>error.includes('queue byte budget exceeded')));
  assert.equal(h.run('queuedHttpBytes'),0);
  assert.equal(h.run('pendingBytes'),0);
});

test('HTTP bursts wait for capacity instead of dropping requests, and cancellation clears the queue',async () => {
  const h=harness();
  h.run('operationId = 1');
  const promises=h.run('Array.from({length:20},()=>http(fakeRequest).catch(error=>error.message))');
  await Promise.resolve();
  assert.equal(h.run('activeHttp'),16);
  assert.equal(h.run('httpQueue.length'),4);
  h.run('finishCallback(1, new Error("first request ended"))');
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(h.run('httpQueue.length'),3);
  assert.equal(h.run('activeHttp'),16);
  h.run('operationId = 0; cancelPending("operation finished")');
  await Promise.all(promises);
  assert.equal(h.run('activeHttp'),0);
  assert.equal(h.run('httpQueue.length'),0);
  assert.equal(h.run('callbacks.size'),0);
});

test('network failures lead with the current operation cause, not stale tab failures', () => {
  const h=harness();
  h.run(`tabs.set(1,{page:{url(){return 'https://example.test/'}},console:[],dialogs:[],downloads:[],network:[
    {kind:'blocked',operation_id:1,error:'stale failure'},
    {kind:'blocked',operation_id:2,error:'refusing browser request to 127.0.0.1: loopback address'}
  ]})`);
  h.run(`tabs.get(1).networkFailures = tabs.get(1).network`);
  const message=h.run(`failureMessage(new Error('page.goto: net::ERR_BLOCKED_BY_CLIENT'),2)`);
  assert(message.startsWith('Browser network failure: refusing browser request'));
  assert(message.includes('page.goto: net::ERR_BLOCKED_BY_CLIENT'));
  assert(message.includes('Browser failure evidence (untrusted page data):'));
  assert(!h.run(`failureMessage(new Error('page crashed'),3)`).startsWith('Browser network failure:'));
});

test('events merge tab channels in sequence order and return only newer entries', () => {
  const h=harness();
  h.run(`tab={seq:0,console:[],network:[],dialogs:[],downloads:[],navigation:[]};
    record(tab,'navigation',{kind:'navigated',url:'https://example.test/'});
    record(tab,'network',{kind:'request',url:'https://example.test/'});
    record(tab,'console',{type:'log',text:'ready'})`);
  const all=h.run('tabEvents(tab)');
  assert.deepEqual([...all.events].map(event=>[event.seq,event.channel]),[[1,'navigation'],[2,'network'],[3,'console']]);
  assert.equal(all.last_seq,3);
  assert.deepEqual([...h.run('tabEvents(tab,2)').events].map(event=>event.text),['ready']);
  assert.equal(h.run('tabEvents(tab,3)').events.length,0);
  assert.throws(()=>h.run('tabEvents(tab,-1)'),/non-negative integer/);
  h.run(`for (let i=0;i<150;i++) record(tab,'console',{type:'log',text:String(i)})`);
  assert.equal(h.run('tab.console.length'),100);
  assert.equal(h.run('tab.seq'),153);
});

test('run uses DOM globals and assert/sleep in the page realm, not Playwright or Node', async () => {
  const { runCode } = require('./run.cjs');
  const heading = { textContent: 'Page heading' };
  const context = vm.createContext({
    document: { title: 'DOM title', querySelector(selector) { return selector === 'h1' ? heading : null; } },
    setTimeout,
  });
  // Like Playwright, serialize the actual helper into a separate page realm, without Node bindings.
  const page = { evaluate(fn, arg) {
    context.input = arg;
    return vm.runInContext(`(${fn.toString()})(input)`, context);
  } };
  const result = await runCode({ page, timeoutMs: 1000, code: `
    assert(document.querySelector('h1').textContent === 'Page heading');
    await sleep(0);
    assert(typeof page === 'undefined', 'Playwright page must not be injected');
    assert(typeof require === 'undefined' && typeof process === 'undefined', 'Node must not be exposed');
    return document.title;
  ` });
  assert.equal(result.value, 'DOM title');
  await assert.rejects(runCode({ page, timeoutMs: 1000, code: 'return page.title();' }), /page is not defined/);
  await assert.rejects(runCode({ page, timeoutMs: 1000, code: "assert(false, 'DOM assertion failed');" }), /DOM assertion failed/);
});

test('navigation settlement includes response fulfillment and continues beyond one second', async () => {
  const h = harness({realTimers:true});
  h.run('activeRoutes = 1');
  const result = h.run('settleRequests(null, 2500)');
  await new Promise(resolve=>setTimeout(resolve,1200));
  h.run('activeRoutes = 0');
  assert.equal((await result).settled,true);
});

test('navigation settlement still respects the approved operation deadline', async () => {
  const h = harness({realTimers:true});
  h.run('activeRoutes = 1');
  const start = Date.now();
  assert.equal((await h.run('settleRequests(Date.now()+100,15000)')).settled,false);
  assert(Date.now()-start < 1000);
});

test('module bursts exceeding the old queue are admitted, and never cross operation ownership', async () => {
  const h = harness();
  h.run('operationId=1');
  const pending=h.run('Array.from({length:1500},()=>http(fakeRequest).catch(error=>error.message))');
  await Promise.resolve();
  assert.equal(h.run('httpQueue.length'),1484);
  h.run('operationId=2; finishCallback(1,new Error("ended"))');
  for (let i=0;i<10;i++) await Promise.resolve();
  assert.equal(h.frames.length,16,'queued requests may not dispatch in the next operation');
  h.run('cancelPending("cleanup")');
  await Promise.all(pending);
  assert.equal(h.run('activeHttp'),0);
  assert.equal(h.run('queuedHttpBytes'),0);
});

test('blocked causes survive ordinary network history eviction with bounded storage and cursor filtering', () => {
  const h = harness();
  h.run(`tab={seq:0,page:{url(){return 'http://localhost/'}},console:[],network:[],dialogs:[],downloads:[],navigation:[]};
    tabs.set(1,tab);
    record(tab,'network',{kind:'blocked',operation_id:2,error:'queue exhausted'});
    for(let i=0;i<150;i++) record(tab,'network',{kind:'response'});`);
  assert.equal(h.run('tabEvents(tab).network_failures[0].error'),'queue exhausted');
  assert.equal(h.run('tabEvents(tab,1).network_failures.length'),0);
  assert(h.run(`failureMessage(new Error('timeout'),2)`).startsWith('Browser network failure: queue exhausted'));
  h.run(`for(let i=0;i<150;i++) record(tab,'network',{kind:'blocked',error:'bounded'})`);
  assert.equal(h.run('tab.networkFailures.length'),100);
});

test('native screenshot returns its exact URL and actual captured image, including long locators', async () => {
  const capture = require('./capture.cjs');
  const url = 'http://localhost:3000/preview?' + 'q='.repeat(3000);
  const bytes = Buffer.from('native pixels');
  const tab = {};
  const page = { url: () => url, screenshot: async () => bytes };
  const result = await capture.run({action:'screenshot',tab:1}, {page,tab});
  assert.equal(result.url, url);
  assert.equal(result.artifact.data, bytes.toString('base64'));
  const unchanged = await capture.run({action:'screenshot',tab:1,if_changed:true}, {page,tab});
  assert.equal(unchanged.changed, false);
  assert.equal(unchanged.artifact, undefined);
});

function clipboardHarness(failure) {
  const site = require('./site.cjs');
  const grants = [], closed = [], revoked = [];
  const scratchPage = {
    async route(pattern, handler) { await handler({fulfill: async () => {}}); },
    async goto() { if (failure === 'navigate') throw new Error('navigate failed'); },
    async bringToFront() { if (failure === 'focus') throw new Error('focus failed'); },
    async evaluate(fn, text) { if (failure === 'evaluate') throw new Error('evaluate failed'); return text === undefined ? 'fixture clipboard' : undefined; },
  };
  const scratch = {
    async grantPermissions(permissions, options) { grants.push({permissions, options}); if (failure === 'grant') throw new Error('grant failed'); },
    async clearPermissions() { revoked.push(true); if (failure === 'revoke') throw new Error('revoke failed'); },
    async newPage() { return scratchPage; },
    async close() { assert(revoked.length > 0, 'revoke before closing'); closed.push(true); if (failure === 'close') throw new Error('close failed'); },
  };
  const context = {
    browser() { return {async newContext() { return scratch; }}; },
    async grantPermissions() { throw new Error('must not mutate user context permissions'); },
  };
  const host = {context, tabFor() { return {ready:Promise.resolve(), page:{url:()=>'https://user.example/'}}; }};
  return {site, host, grants, closed, revoked};
}

test('clipboard uses isolated permissions and closes its controller context on every settled exit', async () => {
  for (const action of ['clipboard_read', 'clipboard_write']) {
    for (const failure of [undefined, 'navigate', 'grant', 'focus', 'evaluate', 'revoke', 'close']) {
      const h = clipboardHarness(failure);
      const operation = h.site.run({action, text:'hello'}, h.host);
      if (failure) await assert.rejects(operation, new RegExp(`${failure} failed`));
      else assert.deepEqual(await operation, action === 'clipboard_read' ? {text:'fixture clipboard'} : {chars:5});
      assert.equal(h.closed.length, 1, `${action}: ${failure}`);
      assert.equal(h.revoked.length, 1, `${action}: ${failure} revokes before close`);
      for (const grant of h.grants) {
        assert.deepEqual(grant.permissions, [action === 'clipboard_read' ? 'clipboard-read' : 'clipboard-write']);
        assert.equal(grant.options.origin, 'https://dial-clipboard.invalid');
      }
    }
  }
});

test('clipboard cancellation closes the isolated grant without changing user permissions', async () => {
  // Use a fresh module because shutdown is terminal for a driver process.
  delete require.cache[require.resolve('./site.cjs')];
  const h = clipboardHarness();
  let started, rejectEvaluation;
  const evaluating = new Promise(resolve => { started = resolve; });
  const originalBrowser = h.host.context.browser();
  const scratch = await originalBrowser.newContext();
  const page = await scratch.newPage();
  page.evaluate = () => new Promise((resolve, reject) => { rejectEvaluation = reject; started(); });
  const close = scratch.close;
  scratch.close = async () => { await close(); rejectEvaluation?.(new Error('controller context closed')); };
  h.host.context.browser = () => ({newContext: async () => scratch});
  const operation = h.site.run({action:'clipboard_read'}, h.host);
  const failed = assert.rejects(operation, /controller context closed/);
  await evaluating;
  await h.site.closeClipboardContexts();
  await failed;
  assert(h.closed.length >= 1);
  assert(h.revoked.length >= 1);
  assert.deepEqual(h.grants.map(grant=>grant.permissions), [['clipboard-read']]);
});

test('observations preserve one successful action despite partial capture failure', async () => {
  const h = harness({realTimers:true});
  h.run(`
    let actionCount=0, shotCount=0;
    tabs.set(1,{seq:0,console:[],network:[],dialogs:[],downloads:[],navigation:[],page:{url:()=> 'http://localhost:3000/'}});
    run=async request=>{
      actionCount++;
      const tab=tabs.get(1);
      record(tab,'console',{type:'log',text:'ignored'});
      record(tab,'console',{type:'error',text:'new failure'});
      record(tab,'network',{kind:'response',status:503});
      return {tab:1,completed:true,action:request.action};
    };
    scopeFor=()=>({});
    snapshot=async()=>{throw new Error('detached frame');};
    captureModule={run:async()=>{shotCount++;return {url:'http://localhost:3000/after',artifact:{encoding:'base64',media_type:'image/png',data:'cGl4ZWxz'}};}};
    record(tabs.get(1),'console',{type:'error',text:'old error'});
  `);
  await h.dispatch({id:1,action:'click',tab:1,observe:{snapshot:true,screenshot:true,events:true}});
  const result=h.frames.at(-1).result;
  assert.equal(h.run('actionCount'),1);
  assert.equal(h.run('shotCount'),1);
  assert.equal(result.completed,true);
  assert.equal(result.action,'click');
  assert.equal(result.browser_error,undefined);
  assert.equal(result.observations.snapshot.error,'detached frame');
  assert.equal(result.observations.screenshot.image,'native attachment');
  assert.equal(result.observations.screenshot.artifact,undefined);
  assert.equal(result.artifact.data,'cGl4ZWxz');
  assert.deepEqual(result.observations.events.events.map(event=>event.text || event.status),['new failure',503]);
});

test('invalid observation options prevent the action and absent options preserve receipts', async () => {
  const h=harness({realTimers:true});
  h.run('let actionCount=0; run=async()=>{actionCount++;return {completed:true};};');
  for (const observe of [null,[],{screenshot:'yes'},{max_nodes:1001},{unknown:true}]) {
    await h.dispatch({id:1,action:'click',observe});
    assert.match(h.frames.at(-1).result.browser_error,/observe/);
  }
  assert.equal(h.run('actionCount'),0);
  await h.dispatch({id:2,action:'click'});
  assert.equal(h.run('actionCount'),1);
  assert.deepEqual(h.frames.at(-1).result,{completed:true});
});

test('failed or expired screenshots do not replay or relabel completed actions', async () => {
  for (const failure of ['throw', 'expired']) {
    const h=harness({realTimers:true});
    h.run(`
      let actionCount=0, shotCount=0;
      tabs.set(1,{seq:0,console:[],network:[],dialogs:[],downloads:[],navigation:[],page:{url:()=> 'http://localhost/'}});
      run=async()=>{actionCount++;return {tab:1,completed:true};};
      captureModule={run:async()=>{shotCount++;throw new Error('screenshot unavailable');}};
      if (${JSON.stringify(failure)} === 'expired') settleRequests=async()=>{operationDeadline=Date.now()-1;return {settled:true};};
    `);
    await h.dispatch({id:1,action:'click',tab:1,observe:{screenshot:true}});
    const result=h.frames.at(-1).result;
    assert.equal(h.run('actionCount'),1);
    assert.equal(h.run('shotCount'),failure === 'throw' ? 1 : 0);
    assert.equal(result.completed,true);
    assert.equal(result.browser_error,undefined);
    assert.equal(result.artifact,undefined);
    assert.match(result.observations.screenshot.error,/screenshot unavailable|observation budget exhausted/);
  }
});

test('implicit sole-tab observations exclude old failures even without a tab in the receipt', async () => {
  const h=harness({realTimers:true});
  h.run(`
    tabs.set(7,{seq:0,console:[],network:[],dialogs:[],downloads:[],navigation:[],page:{url:()=> 'http://localhost/'}});
    record(tabs.get(7),'console',{type:'error',text:'old'});
    run=async()=>{record(tabs.get(7),'console',{type:'error',text:'new'});return {completed:true};};
  `);
  await h.dispatch({id:1,action:'click',observe:{events:true}});
  const result=h.frames.at(-1).result;
  assert.equal(result.completed,true);
  assert.deepEqual(result.observations.events.events.map(event=>event.text),['new']);
});
