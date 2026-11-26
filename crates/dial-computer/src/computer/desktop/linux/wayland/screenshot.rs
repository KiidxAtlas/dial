//! Read-only desktop capture when the optional PipeWire client is not linked.
use std::{
    fs::OpenOptions, io::BufReader, os::unix::fs::OpenOptionsExt, path::Path, time::Duration,
};

use ashpd::desktop::screenshot::Screenshot;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage};

use crate::computer::desktop::{
    control,
    error::{CoreResult, DesktopError},
};

pub(super) fn capture() -> CoreResult<RgbaImage> {
    control::check()?;
    let shot = super::portal::portal_runtime()?.block_on(async {
        super::libei::cancellable(tokio::time::timeout(Duration::from_secs(30), async {
            Screenshot::request()
                .interactive(true)
                .send()
                .await?
                .response()
        }))
        .await?
        .map_err(|_| DesktopError::capture_failed("Wayland Screenshot portal timed out"))?
        .map_err(|e| DesktopError::capture_failed(format!("Wayland Screenshot portal: {e}")))
    })?;
    let path = shot.uri().to_file_path().map_err(|_| {
        DesktopError::capture_failed("Wayland Screenshot portal returned a non-local URI")
    })?;
    // This is the portal's result, not a caller-supplied path. Do not remove a file
    // managed by the portal/compositor or chosen by the user in its consent dialog.
    load_image(&path)
}

fn load_image(path: &Path) -> CoreResult<RgbaImage> {
    control::check()?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| DesktopError::capture_failed(format!("opening portal screenshot: {e}")))?;
    let meta = file.metadata().map_err(|e| {
        DesktopError::capture_failed(format!("reading portal screenshot metadata: {e}"))
    })?;
    if !meta.is_file() || meta.len() > 64 * 1024 * 1024 {
        return Err(DesktopError::capture_failed(
            "portal screenshot is not a regular image file of at most 64 MiB",
        ));
    }
    let mut reader = ImageReader::with_format(BufReader::new(file), ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decode_error =
        |e| DesktopError::capture_failed(format!("decoding portal screenshot PNG: {e}"));
    let decoder = reader.into_decoder().map_err(decode_error)?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > 32 * 1024 * 1024 || decoder.total_bytes() > 128 * 1024 * 1024 {
        return Err(DesktopError::capture_failed(
            "portal screenshot exceeds the decoded image limit",
        ));
    }
    let image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    control::check()?;
    Ok(image.into_rgba8())
}

#[cfg(test)]
mod tests;
