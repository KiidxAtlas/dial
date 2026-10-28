//! Platform clipboard image acquisition for the composer's image paste.
//!
//! Tool selection is pure ([`readers`]) so the platform matrix is testable without a display.
//! Every reader hands back PNG bytes within [`MAX_IMAGE_BYTES`]: other formats are decoded and
//! re-encoded, and oversized images are downscaled, so the attachment path never receives a
//! payload whose contents disagree with its `.png` file name or exceed the provider limit.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(test)]
mod tests;

use crate::llm::images::MAX_IMAGE_BYTES;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat, ImageReader, Limits, imageops::FilterType};
use std::{
    io::{Cursor, Read},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[cfg(any(target_os = "macos", test))]
use std::{
    io::{Seek, SeekFrom},
    path::Path,
};

const MIB: u64 = 1024 * 1024;
/// Largest raw clipboard payload held in memory before normalization.
const MAX_CLIPBOARD_BYTES: u64 = 64 * MIB;
/// Base64 output carries the payload at 4/3 size, plus line slack.
const MAX_BASE64_BYTES: u64 = MAX_CLIPBOARD_BYTES / 3 * 4 + 1024;
/// Longest side accepted before decoding, bounding decoder allocation.
const MAX_DIMENSION: u32 = 16_384;
const MAX_DOWNSCALE_STEPS: usize = 4;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Cold PowerShell start plus clipboard access can take several seconds.
const POWERSHELL_TIMEOUT: Duration = Duration::from_secs(8);

/// Image formats the clipboard path requests and can decode.
const SUPPORTED_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Reads the platform clipboard image and returns it as PNG within the attachment limit.
///
/// The error lists every reader tried and why it yielded nothing, so the user can tell a missing
/// tool from an empty clipboard.
pub(crate) fn read_image() -> Result<Vec<u8>> {
    let readers = readers(Environment::detect());
    first_image(&readers, |reader| {
        let Some(raw) = read_raw(reader)? else {
            return Ok(None);
        };
        normalize_png(&raw).map(Some)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Os {
    MacOs,
    Windows,
    Linux,
    Other,
}

/// Facts that decide which clipboard readers apply. Detected once; tool selection is pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Environment {
    os: Os,
    wsl: bool,
    termux: bool,
    wayland: bool,
    x11: bool,
}

impl Environment {
    fn detect() -> Self {
        let set = |key: &str| std::env::var_os(key).is_some_and(|value| !value.is_empty());
        let os = if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else if cfg!(any(target_os = "linux", target_os = "android")) {
            Os::Linux
        } else {
            Os::Other
        };
        Self {
            os,
            wsl: cfg!(target_os = "linux")
                && (set("WSL_DISTRO_NAME") || set("WSL_INTEROP") || kernel_is_wsl()),
            termux: cfg!(target_os = "android") || set("TERMUX_VERSION"),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
        }
    }
}

fn kernel_is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reader {
    MacPasteboard,
    MacFileUrls,
    PowerShell,
    WlPaste,
    Xclip,
    Termux,
}

impl Reader {
    fn name(self) -> &'static str {
        match self {
            Reader::MacPasteboard => "macOS pasteboard",
            Reader::MacFileUrls => "Finder file URLs",
            Reader::PowerShell => "powershell.exe",
            Reader::WlPaste => "wl-paste",
            Reader::Xclip => "xclip",
            Reader::Termux => "termux",
        }
    }
}

/// Readers to try, in order, for `env`. The first reader with an image wins.
fn readers(env: Environment) -> Vec<Reader> {
    match env.os {
        Os::MacOs => vec![Reader::MacPasteboard, Reader::MacFileUrls],
        Os::Windows => vec![Reader::PowerShell],
        Os::Linux => linux_readers(env),
        Os::Other => Vec::new(),
    }
}

fn linux_readers(env: Environment) -> Vec<Reader> {
    let mut readers = Vec::new();
    if env.termux {
        // Termux:X11 provides a real X server; otherwise only the text-only Termux API exists.
        if env.x11 {
            readers.push(Reader::Xclip);
        }
        readers.push(Reader::Termux);
        return readers;
    }
    // WSL reaches the Windows clipboard through interop, which holds the real image; the WSLg
    // Wayland/X11 bridges are fallbacks.
    if env.wsl {
        readers.push(Reader::PowerShell);
    }
    // Without a display variable, try both so setups that export neither still work.
    let display_known = env.wayland || env.x11;
    if env.wayland || !display_known {
        readers.push(Reader::WlPaste);
    }
    if env.x11 || !display_known {
        readers.push(Reader::Xclip);
    }
    readers
}

fn read_raw(reader: Reader) -> Result<Option<Vec<u8>>> {
    match reader {
        #[cfg(target_os = "macos")]
        Reader::MacPasteboard => macos::pasteboard_image(),
        #[cfg(target_os = "macos")]
        Reader::MacFileUrls => macos::finder_image(),
        #[cfg(not(target_os = "macos"))]
        Reader::MacPasteboard | Reader::MacFileUrls => {
            bail!("macOS clipboard is unavailable on this platform")
        }
        Reader::PowerShell => powershell_image(),
        Reader::WlPaste => mime_image("wl-paste", wl_paste_args),
        Reader::Xclip => mime_image("xclip", xclip_args),
        Reader::Termux => {
            bail!("termux-clipboard-get exposes text only; Termux has no clipboard image API")
        }
    }
}

/// Returns the first image from `readers`, or an error naming every reader and its outcome.
fn first_image(
    readers: &[Reader],
    mut attempt: impl FnMut(Reader) -> Result<Option<Vec<u8>>>,
) -> Result<Vec<u8>> {
    if readers.is_empty() {
        bail!("clipboard image paste is not supported on this platform");
    }
    let mut tried = Vec::with_capacity(readers.len());
    for &reader in readers {
        match attempt(reader) {
            Ok(Some(bytes)) => return Ok(bytes),
            Ok(None) => tried.push(format!("{}: no image", reader.name())),
            Err(error) => tried.push(format!("{}: {error:#}", reader.name())),
        }
    }
    bail!("no clipboard image found (tried {})", tried.join("; "))
}

/// Converts clipboard bytes into PNG no larger than the attachment limit.
fn normalize_png(bytes: &[u8]) -> Result<Vec<u8>> {
    normalize_within(bytes, MAX_IMAGE_BYTES)
}

fn normalize_within(bytes: &[u8], limit: u64) -> Result<Vec<u8>> {
    ensure!(!bytes.is_empty(), "clipboard image is empty");
    check_size(bytes.len())?;
    let format = image::guess_format(bytes)
        .context("clipboard data is not a PNG, JPEG, GIF or WebP image")?;
    ensure!(
        supported_format(format),
        "clipboard image is {format:?}, which Dial cannot decode"
    );
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .context("clipboard image header is unreadable")?;
    ensure!(
        width <= MAX_DIMENSION && height <= MAX_DIMENSION,
        "clipboard image is {width}x{height}; the limit is {MAX_DIMENSION} pixels per side"
    );
    // Decoding validates the pixel data, so a truncated payload fails here rather than at
    // provider upload. PNG within the limit keeps its original bytes.
    let image = decode(bytes, format)?;
    if format == ImageFormat::Png && bytes.len() as u64 <= limit {
        return Ok(bytes.to_vec());
    }
    encode_within(image, limit)
}

fn supported_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    )
}

