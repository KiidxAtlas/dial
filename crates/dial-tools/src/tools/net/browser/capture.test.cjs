'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname,'capture.cjs'),'utf8');
function harness() {
  const writes=[], timers=new Set(), listeners=new Map(), commands=[];
  const fakeFs={promises:{mkdir:async()=>{},rm:async()=>{},writeFile(file,bytes){return new Promise(resolve=>writes.push({file,bytes,resolve}));}}};
  const realm=vm.createContext({require(name){return name==='node:fs'?fakeFs:require(name);},module:{exports:{}},Buffer,Date,
    setTimeout(fn){timers.add(fn);return fn;},clearTimeout(fn){timers.delete(fn);},clip:String,artifactsDir:()=>'/artifacts'});
  vm.runInContext(source,realm);
  const cdp={on(name,fn){listeners.set(name,fn);},removeListener(name){listeners.delete(name);},async send(name){commands.push(name);}};
  const ctx={id:1,request:{},tab:{cdp},page:{viewportSize:()=>null}};
  return {ctx,writes,timers,commands,run:code=>vm.runInContext(code,realm),frame(){listeners.get('Page.screencastFrame')?.({data:Buffer.from('jpeg').toString('base64'),sessionId:1});}};
}
test('recording bounds pending writes and stops explicitly on frame backpressure',async()=>{
  const h=harness();
  await h.run('recordStart')(h.ctx);
  for(let i=0;i<100;i++) h.frame();
  assert(h.writes.length<=8);
  assert.equal(h.ctx.tab.recording.capped,'pending_writes');
  for(const write of h.writes) write.resolve();
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(h.ctx.tab.recording.pending.size,0);
  assert(h.commands.includes('Page.stopScreencast'));
});
test('recording duration expires without requiring more frames',async()=>{
  const h=harness();
  await h.run('recordStart')(h.ctx);
  for(const timer of [...h.timers]) timer();
  assert.equal(h.ctx.tab.recording.capped,'duration');
  h.frame();
  assert.equal(h.writes.length,0);
});
test('vitals uses bounded early observers and reports unsupported metrics as unavailable',()=>{
  const h=harness();
  const callbacks=new Map();
  const realm=vm.createContext({performance:{getEntriesByType:()=>[],getEntriesByName:()=>[]},PerformanceObserver:class {
    static supportedEntryTypes=['largest-contentful-paint','layout-shift','event','longtask'];
    constructor(callback){this.callback=callback;}
    observe({type}){callbacks.set(type,this.callback);}
    takeRecords(){return [];}
  }});
  vm.runInContext(`(${h.run('installVitals').toString()})()`,realm);
  callbacks.get('largest-contentful-paint')({getEntries:()=>[{startTime:123}]});
  callbacks.get('layout-shift')({getEntries:()=>[{startTime:1,value:0.2,hadRecentInput:false}]});
  callbacks.get('event')({getEntries:()=>[{interactionId:1,duration:48}]});
  callbacks.get('longtask')({getEntries:()=>[{duration:75}]});
  const result=vm.runInContext(`(${h.run('collectVitals').toString()})()`,realm);
  assert.equal(result.lcp_ms,123);
  assert.equal(result.cls,0.2);
  assert.equal(result.inp_ms,48);
  assert.equal(result.long_tasks,1);
  assert.equal(result.long_task_total_ms,75);
  const unavailable=vm.runInNewContext(`(${h.run('collectVitals').toString()})()`,{performance:{getEntriesByType:()=>[],getEntriesByName:()=>[]}});
  assert.equal(unavailable.cls,null);
  assert.equal(unavailable.long_tasks,null);
});

test('recording byte and frame caps stop before allocating or writing another frame',async()=>{
  for(const [field,value,reason] of [['bytes',128*1024*1024,'bytes'],['frames',9000,'frames']]) {
    const h=harness();
    await h.run('recordStart')(h.ctx);
    if(field==='frames') h.ctx.tab.recording.frames.length=value;
    else h.ctx.tab.recording[field]=value;
    h.frame();
    assert.equal(h.ctx.tab.recording.capped,reason);
    assert.equal(h.writes.length,0);
  }
});

