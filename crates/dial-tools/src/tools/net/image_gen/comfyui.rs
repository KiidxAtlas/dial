//! ComfyUI executes an explicitly configured API-format workflow, never a guessed node graph.
use super::{Request, Target, native::Client};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(super) fn request(
    client: &Client<'_>,
    target: &Target,
    request: &Request<'_>,
) -> Result<Vec<u8>> {
    ensure!(
        request.source.is_none(),
        "ComfyUI image edits require an image-upload workflow and are not supported"
    );
    let settings = target.settings.comfyui.as_ref().context(
        "ComfyUI requires image.models.<key>.comfyui with an API-format workflow, prompt_node and output_node",
    )?;
    ensure!(
        request.size == "auto",
        "ComfyUI dimensions are controlled by its workflow; omit size or choose auto"
    );
    let mut workflow = settings.workflow.clone();
    let text = workflow
        .get_mut(&settings.prompt_node)
        .and_then(|node| node.get_mut("inputs"))
        .and_then(|inputs| inputs.get_mut("text"))
        .context("ComfyUI prompt_node must identify a node with inputs.text")?;
    ensure!(
        text.is_string(),
        "ComfyUI prompt node inputs.text must be a string"
    );
    *text = Value::String(request.prompt.to_owned());
    ensure!(
        workflow.get(&settings.output_node).is_some(),
        "ComfyUI output_node is not in the workflow"
    );
    let submitted = client.post(&client.endpoint("prompt"), &json!({"prompt":workflow}))?;
    if let Some(errors) = submitted.get("node_errors").and_then(Value::as_object) {
        ensure!(
            errors.is_empty(),
            "ComfyUI rejected workflow nodes; inspect the configured workflow and installed node types"
        );
    }
    let id = submitted["prompt_id"]
        .as_str()
        .context("ComfyUI returned no prompt_id")?;
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "ComfyUI returned an invalid prompt_id"
    );
    let outcome = (|| {
        loop {
            let history = client.get(&client.endpoint(&format!("history/{id}")))?;
            if let Some(entry) = history.get(id) {
                ensure!(
                    entry["status"]["status_str"] != "error",
                    "ComfyUI workflow failed; inspect its execution history"
                );
                let images = entry["outputs"][&settings.output_node]["images"]
                    .as_array()
                    .context("ComfyUI output node produced no images")?;
                ensure!(
                    images.len() == 1,
                    "ComfyUI workflow must produce exactly one image at output_node"
                );
                let image = &images[0];
                let filename = image["filename"]
                    .as_str()
                    .context("ComfyUI output has no filename")?;
                let subfolder = image["subfolder"].as_str().unwrap_or_default();
                let kind = image["type"]
                    .as_str()
                    .context("ComfyUI output has no folder type")?;
                ensure!(
                    !filename.is_empty() && matches!(kind, "output" | "temp"),
                    "ComfyUI returned an invalid image location"
                );
                let query = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("filename", filename)
                    .append_pair("subfolder", subfolder)
                    .append_pair("type", kind)
                    .finish();
                return client.download(&client.endpoint(&format!("view?{query}")));
            }
            client.pause()?;
        }
    })();
    outcome.with_context(|| {
        format!("ComfyUI prompt {id}; stopping the wait does not cancel the remote workflow")
    })
}

#[cfg(test)]
mod tests;
