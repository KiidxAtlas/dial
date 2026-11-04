//! Loopback dashboard for weight inspection results: per-layer heatmaps of parameter counts,
//! weight norms, activation contrast and ablation sensitivity, plus the attention-head grid.
//! It reuses the stats dashboard's server (127.0.0.1 only, `Host` gate, no-store responses) and
//! only reads the stored results; it never starts a job.
use super::status;
use crate::config::types::Config;
use crate::stats::server::{HTML, Response, TEXT, bind_loopback, json_response, serve_routes};
use anyhow::{Context, Result};
use std::net::TcpListener;
use std::path::PathBuf;

const PAGE: &str = r##"<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width"><title>Dial weights</title>
<style>
body{font:14px system-ui;max-width:1200px;margin:1.5rem auto;padding:0 1rem;background:#111;color:#ddd}
h1{font-size:20px;margin-bottom:.2rem}h2{font-size:15px;margin:1.6rem 0 .5rem}.muted{color:#888}
.row{display:flex;align-items:center;gap:.5rem;font:12px ui-monospace,monospace;height:14px}
.row .name{width:4em;text-align:right}.bar{height:10px;background:#46aa96}.bar.neg{background:#dc5a50}
table{border-collapse:collapse}th,td{padding:2px 6px;font:11px ui-monospace,monospace;text-align:right}
td.text,th.text{text-align:left}
tr.win td{background:rgba(70,170,150,.22)}tr.loss td{background:rgba(220,90,80,.22)}tr.tie td{background:rgba(136,136,136,.18)}
.heads{display:grid;gap:2px;width:max-content}.heads div{width:44px;height:44px;display:flex;align-items:center;justify-content:center;font:11px ui-monospace,monospace;border-radius:4px}
.budget{width:320px;height:8px;background:#333;border-radius:4px;margin:.3rem 0}.budget div{height:8px;background:#46aa96;border-radius:4px}
.tokens{font:11px ui-monospace,monospace;margin:0 0 .4rem 4.5em}code{font:12px ui-monospace,monospace}
button{font:11px system-ui;margin-left:.3rem;background:#222;color:#ddd;border:1px solid #444;border-radius:3px;cursor:pointer}
.swatch{display:inline-block;width:10px;height:10px;border-radius:2px;vertical-align:middle;margin:0 .2rem 0 .6rem}
</style>
<h1>Dial weights</h1><p id=sub class=muted>Loading…</p><p id=progress class=muted></p><div id=root></div>
<script>
const el=(tag,attrs={},...children)=>{const e=document.createElement(tag);for(const[k,v]of Object.entries(attrs))e.setAttribute(k,v);e.append(...children.filter(c=>c!=null&&c!==false).map(c=>c instanceof Node?c:String(c)));return e};
const svg=(tag,attrs={},...children)=>{const e=document.createElementNS('http://www.w3.org/2000/svg',tag);for(const[k,v]of Object.entries(attrs))e.setAttribute(k,v);e.append(...children.filter(c=>c!=null).map(c=>c instanceof Node?c:document.createTextNode(String(c))));return e};
const num=v=>v==null?'n/a':Number(v).toPrecision(4);
const pct=v=>typeof v==='number'&&isFinite(v)?(v*100).toFixed(1)+'%':'n/a';
const when=t=>typeof t==='number'&&isFinite(t)?new Date(t*1000).toLocaleString():(t==null?'n/a':String(t));
const list=v=>Array.isArray(v)?v:[];
const shade=(v,max)=>{if(v==null)return'transparent';const t=max>0?Math.min(1,Math.abs(v)/max):0;return v<0?`rgba(220,90,80,${t})`:`rgba(70,170,150,${t})`};
const peak=values=>Math.max(0,...values.filter(v=>v!=null).map(Math.abs));
const safe=build=>{try{return build()}catch(e){return el('p',{class:'muted'},'Section unavailable: '+e)}};
function section(title,...kids){return el('section',{},el('h2',{},title),...kids)}
function table(head,rows){return el('table',{},el('tr',{},...head.map(h=>el('th',{},h))),...rows.map(r=>el('tr',{},...r.map(c=>el('td',{},c)))))}
function bars(title,rows,label,key){if(!rows||!rows.length)return null;const max=peak(rows.map(r=>r[key]));const box=el('section',{},el('h2',{},title));for(const r of rows){const v=r[key];const width=v==null||!max?0:Math.abs(v)/max*420;box.append(el('div',{class:'row'},el('span',{class:'name'},String(r[label])),el('div',{class:v<0?'bar neg':'bar',style:`width:${width}px`}),el('span',{},num(v))))}return box}
function heat(title,rows,label,columns,value){if(!rows||!rows.length||!columns.length)return null;const max=peak(rows.flatMap(r=>columns.map(c=>value(r,c))));const table=el('table',{},el('tr',{},el('th',{},label),...columns.map(c=>el('th',{},c))));for(const r of rows){const tr=el('tr',{},el('th',{},String(r[label])));for(const c of columns){const v=value(r,c);const td=el('td',{style:`background:${shade(v,max)}`,title:`${label} ${r[label]} · ${c}: ${num(v)}`},v==null?'':Number(v).toPrecision(3));tr.append(td)}table.append(tr)}return el('section',{},el('h2',{},title),table)}
const components=rows=>[...new Set((rows||[]).flatMap(r=>Object.keys(r.components||{})))].sort();
function heads(trace){const list=trace.heads||[];const max=peak(list.map(h=>h.delta));const grid=el('div',{class:'heads',style:`grid-template-columns:repeat(${Math.ceil(Math.sqrt(list.length))||1},44px)`});for(const h of list)grid.append(el('div',{style:`background:${h.delta==null?'#222':shade(h.delta,max)}`,title:`head ${h.head}: loss change ${num(h.delta)}`},String(h.head)));return el('section',{},el('h2',{},`Layer ${trace.layer} attention heads: loss increase when the head is removed (baseline ${num(trace.baseline_loss)})`),grid)}

// Edited copies: one row per stored edit, with its verdict, measured quality and promotion command.
function ids(values){return list(values).map(x=>x&&typeof x==='object'?(x.layer??x.expert??x.tensor??''):x).join(', ')}
function changed(d){const out=[];if(d.layers)out.push('layers '+ids(d.layers));if(list(d.tensors).length)out.push('tensors '+ids(d.tensors));if(list(d.experts).length)out.push('experts '+ids(d.experts));if(d.shared_expert)out.push('shared expert');if(list(d.sources).length)out.push('sources '+list(d.sources).map(s=>s&&typeof s==='object'?s.name:s).join(', '));return out.join('; ')||'n/a'}
function params(d){const out=[];if(d.alpha!=null)out.push('alpha '+d.alpha);if(d.columns!=null)out.push('columns '+d.columns);if(d.strength!=null)out.push('strength '+d.strength);for(const t of list(d.tensors))if(t&&t.scale!=null)out.push(`${t.tensor??''} ×${t.scale}`);return out.join(', ')||'n/a'}
function verdict(e){const ev=e.evaluation;if(ev)return ev.passed?'pass':'fail'+(ev.reason?': '+ev.reason:'');if(e.passed==null)return'not evaluated';return e.passed?'pass':'fail'}
function promoteCell(e){if(!(e.evaluation?.passed??e.passed))return el('span',{class:'muted'},'—');const cmd=`dial weights promote ${e.name}`;const button=el('button',{type:'button'},'copy');button.addEventListener('click',()=>navigator.clipboard?.writeText(cmd));return el('span',{},el('code',{},cmd),button)}
function evaluationView(e){const ev=e.evaluation;if(!ev)return el('span',{class:'muted'},'not evaluated');const cells=o=>[o.tokens,o.hit_limit==null?'n/a':o.hit_limit?'yes':'no',o.passed==null?'n/a':o.passed?'yes':'no',num(o.repeat_ratio)];const rows=list(ev.rows).map(r=>{const outcome=['win','loss','tie'].includes(r.outcome)?r.outcome:'tie';return el('tr',{class:outcome},el('td',{class:'text'},String(r.case??'')),el('td',{class:'text'},outcome),...cells(r.base||{}),...cells(r.edited||{}))});const q=ev.quality;const quality=q?`probe loss ${num(q.base_probe_loss)} → ${num(q.edited_probe_loss)} (${pct(q.relative_change)})`:'quality not measured';const head=['case','outcome','base tokens','base hit limit','base passed','base repeat','edit tokens','edit hit limit','edit passed','edit repeat'];return el('details',{},el('summary',{},`${ev.wins??0} wins · ${ev.losses??0} losses · ${ev.ties??0} ties · p ${num(ev.p_value)}`),el('table',{},el('tr',{},...head.map(h=>el('th',{class:h==='case'||h==='outcome'?'text':''},h))),...rows),el('p',{class:'muted'},quality+(ev.reason?` · ${ev.reason}`:'')))}
function editsSection(edits){const rows=list(edits);if(!rows.length)return null;const head=['name','experiment','method','changed','params','max requant. error','verdict','wins / losses / ties','p','quality Δ','promote','evaluation'];return section('Edited copies',table(head,rows.map(e=>{const d=e.details||{};const ev=e.evaluation||{};return[e.name,e.experiment,d.method,changed(d),params(d),num(d.max_requantization_error),verdict(e),ev.wins==null?'n/a':`${ev.wins} / ${ev.losses} / ${ev.ties}`,num(ev.p_value),pct(ev.quality?.relative_change),promoteCell(e),evaluationView(e)]})))}

// `try` dose curves: loss delta and neutral KL per dose, each series scaled to its own maximum.
function doseChart(t){const doses=list(t&&t.doses);if(!doses.length)return null;const W=560,H=190,P=34,n=doses.length;const x=i=>P+(n>1?i/(n-1):0.5)*(W-2*P);const y=(v,m)=>v==null||!m?H-P:H-P-(Math.abs(v)/m)*(H-2*P);const maxDelta=peak(doses.map(d=>d.delta)),maxKl=peak(doses.map(d=>d.neutral_kl));const chart=svg('svg',{viewBox:`0 0 ${W} ${H}`,width:W,height:H,role:'img'});chart.append(svg('line',{x1:P,y1:H-P,x2:W-P,y2:H-P,stroke:'#555'}));const series=(key,max,color)=>{const points=doses.map((d,i)=>[x(i),y(d[key],max),d]).filter(p=>p[2][key]!=null);if(points.length>1)chart.append(svg('polyline',{points:points.map(p=>`${p[0]},${p[1]}`).join(' '),fill:'none',stroke:color,'stroke-width':2}));for(const[px,py,d]of points)chart.append(svg('circle',{cx:px,cy:py,r:d.noisy?7:4,fill:color,stroke:d.noisy?'#fff':'none','stroke-width':2},svg('title',{},`dose ${d.dose}: ${key} ${num(d[key])}${d.noisy?' (noisy)':''}`)))};series('delta',maxDelta,'#46aa96');series('neutral_kl',maxKl,'#e0b050');doses.forEach((d,i)=>chart.append(svg('text',{x:x(i),y:H-P+14,'text-anchor':'middle',fill:'#888','font-size':11},String(d.dose))));return section('Try: dose response',el('p',{class:'muted'},el('span',{style:'color:#46aa96'},`■ loss delta (max ${num(maxDelta)})`),el('span',{style:'color:#e0b050'},` · ■ neutral KL (max ${num(maxKl)})`),' · white ring = noisy dose'),chart)}

// `attribute` rankings: heads, neurons and experts ranked by the stored key, with promote/suppress tokens.
function rankRows(title,rows,key,name){rows=list(rows).slice(0,12);if(!rows.length)return null;const value=r=>r[key]??r.support??null;const max=peak(rows.map(value));const box=el('section',{},el('h2',{},title));for(const r of rows){const v=value(r);const width=v==null||!max?0:Math.abs(v)/max*380;box.append(el('div',{class:'row'},el('span',{class:'name',style:'width:9em'},`L${r.layer} ${name} ${r[name]}`),el('div',{class:v<0?'bar neg':'bar',style:`width:${width}px`}),el('span',{},num(v)),el('span',{class:'muted'},r.agreement==null?'':`agreement ${Number(r.agreement).toFixed(2)}`)));const promotes=list(r.promotes),suppresses=list(r.suppresses);if(promotes.length||suppresses.length)box.append(el('div',{class:'tokens'},`promotes ${promotes.join(' ')||'n/a'} · suppresses ${suppresses.join(' ')||'n/a'}`))}return box}
function attributeSection(a){if(!a)return null;const key=a.ranked_by||'support';const parts=[rankRows(`Attention heads ranked by ${key} (estimated loss rise if removed)`,a.heads,key,'head'),rankRows('MLP neurons',a.neurons,key,'neuron'),rankRows('MoE experts',a.experts,key,'expert')].filter(Boolean);return parts.length?el('div',{},...parts):null}

// Analyze: head roles as a grid per layer (heads without a notable role count as mixed), MoE routing, linear attention.
const ROLE_COLOR={induction:'#46aa96','previous-token':'#5a8ddc','attention-sink':'#c8a03c',mixed:'#555'};
function headGrid(rows){const layers=list(rows).filter(r=>r&&r.head_count);if(!layers.length)return null;const out=[el('h2',{},'Attention heads by role')];out.push(el('p',{class:'muted'},...Object.entries(ROLE_COLOR).flatMap(([role,color])=>[el('span',{class:'swatch',style:`background:${color}`}),role])));for(const r of layers){const notable=new Map(list(r.heads).map(h=>[h.head,h]));const grid=el('div',{class:'heads',style:`grid-template-columns:repeat(${Math.ceil(Math.sqrt(r.head_count))},44px)`});for(let h=0;h<r.head_count;h++){const n=notable.get(h);const role=n?.role??'mixed';grid.append(el('div',{style:`background:${ROLE_COLOR[role]??'#555'}`,title:`layer ${r.layer} head ${h}: ${role}`+(n?`; previous ${num(n.previous)}, sink ${num(n.sink)}, induction ${num(n.induction)}`:'')},String(h)))}out.push(el('div',{class:'muted'},`Layer ${r.layer} · ${r.head_count} heads`),grid)}return el('section',{},...out)}
function routingSection(a){if(!a)return null;const layers=list(a.layers).filter(r=>r&&r.experts);if(!layers.length)return null;const summary=a.routing||{};const box=section(`MoE routing per layer (${summary.moe_layers??layers.length} MoE layers${summary.mean_entropy!=null?`, mean entropy ${num(summary.mean_entropy)}`:''})`);for(const r of layers){const x=r.experts;const top=list(x.top).slice(0,8).map(t=>`e${t.expert} ${pct(t.share)}`).join(' · ');box.append(el('div',{class:'row'},el('span',{class:'name'},`L${r.layer}`),el('div',{class:'bar',style:`width:${Math.max(0,Math.min(1,x.entropy??0))*380}px`}),el('span',{},num(x.entropy)),el('span',{class:'muted'},`${x.count??'?'} experts, top ${x.top_k??'?'} · ${top}`)))}return box}

// Experiment timeline: budget bar, runs in order, held-out count.
function experimentTimeline(exps){const rows=list(exps);if(!rows.length)return null;const box=section('Experiments');for(const e of rows){const budget=Number(e.budget)||0,used=Number(e.used)||0;const frac=budget>0?Math.min(1,used/budget):0;const held=typeof e.held_out==='number'?e.held_out:list(e.held_out).length;box.append(el('div',{},el('div',{},el('strong',{},String(e.experiment??'')),` · ${used}/${budget} runs · ${held} held-out cases · started ${when(e.created_at)}`),el('div',{class:'budget'},el('div',{style:`width:${frac*100}%`})),...list(e.runs).map(r=>el('div',{class:'muted'},`${r.op??'?'} · ${when(r.at)} · cost ${r.cost??'n/a'}${r.result?` · run ${r.result}`:''}`))))}return box}

// Recent runs, newest first, from the stored run list.
function recentRuns(runs){const rows=list(runs);if(!rows.length)return null;return section('Recent runs',el('ul',{},...rows.map(r=>el('li',{},el('code',{},String(r.id??'')),` ${r.op??''} · ${when(r.created_at)}${r.experiment?' · '+r.experiment:''}${r.model_id?' · '+r.model_id:''} — ${r.summary??''}`))))}

function render(s){
const target=s.target||{};sub.textContent=`${s.model} · ${target.path||('unsupported: '+target.unsupported)}`;
const p=s.progress;progress.textContent=p?`Running: ${p.phase||'working'}${p.total?` ${p.completed}/${p.total}`:''}`:'';
const parts=[];
if(s.map){const a=s.map.architecture||{},t=s.map.totals||{};parts.push(el('section',{},el('h2',{},'Checkpoint'),el('p',{},`${a.model_type||'unknown'} · ${a.layers} layers · hidden ${a.hidden_size} · ${a.heads} heads · ${t.tensors} tensors · ${t.params} parameters${t.quantized?' · quantized':''}`)));parts.push(heat('Parameters per layer and component',s.map.layers,'layer',components(s.map.layers),(r,c)=>r.components[c]??null))}
if(s.stats){parts.push(bars('Weight norm per layer',s.stats.layers,'layer','norm'));parts.push(heat('Weight RMS per layer and component',s.stats.layers,'layer',components(s.stats.layers),(r,c)=>r.components[c]?.rms??null))}
if(s.activations){const a=s.activations;parts.push(bars(`Activation contrast score per layer (${a.pairs} pairs, suggested layer ${a.suggested_layer})`,a.layers,'layer','score'));parts.push(heat('Activation contrast metrics',a.layers,'layer',['contrast','residual_norm','relative_contrast','alignment','score'],(r,c)=>r[c]??null))}
if(s.trace){const t=s.trace;if(t.mode==='heads'){parts.push(heads(t));parts.push(bars('Per-head loss change',t.heads,'head','delta'))}else parts.push(bars(`Sensitivity: loss increase when the layer is skipped (baseline ${num(t.baseline_loss)} over ${t.answer_tokens} answer tokens)`,t.layers,'layer','delta'))}
if(s.analyze){const a=s.analyze;parts.push(bars(`Layer importance: loss increase when the layer is skipped (${a.probes} probe texts)`,list(a.layers),'layer','importance'));parts.push(heat('Attention heads by role per layer (notable heads only)',list(a.layers).filter(r=>r.head_roles),'layer',['induction','previous-token','attention-sink','mixed'],(r,c)=>r.head_roles[c]??0))}
parts.push(safe(()=>editsSection(s.edits)),safe(()=>doseChart(s.try)),safe(()=>attributeSection(s.attribute)));
if(s.analyze)parts.push(safe(()=>headGrid(s.analyze.layers)),safe(()=>routingSection(s.analyze)),safe(()=>bars('Linear-attention output RMS per layer',list(s.analyze.layers).filter(r=>r.linear_attention).map(r=>({layer:r.layer,rms:r.linear_attention.output_rms})),'layer','rms')));
parts.push(safe(()=>experimentTimeline(s.experiments)),safe(()=>recentRuns(s.runs)));
const shown=parts.filter(Boolean);root.replaceChildren(...(shown.length?shown:[el('p',{class:'muted'},'No results yet. Run dial weights map, analyze, stats, activations or trace, or /weights map in a session.')]))}
function load(){fetch('/api/status').then(r=>{if(!r.ok)return r.text().then(t=>{throw Error(r.status+' '+t)});return r.json()}).then(s=>{render(s);setTimeout(load,s.progress?2000:10000)}).catch(e=>{sub.textContent='Unable to load: '+e;setTimeout(load,5000)})}
load();
</script>"##;

/// The process's dashboard: the state directory and model it serves, and its port.
static SERVER: parking_lot::Mutex<Option<(PathBuf, String, u16)>> = parking_lot::Mutex::new(None);

/// Serves the dashboard for `config` on `listener` until it fails.
pub fn serve_listener(listener: TcpListener, config: Config) -> Result<()> {
    serve_routes(listener, "weights dashboard", move |target| {
        route(&config, target)
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

#[cfg(test)]
mod tests;
