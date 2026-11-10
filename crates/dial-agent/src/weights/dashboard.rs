//! Loopback dashboard for weight inspection results: a tabbed page (overview, layers, attention,
//! experts, neurons, checks, checkpoint, experiments, history) of summary tiles, charts, heatmaps
//! and tables built from stored results. Explicit scale previews and edited copies reuse the
//! existing weight owner; no dashboard action promotes a model or modifies the base checkpoint.
use super::{Operation, status};
use crate::config::types::Config;
use crate::stats::server::{HTML, Response, TEXT, bind_loopback, json_response, serve_requests};
use anyhow::{Context, Result, ensure};
use std::net::TcpListener;
use std::path::PathBuf;

const PAGE: &str = r##"<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><title>Dial weights</title>
<style>
:root{--bg:#0e1014;--panel:#161920;--panel2:#1e222b;--line:#2a2f3a;--text:#e7e9ee;--muted:#8d95a5;--accent:#3fb6a0;--accent-soft:rgba(63,182,160,.16);--warn:#e0b050;--warn-soft:rgba(224,176,80,.15);--bad:#e0675c;--bad-soft:rgba(224,103,92,.15);--pos:63,182,160;--neg:224,103,92;--mixed:#3a3f4b;--mono:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;color-scheme:dark}
@media(prefers-color-scheme:light){:root{--bg:#f5f6f8;--panel:#fff;--panel2:#f0f2f5;--line:#e1e4ea;--text:#1a1e26;--muted:#646d7e;--accent:#0f7f6c;--accent-soft:rgba(15,127,108,.12);--warn:#a8700a;--warn-soft:rgba(200,140,20,.14);--bad:#c0443a;--bad-soft:rgba(192,68,58,.12);--pos:15,127,108;--neg:192,68,58;--mixed:#d5d9e0;color-scheme:light}}
*{box-sizing:border-box}
body{margin:0;font:14px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif;background:var(--bg);color:var(--text)}
.wrap{max-width:1320px;margin:0 auto;padding:0 28px}
header{position:sticky;top:0;z-index:5;background:var(--bg);border-bottom:1px solid var(--line)}
.top{display:flex;align-items:center;gap:18px;padding:16px 0 8px;min-width:0}
.brand{font-weight:650;font-size:15px;letter-spacing:.01em;white-space:nowrap}.brand b{color:var(--accent)}
.who{min-width:0}.model{font-weight:550;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.path{color:var(--muted);font:12px var(--mono);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
#pill{margin-left:auto;display:flex;align-items:center;gap:10px;white-space:nowrap;font-size:12.5px;color:var(--muted)}
.dot{width:8px;height:8px;border-radius:50%;background:var(--accent);animation:pulse 1.2s ease-in-out infinite}@keyframes pulse{50%{opacity:.3}}
nav{display:flex;gap:2px;overflow-x:auto;scrollbar-width:none}
nav button{background:none;border:0;border-bottom:2px solid transparent;color:var(--muted);padding:8px 12px 9px;font:inherit;font-size:13.5px;cursor:pointer;white-space:nowrap}
nav button:hover{color:var(--text)}nav button.on{color:var(--text);border-bottom-color:var(--accent)}
main.wrap{padding-top:24px;padding-bottom:80px}
.tiles{display:grid;grid-template-columns:repeat(auto-fill,minmax(160px,1fr));gap:12px;margin-bottom:16px}
.tile{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:12px 16px}
.tile .v{font-size:22px;font-weight:620;font-variant-numeric:tabular-nums}.tile .k{color:var(--muted);font-size:12px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:14px;padding:18px 20px;margin-bottom:16px;overflow-x:auto}
.card h2{font-size:15px;font-weight:620;margin:0}.cap{color:var(--muted);font-size:12.5px;margin:3px 0 14px;max-width:95ch}
.cols{display:grid;grid-template-columns:repeat(auto-fit,minmax(480px,1fr));gap:16px}.cols>.card{margin-bottom:0}.cols{margin-bottom:16px}
table{border-collapse:collapse;width:100%;font-size:12.5px}
th{text-align:left;font-weight:520;color:var(--muted);padding:7px 10px;border-bottom:1px solid var(--line);white-space:nowrap}
td{padding:7px 10px;border-bottom:1px solid var(--line);vertical-align:top}
tbody tr:last-child td{border-bottom:0}tbody tr:hover td{background:var(--panel2)}
td.n,th.n{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}td.n{font-family:var(--mono)}td:first-child{white-space:nowrap}td>.track{margin-top:6px}
tr.win td{background:var(--accent-soft)}tr.loss td{background:var(--bad-soft)}
.heat{width:auto}.heat td{padding:0 4px;height:22px;min-width:30px;text-align:center;vertical-align:middle;font:10.5px var(--mono);border:1px solid var(--panel);color:var(--text)}
.heat.dense td{min-width:13px;height:15px;padding:0}.heat th{padding:2px 6px;font:11px var(--mono)}.heat tbody tr:hover td{filter:brightness(1.25)}
.scale{display:flex;align-items:center;gap:8px;color:var(--muted);font-size:11.5px;margin-top:10px}
.scale i{display:inline-block;width:120px;height:8px;border-radius:4px;background:linear-gradient(90deg,rgba(var(--neg),1),transparent 50%,rgba(var(--pos),1))}
.badge{display:inline-block;padding:1px 9px;border-radius:999px;font-size:11.5px;font-weight:550;white-space:nowrap}
.ok{background:var(--accent-soft);color:var(--accent)}.no{background:var(--warn-soft);color:var(--warn)}.bad{background:var(--bad-soft);color:var(--bad)}.dim{background:var(--panel2);color:var(--muted)}
.snip{font:12px/1.45 var(--mono);color:var(--muted);white-space:pre-wrap;word-break:break-word;margin:1px 0 5px;max-width:70ch}
.snip mark{background:var(--accent-soft);color:var(--text);border-radius:3px;padding:0 2px;font-weight:600}
.tok{display:inline-block;font:11.5px var(--mono);background:var(--panel2);border:1px solid var(--line);border-radius:5px;padding:0 6px;margin:1px 4px 1px 0;white-space:pre}
.muted{color:var(--muted)}code{font:12px var(--mono)}
.findings{list-style:none;padding:0;margin:0}.findings li{padding:10px 12px;border-left:3px solid var(--line);margin-bottom:8px;background:var(--panel2);border-radius:0 8px 8px 0}
.findings li.confirmed{border-left-color:var(--accent)}
svg.chart{width:100%;height:auto;max-height:280px;display:block}.chart .axis{stroke:var(--line)}.chart .tick{fill:var(--muted);font:10px var(--mono)}
.chart rect.pos{fill:var(--accent)}.chart rect.neg{fill:var(--bad)}.chart rect:hover{opacity:.7}
.hbars{display:grid;grid-template-columns:max-content minmax(120px,1fr) max-content;gap:4px 12px;align-items:center;font-size:12px}
.hbars .l{font-family:var(--mono);color:var(--muted);text-align:right}.hbars .v{font-family:var(--mono);text-align:right}.hbars .sub{grid-column:2/4;margin:-2px 0 6px}
.track{height:8px;background:var(--panel2);border-radius:4px;min-width:60px}.fill{height:100%;background:var(--accent);border-radius:4px}.fill.neg{background:var(--bad)}
.matrix{display:flex;flex-direction:column;gap:3px}.mrow{display:flex;align-items:center;gap:10px}
.mrow .l{width:3.2em;text-align:right;font:11px var(--mono);color:var(--muted)}.cells{display:flex;gap:2px;flex-wrap:wrap}
.cells i,.legend i{display:inline-block;width:14px;height:14px;border-radius:3px}.legend{display:flex;flex-wrap:wrap;gap:4px 14px;font-size:12px;color:var(--muted);margin-bottom:12px}.legend span{display:flex;align-items:center;gap:6px}
input.filter{background:var(--panel2);border:1px solid var(--line);border-radius:8px;color:var(--text);padding:6px 10px;font:inherit;font-size:13px;width:260px;margin-bottom:10px}
button.copy{font:11px system-ui;margin-left:6px;background:var(--panel2);color:var(--text);border:1px solid var(--line);border-radius:5px;cursor:pointer;padding:1px 7px}
details summary{cursor:pointer;color:var(--muted)}details[open] summary{margin-bottom:8px}
.budget{height:6px;background:var(--panel2);border-radius:3px;margin:8px 0;max-width:360px}.budget div{height:100%;background:var(--accent);border-radius:3px}
.empty{text-align:center;color:var(--muted);padding:90px 20px}.empty code{background:var(--panel2);padding:2px 6px;border-radius:5px}
.error{color:var(--bad)}
.editor-tools{display:flex;flex-wrap:wrap;gap:12px;align-items:end;margin:12px 0}.editor-tools label{display:grid;gap:4px;color:var(--muted);font-size:12px}
.editor-tools input,.editor-tools textarea,.scale-number{background:var(--panel2);border:1px solid var(--line);border-radius:6px;color:var(--text);padding:6px;font:13px var(--mono)}
.editor-tools textarea{width:min(600px,100%);min-height:100px}.editor-tools .cases{width:100%}.editor-tools button{background:var(--panel2);border:1px solid var(--line);border-radius:6px;color:var(--text);padding:7px 12px;cursor:pointer}.editor-tools button:disabled{opacity:.45;cursor:not-allowed}
.scale-control{display:flex;align-items:center;gap:12px;min-width:260px}.scale-control input[type=range]{width:160px;accent-color:var(--accent)}.scale-number{width:88px}.scale-number:invalid{border-color:var(--bad)}.editor-table{max-height:440px;overflow:auto}.editor-table td:first-child{white-space:normal;overflow-wrap:anywhere;max-width:600px}
@media(max-width:600px){.editor-table table,.editor-table tbody,.editor-table tr,.editor-table td{display:block}.editor-table thead{display:none}.editor-table td:first-child{max-width:none}.editor-table td:nth-child(2){color:var(--muted)}.editor-table tr{border-bottom:1px solid var(--line)}.editor-table td{border:0}.scale-control{min-width:0}.scale-control input[type=range]{flex:1;min-width:0}.scale-number{flex-shrink:0}}
</style>
<header><div class=wrap><div class=top><div class=brand><b>◆</b> Dial weights</div><div class=who><div id=model class=model>Loading…</div><div id=path class=path></div></div><div id=pill></div></div><nav id=nav></nav></div></header>
<main class=wrap id=root></main>
<script>
const $=id=>document.getElementById(id);
const el=(tag,attrs={},...children)=>{const e=document.createElement(tag);for(const[k,v]of Object.entries(attrs))if(v!=null&&v!==false)e.setAttribute(k,v);e.append(...children.flat().filter(c=>c!=null&&c!==false).map(c=>c instanceof Node?c:String(c)));return e};
const svg=(tag,attrs={},...children)=>{const e=document.createElementNS('http://www.w3.org/2000/svg',tag);for(const[k,v]of Object.entries(attrs))e.setAttribute(k,v);e.append(...children.filter(c=>c!=null).map(c=>c instanceof Node?c:document.createTextNode(String(c))));return e};
const num=v=>v==null||v===''||!isFinite(Number(v))?'n/a':Math.abs(Number(v))>=1e4?Math.round(Number(v)).toLocaleString():String(parseFloat(Number(v).toPrecision(4)));
const short=v=>{if(Number.isInteger(v))return String(v);const a=Math.abs(v);return a>=100?v.toFixed(0):a>=10?v.toFixed(1):a>=1?v.toFixed(2):String(parseFloat(v.toPrecision(2)))};
const pct=v=>typeof v==='number'&&isFinite(v)?(v*100).toFixed(1)+'%':'n/a';
const signed=(v,d)=>typeof v==='number'&&isFinite(v)?(v>0?'+':'')+v.toFixed(d):null;
const human=n=>typeof n!=='number'?n:n>=1e9?(n/1e9).toFixed(1)+'B':n>=1e6?(n/1e6).toFixed(1)+'M':n>=1e3?(n/1e3).toFixed(1)+'K':String(n);
const when=t=>typeof t==='number'&&isFinite(t)?new Date(t*1000).toLocaleString():(t==null?'n/a':String(t));
const list=v=>Array.isArray(v)?v:[];
const shade=(v,max)=>{if(v==null||!isFinite(v))return'transparent';const t=max>0?Math.min(1,Math.abs(v)/max):0;return`rgba(var(${v<0?'--neg':'--pos'}),${(t*.85).toFixed(3)})`};
const peak=values=>Math.max(0,...values.filter(v=>v!=null&&isFinite(v)).map(Math.abs));
const safe=build=>{try{return build()}catch(e){return el('p',{class:'error'},'Section unavailable: '+e)}};
const NUMERIC=/^[-+]?(\d[\d,]*\.?\d*(e[-+]?\d+)?)(%|×)?$|^n\/a$/;
function card(title,cap,...kids){return el('section',{class:'card'},el('h2',{},title),cap?el('p',{class:'cap'},cap):null,...kids)}
function section(title,...kids){return card(title,null,...kids)}
function table(head,rows){const numeric=head.map((_,j)=>rows.length>0&&rows.every(r=>r[j]==null||typeof r[j]==='number'||(typeof r[j]==='string'&&NUMERIC.test(r[j]))));const cell=(c,j)=>el('td',{class:numeric[j]?'n':null},c==null?'—':typeof c==='number'?num(c):c);return el('table',{},el('thead',{},el('tr',{},...head.map((h,j)=>el('th',{class:numeric[j]?'n':null},h)))),el('tbody',{},...rows.map(r=>el('tr',{},...r.map(cell)))))}
function filterable(title,cap,tbl){const input=el('input',{type:'search',class:'filter',placeholder:'Filter rows…'});input.addEventListener('input',()=>{const q=input.value.toLowerCase();for(const tr of tbl.tBodies[0].rows)tr.hidden=!!q&&!tr.textContent.toLowerCase().includes(q)});return card(title,cap,input,tbl)}
function pair(...cards){const shown=cards.filter(Boolean);return shown.length>1?el('div',{class:'cols'},...shown):shown[0]??null}
function meter(v,max,neg){const w=v==null||!max?0:Math.min(1,Math.abs(v)/max)*100;return el('div',{class:'track'},el('div',{class:v<0||neg?'fill neg':'fill',style:`width:${w}%`}))}
function chips(values){const v=list(values);if(!v.length)return el('span',{class:'muted'},'—');return el('span',{},...v.map(t=>el('span',{class:'tok'},typeof t==='object'&&t?String(t.token):String(t))))}
function snip(text){return el('div',{class:'snip'},...String(text??'').split(/[«»]/).map((p,i)=>i%2?el('mark',{},p):p))}
function snips(contexts){return el('div',{},...list(contexts).map(c=>snip(c&&typeof c==='object'?c.text:c)))}
// Per-layer (or per-head) column chart; negative values hang below the axis.
function bars(title,rows,label,key,cap){rows=list(rows).filter(Boolean);if(!rows.length)return null;const vals=rows.map(r=>r[key]);const ok=vals.filter(v=>v!=null&&isFinite(v));const hi=Math.max(0,...ok),lo=Math.max(0,...ok.map(v=>-v));if(!hi&&!lo)return null;const sizes=ok.map(Math.abs).filter(v=>v>0).sort((a,b)=>a-b);const median=sizes[Math.floor(sizes.length/2)]||0;const root=Math.max(hi,lo)>8*median&&sizes.length>3;const f=v=>root?Math.sqrt(Math.abs(v)):Math.abs(v);const total=f(hi)+f(lo);const step=Math.max(10,Math.min(26,900/rows.length)),P=46,H=190,top=14,bottom=24,plot=H-top-bottom;const W=P+rows.length*step+8;const zero=top+plot*f(hi)/total,scale=plot/total;const c=svg('svg',{viewBox:`0 0 ${W} ${H}`,class:'chart',role:'img'});c.append(svg('line',{x1:P,x2:W,y1:zero,y2:zero,class:'axis'}));if(hi)c.append(svg('text',{x:P-8,y:top+4,class:'tick','text-anchor':'end'},num(hi)));if(lo)c.append(svg('text',{x:P-8,y:top+plot,class:'tick','text-anchor':'end'},num(-lo)));const every=Math.ceil(rows.length/36);rows.forEach((r,i)=>{const v=r[key],x=P+i*step+step*.15,w=step*.7;if(v!=null&&isFinite(v)){const h=f(v)*scale;c.append(svg('rect',{x,width:w,y:v<0?zero:zero-h,height:Math.max(h,.6),rx:Math.min(3,w/3),class:v<0?'neg':'pos'},svg('title',{},`${label} ${r[label]}: ${num(v)}`)))}if(i%every===0)c.append(svg('text',{x:x+w/2,y:H-8,class:'tick','text-anchor':'middle'},String(r[label])))});return card(title,cap,c,root?el('div',{class:'scale'},'square-root scale, so small values stay visible next to a few large ones · hover a bar for its value'):null)}
// Heatmap with the colour scale clipped at the 95th percentile, so a few outliers do not wash out the rest.
function heat(title,rows,label,columns,value,cap){rows=list(rows);if(!rows.length||!columns.length)return null;const vals=rows.flatMap(r=>columns.map(c=>value(r,c))).filter(v=>v!=null&&isFinite(v)).map(Math.abs).sort((a,b)=>a-b);if(!vals.length)return null;const max=vals[Math.floor((vals.length-1)*.95)]||vals[vals.length-1];const dense=columns.length>14;const tbl=el('table',{class:dense?'heat dense':'heat'},el('thead',{},el('tr',{},el('th',{},label),...columns.map(c=>el('th',{},dense?'':String(c))))));const body=el('tbody');for(const r of rows){const tr=el('tr',{},el('th',{class:'n'},String(r[label])));for(const c of columns){const v=value(r,c);tr.append(el('td',{style:`background:${shade(v,max)}`,title:`${label} ${r[label]} · ${c}: ${num(v)}`},!dense&&v!=null&&isFinite(v)?short(v):''))}body.append(tr)}tbl.append(body);return card(title,cap,tbl,el('div',{class:'scale'},el('i'),`colour scale ±${num(max)} (95th percentile; stronger values are clipped) · hover a cell for its value`))}
const components=rows=>[...new Set((rows||[]).flatMap(r=>Object.keys(r.components||{})))].sort();
function heads(trace){const items=list(trace.heads);const max=peak(items.map(h=>h.delta));const cells=items.map(h=>el('i',{style:`background:${h.delta==null?'var(--mixed)':shade(h.delta,max)};width:30px;height:30px`,title:`head ${h.head}: loss change ${num(h.delta)}`}));return card(`Layer ${trace.layer} attention heads`,`Loss increase when each head is removed (baseline ${num(trace.baseline_loss)}); hover a square for its value.`,el('div',{class:'cells'},...cells))}
const CHECKS={causal:'removing it changes the output where it fires no more than removing an equally active part',category:'removing it does not change its category more than other text',replicated:'the two halves of the text disagree',explanation:'its explanation does not beat a neutral one'};
function checkBadge(i){const checks=(i&&i.checks)||{};if(!('causal' in checks))return el('span',{class:'badge dim',title:'not among the parts removed in the causal check'},'not checked');if(i.confirmed)return el('span',{class:'badge ok',title:'passed every check that applies'},'✓ confirmed');const failed=Object.entries(checks).filter(([,v])=>v===false).map(([k])=>k),untested=Object.entries(checks).filter(([,v])=>v==null).map(([k])=>k);const title=[failed.length?'failed: '+failed.map(k=>CHECKS[k]||k).join('; '):null,untested.length?'untested (too few held-out texts): '+untested.join(', '):null].filter(Boolean).join(' · ');const text=[failed.join(', '),untested.length?untested.join(', ')+' untested':''].filter(Boolean).join(' · ');return el('span',{class:failed.length?'badge no':'badge dim',title},'unconfirmed · '+text)}
const partName=i=>i.kind==='expert'||(i.expert!=null&&i.neuron==null)?`L${i.layer} · expert ${i.expert}`:`L${i.layer} · neuron ${i.neuron}`;

// Edited copies: one row per stored edit, with its verdict, measured quality and promotion command.
function ids(values){return list(values).map(x=>x&&typeof x==='object'?(x.layer??x.expert??x.tensor??''):x).join(', ')}
function changed(d){const out=[];if(d.layers)out.push('layers '+ids(d.layers));if(list(d.tensors).length)out.push('tensors '+ids(d.tensors));if(list(d.experts).length)out.push('experts '+ids(d.experts));if(d.shared_expert)out.push('shared expert');if(list(d.sources).length)out.push('sources '+list(d.sources).map(s=>s&&typeof s==='object'?s.name:s).join(', '));return out.join('; ')||'n/a'}
function params(d){const out=[];if(d.alpha!=null)out.push('alpha '+d.alpha);if(d.columns!=null)out.push('columns '+d.columns);if(d.strength!=null)out.push('strength '+d.strength);for(const t of list(d.tensors))if(t&&t.scale!=null)out.push(`${t.tensor??''} ×${t.scale}`);return out.join(', ')||'n/a'}
function verdict(e){const ev=e.evaluation;if(ev)return ev.passed?'pass':'fail'+(ev.reason?': '+ev.reason:'');if(e.passed==null)return'not evaluated';return e.passed?'pass':'fail'}
function verdictBadge(e){const v=verdict(e);return el('span',{class:'badge '+(v==='pass'?'ok':v==='not evaluated'?'dim':'bad')},v)}
function promoteCell(e){if(!(e.evaluation?.passed??e.passed))return el('span',{class:'muted'},'—');const cmd=`dial weights promote ${e.name}`;const button=el('button',{type:'button',class:'copy'},'copy');button.addEventListener('click',()=>navigator.clipboard?.writeText(cmd));return el('span',{},el('code',{},cmd),button)}
function evaluationView(e){const ev=e.evaluation;if(!ev)return el('span',{class:'muted'},'not evaluated');const cells=o=>[o.tokens,o.hit_limit==null?'n/a':o.hit_limit?'yes':'no',o.passed==null?'n/a':o.passed?'yes':'no',o.repeat_ratio];const rows=list(ev.rows).map(r=>{const outcome=['win','loss','tie'].includes(r.outcome)?r.outcome:'tie';return el('tr',{class:outcome},el('td',{},String(r.case??'')),el('td',{},outcome),...cells(r.base||{}).map(c=>el('td',{class:'n'},typeof c==='number'?num(c):c??'n/a')),...cells(r.edited||{}).map(c=>el('td',{class:'n'},typeof c==='number'?num(c):c??'n/a')))});const q=ev.quality;const quality=q?`probe loss ${num(q.base_probe_loss)} → ${num(q.edited_probe_loss)} (${pct(q.relative_change)})`:'quality not measured';const head=['case','outcome','base tokens','base hit limit','base passed','base repeat','edit tokens','edit hit limit','edit passed','edit repeat'];return el('details',{},el('summary',{},`${ev.wins??0} wins · ${ev.losses??0} losses · ${ev.ties??0} ties · p ${num(ev.p_value)}`),el('table',{},el('thead',{},el('tr',{},...head.map((h,j)=>el('th',{class:j>1?'n':null},h)))),el('tbody',{},...rows)),el('p',{class:'muted'},quality+(ev.reason?` · ${ev.reason}`:'')))}
function editsSection(edits){const rows=list(edits);if(!rows.length)return null;const head=['name','experiment','method','changed','params','max requant. error','verdict','wins / losses / ties','p','quality Δ','promote','evaluation'];return card('Edited copies','Model copies written by edit, with their held-out evaluation. A copy becomes the project model only after its evaluation passes and you promote it.',table(head,rows.map(e=>{const d=e.details||{};const ev=e.evaluation||{};return[e.name,e.experiment,d.method,changed(d),params(d),d.max_requantization_error,verdictBadge(e),ev.wins==null?'n/a':`${ev.wins} / ${ev.losses} / ${ev.ties}`,ev.p_value,pct(ev.quality?.relative_change),promoteCell(e),evaluationView(e)]})))}

// `try` dose curves: loss delta and neutral KL per dose, each series scaled to its own maximum.
function doseChart(t){const doses=list(t&&t.doses);if(!doses.length)return null;const W=640,H=200,P=36,n=doses.length;const x=i=>P+(n>1?i/(n-1):0.5)*(W-2*P);const y=(v,m)=>v==null||!m?H-P:H-P-(Math.abs(v)/m)*(H-2*P);const maxDelta=peak(doses.map(d=>d.delta)),maxKl=peak(doses.map(d=>d.neutral_kl));const chart=svg('svg',{viewBox:`0 0 ${W} ${H}`,class:'chart',role:'img'});chart.append(svg('line',{x1:P,y1:H-P,x2:W-P,y2:H-P,class:'axis'}));const series=(key,max,color)=>{const points=doses.map((d,i)=>[x(i),y(d[key],max),d]).filter(p=>p[2][key]!=null);if(points.length>1)chart.append(svg('polyline',{points:points.map(p=>`${p[0]},${p[1]}`).join(' '),fill:'none',stroke:color,'stroke-width':2}));for(const[px,py,d]of points)chart.append(svg('circle',{cx:px,cy:py,r:d.noisy?7:4,fill:color,stroke:d.noisy?'#888':'none','stroke-width':2},svg('title',{},`dose ${d.dose}: ${key} ${num(d[key])}${d.noisy?' (noisy)':''}`)))};series('delta',maxDelta,'#3fb6a0');series('neutral_kl',maxKl,'#e0b050');doses.forEach((d,i)=>chart.append(svg('text',{x:x(i),y:H-P+16,'text-anchor':'middle',class:'tick'},String(d.dose))));return card('Try: dose response','Each strength of the edit, scored in memory without writing a copy. Each line is scaled to its own maximum; a ringed point is a noisy dose.',el('div',{class:'legend'},el('span',{},el('i',{style:'background:#3fb6a0'}),`loss delta (max ${num(maxDelta)})`),el('span',{},el('i',{style:'background:#e0b050'}),`neutral KL (max ${num(maxKl)})`)),chart)}

// `attribute` rankings: heads, neurons and experts ranked by the stored key, with promote/suppress tokens.
function rankRows(title,rows,key,name){rows=list(rows).slice(0,12);if(!rows.length)return null;const value=r=>r[key]??r.support??null;const max=peak(rows.map(value));const box=el('div',{class:'hbars'});for(const r of rows){const v=value(r);box.append(el('span',{class:'l'},`L${r.layer} ${name} ${r[name]}`),meter(v,max),el('span',{class:'v'},num(v)+(r.agreement==null?'':` · agree ${Number(r.agreement).toFixed(2)}`)));const promotes=list(r.promotes),suppresses=list(r.suppresses);if(promotes.length||suppresses.length)box.append(el('span'),el('div',{class:'sub muted'},'promotes ',chips(promotes.slice(0,5)),' suppresses ',chips(suppresses.slice(0,5))))}return card(title,null,box)}
function attributeSection(a){if(!a)return null;const key=a.ranked_by||'support';const parts=[rankRows(`Attention heads by ${key}`,a.heads,key,'head'),rankRows(`MLP neurons by ${key}`,a.neurons,key,'neuron'),rankRows(`Experts by ${key}`,a.experts,key,'expert')].filter(Boolean);return parts.length?el('div',{class:'cols'},...parts):null}

// Analyze sections.
const ROLE_COLOR={induction:'#3fb6a0','previous-token':'#5a8ddc','attention-sink':'#c8a03c','current-token':'#a070d0','same-token':'#d07aa0',local:'#70b0d0','long-range':'#c87850',mixed:'var(--mixed)'};
function headGrid(rows){const layers=list(rows).filter(r=>r&&r.head_count);if(!layers.length)return null;const legend=el('div',{class:'legend'},...Object.entries(ROLE_COLOR).map(([role,color])=>el('span',{},el('i',{style:`background:${color}`}),role)));const box=el('div',{class:'matrix'});for(const r of layers){const notable=new Map(list(r.heads).map(h=>[h.head,h]));const cells=[];for(let h=0;h<r.head_count;h++){const n=notable.get(h);const role=n?.role??'mixed';cells.push(el('i',{style:`background:${ROLE_COLOR[role]??ROLE_COLOR.mixed}`,title:`layer ${r.layer} head ${h}: ${role}`+(n?`\nprevious ${num(n.previous)} · sink ${num(n.sink)} · self ${num(n.self)} · same-token ${num(n.duplicate)} · local ${num(n.local)} · far ${num(n.far)} · induction ${num(n.induction)}`:'')}))}box.append(el('div',{class:'mrow'},el('span',{class:'l'},`L${r.layer}`),el('span',{class:'cells'},...cells),el('span',{class:'muted',style:'font-size:11.5px'},r.attention_type?`${r.attention_type} attention`:'')))}return card('Attention heads by role','One row per layer and one square per head. Hover a square for its attention scores; heads without a clear role are grey.',legend,box)}
function findingsSection(a){const lines=list(a&&a.findings);if(!lines.length)return null;return card('What the analysis found','Measured on built-in labelled text. Highlighted lines passed the causal, replication and explanation checks; the rest are leads to test with attribute and try.',el('ul',{class:'findings'},...lines.map(l=>{const s=String(l);return el('li',{class:s.startsWith('Confirmed:')?'confirmed':null},s)})))}
function categoryHeat(a,title,pick,cap){const cats=list(a&&a.categories);const rows=list(a&&a.layers).filter(r=>pick(r));if(!cats.length||!rows.length)return null;return heat(title,rows,'layer',cats,(r,c)=>pick(r)[c]??null,cap)}
function specialistsSection(a){const items=list(a&&a.summary&&a.summary.routing&&a.summary.routing.specialists);if(!items.length)return null;return card('Experts that favour one kind of text','Lift: how many times more often the expert is chosen on that category than overall; per half: the same measured on each half of the text separately.',table(['part','category','lift','share of category','lift per half','checks'],items.map(i=>[`L${i.layer} · expert ${i.expert}`,i.category,num(i.lift)+'×',pct(i.category_share),list(i.half_lifts).map(v=>num(v)+'×').join(' / ')||'—',checkBadge(i)])))}
function neuronsSection(a){const rows=list(a&&a.layers).flatMap(r=>list(r.neurons));if(!rows.length)return null;return filterable(`Most active MLP neurons (${rows.length})`,'What fires each neuron (the marked token in its strongest snippets), the kind of text it favours and the tokens it pushes toward.',table(['part','activity','fires on','pushes toward','strongest contexts','own words','checks'],rows.map(n=>[`L${n.layer} · neuron ${n.neuron}`,n.activity,n.fires_on??'—',chips(list(n.promotes).slice(0,5)),snips(n.contexts),n.explanation?el('div',{},n.explanation,el('div',{class:'muted'},`score ${num(n.explanation_score)} · neutral ${num(n.control_score)}`)):'—',checkBadge(n)])))}
function explanationsSection(a){const items=list(a&&a.summary&&a.summary.explanations);if(!items.length)return null;const who=a.explainer&&a.explainer!=='self'?a.explainer:null;return card(who?'Explained by '+who:'In the model\'s own words',(who?who+' described each part (the analyzed model did every measurement)':'The model described each part')+' from snippets in one half of the text. Score: rank correlation between how well the description fits and how strongly the part fires on the other half (0 = chance, 1 = perfect). Neutral: the same with a description that says nothing. Look-alikes: how often it tells real firing from snippets with the same tokens where the part stays quiet (50% = chance).',table(['part','explanation','score','neutral','look-alikes','checks'],items.map(i=>[partName(i),el('strong',{},i.explanation),signed(i.explanation_score,2),signed(i.control_score,2),i.hard_negative_score==null?null:pct(i.hard_negative_score),checkBadge(i)])))}
function checkedSection(a){const items=list(a&&a.summary&&a.summary.checked_parts);if(!items.length)return null;const ok=items.filter(i=>i.confirmed).length;return card(`Checked parts · ${ok} of ${items.length} confirmed`,'Candidates were screened on one half of the text and the parts with the largest effect tested on the other. Each part was removed only where it fires, and so was an equally active control part (the co-chosen expert with the closest router weight, or the neuron with the closest activation there). Effect: how far the next-token distribution moves where it fires (KL, nats). z: signed-rank test over held-out texts that the part beats its control (2 or more passes; fewer than 6 texts leaves it untested). Loss: the actual next token (+ removal hurts, − it helps). A category claim must change that category\'s texts more than the rest (rank-sum z). An explanation must beat a neutral one by 0.25 and tell look-alikes apart 60% of the time.',table(['part','label','verdict','effect where it fires','control part','texts','z','loss change','on its category','other text'],items.map(i=>{const c=i.causal||{};return[partName(i),i.explanation?`“${i.explanation}”`:(i.category?`${i.category} text`:'—'),checkBadge(i),c.effect==null?null:c.effect.toFixed(4),c.control_effect==null?null:c.control_effect.toFixed(4),c.texts,signed(c.z,1),signed(c.loss_change,3),c.category_effect==null?null:c.category_effect.toFixed(4),c.elsewhere_effect==null?null:c.elsewhere_effect.toFixed(4)]})))}
function writesSection(a){const rows=list(a&&a.layers).filter(r=>r.writes);if(!rows.length)return null;return[bars('What each layer adds',rows.map(r=>({layer:r.layer,gain:r.writes.target_gain})),'layer','gain','Change in the log-probability of the actual next token across each layer (logit lens). Negative layers lower it directly; they may still prepare something later layers use.'),filterable('What each layer writes','Tokens each layer\'s update pushes up or down more often than other layers do.',table(['layer','gain','pushes up','pushes down'],rows.map(r=>[r.layer,r.writes.target_gain,chips(list(r.writes.promotes).slice(0,6)),chips(list(r.writes.suppresses).slice(0,6))])))]}
function profilesSection(a){const rows=list(a&&a.layers).flatMap(r=>list(r.experts&&r.experts.profiles).map(p=>({...p,layer:r.layer})));if(!rows.length)return null;return filterable(`Expert profiles (${rows.length})`,'The busiest and most specialised experts: input tokens routed to them more than chance, what their output pushes toward, their strongest snippets and the model\'s own description.',table(['part','share','favours','routed tokens','pushes toward','strongest contexts','own words','checks'],rows.map(p=>[`L${p.layer} · expert ${p.expert}`,pct(p.share),p.category??'—',chips(p.tokens),chips(list(p.promotes).slice(0,5)),snips(p.contexts),p.explanation?el('div',{},p.explanation,el('div',{class:'muted'},`score ${num(p.explanation_score)} · neutral ${num(p.control_score)}`)):'—',checkBadge(p)])))}
function memoryHeat(a){const rows=list(a&&a.layers).filter(r=>r.linear_attention&&r.linear_attention.memory);if(!rows.length)return null;const count=Math.max(...rows.map(r=>list(r.linear_attention.memory.half_life).length));const cols=[...Array(count).keys()].map(i=>`head ${i}`);return heat('Linear-attention memory',rows,'layer',cols,(r,c)=>{const v=r.linear_attention.memory.half_life[Number(c.slice(5))];return v==null?null:Math.log10(1+v)},'Half-life in tokens per head: how long its decay gate keeps half of what was written. Colour is log-scaled; hover for log10(1 + tokens).')}
function structureHeat(a){const rows=list(a&&a.layers).filter(r=>r.structure);if(!rows.length)return null;const parts=[...new Set(rows.flatMap(r=>Object.keys(r.structure)))];const dims=list(a.summary&&a.summary.structure&&a.summary.structure.outlier_dimensions);const box=heat('Weight matrices: effective rank',rows,'layer',parts,(r,c)=>r.structure[c]?.effective_rank??null,'Share of directions each matrix really uses; low values mean a matrix does a narrow job.'+(dims.length?' Residual dimensions with outsized weights: '+dims.map(d=>`${d.dimension} (${d.matrices} matrices)`).join(', ')+'.':''));return box}
function routingSection(a){const layers=list(a&&a.layers).filter(r=>r&&r.experts);if(!layers.length)return null;const s=(a.summary&&a.summary.routing)||{};const cap=`${s.moe_layers??layers.length} mixture-of-experts layers${s.mean_entropy!=null?` · mean routing entropy ${num(s.mean_entropy)}`:''}${s.unused_experts?` · ${s.unused_experts} expert slots never chosen`:''}. Entropy 1 means tokens spread evenly over all experts; lower means a few experts take most tokens.`;return card('Routing per layer',cap,table(['layer','entropy','','experts','top k','unused','busiest experts'],layers.map(r=>{const x=r.experts;return[r.layer,x.entropy,meter(x.entropy,1),x.count,x.top_k,x.unused,el('span',{},...list(x.top).slice(0,6).map(t=>el('span',{class:'tok'},`e${t.expert} ${pct(t.share)}`)))]})))}

// Experiment timeline: budget bar, runs in order, held-out count.
function experimentTimeline(exps){const rows=list(exps);if(!rows.length)return null;return card('Experiments',null,...rows.map(e=>{const budget=Number(e.budget)||0,used=Number(e.used)||0;const frac=budget>0?Math.min(1,used/budget):0;const held=typeof e.held_out==='number'?e.held_out:list(e.held_out).length;return el('div',{style:'margin-bottom:14px'},el('div',{},el('strong',{},String(e.experiment??'')),el('span',{class:'muted'},` · ${used}/${budget} runs · ${held} held-out cases · started ${when(e.created_at)}`)),el('div',{class:'budget'},el('div',{style:`width:${frac*100}%`})),...list(e.runs).map(r=>el('div',{class:'muted',style:'font-size:12.5px'},`${r.op??'?'} · ${when(r.at)} · cost ${r.cost??'n/a'}${r.result?` · run ${r.result}`:''}`)))}))}

// Recent runs, newest first, from the stored run list.
function recentRuns(runs){const rows=list(runs);if(!rows.length)return null;return card('Recent runs',null,table(['run','operation','when','experiment','summary'],rows.map(r=>[el('code',{},String(r.id??'')),el('span',{class:'badge dim'},r.op??''),when(r.created_at),r.experiment??'—',r.summary??''])))}

// Tabs: each builds its sections from the status; tabs with nothing to show are hidden.
// Drafts are browser-memory only: polling and tab switches must not discard an edit.
const scaleDraft={values:new Map(),experiment:'',name:'',cases:'',filter:'',notice:'',pending:false};
function scaleControl(name){
 const value=scaleDraft.values.has(name)?(scaleDraft.values.get(name)??''):1;
 const slider=el('input',{type:'range',min:-4,max:4,step:'any',value,'aria-label':name+' scale slider'});
 const number=el('input',{type:'number',class:'scale-number',min:-4,max:4,step:'any',value,'aria-label':name+' scale value'});
 if(value==='')number.setCustomValidity('Enter a scale between -4 and 4');
 const update=source=>{const v=source.valueAsNumber;const valid=source.value!==''&&Number.isFinite(v)&&v>=-4&&v<=4;number.setCustomValidity(valid?'':'Enter a scale between -4 and 4');scaleDraft.values.set(name,valid?v:null);if(valid){slider.value=String(v);number.value=String(v)}};
 slider.addEventListener('input',()=>update(slider));number.addEventListener('input',()=>update(number));
 return el('div',{class:'scale-control'},slider,number);
}
function scaleChanges(){const changes=[];for(const[tensor,scale]of scaleDraft.values){if(scale===null)throw Error('Fix invalid scale values before continuing.');if(scale!==1)changes.push({tensor,scale})}if(!changes.length)throw Error('Change at least one scale from 1.');if(changes.length>32)throw Error('Select at most 32 tensors per edit.');return changes}
async function submitScale(action){
 try{
  const changes=scaleChanges();if(!scaleDraft.experiment.trim())throw Error('Enter an existing experiment name.');
  const input={action,model:state.model,experiment:scaleDraft.experiment.trim(),changes};
  if(action==='preview')input.cases=JSON.parse(scaleDraft.cases);else{if(!scaleDraft.name.trim())throw Error('Enter a new edited-copy name.');input.name=scaleDraft.name.trim()}
  scaleDraft.pending=true;scaleDraft.notice='Starting '+action+'…';draw();
  const response=await fetch('/api/scale',{method:'POST',headers:{'Content-Type':'application/json','X-Dial-Weights':'1'},body:JSON.stringify(input)});
  const text=await response.text();if(!response.ok)throw Error(text);
  state.dashboard_job={running:true,operation:action==='preview'?'try':'edit'};
  scaleDraft.notice=action==='preview'?'Preview started. Results appear below when complete.':'Copy creation started. The active model is unchanged.';
 }catch(error){scaleDraft.notice=String(error)}finally{scaleDraft.pending=false;draw()}
}
function scaleEditor(s){
 const rows=list(s.map?.tensors??s.stats?.tensors).filter(r=>typeof r.name==='string');
 if(!rows.length)return card('Adjust weights','Run dial weights map (or stats) first to list editable tensors. Measurements such as importance and loss are read-only.');
 const field=(label,key,attrs={})=>{const input=el('input',{type:'text',value:scaleDraft[key],...attrs});input.addEventListener('input',()=>scaleDraft[key]=input.value);return el('label',{},label,input)};
 const filter=el('input',{type:'search',class:'filter',placeholder:'Filter tensor names…',value:scaleDraft.filter,'aria-label':'Filter editable tensors'});
 const tbl=table(['Tensor','Shape','Scale ×'],rows.map(r=>[el('code',{},r.name),list(r.shape).join(' × '),scaleControl(r.name)]));
 const applyFilter=()=>{const q=filter.value.toLowerCase();for(const tr of tbl.tBodies[0].rows)tr.hidden=!tr.cells[0].textContent.toLowerCase().includes(q)};
 filter.addEventListener('input',()=>{scaleDraft.filter=filter.value;applyFilter()});applyFilter();
 const cases=el('textarea',{'aria-label':'Preview cases JSON',placeholder:'[{"messages":[{"role":"user","content":"Question"},{"role":"assistant","content":"Reference answer"}]}]'});cases.value=scaleDraft.cases;cases.addEventListener('input',()=>scaleDraft.cases=cases.value);
 const busy=scaleDraft.pending||s.dashboard_job?.running||s.progress||!s.target?.path;
 const preview=el('button',{type:'button',disabled:!!busy},'Preview / Test');preview.addEventListener('click',()=>submitScale('preview'));
 const apply=el('button',{type:'button',disabled:!!busy},'Apply to new copy');apply.addEventListener('click',()=>submitScale('apply'));
 const reset=el('button',{type:'button'},'Reset draft');reset.addEventListener('click',()=>{scaleDraft.values.clear();scaleDraft.notice='Draft reset; saved copies and the active model are unchanged.';draw()});
 const job=s.dashboard_job;let outcome=null;
 if(job?.running)outcome=el('p',{role:'status'},'Running '+job.operation+'…');
 else if(job?.error)outcome=el('p',{class:'error',role:'alert'},job.error);
 else if(job?.result)outcome=el('p',{role:'status'},job.operation==='try'?'Preview complete. See the dose response in Experiments.':'Edited copy created. Evaluate and promote explicitly with dial weights evaluate / promote; the active model is unchanged.');
 return card('Adjust weights','Each slider and number box controls a tensor multiplier, not an analysis measurement. 1 = unchanged, 0 = zero, negative = invert. Range −4 to 4; at most 32 changed tensors. Values update instantly in this draft; model work starts only when you click an action. Quantized models may round small edits.',filter,el('div',{class:'editor-table'},tbl),
 el('div',{class:'editor-tools'},field('Existing experiment','experiment'),field('New copy name (Apply)','name'),el('label',{class:'cases'},'Preview cases (JSON conversations ending with a reference assistant answer)',cases)),
 el('p',{class:'muted'},'Create an experiment first: dial weights experiment NAME --budget N. Preview costs one run. Apply writes a separate copy; evaluation and promotion remain required.'),
 el('div',{class:'editor-tools'},preview,apply,reset),el('p',{role:'status'},scaleDraft.notice),outcome);
}
function tiles(s){const a=s.analyze||{},m=(s.map&&s.map.architecture)||{},t=(s.map&&s.map.totals)||{};const checked=list(a.summary&&a.summary.checked_parts);const items=[['Layers',m.layers??a.layer_count],['Parameters',human(t.params)],['Hidden size',m.hidden_size],['Attention heads',m.heads],['Experts',m.experts],['Baseline loss',a.baseline_loss==null?null:Number(a.baseline_loss).toFixed(2)],['Texts read',a.observation_texts],['Confirmed parts',checked.length?`${checked.filter(i=>i.confirmed).length} / ${checked.length}`:null],['Edited copies',list(s.edits).length||null]].filter(([,v])=>v!=null&&v!=='');if(!items.length)return null;return el('div',{class:'tiles'},...items.map(([k,v])=>el('div',{class:'tile'},el('div',{class:'v'},String(v)),el('div',{class:'k'},k))))}
const TABS=[
['overview','Overview',s=>{const a=s.analyze;return[tiles(s),a&&findingsSection(a),a&&bars('Layer importance',list(a.layers),'layer','importance',`Loss increase when each layer is skipped, over ${a.probes??'the'} probe texts. Tall bars are layers the model cannot do without.`)]}],
['adjust','Adjust weights',s=>[scaleEditor(s)]],
['checks','Checks',s=>s.analyze?[checkedSection(s.analyze),explanationsSection(s.analyze)]:[]],
['layers','Layers',s=>{const a=s.analyze;if(!a)return[];return[categoryHeat(a,'Layer importance per kind of text',r=>r.importance_by_category,'Loss increase on each category of text when the layer is skipped.'),categoryHeat(a,'Logit lens',r=>r.lens&&(r.lens.top10_by_category||r.lens.by_category),'Share of positions where the final prediction is already among the layer\'s top 10 guesses.'),...list(writesSection(a)),pair(structureHeat(a),memoryHeat(a)),bars('Linear-attention output RMS',list(a.layers).filter(r=>r.linear_attention).map(r=>({layer:r.layer,rms:r.linear_attention.output_rms})),'layer','rms','Typical size of each linear-attention block\'s output.')]}],
['attention','Attention',s=>{const a=s.analyze,t=s.trace;return[a&&headGrid(a.layers),a&&heat('Head roles per layer',list(a.layers).filter(r=>r.head_roles),'layer',Object.keys(ROLE_COLOR).filter(r=>r!=='mixed'),(r,c)=>r.head_roles[c]??0,'Number of notable heads of each role in each layer.'),t&&t.mode==='heads'&&heads(t),t&&t.mode==='heads'&&bars('Per-head loss change',t.heads,'head','delta')]}],
['experts','Experts',s=>s.analyze?[routingSection(s.analyze),specialistsSection(s.analyze),profilesSection(s.analyze)]:[]],
['neurons','Neurons',s=>s.analyze?[neuronsSection(s.analyze)]:[]],
['checkpoint','Checkpoint',s=>{const out=[];if(s.map){const a=s.map.architecture||{},t=s.map.totals||{};out.push(card('Checkpoint',`${a.model_type||'unknown'} · ${a.layers} layers · hidden ${a.hidden_size} · ${a.heads} heads · ${num(t.tensors)} tensors · ${human(t.params)} parameters${t.quantized?' · quantized':''}`),heat('Parameters per layer and component',s.map.layers,'layer',components(s.map.layers),(r,c)=>r.components[c]??null))}if(s.stats)out.push(bars('Weight norm per layer',s.stats.layers,'layer','norm'),heat('Weight RMS per layer and component',s.stats.layers,'layer',components(s.stats.layers),(r,c)=>r.components[c]?.rms??null));return out}],
['experiments','Experiments',s=>{const out=[editsSection(s.edits),doseChart(s.try),attributeSection(s.attribute)];if(s.activations){const a=s.activations;out.push(bars('Activation contrast per layer',a.layers,'layer','score',`${a.pairs} contrast pairs; suggested layer ${a.suggested_layer}.`),heat('Activation contrast metrics',a.layers,'layer',['contrast','residual_norm','relative_contrast','alignment','score'],(r,c)=>r[c]??null))}if(s.trace&&s.trace.mode!=='heads')out.push(bars('Sensitivity per layer',s.trace.layers,'layer','delta',`Loss increase when the layer is skipped (baseline ${num(s.trace.baseline_loss)} over ${s.trace.answer_tokens} answer tokens).`));if(s.investigate)out.push(bars('Investigate: layer interventions',s.investigate.layers,'layer','delta',`Answer-loss change when each layer's output is replaced by its input, over ${s.investigate.case_count} cases and ${s.investigate.forward_passes} forward passes${s.investigate.restoration_checked?'; the intact baseline was reproduced afterwards':''}.`));return out}],
['history','History',s=>[experimentTimeline(s.experiments),recentRuns(s.runs)]],
];
let current=location.hash.slice(1)||'overview',state=null,lastText='';
function empty(){return el('div',{class:'empty'},el('p',{},'No results yet.'),el('p',{},'Run ',el('code',{},'dial weights analyze'),' (or map, stats, trace…), or ',el('code',{},'/weights analyze'),' in a session.'))}
function draw(){const built=TABS.map(([id,name,build])=>[id,name,state?[safe(()=>build(state))].flat(2).filter(Boolean):[]]).filter(([id,,kids])=>kids.length||id==='overview');if(!built.some(([id])=>id===current))current='overview';$('nav').replaceChildren(...built.map(([id,name])=>{const b=el('button',{type:'button',class:id===current?'on':null},name);b.addEventListener('click',()=>{current=id;history.replaceState(null,'','#'+id);draw();window.scrollTo(0,0)});return b}));const kids=built.find(([id])=>id===current)[2];$('root').replaceChildren(...(kids.length?kids:[empty()]))}
function header(s){const target=s.target||{};$('model').textContent=s.model||'';$('path').textContent=target.path||('unsupported: '+target.unsupported);const p=s.progress;$('pill').replaceChildren(...(p?[el('span',{class:'dot'}),`${p.phase||'working'}${p.total?` · ${p.completed}/${p.total}`:''}`,p.total?el('div',{class:'track',style:'width:120px'},el('div',{class:'fill',style:`width:${Math.min(100,100*p.completed/p.total)}%`})):null]:[`updated ${new Date().toLocaleTimeString()}`]))}
window.addEventListener('hashchange',()=>{current=location.hash.slice(1)||'overview';draw()});
function load(){
 fetch('/api/status').then(r=>{if(!r.ok)return r.text().then(t=>{throw Error(r.status+' '+t)});return r.json()}).then(s=>{
  const text=JSON.stringify(s);header(s);
  if(state&&state.model!==s.model){scaleDraft.values.clear();scaleDraft.notice='Model changed; draft reset.'}
  state=s;
  if(text!==lastText&&!document.querySelector('#root input:focus,#root textarea:focus')){lastText=text;draw()}
  setTimeout(load,s.progress||s.dashboard_job?.running?2000:10000);
 }).catch(e=>{$('pill').replaceChildren(el('span',{class:'badge bad'},'Unable to load: '+e));setTimeout(load,5000)})
}
load();
</script>"##;

/// The process's dashboard: the state directory and model it serves, and its port.
static SERVER: parking_lot::Mutex<Option<(PathBuf, String, u16)>> = parking_lot::Mutex::new(None);

/// Serves the dashboard for `config` on `listener` until it fails.
pub fn serve_listener(listener: TcpListener, config: Config) -> Result<()> {
    let job = std::sync::Arc::new(parking_lot::Mutex::new(serde_json::Value::Null));
    serve_requests(listener, "weights dashboard", move |request| {
        actions::respond(&config, &job, request)
    })
}

/// Starts (once per process, state directory and model) the dashboard on an ephemeral loopback
/// port and returns its URL.
pub fn url(config: &Config) -> Result<String> {
    let mut slot = SERVER.lock();
    if let Some((dir, model, port)) = slot.as_ref()
        && *dir == config.state_dir
        && model == super::target_name(config)
    {
        return Ok(format!("http://127.0.0.1:{port}/"));
    }
    let listener = bind_loopback(0)?;
    let port = listener.local_addr()?.port();
    let served = config.clone();
    std::thread::Builder::new()
        .name("dial-weights-dashboard".into())
        .spawn(move || {
            if let Err(error) = serve_listener(listener, served) {
                eprintln!("weights dashboard stopped: {error:#}");
            }
        })
        .context("starting the weights dashboard thread")?;
    *slot = Some((
        config.state_dir.clone(),
        super::target_name(config).to_owned(),
        port,
    ));
    Ok(format!("http://127.0.0.1:{port}/"))
}

/// Maps a request target to `(status, content type, body)`.
pub(super) fn route(config: &Config, target: &str) -> Response {
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    match path {
        "/" => ("200 OK", HTML, PAGE.to_owned()),
        "/api/status" => json_response(Ok(status(config))),
        _ => ("404 Not Found", TEXT, "Not found".into()),
    }
}

mod actions;

#[cfg(test)]
mod tests;