fn check_size(len: usize) -> Result<()> {
    ensure!(
        len as u64 <= MAX_CLIPBOARD_BYTES,
        "clipboard image is {len} bytes; the limit is {MAX_CLIPBOARD_BYTES} bytes"
    );
    Ok(())
}

fn decode(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    reader.limits(limits);
    reader
        .decode()
        .context("clipboard image could not be decoded")
}

/// Encodes `image` as PNG, downscaling until it fits `limit` bytes.
fn encode_within(mut image: DynamicImage, limit: u64) -> Result<Vec<u8>> {
    for _ in 0..MAX_DOWNSCALE_STEPS {
        let png = encode_png(&image)?;
        if png.len() as u64 <= limit {
            return Ok(png);
        }
        // PNG size tracks pixel count, so each side scales by the square root of the byte
        // ratio, with headroom for compression variance.
        let ratio = (limit as f64 / png.len() as f64).sqrt() * 0.9;
        let width = scaled_side(image.width(), ratio);
        let height = scaled_side(image.height(), ratio);
        image = image.resize_exact(width, height, FilterType::Triangle);
    }
    let png = encode_png(&image)?;
    ensure!(
        png.len() as u64 <= limit,
        "clipboard image is {} bytes as PNG even after downscaling; the limit is {limit} bytes",
        png.len()
    );
    Ok(png)
}

fn scaled_side(side: u32, ratio: f64) -> u32 {
    ((f64::from(side) * ratio).floor() as u32).max(1)
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    image
        .write_to(&mut out, ImageFormat::Png)
        .context("PNG encoding failed")?;
    Ok(out.into_inner())
}

