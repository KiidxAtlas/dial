//! Emulation options for the browser tool: viewport, device presets, geolocation, offline mode,
//! colour scheme, timezone, locale and user agent. Chromium applies them in the driver
//! (`site.cjs`); this module validates and bounds model-supplied input only.

use serde_json::{Map, Value, json};

#[cfg(test)]
mod tests;

/// Device preset names accepted by `emulate` and listed by `devices` (the Puppeteer `KnownDevices`
/// table). Names the installed Playwright runtime lacks are reported at runtime.
pub(super) const DEVICE_NAMES: &[&str] = &[
    "Blackberry PlayBook",
    "Blackberry PlayBook landscape",
    "BlackBerry Z30",
    "BlackBerry Z30 landscape",
    "Galaxy Note 3",
    "Galaxy Note 3 landscape",
    "Galaxy Note II",
    "Galaxy Note II landscape",
    "Galaxy S III",
    "Galaxy S III landscape",
    "Galaxy S5",
    "Galaxy S5 landscape",
    "Galaxy S8",
    "Galaxy S8 landscape",
    "Galaxy S9+",
    "Galaxy S9+ landscape",
    "Galaxy Tab S4",
    "Galaxy Tab S4 landscape",
    "iPad",
    "iPad landscape",
    "iPad (gen 6)",
    "iPad (gen 6) landscape",
    "iPad (gen 7)",
    "iPad (gen 7) landscape",
    "iPad Mini",
    "iPad Mini landscape",
    "iPad Pro",
    "iPad Pro landscape",
    "iPad Pro 11",
    "iPad Pro 11 landscape",
    "iPhone 4",
    "iPhone 4 landscape",
    "iPhone 5",
    "iPhone 5 landscape",
    "iPhone 6",
    "iPhone 6 landscape",
    "iPhone 6 Plus",
    "iPhone 6 Plus landscape",
    "iPhone 7",
    "iPhone 7 landscape",
    "iPhone 7 Plus",
    "iPhone 7 Plus landscape",
    "iPhone 8",
    "iPhone 8 landscape",
    "iPhone 8 Plus",
    "iPhone 8 Plus landscape",
    "iPhone SE",
    "iPhone SE landscape",
    "iPhone X",
    "iPhone X landscape",
    "iPhone XR",
    "iPhone XR landscape",
    "iPhone 11",
    "iPhone 11 landscape",
    "iPhone 11 Pro",
    "iPhone 11 Pro landscape",
    "iPhone 11 Pro Max",
    "iPhone 11 Pro Max landscape",
    "iPhone 12",
    "iPhone 12 landscape",
    "iPhone 12 Pro",
    "iPhone 12 Pro landscape",
    "iPhone 12 Pro Max",
    "iPhone 12 Pro Max landscape",
    "iPhone 12 Mini",
    "iPhone 12 Mini landscape",
    "iPhone 13",
    "iPhone 13 landscape",
    "iPhone 13 Pro",
    "iPhone 13 Pro landscape",
    "iPhone 13 Pro Max",
    "iPhone 13 Pro Max landscape",
    "iPhone 13 Mini",
    "iPhone 13 Mini landscape",
    "iPhone 14",
    "iPhone 14 landscape",
    "iPhone 14 Plus",
    "iPhone 14 Plus landscape",
    "iPhone 14 Pro",
    "iPhone 14 Pro landscape",
    "iPhone 14 Pro Max",
    "iPhone 14 Pro Max landscape",
    "iPhone 15",
    "iPhone 15 landscape",
    "iPhone 15 Plus",
    "iPhone 15 Plus landscape",
    "iPhone 15 Pro",
    "iPhone 15 Pro landscape",
    "iPhone 15 Pro Max",
    "iPhone 15 Pro Max landscape",
    "JioPhone 2",
    "JioPhone 2 landscape",
    "Kindle Fire HDX",
    "Kindle Fire HDX landscape",
    "LG Optimus L70",
    "LG Optimus L70 landscape",
    "Microsoft Lumia 550",
    "Microsoft Lumia 950",
    "Microsoft Lumia 950 landscape",
    "Nexus 10",
    "Nexus 10 landscape",
    "Nexus 4",
    "Nexus 4 landscape",
    "Nexus 5",
    "Nexus 5 landscape",
    "Nexus 5X",
    "Nexus 5X landscape",
    "Nexus 6",
    "Nexus 6 landscape",
    "Nexus 6P",
    "Nexus 6P landscape",
    "Nexus 7",
    "Nexus 7 landscape",
    "Nokia Lumia 520",
    "Nokia Lumia 520 landscape",
    "Nokia N9",
    "Nokia N9 landscape",
    "Pixel 2",
    "Pixel 2 landscape",
    "Pixel 2 XL",
    "Pixel 2 XL landscape",
    "Pixel 3",
    "Pixel 3 landscape",
    "Pixel 4",
    "Pixel 4 landscape",
    "Pixel 4a (5G)",
    "Pixel 4a (5G) landscape",
    "Pixel 5",
    "Pixel 5 landscape",
    "Moto G4",
    "Moto G4 landscape",
];

