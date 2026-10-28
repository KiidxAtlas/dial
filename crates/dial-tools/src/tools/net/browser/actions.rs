//! Browser pointer, keyboard, element-query and conditional `wait` actions: pure argument
//! validation. Execution lives in `actions.cjs`, which runs inside the driver with Playwright.
//! Nothing here touches Chromium, so every function is unit-testable.

use serde_json::Value;

/// Actions owned end to end by `actions.cjs`. `wait` joins them only with a condition (see
/// [`is_page_action`]).
pub const ACTIONS: &[&str] = &[
    "scroll",
    "dblclick",
    "drag",
    "click_at",
    "mouse_move",
    "mouse_down",
    "mouse_up",
    "wheel",
    "key_down",
    "key_up",
    "highlight",
    "text",
    "html",
    "value",
    "attr",
    "count",
    "box",
    "is_visible",
    "is_enabled",
    "is_checked",
    "styles",
];
/// Driver module source, inlined next to the driver by `driver_source`.
pub const SOURCE: &str = include_str!("actions.cjs");
/// Largest accepted selector or drag target, in bytes.
pub const SELECTOR_MAX_BYTES: usize = 2048;
/// Largest accepted URL or response substring for `wait`, in bytes.
pub const PATTERN_MAX_BYTES: usize = 2048;
/// Largest accepted `wait` text, in bytes.
pub const TEXT_MAX_BYTES: usize = 4096;
/// Largest accepted keyboard key name, in bytes.
pub const KEY_MAX_BYTES: usize = 64;
/// Largest accepted attribute name, in bytes.
pub const ATTRIBUTE_MAX_BYTES: usize = 256;
/// Most computed-style properties one `styles` call may request.
pub const STYLE_PROPERTIES_MAX: usize = 32;
/// Largest accepted computed-style property name, in bytes.
pub const STYLE_PROPERTY_MAX_BYTES: usize = 64;
/// Largest absolute viewport coordinate or wheel delta, in CSS pixels.
pub const COORDINATE_LIMIT: f64 = 100_000.0;
/// Longest accepted `highlight` overlay lifetime, in milliseconds.
pub const HIGHLIGHT_MAX_MS: u64 = 10_000;

/// Whether this module owns the request. `wait` is owned only with a url, text or response
/// condition; a selector-only wait keeps its existing path.
pub fn is_page_action(action: &str, args: &Value) -> bool {
    ACTIONS.contains(&action) || (action == "wait" && has_wait_condition(args))
}

/// Validate the arguments of an action owned by this module. Other actions pass through unchanged.
pub fn validate(action: &str, args: &Value) -> Result<(), String> {
    match action {
        "scroll" => {
            selector(args, false)?;
            delta(args)?;
        }
        "wheel" => delta(args)?,
        "dblclick" | "value" | "count" | "box" | "is_visible" | "is_enabled" | "is_checked" => {
            selector(args, true)?
        }
        "text" | "html" => selector(args, false)?,
        "attr" => {
            selector(args, true)?;
            required_text(args, "name", ATTRIBUTE_MAX_BYTES)?;
        }
        "styles" => {
            selector(args, true)?;
            style_properties(args)?;
        }
        "highlight" => {
            selector(args, true)?;
            duration(args)?;
        }
        "drag" => {
            selector(args, true)?;
            required_text(args, "target", SELECTOR_MAX_BYTES)?;
        }
        "click_at" => {
            coordinate(args, "x")?;
            coordinate(args, "y")?;
            button(args)?;
        }
        "mouse_move" => {
            coordinate(args, "x")?;
            coordinate(args, "y")?;
        }
        "mouse_down" | "mouse_up" => button(args)?,
        "key_down" | "key_up" => {
            required_text(args, "key", KEY_MAX_BYTES)?;
        }
        "wait" if has_wait_condition(args) => wait(args)?,
        _ => {}
    }
    Ok(())
}

fn has_wait_condition(args: &Value) -> bool {
    ["url", "text", "response"]
        .iter()
        .any(|key| present(args, key).is_some())
}