/// Reads the image from the Finder file-URL representation, a file path rather than pixel data.
#[cfg(any(target_os = "macos", test))]
fn file_url_paths(urls: &[String]) -> Vec<std::path::PathBuf> {
    urls.iter()
        .filter_map(|raw| url::Url::parse(raw).ok())
        .filter_map(|url| url.to_file_path().ok())
        .collect()
}

/// Reads a regular file that sniffs as an image. Non-image files yield `None` so a Finder
/// selection can contain other files alongside the image.
#[cfg(any(target_os = "macos", test))]
fn image_file(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut head = Vec::new();
    (&mut file).take(32).read_to_end(&mut head)?;
    if image::guess_format(&head).is_err() {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_CLIPBOARD_BYTES + 1).read_to_end(&mut bytes)?;
    check_size(bytes.len())?;
    Ok(Some(bytes))
}

#[derive(Debug)]
struct Captured {
    success: bool,
    stdout: Vec<u8>,
}

/// Runs `program` and captures up to `limit` bytes of stdout. `Ok(None)` means it is not installed.
fn capture(
    program: &str,
    args: &[&str],
    limit: u64,
    timeout: Duration,
) -> Result<Option<Captured>> {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!("{program} failed to start")));
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        bail!("{program}: stdout was not captured");
    };
    // Drain on a thread so a large payload cannot stall the child on a full pipe.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{program} timed out after {}s", timeout.as_secs());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                return Err(
                    anyhow::Error::new(error).context(format!("waiting for {program} failed"))
                );
            }
        }
    };
    let stdout = reader
        .join()
        .map_err(|_| anyhow::anyhow!("{program} output reader panicked"))?
        .with_context(|| format!("reading {program} output failed"))?;
    ensure!(
        stdout.len() as u64 <= limit,
        "{program} returned more data than the clipboard limit allows"
    );
    Ok(Some(Captured {
        success: status.success(),
        stdout,
    }))
}

fn wl_paste_args(mime: &str) -> Vec<&str> {
    vec!["--no-newline", "--type", mime]
}

fn xclip_args(mime: &str) -> Vec<&str> {
    vec!["-selection", "clipboard", "-t", mime, "-o"]
}

/// Tries each supported MIME type in order. A tool that exits non-zero for a missing type simply
/// does not have that type; only failures to run it are errors.
fn mime_image(program: &str, args: fn(&str) -> Vec<&str>) -> Result<Option<Vec<u8>>> {
    for mime in SUPPORTED_MIME_TYPES {
        let argv = args(mime);
        let Some(captured) = capture(program, &argv, MAX_CLIPBOARD_BYTES, COMMAND_TIMEOUT)? else {
            bail!("not installed");
        };
        if captured.success && !captured.stdout.is_empty() {
            return Ok(Some(captured.stdout));
        }
    }
    Ok(None)
}

// PowerShell one-liner that emits the Windows clipboard image as base64 PNG on stdout, or
// nothing when the clipboard holds no image. `-Sta` is required for clipboard access.
const POWERSHELL_IMAGE_SCRIPT: &str = "\
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$img = [System.Windows.Forms.Clipboard]::GetImage()
if ($null -ne $img) {
    $stream = New-Object System.IO.MemoryStream
    $img.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    [Console]::Out.Write([Convert]::ToBase64String($stream.ToArray()))
}";

/// Reads the host clipboard through `powershell.exe`: native Windows, or WSL via interop.
fn powershell_image() -> Result<Option<Vec<u8>>> {
    // `-EncodedCommand` takes UTF-16LE base64, which avoids shell quoting of the script.
    let encoded = STANDARD.encode(script_utf16le(POWERSHELL_IMAGE_SCRIPT));
    let Some(captured) = capture(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Sta",
            "-EncodedCommand",
            encoded.as_str(),
        ],
        MAX_BASE64_BYTES,
        POWERSHELL_TIMEOUT,
    )?
    else {
        bail!("not installed");
    };
    ensure!(captured.success, "powershell.exe exited with an error");
    parse_powershell_png(&captured.stdout)
}

fn script_utf16le(script: &str) -> Vec<u8> {
    script.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn parse_powershell_png(stdout: &[u8]) -> Result<Option<Vec<u8>>> {
    let text = std::str::from_utf8(stdout)
        .context("powershell.exe returned non-text output")?
        .trim();
    if text.is_empty() {
        return Ok(None);
    }
    STANDARD
        .decode(text)
        .map(Some)
        .context("powershell.exe returned malformed base64 image data")
}