test('record_stop exposes the capped outcome and preserves the automatic end time',async()=>{
  const h=harness();
  await h.run('recordStart')(h.ctx);
  h.frame();
  h.writes[0].resolve();
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(h.ctx.tab.recording.pending.size,0);
  for(const timer of [...h.timers]) timer();
  const stopping=h.run('recordStop')(h.ctx);
  await new Promise(resolve=>setImmediate(resolve));
  h.writes.at(-1).resolve();
  const receipt=await stopping;
  assert.equal(receipt.capped,'duration');
  assert.equal(receipt.frames,1);
  assert.equal(receipt.bytes,4);
  assert(receipt.duration_ms<=5*60*1000);
  assert.equal(h.ctx.tab.recording,null);
});

test('vitals distinguishes observed zero from unsupported and caps observer samples',()=>{
  const h=harness();
  const callbacks=new Map(),disconnected=[];
  const realm=vm.createContext({performance:{getEntriesByType:()=>[],getEntriesByName:()=>[]},PerformanceObserver:class {
    static supportedEntryTypes=['layout-shift','longtask'];
    constructor(callback){this.callback=callback;}
    observe({type}){this.type=type;callbacks.set(type,this.callback);}
    disconnect(){disconnected.push(this.type);}
    takeRecords(){return [];}
  }});
  vm.runInContext(`(${h.run('installVitals').toString()})({documentStart:true})`,realm);
  const collect=()=>vm.runInContext(`(${h.run('collectVitals').toString()})()`,realm);
  assert.equal(collect().cls,0);
  assert.equal(collect().long_tasks,0);
  assert.equal(collect().lcp_ms,null);
  callbacks.get('longtask')({getEntries:()=>Array.from({length:10001},()=>({duration:50}))});
  assert.equal(collect().long_tasks,10000);
  assert.equal(collect().capped.longtask,true);
  assert(disconnected.includes('longtask'));
});

test('recording restart and tab retirement clear the timer and settle writes before cleanup',async()=>{
  const h=harness();
  await h.run('recordStart')(h.ctx);
  h.frame();
  const discarding=h.run('discardRecording')(h.ctx.tab);
  assert.equal(h.timers.size,0);
  assert.equal(h.ctx.tab.recording,null);
  h.writes[0].resolve();
  await discarding;
});

test('annotation names preserve emoji at the transport clipping boundary',()=>{
  const h=harness();
  const element={nodeType:1,id:'button',localName:'button',textContent:'a'.repeat(119)+'😀',
    getBoundingClientRect:()=>({width:10,height:10,top:0,left:0,bottom:10,right:10}),getAttribute:()=>null,closest:()=>null};
  const realm=vm.createContext({document:{querySelectorAll:()=>[element]},CSS:{escape:value=>value},
    getComputedStyle:()=>({display:'block',visibility:'visible'}),innerWidth:100,innerHeight:100});
  const result=vm.runInContext(`(${h.run('collectAnnotationTargets').toString()})({limit:1,fullPage:false})`,realm);
  assert.equal(result[0].name,'a'.repeat(119));
});

test('late attached-page vitals disclose partial history and cannot claim document-lifetime zeros',()=>{
  const h=harness();
  for(const early of [false,true]) {
    const realm=vm.createContext({performance:{now:()=>5000,getEntriesByType:()=>[],getEntriesByName:()=>[]},PerformanceObserver:class {
      static supportedEntryTypes=['layout-shift','longtask'];
      observe(){} takeRecords(){return [];} disconnect(){}
    }});
    vm.runInContext(`(${h.run('installVitals').toString()})({documentStart:${early}})`,realm);
    // A later register() pass must not downgrade an early installation or reset late timing.
    vm.runInContext(`(${h.run('installVitals').toString()})()`,realm);
    const result=vm.runInContext(`(${h.run('collectVitals').toString()})()`,realm);
    assert.equal(result.coverage.history,early?'document_start':'partial');
    assert.equal(result.coverage.installed_at_ms,5000);
    assert.equal(result.cls,early?0:null);
    assert.equal(result.long_tasks,early?0:null);
  }
});
