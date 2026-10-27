'use strict';
// Deterministic producer-side protocol tests: make test-drivers
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(require('node:path').join(__dirname,'driver.cjs'),'utf8');
function harness({realTimers = false} = {}) {
  const timers = new Set(), frames = [];
  const stdout = { writableNeedDrain:false,writableLength:0,write(line,callback) { frames.push(JSON.parse(line));callback();return true; } };
  const context = vm.createContext({ Buffer, setTimeout: realTimers ? setTimeout : (fn) => {timers.add(fn);return fn;},clearTimeout: realTimers ? clearTimeout : (fn) => {timers.delete(fn);},
    require(name) { return name === 'node:readline' ? {createInterface(){return {on(){}};}} : {chromium:{}}; },
    process:{env:{DIAL_PLAYWRIGHT:'external-operator-module'},stdout,stderr:{write(){}},on(){},stdin:{on(){}},exit(){}},
  });
  vm.runInContext(source,context);
  context.fakeRequest = {postDataBuffer(){return null;},url(){return 'https://example.com/flood';},method(){return 'GET';},headers(){return {};} };
  return {run(code){return vm.runInContext(code,context);},timers,frames,stdout};
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
  assert.throws(()=>h.run('tabFor({})'),/No browser tab is open/);
  h.run('tabs.set(4,{page:{title:"preview"}}); request={}; tabFor(request)');
  assert.equal(h.run('request.tab'),4);
  h.run('tabs.set(5,{page:{title:"other"}})');
  assert.throws(()=>h.run('tabFor({})'),/Multiple browser tabs/);
  assert.throws(()=>h.run('tabFor({tab:999})'),/999 is not open/);
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
