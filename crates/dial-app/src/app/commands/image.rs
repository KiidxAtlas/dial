//! `/image generate <prompt>` and `/image edit <file> <prompt>`: run `image_gen` with the typed
//! prompt directly, without a model turn. Approval, plan mode and mutation authority are the
//! agent's (`Agent::generate_image`); the output is a new file named from the prompt (in the
//! workspace root, or beside the edited source), never an existing one, and an edited source is
//! only read.
use super::CommandSpec;
use crate::agent::Agent;
use crate::app::{App, Bridge, PAINT_EVERY, Shell, Step};
use crate::storage::redact::redact_text;
use crate::tools::net::image_gen;
use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

const USAGE: &str =
    "Usage: /image generate <prompt> · /image edit <file> <prompt> · /image settings";
/// Prompt words used in the output file name, and that name's maximum slug length.
const SLUG_WORDS: usize = 6;
const SLUG_CHARS: usize = 48;

pub(super) const COMMANDS: &[CommandSpec] = &[CommandSpec {
    name: "image",
    description: "generate or edit an image with image_gen (asks for approval) or tune the model",
    args_hint: "generate <prompt> | edit <file> <prompt> | settings",
    subcommands: &["generate", "edit", "settings"],
}];

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if name != "image" {
        return None;
    }
    let Some(request) = parse(args) else {
        shell.note("Image", USAGE);
        return Some(Ok(Step::Done));
    };
    Some(generate(shell, app, agent, request))
}

/// A parsed `/image generate` or `/image edit` request.
#[derive(Debug, PartialEq)]
struct ImageRequest<'a> {
    prompt: &'a str,
    /// The workspace image to edit; `None` for `generate`.
    source: Option<&'a str>,
}

/// `generate <prompt>` or `edit <file> <prompt>`, or `None` for any other or incomplete form.
fn parse(args: &str) -> Option<ImageRequest<'_>> {
    let (action, rest) = args.trim().split_once(char::is_whitespace)?;
    let rest = rest.trim();
    match action {
        "generate" => (!rest.is_empty()).then_some(ImageRequest {
            prompt: rest,
            source: None,
        }),
        "edit" => {
            let (source, prompt) = rest.split_once(char::is_whitespace)?;
            let prompt = prompt.trim();
            (!prompt.is_empty()).then_some(ImageRequest {
                prompt,
                source: Some(source),
            })
        }
        _ => None,
    }
}

fn generate(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    request: ImageRequest<'_>,
) -> Result<Step> {
    let ImageRequest { prompt, source } = request;
    let extension = match image_gen::output_extension(agent.effective_config()) {
        Ok(extension) => extension,
        Err(error) => {
            shell.note("Image", redact_text(&format!("{error:#}")));
            return Ok(Step::Done);
        }
    };
    let output = output_name(agent.workspace().root(), prompt, extension, source);
    let typed = match source {
        Some(source) => format!("/image edit {source} {prompt}"),
        None => format!("/image generate {prompt}"),
    };
    app.interrupt.store(false, Ordering::SeqCst);
    shell.transcript.push_user(&typed);
    shell.composer.set_busy(true);
    let result = {
        let mut bridge = Bridge {
            app,
            shell: &mut *shell,
            interrupt: app.interrupt.clone(),
            started: Instant::now(),
            last_paint: Instant::now() - PAINT_EVERY,
            failed: false,
            pending: Vec::new(),
            next_id: 0,
        };
        bridge.repaint(true);
        agent.generate_image(prompt, &output, source, &mut bridge)
    };
    shell.composer.set_busy(false);
    shell.transcript.finish_turn();
    match result {
        // Like `!cmd`, the result joins the conversation so the model can refer to the image.
        Ok(output) => {
            let ran = match source {
                Some(source) => format!("`/image edit` on {source:?}"),
                None => "`/image generate`".to_owned(),
            };
            agent.session_mut().append(json!({
                "role": "user",
                "content": format!(
                    "I ran {ran} myself with the prompt {prompt:?}:\n{}",
                    output.text
                ),
            }))?;
        }
        Err(error) => shell.note("Image", redact_text(&format!("{error:#}"))),
    }
    Ok(Step::Done)
}

/// The new image's workspace-relative path. `generate` writes `image-<first prompt words>.<ext>`
/// in the workspace root; `edit` writes `<source stem>-<first prompt words>.<ext>` beside the
/// source. A taken name is numbered (`-2`, `-3`, ...), so no existing file is ever the target.
fn output_name(root: &Path, prompt: &str, extension: &str, source: Option<&str>) -> String {
    let slug = prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(SLUG_WORDS)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(SLUG_CHARS).collect();
    let slug = slug.trim_end_matches('-');
    let (dir, base) = match source.map(Path::new) {
        Some(path) => (
            path.parent().map(Path::to_path_buf).unwrap_or_default(),
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| !stem.is_empty())
                .unwrap_or("image"),
        ),
        None => (PathBuf::new(), "image"),
    };
    let stem = if slug.is_empty() {
        base.to_owned()
    } else {
        format!("{base}-{slug}")
    };
    (1usize..)
        .map(|n| {
            dir.join(match n {
                1 => format!("{stem}.{extension}"),
                n => format!("{stem}-{n}.{extension}"),
            })
        })
        .find(|name| !root.join(name).exists())
        .expect("an unused numbered name exists")
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests;
