'use strict';
// Run: node --test assets/collab-web/viewer.test.cjs
// Executes the shipped viewer, with real AES-GCM and only DOM/socket/timer boundaries faked.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const {webcrypto} = require('node:crypto');
const source = fs.readFileSync(require('node:path').join(__dirname, 'index.html'), 'utf8').match(/<script type="module">([\s\S]*?)<\/script>/)[1];
const secret = new Uint8Array(32).fill(7);
const tick = () => new Promise(resolve => setImmediate(resolve));
async function harness() {
  const sockets = [], timers = [], pending = [];
  const status = {textContent:'', className:''};
  const transcript = {children:[], append(el){this.children.push(el);}, replaceChildren(){this.children=[];}};
  class Socket {
    static OPEN = 1;
    constructor(url){this.url=url;this.readyState=1;this.sent=[];sockets.push(this);}
    send(data){this.sent.push(data);}
    close(){this.readyState=3;this.closed=true;}
  }
  const crypto = {getRandomValues: bytes => webcrypto.getRandomValues(bytes), subtle:{
    importKey:(...args)=>webcrypto.subtle.importKey(...args),
    encrypt:(...args)=>webcrypto.subtle.encrypt(...args),
    decrypt:(...args)=>{const result=webcrypto.subtle.decrypt(...args);pending.push(result.catch(()=>{}));return result;},
  }};
  const context = vm.createContext({crypto, Uint8Array, TextEncoder, TextDecoder, URL, atob, btoa,
    document:{querySelector: selector => selector==='#status'?status:transcript, createElement:()=>({textContent:''}), body:{scrollHeight:0}},
    window:{scrollTo(){}}, WebSocket:Socket, location:{hash:`#wss://relay.invalid/r/fixture.${Buffer.from(secret).toString('base64url')}`},
    setTimeout(fn, delay){timers.push({fn,delay});},
  });
  await vm.runInContext(`(async()=>{${source}\n})()`, context);
  const key = await webcrypto.subtle.importKey('raw',secret,{name:'AES-GCM'},false,['encrypt','decrypt']);
  async function packet(frame) {
    const iv = webcrypto.getRandomValues(new Uint8Array(12));
    const sealed = new Uint8Array(await webcrypto.subtle.encrypt({name:'AES-GCM',iv},key,new TextEncoder().encode(JSON.stringify(frame))));
    const bytes = new Uint8Array(16+sealed.length);bytes.set(iv,4);bytes.set(sealed,16);return bytes;
  }
  async function flush() {
    await tick();
    while(pending.length){await Promise.all(pending.splice(0));await tick();}
  }
  async function send(frame, ws=sockets.at(-1)) {ws.onmessage({data:await packet(frame)});await flush();}
  async function snapshot(epoch, transportSeq, entries) {
    await send({t:'welcome',role:'host',epoch,transportSeq});
    await send({t:'snapshot-chunk',role:'host',epoch,transportSeq:transportSeq+1,entries,final:true});
  }
  return {sockets,timers,status,transcript,key,send,snapshot,packet,flush,
    async drop(code, ws=sockets.at(-1)){ws.readyState=3;await ws.onclose({code});},
    retry(){assert(timers.length);timers.shift().fn();},
  };
}
test('room teardown retries through missing room and accepts a recreated host snapshot', async () => {
  const h=await harness();
  await h.snapshot('old',10,[{type:'old'}]);
  h.sockets[0].onmessage({data:'{"t":"room-closed"}'});
  await h.drop(4001);
  assert.match(h.status.textContent,/reconnecting/);
  h.retry();await h.drop(4004);
  assert.equal(h.timers[0].delay,2000);
  h.retry();await h.snapshot('new',1,[{type:'new'}]);
  assert.deepEqual(h.transcript.children.map(el=>el.textContent),['new: {"type":"new"}']);
  await h.drop(1006);assert.equal(h.timers[0].delay,1000);
});
test('reconnect preserves replay history and accepts forward sequences from the same host', async () => {
  const h=await harness();await h.snapshot('host',10,[{type:'original'}]);
  await h.drop(1006);h.retry();
  await h.snapshot('host',10,[{type:'replayed'}]);
  assert.match(h.transcript.children[0].textContent,/original/);
  await h.snapshot('host',12,[{type:'fresh'}]);
  assert.match(h.transcript.children[0].textContent,/fresh/);
  await h.send({t:'notice',role:'guest',epoch:'host',transportSeq:99,message:'reflected'});
  await h.send({t:'notice',role:'host',epoch:'other',transportSeq:99,message:'wrong epoch'});
  await h.send({t:'notice',role:'host',epoch:'host',transportSeq:13,message:'duplicate'});
  assert.equal(h.transcript.children.length,1);
});
test('invalid first sequence cannot pin the next connection to a bogus epoch', async () => {
  const h=await harness();
  await h.send({t:'welcome',role:'host',epoch:'bogus',transportSeq:0});
  await h.snapshot('actual',1,[{type:'valid'}]);
  assert.match(h.transcript.children[0].textContent,/valid/);
});
test('encrypted bye wins the async close race and remains terminal', async () => {
  const h=await harness();const ws=h.sockets[0];
  ws.onmessage({data:await h.packet({t:'bye',role:'host',epoch:'host',transportSeq:1,reason:'done'})});
  await h.drop(4001);
  assert.match(h.status.textContent,/Host ended this session: done/);
  assert.equal(h.timers.length,0);assert.equal(ws.closed,true);
  await h.send({t:'notice',role:'host',epoch:'host',transportSeq:2,message:'late'});
  ws.onerror();assert.match(h.status.textContent,/done/);
  assert.equal(h.transcript.children.length,0);
});
test('initial missing room and other fatal relay codes do not retry', async () => {
  for(const code of [4004,4009,4029]){
    const h=await harness();await h.drop(code);
    assert.equal(h.timers.length,0);assert.match(h.status.textContent,/Session unavailable/);
  }
});
test('authentication failure is terminal rather than silently reconnecting', async () => {
  const h=await harness();const ws=h.sockets[0];
  const bytes=await h.packet({t:'welcome',role:'host',epoch:'host',transportSeq:1});bytes[bytes.length-1]^=1;
  ws.onmessage({data:bytes});await h.flush();await h.drop(1006);
  assert.equal(h.timers.length,0);assert.equal(ws.closed,true);
  assert.match(h.status.textContent,/Frame unavailable/);
});
test('hello stays encrypted and uses a fresh guest epoch for each connection', async () => {
  const h=await harness();
  async function hello(ws){
    await ws.onopen();const bytes=ws.sent[0];
    return JSON.parse(new TextDecoder().decode(await webcrypto.subtle.decrypt({name:'AES-GCM',iv:bytes.slice(4,16)},h.key,bytes.slice(16))));
  }
  const first=await hello(h.sockets[0]);await h.drop(1006);h.retry();const next=await hello(h.sockets[1]);
  assert.equal(first.role,'guest');assert.equal(first.proto,1003);assert.equal(first.transportSeq,1);assert.equal(first.seq,undefined);
  assert.notEqual(first.epoch,next.epoch);
});
test('entry zero, controls and reset use separate log and transport sequences', async () => {
  const h=await harness();await h.snapshot('host',1,[]);
  const first={t:'entry',role:'host',epoch:'host',transportSeq:3,seq:0,entry:{type:'first'}};
  await h.send(first);
  assert.deepEqual(h.transcript.children.map(el=>el.textContent),['first: {"type":"first"}']);
  await h.send({t:'notice',role:'host',epoch:'host',transportSeq:4,message:'control'});
  await h.send({t:'entry',role:'host',epoch:'host',transportSeq:5,seq:1,entry:{type:'second'}});
  assert.equal(h.transcript.children.length,3);
  await h.send({t:'reset',role:'host',epoch:'host',transportSeq:6});
  assert.equal(h.transcript.children.length,0);
  // Replayed ciphertext/frame cannot refill a reset log even though its log index is zero.
  await h.send(first);assert.equal(h.transcript.children.length,0);
  const replacement={t:'entry',role:'host',epoch:'host',transportSeq:7,seq:0,entry:{type:'replacement'}};
  await h.send(replacement);await h.send(replacement);
  assert.deepEqual(h.transcript.children.map(el=>el.textContent),['replacement: {"type":"replacement"}']);
});
test('legacy seq stamps and zero transport sequences never substitute for transportSeq', async () => {
  const h=await harness();await h.snapshot('host',1,[]);
  await h.send({t:'entry',role:'host',epoch:'host',seq:0,entry:{type:'legacy'}});
  await h.send({t:'entry',role:'host',epoch:'host',transportSeq:0,seq:0,entry:{type:'invalid'}});
  assert.equal(h.transcript.children.length,0);
  await h.send({t:'entry',role:'host',epoch:'host',transportSeq:3,seq:0,entry:{type:'valid'}});
  assert.match(h.transcript.children[0].textContent,/valid/);
});
