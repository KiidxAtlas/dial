//! A private, session-scoped live feed and its append-only side-pane follower.
use crate::config::paths::Layout;
use crate::platform::{atomic, fs as platform_fs, procinfo::ProcId};
use crate::ui::{
    images::{ImageProtocol, Thumbnail},
    text,
    theme::Theme,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

const MAX_EVENT: usize = 128 * 1024;
const MAX_FEED: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, clap::Args)]
pub struct WatchArgs {
    /// Session ID printed by /watch.
    pub session: String,
    /// Print the recorded feed and exit instead of following it.
    #[arg(long)]
    pub once: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Event {
    Text { title: String, body: String },
    Image { label: String, file: String },
}

/// The directory follows the session, not the shell's initial session ID.
pub(crate) fn directory(layout: &Layout, session: &str) -> Result<PathBuf> {
    ensure!(
        !session.is_empty()
            && session.len() <= 128
            && session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid live-feed session ID"
    );
    Ok(layout.state_dir.join("live").join(session))
}

fn image_name(file: &str) -> bool {
    file.strip_suffix(".image")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(crate) fn enabled(layout: &Layout, session: &str) -> bool {
    directory(layout, session).is_ok_and(|dir| dir.join("events.jsonl").is_file())
}

/// Human-readable preview only: changed paths come from execution facts, never prose.
pub(crate) fn tool_result(
    layout: &Layout,
    session: &str,
    name: &str,
    output: &crate::runtime::tool_output::ToolOutput,
) -> Result<()> {
    if !enabled(layout, session) {
        return Ok(());
    }
    let errors = crate::tools::code::lsp::new_errors_in(&output.text);
    if output.execution.changed_paths.is_empty() && errors.is_none() {
        return Ok(());
    }
    let paths = output
        .execution
        .changed_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut body = format!("{paths}\n{}", bounded(&output.text, 12 * 1024));
    if let Some((count, lines)) = errors {
        body.push_str(&format!(
            "\nLSP advisory, not verification: {count} new error(s)\n{}",
            lines.join("\n")
        ));
    }
    append(
        layout,
        session,
        Event::Text {
            title: format!(
                "{name} · {} changed path(s)",
                output.execution.changed_paths.len()
            ),
            body,
        },
    )
}

fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.into();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[preview truncated]", &value[..end])
}

pub(crate) fn append(layout: &Layout, session: &str, event: Event) -> Result<()> {
    let dir = directory(layout, session)?;
    atomic::private_dir(&dir)?;
    let path = dir.join("events.jsonl");
    let file = match platform_fs::open_nofollow_nonblocking(OpenOptions::new().append(true), &path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            platform_fs::open_owner_only(OpenOptions::new().append(true).create_new(true), &path)?
        }
        Err(error) => return Err(error.into()),
    };
    atomic::check_file(&file)?;
    fs2::FileExt::lock_exclusive(&file)?;
    ensure!(
        file.metadata()?.len() < MAX_FEED,
        "live feed reached its 16 MiB limit"
    );
    let event = match event {
        Event::Text { title, body } => Event::Text {
            title: bounded(&title, 1024),
            body: bounded(&body, 16 * 1024),
        },
        Event::Image { label, file } => Event::Image {
            label: bounded(&label, 1024),
            file,
        },
    };
    let mut bytes = serde_json::to_vec(&event)?;
    ensure!(bytes.len() < MAX_EVENT, "live-feed event exceeds 128 KiB");
    bytes.push(b'\n');
    ensure!(
        file.metadata()?.len() + bytes.len() as u64 <= MAX_FEED,
        "live feed reached its 16 MiB limit"
    );
    (&file).write_all(&bytes)?;
    Ok(())
}

pub(crate) fn attached(layout: &Layout, session: &str) -> bool {
    let Ok(dir) = directory(layout, session) else {
        return false;
    };
    atomic::read_private(&dir.join("viewer.json"))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<ProcId>(&bytes).ok())
        .is_some_and(|id| id.alive())
}

pub(crate) fn image(layout: &Layout, session: &str, reference: &serde_json::Value) -> Result<()> {
    let (bytes, _) = crate::llm::images::verified_bytes(reference)?;
    let sha = reference["sha256"]
        .as_str()
        .context("image has no identity")?;
    ensure!(
        sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid image identity"
    );
    let dir = directory(layout, session)?;
    atomic::private_dir(&dir)?;
    let used = std::fs::read_dir(&dir)?.try_fold(0_u64, |total, entry| -> Result<u64> {
        let entry = entry?;
        Ok(total
            + if image_name(&entry.file_name().to_string_lossy()) {
                entry.metadata()?.len()
            } else {
                0
            })
    })?;
    ensure!(
        used + bytes.len() as u64 <= 64 * 1024 * 1024,
        "live screenshots reached their 64 MiB limit"
    );
    let file = format!("{sha}.image");
    atomic::write_private(&dir.join(&file), &bytes)?;
    append(
        layout,
        session,
        Event::Image {
            label: reference["path"].as_str().unwrap_or("Screenshot").into(),
            file,
        },
    )
}

