//! Benchmark child protocol: every turn stays in one process/session, with bounded event capture.
use anyhow::{Context,Result,ensure};
use dial::benchmark::{BenchmarkCase,RunMetrics};
use serde_json::{Value,json};
use std::{io::{BufRead,BufReader,Write},path::Path,process::{Command,Stdio,Child},sync::mpsc,thread,time::{Duration,Instant}};
struct ChildGuard(Child);
impl Drop for ChildGuard {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
fn receive(rx:&mpsc::Receiver<Result<Value>>,deadline:Instant)->Result<Value>{rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).context("benchmark RPC timed out")?}
fn request(input:&mut impl Write,rx:&mpsc::Receiver<Result<Value>>,id:u64,method:&str,params:Value,deadline:Instant,metrics:&mut RunMetrics)->Result<Value>{
 serde_json::to_writer(&mut *input,&json!({"id":id,"method":method,"params":params}))?;input.write_all(b"\n")?;input.flush()?;
 loop {let event=receive(rx,deadline)?;observe(metrics,&event);if event["id"]==id {ensure!(event.get("error").is_none(),"benchmark RPC failed: {}",event["error"]);return Ok(event["result"].clone());}}
}
fn observe(m:&mut RunMetrics,event:&Value){match event["event"].as_str(){
 Some("usage")=>{m.prompt_tokens+=event["prompt_tokens"].as_u64().unwrap_or(0);m.output_tokens+=event["output_tokens"].as_u64().unwrap_or(0);},
 Some("cached_usage")=>{m.cached_tokens+=event["read_tokens"].as_u64().unwrap_or(0);},
 Some("reasoning_usage")=>m.reasoning_tokens+=event["tokens"].as_u64().unwrap_or(0),
 Some("tool_result")=>{m.tool_calls+=1;if event["is_error"]==true{m.tool_errors+=1;}match event["name"].as_str(){Some("artifact_read")|Some("artifact_search")=>m.artifacts_retrieved+=1,Some("tool_load")=>m.dynamic_tools_used+=1,_=>{}}if event["text"].as_str().is_some_and(|t|t.starts_with("invalid tool call")){m.tool_contract_errors+=1;}},
 Some("notice")=>{if event["message"].as_str().is_some_and(|t|t.contains("output limit")&&t.contains("retry")){m.truncated_recoveries+=1;}},_=>{}}}

pub fn run(executable:&Path,user_home:&Path,case:&BenchmarkCase,workspace:&Path)->Result<RunMetrics>{
 let started=Instant::now();let deadline=started+Duration::from_secs(600);
 let mut command=Command::new(executable);command.args(["rpc","--yes","--sandbox","dev"]).arg("--dir").arg(workspace).env("DIAL_HOME",user_home).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
 let mut child=ChildGuard(command.spawn().context("starting persistent benchmark child")?);
 let mut input=child.0.stdin.take().context("benchmark stdin unavailable")?;let stdout=child.0.stdout.take().context("benchmark stdout unavailable")?;
 let (tx,rx)=mpsc::sync_channel(16);
 let reader=thread::spawn(move ||{let mut captured=0usize;let mut reader=BufReader::new(stdout);loop{let mut line=String::new();match reader.read_line(&mut line){Ok(0)=>break,Ok(_)=>{captured+=line.len();let result=if captured>32*1024*1024{Err(anyhow::anyhow!("benchmark RPC capture exceeds 32 MiB"))}else{serde_json::from_str(&line).context("invalid benchmark RPC event")};let stop=result.is_err();if tx.send(result).is_err()||stop{break;}},Err(e)=>{let _=tx.send(Err(e.into()));break;}}}});
 let mut metrics=RunMetrics::default();
 let result=(||->Result<()> {
  loop {let event=receive(&rx,deadline)?;if event["event"]=="ready"{break;}}
  let mut id=1;
  if !case.context_messages.is_empty(){request(&mut input,&rx,id,"seed_context",json!({"messages":case.context_messages}),deadline,&mut metrics)?;id+=1;}
  for (prompt,compact) in std::iter::once((case.prompt.as_str(),false)).chain(case.turns.iter().map(|t|(t.prompt.as_str(),t.compact_before))){
   if compact {request(&mut input,&rx,id,"compact",json!({}),deadline,&mut metrics)?;metrics.compactions+=1;id+=1;}
   let report=request(&mut input,&rx,id,"send",json!({"text":prompt}),deadline,&mut metrics)?;id+=1;
   ensure!(report["interrupted"]!=true&&report["text"].as_str().is_some_and(|t|!t.trim().is_empty()),"benchmark turn interrupted or empty");
   let completed=report.pointer("/metrics/turns_completed").and_then(Value::as_u64).context("benchmark completion counter unavailable")?;
   ensure!(completed==metrics.turns_completed+1,"agent stopped without completing the benchmark turn");metrics.turns_completed=completed;
   if let Some(v)=report.get("metrics"){for (name,target) in [("tool_contract_errors",&mut metrics.tool_contract_errors),("compactions",&mut metrics.compactions),("artifacts_retrieved",&mut metrics.artifacts_retrieved),("dynamic_tools_used",&mut metrics.dynamic_tools_used),("truncated_recoveries",&mut metrics.truncated_recoveries),("reasoning_tokens",&mut metrics.reasoning_tokens),("cached_tokens",&mut metrics.cached_tokens)]{if let Some(n)=v[name].as_u64(){*target=n;}}}
  }
  request(&mut input,&rx,id,"shutdown",json!({}),deadline,&mut metrics)?;Ok(())
 })();
 metrics.elapsed_ms=started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;metrics.agent_succeeded=result.is_ok();metrics.agent_error=result.is_err();
 drop(input);drop(child);drop(rx);let _=reader.join();
 if let Err(e)=result{eprintln!("benchmark case {}: {e:#}",case.id);}
 Ok(metrics)
}
