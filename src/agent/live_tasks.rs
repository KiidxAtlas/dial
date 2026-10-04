//! Live jobs own their backends and conversations. Approval always returns to the controller.
use super::{approval::{self, Denial, PermissionRule}, subagents::{self, Approver, Env, Outcome, Status}, Ui, make_backend};
use crate::{config::types::{Approval, Config, Harness}, llm::ToolCall, storage::paths::Layout, tools::{ToolOutput, Workspace, ext::Registry}};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, mpsc}, thread::{self, JoinHandle}, time::{Duration, Instant}};
enum Event { Approve { call: ToolCall, summary: String, needs: bool, cancel: Arc<AtomicBool>, answer: mpsc::Sender<Result<(), Denial>> }, Notice(String), Cost(f64), Done(String, Value) }
struct JobState { status: Value, interrupt: Arc<AtomicBool>, worker: Option<JoinHandle<()>>, active_tool: ActiveTool }
type Mailboxes = Arc<Mutex<BTreeMap<String, Arc<ControlJob>>>>;
type ActiveTool = Arc<Mutex<Option<Arc<dyn crate::tools::ext::Tool>>>>;
struct ControlJob { inbox: Arc<Mutex<Vec<String>>>, interrupt: Arc<AtomicBool>, active_tool: ActiveTool, finished: AtomicBool }
#[derive(Clone)]
pub struct LiveTaskControl { jobs: Mailboxes }
impl LiveTaskControl {
    pub fn send(&self, id: &str, text: &str) -> anyhow::Result<Value> {
        anyhow::ensure!(!text.trim().is_empty() && text.len() <= 32768, "message must be 1..32768 bytes");
        let job = self.jobs.lock().map_err(|_| anyhow::anyhow!("live job registry poisoned"))?.get(id).cloned().ok_or_else(|| anyhow::anyhow!("job is unknown or finished"))?;
        anyhow::ensure!(!job.finished.load(Ordering::SeqCst), "job has finished");
        let mut inbox = job.inbox.lock().map_err(|_| anyhow::anyhow!("job inbox poisoned"))?;
        anyhow::ensure!(inbox.len() < 32, "job inbox is full");
        inbox.push(text.to_owned()); Ok(json!({"id":id,"delivered":true}))
    }
    pub fn cancel(&self, id: &str) -> anyhow::Result<Value> {
        let job = self.jobs.lock().map_err(|_| anyhow::anyhow!("live job registry poisoned"))?.get(id).cloned().ok_or_else(|| anyhow::anyhow!("job is unknown or finished"))?;
        job.interrupt.store(true, Ordering::SeqCst);
        let active = job.active_tool.lock().unwrap_or_else(|e|e.into_inner()).clone();
        if let Some(tool) = active { tool.cancel_operation(); }
        Ok(json!({"id":id,"cancellation_requested":true}))
    }
}
struct CancelUi<'a> { inner: &'a mut dyn Ui, cancel: &'a AtomicBool }
impl Ui for CancelUi<'_> {
    fn approve(&mut self, tool:&str, summary:&str)->super::Decision { self.inner.approve_with_cancel(tool, summary, self.cancel) }
    fn approval_reason(&mut self)->Option<String> { self.inner.approval_reason() }
}
pub(super) struct LiveTasks { jobs: BTreeMap<String, JobState>, tx: mpsc::Sender<Event>, rx: mpsc::Receiver<Event>, next: u64, mailboxes: Mailboxes }
impl Default for LiveTasks { fn default() -> Self { let (tx,rx)=mpsc::channel(); Self { jobs:BTreeMap::new(), tx,rx,next:0, mailboxes:Arc::default() } } }
struct Bridge { tx: mpsc::Sender<Event>, interrupt: Arc<AtomicBool>, inbox: Arc<Mutex<Vec<String>>>, mailboxes: Mailboxes, active_tool: ActiveTool }
impl Approver for Bridge {
 fn authorize(&mut self, call:&ToolCall, summary:&str, needs:bool)->Result<(),Denial> {
  let (answer,rx)=mpsc::channel();
  if self.tx.send(Event::Approve { call:call.clone(), summary:summary.to_owned(), needs, cancel:self.interrupt.clone(), answer }).is_err() { return Err(Denial::User); }
  loop { if self.interrupt.load(Ordering::SeqCst) { return Err(Denial::User); } match rx.recv_timeout(Duration::from_millis(80)) { Ok(result)=>return result, Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(_)=>return Err(Denial::User) } }
 }
 fn notice(&mut self, message:&str) { let _=self.tx.send(Event::Notice(message.to_owned())); }
 fn add_cost(&mut self, cost:f64) { let _=self.tx.send(Event::Cost(cost)); }
 fn take_messages(&mut self)->Vec<String> { std::mem::take(&mut *self.inbox.lock().unwrap_or_else(|e|e.into_inner())) }
 fn live_coordination(&self) -> bool { true }
 fn tool_started(&mut self, name:&str, registry:&Registry) { *self.active_tool.lock().unwrap_or_else(|e|e.into_inner())=registry.get(name).cloned(); }
 fn tool_finished(&mut self) { *self.active_tool.lock().unwrap_or_else(|e|e.into_inner())=None; }
 fn peer_message(&mut self, args:&Value)->Result<Value,String> {
  let mailboxes=self.mailboxes.lock().map_err(|_|"peer registry poisoned")?;
  if args["action"]=="list" { return Ok(json!(mailboxes.keys().collect::<Vec<_>>())); }
  let id=args["id"].as_str().ok_or("message requires id")?;
  let text=args["message"].as_str().filter(|s|!s.trim().is_empty()&&s.len()<=32768).ok_or("message must be 1..32768 bytes")?;
  let job=mailboxes.get(id).ok_or("peer is unknown or finished")?;
  if job.finished.load(Ordering::SeqCst) { return Err("peer has finished".into()); }
  let mut inbox=job.inbox.lock().map_err(|_|"peer inbox poisoned")?; if inbox.len()>=32 { return Err("peer inbox is full".into()); }
  inbox.push(text.to_owned()); Ok(json!({"id":id,"delivered":true}))
 }
}
impl LiveTasks {
 pub fn control(&self) -> LiveTaskControl { LiveTaskControl { jobs:self.mailboxes.clone() } }
 pub fn start(&mut self, config:&Config, workspace:&Workspace, ext:&Registry, layout:Option<&Layout>, harness:&Harness, args:&Value)->Result<Value,String> {
  let request=subagents::parse(args,config)?;
  if self.jobs.values().filter(|j|j.status["status"]=="running").count()+request.jobs.len()>32 { return Err("maximum 32 live jobs; wait for completion first".into()); }
  while self.jobs.len() + request.jobs.len() > 256 {
   let oldest = self.jobs.iter().filter(|(_,j)|j.status["status"]!="running").min_by_key(|(id,_)|id.trim_start_matches("task-").parse::<u64>().unwrap_or(0)).map(|(id,_)|id.clone()).ok_or("live job history is full")?;
   self.jobs.remove(&oldest);
  }
  let mut result=Vec::new();
  // Validate every job before launching any. Each thread reparses its owned arguments.
  for job in &request.jobs {
   self.next+=1; let id=format!("task-{}",self.next);
   let task=json!({"prompt":job.prompt,"agent":job.kind,"max_steps":job.max_steps,"name":job.label});
   let mut task=task; if let Some(schema)=&job.schema { task["output_schema"]=schema.clone(); }
   let config=config.clone(); let role=config.for_role("subagent"); let workspace=workspace.clone(); let ext=ext.clone(); let layout=layout.cloned(); let harness=role.effective_harness(harness).map_err(|e|format!("subagent model profile: {e:#}"))?;
   let interrupt=Arc::new(AtomicBool::new(false)); let inbox=Arc::new(Mutex::new(Vec::new())); let cancel=interrupt.clone(); let messages=inbox.clone(); let tx=self.tx.clone(); let done_id=id.clone();
   let active_tool: ActiveTool=Arc::default(); let active=active_tool.clone();
   let control=Arc::new(ControlJob { inbox:inbox.clone(), interrupt:interrupt.clone(), active_tool:active_tool.clone(), finished:AtomicBool::new(false) });
   self.mailboxes.lock().map_err(|_|"peer registry poisoned")?.insert(id.clone(), control.clone());
   let mailboxes=self.mailboxes.clone();
   let worker=thread::Builder::new().name(format!("dial-{id}")).spawn(move || {
    let mut bridge=Bridge { tx:tx.clone(), interrupt:cancel.clone(), inbox:messages, mailboxes, active_tool:active };
    let outcome=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
     let request=match subagents::parse(&task,&config) { Ok(r)=>r,Err(e)=>return Outcome::failed(0,e) };
     let env=Env { config:&role, workspace:&workspace, ext:&ext, layout:layout.as_ref(), harness:&harness, interrupt:&cancel };
     match make_backend(&role) { Ok(mut backend)=> { let outcome=super::child::run(env,&request.jobs[0],&mut backend,None,&mut bridge); backend.release(); outcome }, Err(e)=>Outcome::failed(0,format!("{e:#}")) }
    })).unwrap_or_else(|_|Outcome::failed(0,"subagent worker panicked"));
    let (status,text)=match outcome.status { Status::Done(text)=>("done",text),Status::Failed(text)=>("failed",text),Status::Interrupted=>("cancelled","interrupted".to_owned()) };
    let mut value=json!({"id":done_id,"status":status,"steps":outcome.steps,"result":text});
    if let Some(store)=workspace.artifacts() { match store.retain(&value["result"].as_str().unwrap_or("")) { Ok(uri)=>value["artifact"]=json!(uri),Err(e)=>{ value["status"]=json!("failed"); value["artifact_error"]=json!(format!("{e:#}")); } } }
    control.finished.store(true,Ordering::SeqCst);
    let _=tx.send(Event::Done(done_id, value));
   }).map_err(|e|format!("starting live subagent: {e}"))?;
   let status=json!({"id":id,"status":"running","name":job.label,"agent":job.kind});
   self.jobs.insert(id.clone(),JobState { status:status.clone(),interrupt,worker:Some(worker),active_tool });
   result.push(status);
  }
  Ok(json!(result))
 }
 pub fn pump(&mut self, approval:Approval, read_only:bool, rules:&mut Vec<PermissionRule>, cost:&mut Option<f64>, ui:&mut dyn Ui) {
  while let Ok(event)=self.rx.try_recv() { match event {
   Event::Approve{call,summary,needs,cancel,answer}=> { let result=if cancel.load(Ordering::SeqCst) { Err(Denial::User) } else if read_only && needs { Err(Denial::ReadOnly) } else if approval::denied_by_rule(rules,&call) { Err(Denial::Rule) } else if needs { approval::authorize(approval,rules,&call,&summary,&mut CancelUi { inner:ui, cancel:&cancel }) } else { Ok(()) }; let _=answer.send(result); },
   Event::Notice(text)=>ui.background_notice(&text),
   Event::Cost(value)=>*cost=Some(cost.unwrap_or(0.0)+value),
   Event::Done(id,value)=> { self.mailboxes.lock().unwrap_or_else(|e|e.into_inner()).remove(&id); if let Some(job)=self.jobs.get_mut(&id) { job.status=value; ui.background_notice(&format!("{id} finished: {}",job.status["status"])); if let Some(worker)=job.worker.take() { let _=worker.join(); } } },
  } }
 }
 pub fn status(&self,id:Option<&str>)->Result<Value,String> { match id { Some(id)=>self.jobs.get(id).map(|j|j.status.clone()).ok_or_else(||format!("unknown job {id}")),None=> { let mut jobs:Vec<_>=self.jobs.iter().collect(); jobs.sort_by_key(|(id,_)|id.trim_start_matches("task-").parse::<u64>().unwrap_or(0)); Ok(json!(jobs.into_iter().map(|(_,j)|j.status.clone()).collect::<Vec<_>>())) } } }
 pub fn message(&self,id:&str,text:&str)->Result<Value,String> { self.control().send(id,text).map_err(|e|e.to_string()) }
 pub fn cancel(&self,id:&str)->Result<Value,String> { let job=self.jobs.get(id).ok_or_else(||format!("unknown job {id}"))?; job.interrupt.store(true,Ordering::SeqCst); if let Some(tool)=job.active_tool.lock().unwrap_or_else(|e|e.into_inner()).as_ref() { tool.cancel_operation(); } Ok(json!({"id":id,"cancellation_requested":true})) }
 pub fn cancel_all(&mut self) { for id in self.jobs.keys() { let _=self.cancel(id); } }
 pub fn wait(&mut self,id:Option<&str>,seconds:u64,approval:Approval,read_only:bool,rules:&mut Vec<PermissionRule>,cost:&mut Option<f64>,ui:&mut dyn Ui)->Result<Value,String> {
  self.status(id)?; let until=Instant::now()+Duration::from_secs(seconds.min(120));
  loop { self.pump(approval,read_only,rules,cost,ui); let pending=self.jobs.iter().any(|(name,j)|id.is_none_or(|id|id==name)&&j.status["status"]=="running"); if !pending||Instant::now()>=until { return self.status(id); } ui.heartbeat(); thread::sleep(Duration::from_millis(40)); }
 }
}
impl Drop for LiveTasks { fn drop(&mut self) { self.cancel_all(); for job in self.jobs.values_mut() { if let Some(worker)=job.worker.take() { let _=worker.join(); } } } }
pub(super) fn output(result:Result<Value,String>)->ToolOutput { match result { Ok(value)=>ToolOutput{text:value.to_string(),is_error:false},Err(text)=>ToolOutput{text,is_error:true} } }
pub(super) fn message_schema() -> Value { json!({"type":"function","function":{"name":"task_message","description":"Coordinate with active peer subagents: list job IDs or deliver a bounded steering message. This never starts a child, changes permissions or changes workspace files.","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["list","message"]},"id":{"type":"string"},"message":{"type":"string"}},"required":["action"],"additionalProperties":false}}}) }
