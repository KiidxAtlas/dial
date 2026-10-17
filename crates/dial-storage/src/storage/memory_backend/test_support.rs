use super::*;

impl Operation {
    pub(crate) fn for_test(
        authority: MemoryAuthority,
        scope: BankScope,
        deadline: Option<std::time::Instant>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        Self {
            authority,
            cancel: cancel.clone(),
            owner_cancel: Arc::new(AtomicBool::new(false)),
            scope,
            epoch: Arc::new(AtomicU64::new(0)),
            generation: 0,
            live_cancel: cancel,
            deadline,
            observation_start: None,
        }
    }
}

pub(crate) fn authority(session: &str) -> MemoryAuthority {
    MemoryAuthority {
        session_id: session.into(),
        branch_id: format!("{session}:history:0"),
        revision: "1".into(),
        task_depth: 0,
    }
}

pub(crate) fn scope() -> BankScope {
    BankScope {
        retain_bank: "bank".into(),
        recall_banks: vec!["bank".into()],
        global_bank: None,
        project: "/project".into(),
    }
}

/// A model runner that answers every completion with `reply` and records each system prompt.
pub(crate) struct FakeRunner {
    pub reply: String,
    pub systems: Vec<String>,
}
impl FakeRunner {
    pub(crate) fn new(reply: &str) -> Self {
        Self {
            reply: reply.into(),
            systems: Vec::new(),
        }
    }
}
impl MemoryModelRunner for FakeRunner {
    fn limits(
        &mut self,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: u32,
        _: &Operation,
    ) -> Result<MemoryModelLimits> {
        Ok(MemoryModelLimits {
            context_window: 32768,
            max_output_tokens: 1024,
            max_input_bytes: 1 << 20,
        })
    }
    fn complete(&mut self, request: MemoryModelRequest) -> Result<String> {
        self.systems.push(request.system);
        Ok(self.reply.clone())
    }
    fn authorize_external(&mut self, _: &Operation, _: &str) -> Result<u64> {
        Ok(1)
    }
    fn external_started(&mut self, _: &Operation, _: u64) -> Result<()> {
        Ok(())
    }
    fn account_external(&mut self, _: &Operation, _: u64, _: &Value, _: bool) -> Result<()> {
        Ok(())
    }
    fn abandon_external(&mut self, _: &Operation, _: u64) -> Result<()> {
        Ok(())
    }
    fn authorize_managed_write(&mut self, _: &Operation, _: &[std::path::PathBuf]) -> Result<()> {
        Ok(())
    }
}
