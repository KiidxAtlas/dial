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
.heads{display:grid;gap:2px;width:max-content}.heads div{width:44px;height:44px;display:flex;align-items:center;justify-content:center;font:11px ui-monospace,monospace;border-radius:4px}
</style>
<h1>Dial weights</h1><p id=sub class=muted>Loading…</p><p id=progress class=muted></p><div id=root></div>
<script>
const el=(tag,attrs={},...children)=>{const e=document.createElement(tag);for(const[k,v]of Object.entries(attrs))e.setAttribute(k,v);e.append(...children);return e};
const num=v=>v==null?'n/a':Number(v).toPrecision(4);
const shade=(v,max)=>{if(v==null)return'transparent';const t=max>0?Math.min(1,Math.abs(v)/max):0;return v<0?`rgba(220,90,80,${t})`:`rgba(70,170,150,${t})`};
const peak=values=>Math.max(0,...values.filter(v=>v!=null).map(Math.abs));
function bars(title,rows,label,key){if(!rows||!rows.length)return null;const max=peak(rows.map(r=>r[key]));const box=el('section',{},el('h2',{},title));for(const r of rows){const v=r[key];const width=v==null||!max?0:Math.abs(v)/max*420;box.append(el('div',{class:'row'},el('span',{class:'name'},String(r[label])),el('div',{class:v<0?'bar neg':'bar',style:`width:${width}px`}),el('span',{},num(v))))}return box}
function heat(title,rows,label,columns,value){if(!rows||!rows.length||!columns.length)return null;const max=peak(rows.flatMap(r=>columns.map(c=>value(r,c))));const table=el('table',{},el('tr',{},el('th',{},label),...columns.map(c=>el('th',{},c))));for(const r of rows){const tr=el('tr',{},el('th',{},String(r[label])));for(const c of columns){const v=value(r,c);const td=el('td',{style:`background:${shade(v,max)}`,title:`${label} ${r[label]} · ${c}: ${num(v)}`},v==null?'':Number(v).toPrecision(3));tr.append(td)}table.append(tr)}return el('section',{},el('h2',{},title),table)}
const components=rows=>[...new Set((rows||[]).flatMap(r=>Object.keys(r.components||{})))].sort();
function heads(trace){const list=trace.heads||[];const max=peak(list.map(h=>h.delta));const grid=el('div',{class:'heads',style:`grid-template-columns:repeat(${Math.ceil(Math.sqrt(list.length))||1},44px)`});for(const h of list)grid.append(el('div',{style:`background:${h.delta==null?'#222':shade(h.delta,max)}`,title:`head ${h.head}: loss change ${num(h.delta)}`},String(h.head)));return el('section',{},el('h2',{},`Layer ${trace.layer} attention heads: loss increase when the head is removed (baseline ${num(trace.baseline_loss)})`),grid)}
function render(s){
const target=s.target||{};sub.textContent=`${s.model} · ${target.path||('unsupported: '+target.unsupported)}`;
const p=s.progress;progress.textContent=p?`Running: ${p.phase||'working'}${p.total?` ${p.completed}/${p.total}`:''}`:'';
const parts=[];
if(s.map){const a=s.map.architecture||{},t=s.map.totals||{};parts.push(el('section',{},el('h2',{},'Checkpoint'),el('p',{},`${a.model_type||'unknown'} · ${a.layers} layers · hidden ${a.hidden_size} · ${a.heads} heads · ${t.tensors} tensors · ${t.params} parameters${t.quantized?' · quantized':''}`)));parts.push(heat('Parameters per layer and component',s.map.layers,'layer',components(s.map.layers),(r,c)=>r.components[c]??null))}
if(s.stats){parts.push(bars('Weight norm per layer',s.stats.layers,'layer','norm'));parts.push(heat('Weight RMS per layer and component',s.stats.layers,'layer',components(s.stats.layers),(r,c)=>r.components[c]?.rms??null))}
if(s.activations){const a=s.activations;parts.push(bars(`Activation contrast score per layer (${a.pairs} pairs, suggested layer ${a.suggested_layer})`,a.layers,'layer','score'));parts.push(heat('Activation contrast metrics',a.layers,'layer',['contrast','residual_norm','relative_contrast','alignment','score'],(r,c)=>r[c]??null))}
if(s.trace){const t=s.trace;if(t.mode==='heads'){parts.push(heads(t));parts.push(bars('Per-head loss change',t.heads,'head','delta'))}else parts.push(bars(`Sensitivity: loss increase when the layer is skipped (baseline ${num(t.baseline_loss)} over ${t.answer_tokens} answer tokens)`,t.layers,'layer','delta'))}
const shown=parts.filter(Boolean);root.replaceChildren(...(shown.length?shown:[el('p',{class:'muted'},'No results yet. Run dial weights map, stats, activations or trace, or /weights map in a session.')]))}
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
        && *model == config.model
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
    *slot = Some((config.state_dir.clone(), config.model.clone(), port));
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