const MAX_VIEWPORT_DIMENSION: i64 = 7680;
const MAX_USER_AGENT_CHARS: usize = 512;
const MAX_TIMEZONE_CHARS: usize = 64;
const MAX_LOCALE_CHARS: usize = 35;
const MAX_ACCURACY_METRES: f64 = 1_000_000.0;

/// Validated open-time emulation: tab-scoped fields for the new tab plus the session-scoped flag.
pub(super) struct OpenOptions {
    pub(super) tab: Option<Value>,
    pub(super) ignore_https_errors: Option<bool>,
}

pub(super) fn device_known(name: &str) -> bool {
    DEVICE_NAMES.contains(&name)
}

/// Validates an `emulate` request into the normalized `emulation` object the driver applies.
/// At least one emulation field is required so an empty call cannot pretend to succeed.
pub(super) fn parse_emulate(args: &Value) -> Result<Value, String> {
    let mut out = Map::new();
    if let Some(device) = present(args, "device") {
        let name = device.as_str().ok_or("device must be a string")?;
        if !device_known(name) {
            return Err(format!(
                "unknown device {name:?}; use action=devices to list supported names"
            ));
        }
        out.insert("device".into(), json!(name));
    }
    if let Some(viewport) = present(args, "viewport") {
        out.insert("viewport".into(), parse_viewport(viewport)?);
    }
    if let Some(user_agent) = present(args, "user_agent") {
        out.insert("user_agent".into(), json!(parse_user_agent(user_agent)?));
    }
    if let Some(timezone) = present(args, "timezone") {
        out.insert(
            "timezone".into(),
            json!(parse_text(timezone, "timezone", MAX_TIMEZONE_CHARS)?),
        );
    }
    if let Some(locale) = present(args, "locale") {
        out.insert("locale".into(), json!(parse_locale(locale)?));
    }
    if let Some(scheme) = present(args, "color_scheme") {
        out.insert("color_scheme".into(), json!(parse_color_scheme(scheme)?));
    }
    if let Some(offline) = present(args, "offline") {
        let offline = offline.as_bool().ok_or("offline must be a boolean")?;
        out.insert("offline".into(), json!(offline));
    }
    // Explicit null clears the geolocation override, so it is kept rather than dropped.
    if let Some(geolocation) = args.get("geolocation") {
        out.insert("geolocation".into(), parse_geolocation(geolocation)?);
    }
    if out.is_empty() {
        return Err(
            "emulate requires at least one of device, viewport, user_agent, geolocation, offline, color_scheme, timezone or locale"
                .into(),
        );
    }
    Ok(Value::Object(out))
}