/// Called on the existing image worker so file publication never blocks terminal input.
pub(crate) fn preview_image(
    layout: &Layout,
    session: &str,
    reference: &serde_json::Value,
    native: bool,
) -> Result<(Option<Thumbnail>, Option<String>)> {
    let thumbnail = Thumbnail::load_with_native(reference, native)?;
    let mut warning = None;
    if enabled(layout, session) {
        match image(layout, session, reference) {
            Ok(()) if attached(layout, session) => return Ok((None, None)),
            Ok(()) => {}
            Err(error) => {
                warning = Some(format!(
                    "Could not send screenshot; keeping inline preview: {error:#}"
                ))
            }
        }
    }
    Ok((Some(thumbnail), warning))
}

pub fn run(layout: &Layout, args: WatchArgs) -> Result<i32> {
    let dir = directory(layout, &args.session)?;
    atomic::validate_private_path(&dir)?;
    let file = platform_fs::open_nofollow_nonblocking(
        OpenOptions::new().read(true),
        &dir.join("events.jsonl"),
    )
    .context("no live feed; run /watch in the session first")?;
    atomic::check_file(&file)?;
    let terminal = std::io::stdout().is_terminal();
    if !args.once && terminal {
        let id = ProcId::current().context("cannot identify watcher process")?;
        atomic::write_private(&dir.join("viewer.json"), &serde_json::to_vec(&id)?)?;
    }
    let protocol = terminal.then(ImageProtocol::detect).flatten();
    let mut reader = BufReader::new(file);
    let mut pending = Vec::new();
    let mut out = std::io::stdout().lock();
    loop {
        let Some(event) = next_event(&mut reader, &mut pending)? else {
            if args.once {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let width = crossterm::terminal::size().map_or(80, |(w, _)| w);
        match event {
            Event::Text { title, body } => {
                writeln!(out, "\n{}", text::one_line(&title))?;
                for line in text::sanitize(&body).lines() {
                    let color = if line.starts_with('+') {
                        "\x1b[32m"
                    } else if line.starts_with('-') {
                        "\x1b[31m"
                    } else {
                        ""
                    };
                    if terminal {
                        writeln!(out, "{color}{line}\x1b[0m")?;
                    } else {
                        writeln!(out, "{line}")?;
                    }
                }
            }
            Event::Image { label, file } => {
                ensure!(image_name(&file), "invalid live image path");
                let bytes =
                    atomic::read_private(&dir.join(&file))?.context("live image is missing")?;
                use sha2::{Digest, Sha256};
                ensure!(
                    file == format!("{:x}.image", Sha256::digest(&bytes)),
                    "live image identity mismatch"
                );
                let thumbnail = Thumbnail::from_bytes(&label, &bytes, protocol.is_some())?;
                writeln!(out, "\n{}", text::one_line(&label))?;
                let height = crossterm::terminal::size()
                    .map_or(24, |(_, h)| h)
                    .saturating_sub(3);
                if let Some((native, protocol)) =
                    protocol.and_then(|p| thumbnail.native_fitted(width, height).map(|n| (n, p)))
                {
                    let mut encoded = "\n".repeat(native.rows());
                    native.write_ansi(&mut encoded, protocol);
                    writeln!(out, "{encoded}")?;
                } else if terminal {
                    for row in
                        crate::ui::term::rows_to_ansi(&thumbnail.rows(width, &Theme::dark()), width)
                    {
                        writeln!(out, "{row}")?;
                    }
                } else {
                    writeln!(
                        out,
                        "[image {}x{}]",
                        thumbnail.dimensions().0,
                        thumbnail.dimensions().1
                    )?;
                }
            }
        }
        out.flush()?;
    }
    ensure!(pending.is_empty(), "incomplete live-feed event");
    Ok(0)
}

fn next_event(reader: &mut impl BufRead, pending: &mut Vec<u8>) -> Result<Option<Event>> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        let end = available
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(available.len());
        ensure!(
            pending.len() + end <= MAX_EVENT,
            "oversized live-feed event"
        );
        pending.extend_from_slice(&available[..end]);
        reader.consume(end);
        if pending.last() == Some(&b'\n') {
            let event = serde_json::from_slice(pending).context("invalid live-feed event")?;
            pending.clear();
            return Ok(Some(event));
        }
    }
}

#[cfg(test)]
mod tests;
