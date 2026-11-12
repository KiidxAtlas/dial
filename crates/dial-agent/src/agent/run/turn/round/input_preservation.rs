//! Compare native source metadata after opaque execution; retained bytes never authorize writes.
use super::*;

impl Agent<'_> {
    pub(super) fn note_preserved_input_changes(&mut self, output: &mut ToolOutput) -> Result<()> {
        let mut state = self.session.control_state();
        let Some(files) = state["preserved_inputs"]["files"].as_array().cloned() else {
            return Ok(());
        };
        let mut notices = Vec::new();
        for file in files {
            let (Some(path), Some(original)) = (file["path"].as_str(), file["bytes"].as_u64())
            else {
                continue;
            };
            let current = self
                .workspace
                .resolve_read(path)
                .ok()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|meta| meta.len());
            let reason = match current {
                None => "unavailable".to_owned(),
                Some(bytes) if bytes < original => {
                    format!("shrank from {original} to {bytes} bytes")
                }
                _ => continue,
            };
            if state["preserved_input_warnings"][path].as_str() == Some(&reason) {
                continue;
            }
            state["preserved_input_warnings"][path] = json!(reason);
            notices.push(json!({"path":path,"change":reason,"original":file["artifact"]}));
        }
        if !notices.is_empty() {
            self.session.set_control_state(state)?;
            output.text.push_str(&format!("\n<dial-notice kind=\"input_evidence_changed\">\nOriginal task inputs changed: {}. Retained artifacts contain the actual original bytes; inspect them before reconstruction and restore only through admitted tools. To restore exact bytes without copying them through your reply, run one eval cell: `await dial.writeBytes(path, await dial.readBytes(original))` (Python: `dial.writeBytes(path, dial.readBytes(original))`); writeBytes only creates, so move a damaged file aside first, and work on a copy if a tool may consume the original again. Source access and destination writes still need their normal permissions. If evidence cannot be recovered, report that limitation rather than fabricate data.\n</dial-notice>", json!(notices)));
        }
        Ok(())
    }
}