/// Validates open-time options. Viewport and user agent apply only to the new tab; the
/// HTTPS-error policy is a browser-context setting fixed when the session starts.
pub(super) fn parse_open(args: &Value) -> Result<OpenOptions, String> {
    let mut tab = Map::new();
    if let Some(viewport) = present(args, "viewport") {
        tab.insert("viewport".into(), parse_viewport(viewport)?);
    }
    if let Some(user_agent) = present(args, "user_agent") {
        tab.insert("user_agent".into(), json!(parse_user_agent(user_agent)?));
    }
    let ignore_https_errors = match present(args, "ignore_https_errors") {
        Some(value) => Some(
            value
                .as_bool()
                .ok_or("ignore_https_errors must be a boolean")?,
        ),
        None => None,
    };
    Ok(OpenOptions {
        tab: (!tab.is_empty()).then_some(Value::Object(tab)),
        ignore_https_errors,
    })
}

fn present<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|value| !value.is_null())
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str], name: &str) -> Result<(), String> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("{name} does not accept {key:?}")),
        None => Ok(()),
    }
}

fn bounded_integer(value: Option<&Value>, name: &str, min: i64, max: i64) -> Result<i64, String> {
    value
        .and_then(Value::as_i64)
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| format!("{name} must be an integer from {min} to {max}"))
}

fn bounded_number(value: Option<&Value>, name: &str, min: f64, max: f64) -> Result<f64, String> {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && (min..=max).contains(n))
        .ok_or_else(|| format!("{name} must be a number from {min} to {max}"))
}

fn parse_viewport(value: &Value) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or("viewport must be an object with width and height")?;
    reject_unknown(object, &["width", "height", "scale"], "viewport")?;
    let width = bounded_integer(
        object.get("width"),
        "viewport.width",
        1,
        MAX_VIEWPORT_DIMENSION,
    )?;
    let height = bounded_integer(
        object.get("height"),
        "viewport.height",
        1,
        MAX_VIEWPORT_DIMENSION,
    )?;
    let mut out = json!({"width": width, "height": height});
    if object.contains_key("scale") {
        let scale = bounded_number(object.get("scale"), "viewport.scale", 0.1, 4.0)?;
        out["scale"] = json!(scale);
    }
    Ok(out)
}

fn parse_geolocation(value: &Value) -> Result<Value, String> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let object = value
        .as_object()
        .ok_or("geolocation must be an object or null")?;
    reject_unknown(
        object,
        &["latitude", "longitude", "accuracy"],
        "geolocation",
    )?;
    let latitude = bounded_number(object.get("latitude"), "geolocation.latitude", -90.0, 90.0)?;
    let longitude = bounded_number(
        object.get("longitude"),
        "geolocation.longitude",
        -180.0,
        180.0,
    )?;
    let mut out = json!({"latitude": latitude, "longitude": longitude});
    if object.contains_key("accuracy") {
        let accuracy = bounded_number(
            object.get("accuracy"),
            "geolocation.accuracy",
            0.0,
            MAX_ACCURACY_METRES,
        )?;
        out["accuracy"] = json!(accuracy);
    }
    Ok(out)
}

fn parse_user_agent(value: &Value) -> Result<String, String> {
    let text = parse_text(value, "user_agent", MAX_USER_AGENT_CHARS)?;
    if text.is_empty() {
        return Err("user_agent must be non-empty; omit it to keep the current value".into());
    }
    Ok(text)
}

/// Empty string is accepted and means "reset to the browser default" for timezone and locale.
fn parse_text(value: &Value, name: &str, max_chars: usize) -> Result<String, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{name} must be a string"))?;
    if text.chars().count() > max_chars || text.chars().any(char::is_control) {
        return Err(format!(
            "{name} must be at most {max_chars} printable characters"
        ));
    }
    Ok(text.to_owned())
}

fn parse_locale(value: &Value) -> Result<String, String> {
    let text = parse_text(value, "locale", MAX_LOCALE_CHARS)?;
    let valid = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !text.is_empty() && !valid {
        return Err("locale must contain only ASCII letters, digits, '-' or '_'".into());
    }
    Ok(text)
}

fn parse_color_scheme(value: &Value) -> Result<&'static str, String> {
    match value.as_str() {
        Some("light") => Ok("light"),
        Some("dark") => Ok("dark"),
        Some("no-preference") => Ok("no-preference"),
        _ => Err("color_scheme must be one of light, dark or no-preference".into()),
    }
}
