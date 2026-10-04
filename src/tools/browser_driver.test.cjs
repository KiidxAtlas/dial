'use strict';
// Deterministic producer-side protocol tests: node --test src/tools/browser_driver.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(require('node:path').join(__dirname,'browser_driver.cjs'),'utf8');
function harness() {
  const timers = new Set(), frames = [];
  const stdout = { writableNeedDrain:false,writableLength:0,write(line,callback) { frames.push(JSON.parse(line));callback();return true; } };
  const context = vm.createContext({ Buffer, setTimeout(fn) {timers.add(fn);return fn;},clearTimeout(fn) {timers.delete(fn);},
    require(name) { return name === 'node:readline' ? {createInterface(){return {on(){}};}} : {chromium:{}}; },
    process:{env:{DIAL_PLAYWRIGHT:'external-operator-module'},stdout,stderr:{write(){}},on(){},stdin:{on(){}},exit(){}},
  });
  vm.runInContext(source,context);
  context.fakeRequest = {postDataBuffer(){return null;},url(){return 'https://example.com/flood';},method(){return 'GET';},headers(){return {};} };
  return {run(code){return vm.runInContext(code,context);},timers,frames,stdout};
}
test('idle pages cannot queue network callbacks',async () => {
  const h=harness();
  await assert.rejects(h.run('http(fakeRequest)'),/paused between approved operations/);
  assert.equal(h.run('callbacks.size'),0);
  assert.equal(h.frames.length,0);
});
test('pending callbacks and bytes remain bounded and expire',async () => {
  const h=harness();
  h.run('operationId = 42');
  const promises=h.run('Array.from({length:1000},()=>http(fakeRequest).catch(error=>error.message))');
  assert.equal(h.run('callbacks.size'),16);
  assert(h.run('pendingBytes')<=2*1024*1024);
  assert.equal(h.frames.length,16);
  assert(h.frames.every(frame=>frame.operation_id===42));
  for (const timer of [...h.timers]) timer();
  await Promise.all(promises);
  assert.equal(h.run('callbacks.size'),0);
  assert.equal(h.run('pendingBytes'),0);
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
  assert.equal(h.run('callbacks.size'),1);
  assert(h.run('pendingBytes')<=2*1024*1024);
  h.run('cancelPending("test cleanup")');
  await Promise.all(promises);
  assert.equal(h.run('pendingBytes'),0);
});