fn wait(args: &Value) -> Result<(), String> {
    let url = present(args, "url").is_some();
    let response = present(args, "response").is_some();
    let text = present(args, "text").is_some();
    if url && response {
        return Err("wait accepts url or response, not both".into());
    }
    if url || response {
        if text || present(args, "selector").is_some() || present(args, "state").is_some() {
            return Err("wait url/response conditions take no selector, text or state".into());
        }
        let (key, max) = if url {
            ("url", PATTERN_MAX_BYTES)
        } else {
            ("response", PATTERN_MAX_BYTES)
        };
        required_text(args, key, max)?;
        return Ok(());
    }
    required_text(args, "text", TEXT_MAX_BYTES)?;
    selector(args, false)?;
    if present(args, "state").is_some() {
        return Err("state applies only to a selector wait".into());
    }
    Ok(())
}

fn present<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|value| !value.is_null())
}

fn text_arg<'a>(args: &'a Value, key: &str, max_bytes: usize) -> Result<Option<&'a str>, String> {
    let Some(value) = present(args, key) else {
        return Ok(None);
    };
    let text = value
        .as_str()
        .ok_or_else(|| format!("{key} must be a string"))?;
    if text.is_empty() {
        return Err(format!("{key} must not be empty"));
    }
    if text.len() > max_bytes {
        return Err(format!("{key} exceeds {max_bytes} bytes"));
    }
    Ok(Some(text))
}

fn required_text<'a>(args: &'a Value, key: &str, max_bytes: usize) -> Result<&'a str, String> {
    text_arg(args, key, max_bytes)?.ok_or_else(|| format!("{key} is required"))
}

fn selector(args: &Value, required: bool) -> Result<(), String> {
    if required {
        required_text(args, "selector", SELECTOR_MAX_BYTES)?;
    } else {
        text_arg(args, "selector", SELECTOR_MAX_BYTES)?;
    }
    Ok(())
}

fn number(args: &Value, key: &str) -> Result<Option<f64>, String> {
    let Some(value) = present(args, key) else {
        return Ok(None);
    };
    value
        .as_f64()
        .filter(|number| number.is_finite() && number.abs() <= COORDINATE_LIMIT)
        .map(Some)
        .ok_or_else(|| format!("{key} must be a number within ±{COORDINATE_LIMIT}"))
}

fn coordinate(args: &Value, key: &str) -> Result<(), String> {
    number(args, key)?.ok_or_else(|| format!("{key} is required"))?;
    Ok(())
}

fn delta(args: &Value) -> Result<(), String> {
    let dx = number(args, "dx")?;
    let dy = number(args, "dy")?;
    if dx.is_none() && dy.is_none() {
        return Err("dx or dy is required".into());
    }
    Ok(())
}

fn button(args: &Value) -> Result<(), String> {
    match present(args, "button") {
        None => Ok(()),
        Some(value) => match value.as_str() {
            Some("left" | "right" | "middle") => Ok(()),
            _ => Err("button must be left, right or middle".into()),
        },
    }
}

fn duration(args: &Value) -> Result<(), String> {
    let Some(value) = present(args, "duration") else {
        return Ok(());
    };
    value
        .as_u64()
        .filter(|ms| *ms <= HIGHLIGHT_MAX_MS)
        .map(|_| ())
        .ok_or_else(|| format!("duration must be an integer 0..={HIGHLIGHT_MAX_MS} ms"))
}

fn style_properties(args: &Value) -> Result<(), String> {
    let Some(value) = present(args, "props") else {
        return Ok(());
    };
    let props = value
        .as_array()
        .ok_or("props must be an array of CSS property names")?;
    if props.is_empty() || props.len() > STYLE_PROPERTIES_MAX {
        return Err(format!(
            "props must contain 1..={STYLE_PROPERTIES_MAX} names"
        ));
    }
    for prop in props {
        prop.as_str()
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= STYLE_PROPERTY_MAX_BYTES
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or("props entries must be CSS property names")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
