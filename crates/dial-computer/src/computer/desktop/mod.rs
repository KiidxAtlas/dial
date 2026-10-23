pub mod applications;
mod ax;
mod backend;
mod control;
pub mod error;
mod frame;
mod keys;
#[cfg(dial_desktop_linux)]
mod linux;
#[cfg(dial_desktop_macos)]
mod macos;
mod menus;
#[cfg(dial_desktop_macos)]
mod native_helper;
pub mod types;
#[cfg(dial_desktop_windows)]
mod win32;

use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub use applications::{Application, ApplicationOpenOptions, ApplicationQuery};
use backend::{Backend, DeliveryMode, Modifiers, MouseButton, PointerEvent};
pub(crate) use control::CancelReason;
use control::{CancellationSource, InputLease, OperationToken};
use error::CoreResult;
pub use error::DesktopError;
use frame::{FrameGeometry, apply_capture_caps, encode_png};
use keys::{KeyName, parse_keys, parse_modifiers};
pub use menus::DesktopMenuItem;
use parking_lot::Mutex;
pub use types::*;

use ax::AxRegistry;

const OPERATION_TIMEOUT: Duration = Duration::from_mins(3);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

enum Response {
    Capabilities(DesktopCapabilities),
    Displays(Vec<DesktopDisplay>),
    Windows(Vec<DesktopWindow>),
    Window(DesktopWindow),
    WindowSnapshot(DesktopWindowSnapshot),
    Capture(DesktopCapture),
    Observation(DesktopObservation),
    Applications(Vec<Application>),
    Application(Application),
    MenuItems(Vec<DesktopMenuItem>),
    Unit,
    Snapshot(AxSnapshot),
    Nodes(Vec<AxNode>),
    Node(Option<AxNode>),
    Attributes(Vec<(String, String)>),
}

type Reply = flume::Sender<CoreResult<Response>>;

struct QueuedRequest {
    request: Request,
    token: OperationToken,
}

enum Request {
    Capabilities {
        reply: Reply,
    },
    ListDisplays {
        reply: Reply,
    },
    ListWindows {
        reply: Reply,
    },
    GetWindow {
        id: String,
        reply: Reply,
    },
    ListWindowSnapshot {
        reply: Reply,
    },
    Capture {
        target: Target,
        caps: CaptureCaps,
        reply: Reply,
    },
    CaptureRegion {
        target: Target,
        region: CaptureRegion,
        caps: CaptureCaps,
        reply: Reply,
    },
    Observe {
        target: Target,
        caps: CaptureCaps,
        options: AxSnapshotOptions,
        reply: Reply,
    },
    ListApplications {
        options: ApplicationQuery,
        reply: Reply,
    },
    OpenApplication {
        id: String,
        options: ApplicationOpenOptions,
        reply: Reply,
    },
    MenuItems {
        target: Target,
        path: Vec<String>,
        reply: Reply,
    },
    MenuSelect {
        target: Target,
        path: Vec<String>,
        reply: Reply,
    },
    BringToCurrentSpace {
        id: String,
        reply: Reply,
    },
    HoldKeys {
        target: Target,
        keys: Vec<KeyName>,
        duration: Duration,
        takeover: Option<bool>,
        reply: Reply,
    },
    HoldMouse {
        target: Target,
        x: f64,
        y: f64,
        button: MouseButton,
        keys: Vec<KeyName>,
        duration: Duration,
        takeover: Option<bool>,
        reply: Reply,
    },
    Click {
        target: Target,
        x: f64,
        y: f64,
        options: ParsedPointerOptions,
        reply: Reply,
    },
    MoveMouse {
        target: Target,
        x: f64,
        y: f64,
        takeover: Option<bool>,
        reply: Reply,
    },
    Drag {
        target: Target,
        path: Vec<(f64, f64)>,
        options: ParsedPointerOptions,
        reply: Reply,
    },
    Scroll {
        target: Target,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        takeover: Option<bool>,
        reply: Reply,
    },
    TypeText {
        target: Target,
        text: String,
        takeover: Option<bool>,
        reply: Reply,
    },
    KeyChord {
        target: Target,
        keys: Vec<KeyName>,
        takeover: Option<bool>,
        reply: Reply,
    },
    RaiseWindow {
        id: String,
        reply: Reply,
    },
    AxSnapshot {
        target: Target,
        options: AxSnapshotOptions,
        reply: Reply,
    },
    AxQuery {
        target: Target,
        query: AxQuery,
        reply: Reply,
    },
    AxElementAt {
        target: Target,
        x: f64,
        y: f64,
        reply: Reply,
    },
    AxFocused {
        reply: Reply,
    },
    AxNode {
        reference: String,
        reply: Reply,
    },
    AxAttributes {
        reference: String,
        reply: Reply,
    },
    AxChildren {
        reference: String,
        reply: Reply,
    },
    AxParent {
        reference: String,
        reply: Reply,
    },
    AxPerform {
        reference: String,
        action: String,
        reply: Reply,
    },
    AxSetValue {
        reference: String,
        value: String,
        reply: Reply,
    },
    AxFocus {
        reference: String,
        reply: Reply,
    },
    AxClick {
        reference: String,
        options: ParsedPointerOptions,
        reply: Reply,
    },
    Close {
        reply: Reply,
    },
}

impl Request {
    fn reply(self, result: CoreResult<Response>) -> bool {
        let reply = match self {
            Self::Capabilities { reply }
            | Self::ListDisplays { reply }
            | Self::ListWindows { reply }
            | Self::GetWindow { reply, .. }
            | Self::ListWindowSnapshot { reply }
            | Self::Capture { reply, .. }
            | Self::CaptureRegion { reply, .. }
            | Self::Observe { reply, .. }
            | Self::ListApplications { reply, .. }
            | Self::OpenApplication { reply, .. }
            | Self::MenuItems { reply, .. }
            | Self::MenuSelect { reply, .. }
            | Self::BringToCurrentSpace { reply, .. }
            | Self::HoldKeys { reply, .. }
            | Self::HoldMouse { reply, .. }
            | Self::Click { reply, .. }
            | Self::MoveMouse { reply, .. }
            | Self::Drag { reply, .. }
            | Self::Scroll { reply, .. }
            | Self::TypeText { reply, .. }
            | Self::KeyChord { reply, .. }
            | Self::RaiseWindow { reply, .. }
            | Self::AxSnapshot { reply, .. }
            | Self::AxQuery { reply, .. }
            | Self::AxElementAt { reply, .. }
            | Self::AxFocused { reply }
            | Self::AxNode { reply, .. }
            | Self::AxAttributes { reply, .. }
            | Self::AxChildren { reply, .. }
            | Self::AxParent { reply, .. }
            | Self::AxPerform { reply, .. }
            | Self::AxSetValue { reply, .. }
            | Self::AxFocus { reply, .. }
            | Self::AxClick { reply, .. }
            | Self::Close { reply } => reply,
        };
        reply.send(result).is_ok()
    }

    fn is_mutation(&self) -> bool {
        matches!(
            self,
            Self::Click { .. }
                | Self::MoveMouse { .. }
                | Self::Drag { .. }
                | Self::Scroll { .. }
                | Self::TypeText { .. }
                | Self::KeyChord { .. }
                | Self::RaiseWindow { .. }
                | Self::AxPerform { .. }
                | Self::AxSetValue { .. }
                | Self::AxFocus { .. }
                | Self::AxClick { .. }
                | Self::OpenApplication { .. }
                | Self::MenuSelect { .. }
                | Self::BringToCurrentSpace { .. }
                | Self::HoldKeys { .. }
                | Self::HoldMouse { .. }
        )
    }

    fn frame_target(&self) -> Option<&Target> {
        match self {
            Self::Capture { target, .. } | Self::Observe { target, .. } => Some(target),
            _ => None,
        }
    }

    fn is_close(&self) -> bool {
        matches!(self, Self::Close { .. })
    }
}

#[derive(Clone)]
struct ParsedPointerOptions {
    button: MouseButton,
    count: u32,
    modifiers: Modifiers,
    keys: Vec<KeyName>,
    takeover: Option<bool>,
}

impl ParsedPointerOptions {
    fn parse(options: Option<PointerOptions>) -> CoreResult<Self> {
        let options = options.unwrap_or_default();
        Ok(Self {
            button: MouseButton::parse(options.button.as_deref())?,
            count: options.count.unwrap_or(1).max(1),
            modifiers: parse_modifiers(options.modifiers.as_deref().unwrap_or_default())?,
            keys: parse_keys(options.keys.as_deref().unwrap_or_default())?,
            takeover: options.takeover,
        })
    }

    fn mode(&self, token: &OperationToken) -> DeliveryMode {
        delivery_mode(self.takeover, token)
    }
}

fn delivery_mode(takeover: Option<bool>, token: &OperationToken) -> DeliveryMode {
    DeliveryMode::from_takeover(Some(takeover.unwrap_or_else(|| token.control_active())))
}

fn hold_duration(seconds: f64) -> CoreResult<Duration> {
    if !seconds.is_finite() || !(0.0..=100.0).contains(&seconds) {
        return Err(DesktopError::input_failed(
            "hold duration must be finite seconds from 0 through 100",
        ));
    }
    Ok(Duration::from_secs_f64(seconds))
}

struct Worker {
    backend: CoreResult<Box<dyn Backend>>,
    registry: AxRegistry,
    frames: HashMap<String, FrameGeometry>,
    /// Latest capabilities the worker computed, shared with the session so the
    /// getter can answer while an operation holds the worker.
    capabilities: Arc<Mutex<Option<DesktopCapabilities>>>,
}

impl Worker {
    fn new(
        selector: DisplaySelector,
        capabilities: Arc<Mutex<Option<DesktopCapabilities>>>,
    ) -> Self {
        let backend = create_backend(selector);
        Self {
            backend,
            registry: AxRegistry::default(),
            frames: HashMap::new(),
            capabilities,
        }
    }

    fn backend(&mut self) -> CoreResult<&mut Box<dyn Backend>> {
        self.backend.as_mut().map_err(|error| error.clone())
    }

    fn window(&mut self, target: &Target) -> CoreResult<DesktopWindow> {
        match target {
            Target::Window(id) => self.backend()?.window(id),
            Target::Desktop | Target::Display(_) => self
                .backend()?
                .windows()?
                .into_iter()
                .find(|window| window.focused)
                .ok_or_else(|| DesktopError::window_not_found("no focused window was found")),
        }
    }

    fn frame(&self, target: &Target) -> CoreResult<FrameGeometry> {
        self.frames.get(target.key()).cloned().ok_or_else(|| {
			DesktopError::invalid_coordinate_frame(format!(
				"no capture of '{}' yet — take a screenshot of this target first; coordinate input is \
				 in pixels of that screenshot",
				target.key()
			))
		})
    }

    fn validated_frame(
        &mut self,
        target: &Target,
    ) -> CoreResult<(FrameGeometry, Option<DesktopWindow>)> {
        let frame = self.frame(target)?;
        self.backend()?.validate_frame_layout(&frame)?;
        let current = if matches!(target, Target::Window(_)) {
            Some(self.window(target)?)
        } else {
            None
        };
        frame.validate_window(current.as_ref())?;
        Ok((frame, current))
    }

    fn map_point(
        &mut self,
        target: &Target,
        x: f64,
        y: f64,
    ) -> CoreResult<(f64, f64, FrameGeometry)> {
        let (frame, current) = self.validated_frame(target)?;
        let (x, y) = frame.map_point(x, y, current.as_ref())?;
        Ok((x, y, frame))
    }

    fn explicit_window(&mut self, target: &Target) -> CoreResult<DesktopWindow> {
        if !matches!(target, Target::Window(_)) {
            return Err(DesktopError::invalid_target(
                "this operation requires an explicit window target",
            ));
        }
        self.window(target)
    }

    fn validate_keyboard_target(&mut self, target: &Target) -> CoreResult<()> {
        if !matches!(target, Target::Display(_)) {
            return Ok(());
        }
        let (frame, _) = self.validated_frame(target)?;
        let window = self.backend()?.focused_keyboard_window()?;
        let displays = self.backend()?.displays()?;
        if !displays.iter().any(|display| {
            i64::from(window.x) < i64::from(display.x) + i64::from(display.width)
                && i64::from(window.x) + i64::from(window.width) > i64::from(display.x)
                && i64::from(window.y) < i64::from(display.y) + i64::from(display.height)
                && i64::from(window.y) + i64::from(window.height) > i64::from(display.y)
        }) {
            return Err(DesktopError::invalid_target(
                "the focused window is outside the known display layout",
            ));
        }
        let active = DisplaySelector::Active.select(displays, Some(&window))?;
        if !frame.contains_display(&active[0].id) {
            return Err(DesktopError::invalid_target(
                "the focused window is on another display; focus a window on this display before \
				 keyboard input",
            ));
        }
        Ok(())
    }

    fn ax(&mut self) -> CoreResult<&mut dyn backend::AxBackend> {
        self.backend()?
            .ax()
            .ok_or_else(DesktopError::ax_unsupported)
    }

    fn snapshot(&mut self, target: &Target, options: &AxSnapshotOptions) -> CoreResult<AxSnapshot> {
        let window = self.window(target)?;
        let (backend, registry) = (&mut self.backend, &mut self.registry);
        let ax = backend
            .as_mut()
            .map_err(|error| error.clone())?
            .ax()
            .ok_or_else(DesktopError::ax_unsupported)?;
        ax::snapshot(ax, registry, &window, options)
    }

    fn capture_full(
        &mut self,
        target: &Target,
        caps: &CaptureCaps,
        token: &OperationToken,
    ) -> CoreResult<DesktopCapture> {
        let selector = target.display_selector();
        let (image, mut geometry) = self.backend()?.capture(target, caps, selector.as_ref())?;
        let source_width = image.width();
        let source_height = image.height();
        let layout = self.backend()?.displays()?;
        geometry.record_layout(&layout)?;
        let image = apply_capture_caps(image, &mut geometry, caps)?;
        let width = image.width();
        let height = image.height();
        let displays = self.capture_metadata(target, &geometry, &layout, width, height)?;
        token.check()?;
        let png = encode_png(image)?;
        token.check()?;
        self.frames.insert(target.key().to_string(), geometry);
        // Refreshing here keeps the snapshot current for getter reads that
        // land while a later operation holds the worker.
        let capabilities = self.backend()?.capabilities();
        *self.capabilities.lock() = Some(capabilities.clone());
        Ok(DesktopCapture {
            data: png,
            width,
            height,
            source_width,
            source_height,
            coordinate_width: width,
            coordinate_height: height,
            region: None,
            target: target.key().to_string(),
            displays,
            backend: capabilities.backend,
            display_server: capabilities.display_server,
        })
    }

    fn capture_metadata(
        &mut self,
        target: &Target,
        geometry: &FrameGeometry,
        layout: &[DesktopDisplay],
        source_width: u32,
        source_height: u32,
    ) -> CoreResult<Vec<DesktopDisplay>> {
        let source = match target {
            Target::Desktop | Target::Display(_) => return Ok(geometry.display_metadata(layout)),
            Target::Window(_) => {
                let window = self.window(target)?;
                geometry.validate_window(Some(&window))?;
                DesktopDisplay {
                    id: window.id,
                    name: format!("{} — {}", window.app, window.title),
                    x: window.x,
                    y: window.y,
                    width: window.width,
                    height: window.height,
                    scale: f64::from(source_width) / f64::from(window.width.max(1)),
                    pixel_x: 0,
                    pixel_y: 0,
                    pixel_width: source_width,
                    pixel_height: source_height,
                    is_primary: false,
                }
            }
        };
        Ok(geometry.display_metadata(std::slice::from_ref(&source)))
    }

    fn dispatch(&mut self, request: Request, token: &OperationToken) {
        let frame_key = request
            .frame_target()
            .map(|target| target.key().to_string());
        let previous = frame_key.as_ref().and_then(|key| self.frames.remove(key));
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| self.execute(&request, token)))
            .unwrap_or_else(|_| Err(DesktopError::internal("native desktop worker panicked")));
        let failed = result.is_err();
        let delivered = request.reply(result);
        if (failed || !delivered)
            && let Some(key) = frame_key
        {
            if let Some(frame) = previous {
                self.frames.insert(key, frame);
            } else {
                self.frames.remove(&key);
            }
        }
    }

    fn execute(&mut self, request: &Request, token: &OperationToken) -> CoreResult<Response> {
        if request.is_close() {
            return Ok(Response::Unit);
        }
        token.check()?;
        let _scope = token.enter();
        let _lease = request
            .is_mutation()
            .then(|| InputLease::acquire(token))
            .transpose()?;
        token.check()?;
        if request.is_mutation() {
            // From here native input/focus may change, even if it later fails.
            token.mark_mutation_started();
        }
        // A full capture replaces coordinates only if it completes in its
        // original generation. Keep the previous frame by move, not by
        // cloning an atlas.
        let previous_frame = request
            .frame_target()
            .and_then(|target| self.frames.remove(target.key()));
        let result = self.process(request, token).and_then(|response| {
            token.check()?;
            Ok(response)
        });
        if result.is_err()
            && let Some(target) = request.frame_target()
        {
            match previous_frame {
                Some(frame) => {
                    self.frames.insert(target.key().to_string(), frame);
                }
                None => {
                    self.frames.remove(target.key());
                }
            }
        }
        result
    }

    fn process(&mut self, request: &Request, token: &OperationToken) -> CoreResult<Response> {
        match request {
            Request::Capabilities { .. } => {
                let caps = match self.backend.as_mut() {
                    Ok(backend) => backend.capabilities(),
                    Err(_) => DesktopCapabilities::unavailable(),
                };
                *self.capabilities.lock() = Some(caps.clone());
                Ok(Response::Capabilities(caps))
            }
            Request::ListDisplays { .. } => Ok(Response::Displays(self.backend()?.displays()?)),
            Request::ListWindows { .. } => Ok(Response::Windows(self.backend()?.windows()?)),
            Request::GetWindow { id, .. } => Ok(Response::Window(self.backend()?.window(id)?)),
            Request::ListWindowSnapshot { .. } => {
                Ok(Response::WindowSnapshot(self.backend()?.window_snapshot()?))
            }
            Request::Capture { target, caps, .. } => {
                Ok(Response::Capture(self.capture_full(target, caps, token)?))
            }
            Request::Observe {
                target,
                caps,
                options,
                ..
            } => {
                let capture = self.capture_full(target, caps, token)?;
                token.check()?;
                let accessibility = self.snapshot(target, options)?;
                Ok(Response::Observation(DesktopObservation {
                    capture,
                    accessibility,
                }))
            }
            Request::ListApplications { options, .. } => {
                Ok(Response::Applications(applications::list(options.clone())?))
            }
            Request::OpenApplication { id, options, .. } => Ok(Response::Application(
                applications::open(id, options.clone())?,
            )),
            Request::MenuItems { target, path, .. } => {
                let window = self.explicit_window(target)?;
                Ok(Response::MenuItems(
                    self.backend()?.menu_items(&window, path)?,
                ))
            }
            Request::MenuSelect { target, path, .. } => {
                let window = self.explicit_window(target)?;
                self.backend()?.menu_select(&window, path, token)?;
                Ok(Response::Unit)
            }
            Request::BringToCurrentSpace { id, .. } => {
                let target = Target::Window(id.clone());
                // The window may be off-Space and absent from capturable windows.
                // Resolve its owner through the Space backend, not capture lookup.
                // A refused or partially completed native move may still change
                // geometry, so never retain its old coordinate frame.
                self.frames.remove(target.key());
                self.backend()?.bring_to_current_space(id, token)?;
                Ok(Response::Unit)
            }
            Request::HoldKeys {
                target,
                keys,
                duration,
                takeover,
                ..
            } => {
                self.validate_keyboard_target(target)?;
                self.backend()?.hold_keys(
                    target,
                    keys,
                    *duration,
                    delivery_mode(*takeover, token),
                    token,
                )?;
                Ok(Response::Unit)
            }
            Request::HoldMouse {
                target,
                x,
                y,
                button,
                keys,
                duration,
                takeover,
                ..
            } => {
                if !keys.is_empty() {
                    self.validate_keyboard_target(target)?;
                }
                let (x, y, frame) = self.map_point(target, *x, *y)?;
                self.backend()?.pointer(
                    target,
                    PointerEvent::Hold {
                        x,
                        y,
                        button: *button,
                        keys: keys.clone(),
                        duration: *duration,
                    },
                    &frame,
                    delivery_mode(*takeover, token),
                    token,
                )?;
                Ok(Response::Unit)
            }
            Request::CaptureRegion {
                target,
                region,
                caps,
                ..
            } => {
                let (base, _) = self.validated_frame(target)?;
                base.validate_region(region)?;
                let selector = base.capture_selector();
                token.check()?;
                let (image, fresh) =
                    self.backend()?
                        .capture(target, &CaptureCaps::default(), selector.as_ref())?;
                let layout = self.backend()?.displays().map_err(|error| {
                    DesktopError::invalid_coordinate_frame(format!(
                        "could not validate captured display layout: {error}"
                    ))
                })?;
                base.validate_layout(&layout)?;
                let displays =
                    self.capture_metadata(target, &base, &layout, image.width(), image.height())?;
                let (image, source_width, source_height) =
                    base.crop_region(image, &fresh, region, caps)?;
                let width = image.width();
                let height = image.height();
                let (coordinate_width, coordinate_height) = base.dimensions();
                token.check()?;
                let png = encode_png(image)?;
                let capabilities = self.backend()?.capabilities();
                *self.capabilities.lock() = Some(capabilities.clone());
                Ok(Response::Capture(DesktopCapture {
                    data: png,
                    width,
                    height,
                    source_width,
                    source_height,
                    coordinate_width,
                    coordinate_height,
                    region: Some(*region),
                    target: target.key().to_string(),
                    displays,
                    backend: capabilities.backend,
                    display_server: capabilities.display_server,
                }))
            }
            Request::Click {
                target,
                x,
                y,
                options,
                ..
            } => {
                if options.modifiers != Modifiers::default() {
                    self.validate_keyboard_target(target)?;
                }
                let (x, y, frame) = self.map_point(target, *x, *y)?;
                self.backend()?.pointer(
                    target,
                    PointerEvent::Click {
                        x,
                        y,
                        button: options.button,
                        count: options.count,
                        modifiers: options.modifiers,
                    },
                    &frame,
                    options.mode(token),
                    token,
                )?;
                Ok(Response::Unit)
            }
            Request::MoveMouse {
                target,
                x,
                y,
                takeover,
                ..
            } => {
                let (x, y, frame) = self.map_point(target, *x, *y)?;
                self.backend()?.pointer(
                    target,
                    PointerEvent::Move { x, y },
                    &frame,
                    delivery_mode(*takeover, token),
                    token,
                )?;
                Ok(Response::Unit)
            }
            Request::Drag {
                target,
                path,
                options,
                ..
            } => {
                if !options.keys.is_empty() || options.modifiers != Modifiers::default() {
                    self.validate_keyboard_target(target)?;
                }
                let (frame, current) = self.validated_frame(target)?;
                let mapped = path
                    .iter()
                    .map(|(x, y)| frame.map_point(*x, *y, current.as_ref()))
                    .collect::<CoreResult<Vec<_>>>()?;
                self.backend()?.pointer(
                    target,
                    PointerEvent::Drag {
                        path: mapped,
                        button: options.button,
                        modifiers: options.modifiers,
                        keys: options.keys.clone(),
                    },
                    &frame,
                    options.mode(token),
                    token,
                )?;
                Ok(Response::Unit)
            }
            Request::Scroll {
                target,
                x,
                y,
                dx,
                dy,
                takeover,
                ..
            } => {
                let (x, y, frame) = self.map_point(target, *x, *y)?;
                self.backend()?.pointer(
                    target,
                    PointerEvent::Scroll {
                        x,
                        y,
                        dx: *dx,
                        dy: *dy,
                    },
                    &frame,
                    delivery_mode(*takeover, token),
                    token,
                )?;
                Ok(Response::Unit)
            }
            Request::TypeText {
                target,
                text,
                takeover,
                ..
            } => {
                self.validate_keyboard_target(target)?;
                self.backend()?
                    .type_text(target, text, delivery_mode(*takeover, token), token)?;
                Ok(Response::Unit)
            }
            Request::KeyChord {
                target,
                keys,
                takeover,
                ..
            } => {
                self.validate_keyboard_target(target)?;
                self.backend()?
                    .key_chord(target, keys, delivery_mode(*takeover, token), token)?;
                Ok(Response::Unit)
            }
            Request::RaiseWindow { id, .. } => {
                self.backend()?.raise_window(id, token)?;
                Ok(Response::Unit)
            }
            Request::AxSnapshot {
                target, options, ..
            } => Ok(Response::Snapshot(self.snapshot(target, options)?)),
            Request::AxQuery { target, query, .. } => {
                let window = self.window(target)?;
                let (backend, registry) = (&mut self.backend, &mut self.registry);
                let ax = backend
                    .as_mut()
                    .map_err(|error| error.clone())?
                    .ax()
                    .ok_or_else(DesktopError::ax_unsupported)?;
                Ok(Response::Nodes(ax::query(ax, registry, &window, query)?))
            }
            Request::AxElementAt { target, x, y, .. } => {
                let (backend, registry) = (&mut self.backend, &mut self.registry);
                let ax = backend
                    .as_mut()
                    .map_err(|error| error.clone())?
                    .ax()
                    .ok_or_else(DesktopError::ax_unsupported)?;
                Ok(Response::Node(ax::element_at_node(
                    ax,
                    registry,
                    target.key(),
                    *x,
                    *y,
                )?))
            }
            Request::AxFocused { .. } => {
                let handle = self.ax()?.focused_element()?;
                let node = match handle {
                    Some(h) => {
                        let (backend, registry) = (&mut self.backend, &mut self.registry);
                        let ax = backend
                            .as_mut()
                            .map_err(|error| error.clone())?
                            .ax()
                            .ok_or_else(DesktopError::ax_unsupported)?;
                        Some(ax::register_node(ax, registry, "desktop", h)?)
                    }
                    None => None,
                };
                Ok(Response::Node(node))
            }
            Request::AxNode { reference, .. } => {
                let h = self.registry.resolve(reference)?;
                let props = self.ax()?.props(&h)?;
                Ok(Response::Node(Some(ax::node_to_ax_node(
                    reference.clone(),
                    props,
                ))))
            }
            Request::AxAttributes { reference, .. } => {
                let h = self.registry.resolve(reference)?;
                let mut attributes = self.ax()?.attributes(&h)?;
                for (_, value) in &mut attributes {
                    if value.chars().count() > 200 {
                        *value = value
                            .chars()
                            .take(199)
                            .chain(std::iter::once('…'))
                            .collect();
                    }
                }
                Ok(Response::Attributes(attributes))
            }
            Request::AxChildren { reference, .. } => {
                let h = self.registry.resolve(reference)?;
                let target = self.registry.target(reference)?;
                let handles = self.ax()?.children(&h)?;
                let mut nodes = Vec::with_capacity(handles.len());
                for h in handles {
                    let (backend, registry) = (&mut self.backend, &mut self.registry);
                    let ax = backend
                        .as_mut()
                        .map_err(|error| error.clone())?
                        .ax()
                        .ok_or_else(DesktopError::ax_unsupported)?;
                    nodes.push(ax::register_node(ax, registry, &target, h)?);
                }
                Ok(Response::Nodes(nodes))
            }
            Request::AxParent { reference, .. } => {
                let h = self.registry.resolve(reference)?;
                let target = self.registry.target(reference)?;
                let parent = self.ax()?.parent(&h)?;
                let node = match parent {
                    Some(h) => {
                        let (backend, registry) = (&mut self.backend, &mut self.registry);
                        let ax = backend
                            .as_mut()
                            .map_err(|error| error.clone())?
                            .ax()
                            .ok_or_else(DesktopError::ax_unsupported)?;
                        Some(ax::register_node(ax, registry, &target, h)?)
                    }
                    None => None,
                };
                Ok(Response::Node(node))
            }
            Request::AxPerform {
                reference, action, ..
            } => {
                let h = self.registry.resolve(reference)?;
                if action.eq_ignore_ascii_case("press") {
                    ax::ax_press(self.ax()?, &h)?;
                } else {
                    self.ax()?.perform(&h, action)?;
                }
                Ok(Response::Unit)
            }
            Request::AxSetValue {
                reference, value, ..
            } => {
                let h = self.registry.resolve(reference)?;
                self.ax()?.set_value(&h, value)?;
                Ok(Response::Unit)
            }
            Request::AxFocus { reference, .. } => {
                let h = self.registry.resolve(reference)?;
                self.ax()?.focus(&h)?;
                Ok(Response::Unit)
            }
            Request::AxClick {
                reference, options, ..
            } => {
                let h = self.registry.resolve(reference)?;
                let bounds = self.ax()?.props(&h)?.bounds.ok_or_else(|| {
                    DesktopError::ax_failed(format!("{reference} has no clickable bounds"))
                })?;
                let x = bounds.x + bounds.width / 2.0;
                let y = bounds.y + bounds.height / 2.0;
                let windows = self.backend()?.windows()?;
                // Desktop refs and parent traversal can leave the snapshot's
                // original window; only the live element can establish ownership.
                let window_id = self.ax()?.window_id(&h, &windows)?;
                let target = Target::Window(window_id);
                self.backend()?.pointer(
                    &target,
                    PointerEvent::Click {
                        x,
                        y,
                        button: options.button,
                        count: options.count,
                        modifiers: options.modifiers,
                    },
                    &FrameGeometry::identity_global(),
                    options.mode(token),
                    token,
                )?;
                Ok(Response::Unit)
            }
            Request::Close { .. } => Ok(Response::Unit),
        }
    }
}

#[cfg(dial_desktop_macos)]
fn create_backend(selector: DisplaySelector) -> CoreResult<Box<dyn Backend>> {
    Ok(Box::new(macos::MacosBackend::new(selector)?))
}

#[cfg(dial_desktop_windows)]
fn create_backend(selector: DisplaySelector) -> CoreResult<Box<dyn Backend>> {
    Ok(Box::new(win32::Win32Backend::new(selector)?))
}

#[cfg(dial_desktop_linux)]
fn create_backend(selector: DisplaySelector) -> CoreResult<Box<dyn Backend>> {
    linux::new_backend(selector)
}

#[cfg(not(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux)))]
fn create_backend(_: DisplaySelector) -> CoreResult<Box<dyn Backend>> {
    Err(DesktopError::native_unavailable("desktop control"))
}

struct Lifecycle {
    tx: Option<flume::Sender<QueuedRequest>>,
    done: Option<flume::Receiver<()>>,
    join: Option<JoinHandle<()>>,
    closed: bool,
}

struct SessionCore {
    selector: DisplaySelector,
    lifecycle: Mutex<Lifecycle>,
    capabilities: Arc<Mutex<Option<DesktopCapabilities>>>,
    /// `call`s sent to the worker that have not returned yet.
    in_flight: AtomicUsize,
    cancellation: CancellationSource,
}

impl SessionCore {
    fn new(selector: DisplaySelector) -> Arc<Self> {
        Arc::new(Self {
            selector,
            lifecycle: Mutex::new(Lifecycle {
                tx: None,
                done: None,
                join: None,
                closed: false,
            }),
            capabilities: Arc::default(),
            in_flight: AtomicUsize::new(0),
            cancellation: CancellationSource::default(),
        })
    }

    fn ensure_started(&self) -> CoreResult<flume::Sender<QueuedRequest>> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.closed {
            return Err(DesktopError::closed());
        }
        if let Some(tx) = &lifecycle.tx {
            return Ok(tx.clone());
        }
        let (tx, rx) = flume::unbounded::<QueuedRequest>();
        let (done_tx, done_rx) = flume::bounded(1);
        let selector = self.selector.clone();
        let caps = Arc::clone(&self.capabilities);
        let join = thread::Builder::new()
            .name("dial-desktop-session".into())
            .spawn(move || {
                let mut worker = Worker::new(selector, caps);
                while let Ok(QueuedRequest { request, token }) = rx.recv() {
                    let close = request.is_close();
                    worker.dispatch(request, &token);
                    if close {
                        break;
                    }
                }
                let _ = done_tx.send(());
            })
            .map_err(|e| {
                DesktopError::internal(format!("failed to start native desktop worker: {e}"))
            })?;
        lifecycle.tx = Some(tx.clone());
        lifecycle.done = Some(done_rx);
        lifecycle.join = Some(join);
        Ok(tx)
    }

    fn call(
        &self,
        token: OperationToken,
        make: impl FnOnce(Reply) -> Request,
    ) -> CoreResult<Response> {
        token.check()?;
        // Rendezvous delivery: a timed-out/dropped caller must never commit a
        // capture frame that it did not receive.
        let (txr, rxr) = flume::bounded(0);
        let tx = self.ensure_started()?;
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let response = tx
            .send(QueuedRequest {
                request: make(txr),
                token,
            })
            .map_err(|_| DesktopError::internal("native desktop worker stopped unexpectedly"))
            .and_then(|()| {
                rxr.recv_timeout(OPERATION_TIMEOUT).map_err(|e| {
                    self.cancellation
                        .cancel_with(CancelReason::OperationTimeout);
                    DesktopError::timeout(format!("native desktop operation did not complete: {e}"))
                })
            });
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        response?
    }

    fn close(&self) -> CoreResult<()> {
        self.cancellation.cancel_with(CancelReason::SessionClosed);
        // The waits below can each block for `CLOSE_TIMEOUT`, and every other
        // caller needs this lock to observe `closed`, so take the handles out
        // and wait without holding it.
        let (tx, done, join) = {
            let mut lifecycle = self.lifecycle.lock();
            lifecycle.closed = true;
            let Some(tx) = lifecycle.tx.take() else {
                return Ok(());
            };
            (tx, lifecycle.done.take(), lifecycle.join.take())
        };
        let (rtx, rrx) = flume::bounded(1);
        tx.send(QueuedRequest {
            request: Request::Close { reply: rtx },
            token: self.cancellation.token(),
        })
        .map_err(|_| DesktopError::closed())?;
        let _ = rrx.recv_timeout(CLOSE_TIMEOUT).map_err(|e| {
            DesktopError::timeout(format!("timed out closing native desktop worker: {e}"))
        })?;
        if let Some(done) = done {
            done.recv_timeout(CLOSE_TIMEOUT).map_err(|e| {
                DesktopError::timeout(format!("native desktop worker did not exit: {e}"))
            })?;
        }
        if let Some(join) = join {
            join.join().map_err(|_| {
                DesktopError::internal("native desktop worker panicked during close")
            })?;
        }
        Ok(())
    }
}

impl Drop for SessionCore {
    fn drop(&mut self) {
        self.cancellation.cancel_with(CancelReason::SessionDropped);
        let lifecycle = self.lifecycle.get_mut();
        if let Some(tx) = lifecycle.tx.take() {
            let (reply, _) = flume::bounded(1);
            let _ = tx.send(QueuedRequest {
                request: Request::Close { reply },
                token: self.cancellation.token(),
            });
        }
        let _ = lifecycle.join.take();
    }
}

fn response_unit(response: Response) -> CoreResult<()> {
    if matches!(response, Response::Unit) {
        Ok(())
    } else {
        Err(DesktopError::internal("unexpected desktop worker response"))
    }
}

fn unexpected<T>() -> CoreResult<T> {
    Err(DesktopError::internal("unexpected response"))
}

/// Persistent, serialized native desktop capture/input/accessibility session.
///
/// Every method is a blocking call that runs on a single worker thread; calls
/// are serialized in submission order. Dropping the session cancels pending
/// work and stops the worker.
pub struct DesktopSession {
    core: Arc<SessionCore>,
    /// Admission-time token for a native RPC; only the owning handle cancels on drop.
    admitted: Option<OperationToken>,
}

impl Drop for DesktopSession {
    fn drop(&mut self) {
        // A running call can retain SessionCore after the session handle dies.
        // Cancel that work now rather than waiting for the last Arc to disappear.
        if self.admitted.is_none() {
            self.core
                .cancellation
                .cancel_with(CancelReason::SessionDropped);
        }
    }
}

impl DesktopSession {
    pub fn new(options: Option<DesktopSessionOptions>) -> CoreResult<Self> {
        Ok(Self {
            core: SessionCore::new(DisplaySelector::parse(options.and_then(|o| o.display))),
            admitted: None,
        })
    }

    /// Asks the worker when it is idle, so permissions are read live. While
    /// another operation holds the worker, answers from the snapshot of the
    /// latest capabilities read or capture instead of blocking behind it.
    /// Worker, timeout, and cancellation failures are returned to the caller
    /// rather than reported as unavailable capabilities.
    pub fn capabilities(&self) -> CoreResult<DesktopCapabilities> {
        if self.core.in_flight.load(Ordering::Acquire) > 0
            && let Some(snapshot) = self.core.capabilities.lock().clone()
        {
            return Ok(snapshot);
        }
        match self.run(|reply| Request::Capabilities { reply })? {
            Response::Capabilities(c) => Ok(c),
            _ => unexpected(),
        }
    }

    /// Bind delayed RPC execution to the generation that admitted it, rather
    /// than whichever generation happens to be current when its thread runs.
    pub(crate) fn admit(&self) -> Self {
        Self {
            core: Arc::clone(&self.core),
            admitted: Some(self.operation_token()),
        }
    }

    fn operation_token(&self) -> OperationToken {
        self.admitted
            .clone()
            .unwrap_or_else(|| self.core.cancellation.token())
    }

    pub(crate) fn check_admission(&self) -> CoreResult<()> {
        self.operation_token().check()
    }

    fn run(&self, make: impl FnOnce(Reply) -> Request) -> CoreResult<Response> {
        self.core.call(self.operation_token(), make)
    }

    fn unit(&self, make: impl FnOnce(Reply) -> Request) -> CoreResult<()> {
        response_unit(self.run(make)?)
    }

    fn nodes(&self, make: impl FnOnce(Reply) -> Request) -> CoreResult<Vec<AxNode>> {
        match self.run(make)? {
            Response::Nodes(v) => Ok(v),
            _ => unexpected(),
        }
    }

    fn node(&self, make: impl FnOnce(Reply) -> Request) -> CoreResult<Option<AxNode>> {
        match self.run(make)? {
            Response::Node(v) => Ok(v),
            _ => unexpected(),
        }
    }

    pub fn list_displays(&self) -> CoreResult<Vec<DesktopDisplay>> {
        match self.run(|reply| Request::ListDisplays { reply })? {
            Response::Displays(v) => Ok(v),
            _ => unexpected(),
        }
    }

    pub fn list_windows(&self) -> CoreResult<Vec<DesktopWindow>> {
        match self.run(|reply| Request::ListWindows { reply })? {
            Response::Windows(v) => Ok(v),
            _ => unexpected(),
        }
    }

    pub fn window(&self, id: &str) -> CoreResult<DesktopWindow> {
        match self.run(|reply| Request::GetWindow {
            id: id.to_owned(),
            reply,
        })? {
            Response::Window(window) => Ok(window),
            _ => unexpected(),
        }
    }

    pub fn window_snapshot(&self) -> CoreResult<DesktopWindowSnapshot> {
        match self.run(|reply| Request::ListWindowSnapshot { reply })? {
            Response::WindowSnapshot(snapshot) => Ok(snapshot),
            _ => unexpected(),
        }
    }

    pub fn list_applications(
        &self,
        options: Option<ApplicationQuery>,
    ) -> CoreResult<Vec<Application>> {
        match self.run(|reply| Request::ListApplications {
            options: options.unwrap_or_default(),
            reply,
        })? {
            Response::Applications(value) => Ok(value),
            _ => unexpected(),
        }
    }

    pub fn open_application(
        &self,
        id: String,
        options: Option<ApplicationOpenOptions>,
    ) -> CoreResult<Application> {
        match self.run(|reply| Request::OpenApplication {
            id,
            options: options.unwrap_or_default(),
            reply,
        })? {
            Response::Application(value) => Ok(value),
            _ => unexpected(),
        }
    }

    /// Capture and accessibility share one serialized request. Neither a failed
    /// snapshot nor an abandoned reply replaces the last delivered input frame.
    pub fn observe(
        &self,
        target: String,
        caps: Option<CaptureCaps>,
        ax_options: Option<AxSnapshotOptions>,
    ) -> CoreResult<DesktopObservation> {
        match self.run(|reply| Request::Observe {
            target: Target::parse(&target),
            caps: caps.unwrap_or_default(),
            options: ax_options.unwrap_or_default(),
            reply,
        })? {
            Response::Observation(value) => Ok(value),
            _ => unexpected(),
        }
    }

    pub fn menu_items(
        &self,
        target: String,
        path: Option<Vec<String>>,
    ) -> CoreResult<Vec<DesktopMenuItem>> {
        match self.run(|reply| Request::MenuItems {
            target: Target::parse(&target),
            path: path.unwrap_or_default(),
            reply,
        })? {
            Response::MenuItems(value) => Ok(value),
            _ => unexpected(),
        }
    }

    pub fn menu_select(&self, target: String, path: Vec<String>) -> CoreResult<()> {
        self.unit(move |reply| Request::MenuSelect {
            target: Target::parse(&target),
            path,
            reply,
        })
    }

    pub fn bring_to_current_space(&self, window_id: String) -> CoreResult<()> {
        self.unit(move |reply| Request::BringToCurrentSpace {
            id: window_id,
            reply,
        })
    }

    pub fn hold_keys(
        &self,
        target: String,
        keys: Vec<String>,
        options: HoldOptions,
    ) -> CoreResult<()> {
        let keys = parse_keys(&keys)?;
        if keys.is_empty() {
            return Err(DesktopError::invalid_key(
                "holdKeys requires at least one key",
            ));
        }
        let duration = hold_duration(options.duration)?;
        self.unit(move |reply| Request::HoldKeys {
            target: Target::parse(&target),
            keys,
            duration,
            takeover: options.takeover,
            reply,
        })
    }

    pub fn hold_mouse(
        &self,
        target: String,
        x: f64,
        y: f64,
        options: HoldOptions,
    ) -> CoreResult<()> {
        let duration = hold_duration(options.duration)?;
        let button = MouseButton::parse(options.button.as_deref())?;
        let keys = parse_keys(options.keys.as_deref().unwrap_or_default())?;
        self.unit(move |reply| Request::HoldMouse {
            target: Target::parse(&target),
            x,
            y,
            button,
            keys,
            duration,
            takeover: options.takeover,
            reply,
        })
    }

    /// Native ownership only. Human approval is required by the host before
    /// calling this method.
    pub fn acquire_control(&self) -> CoreResult<DesktopControlState> {
        let c = Arc::clone(&self.core);
        let token = self.operation_token();
        token.check()?;
        if c.lifecycle.lock().closed {
            return Err(DesktopError::closed());
        }
        c.cancellation.acquire_control(&token)?;
        if let Err(error) = token.check() {
            c.cancellation.release_control();
            return Err(error);
        }
        Ok(DesktopControlState {
            active: c.cancellation.control_active(),
        })
    }

    pub fn release_control(&self) {
        self.core.cancellation.release_control();
    }

    pub fn control_state(&self) -> DesktopControlState {
        DesktopControlState {
            active: self.core.cancellation.control_active(),
        }
    }

    #[cfg(test)]
    pub(crate) fn token_for_test(&self) -> OperationToken {
        self.core.cancellation.token()
    }

    /// Retire queued work from a completed helper without revoking task control.
    pub fn retire(&self) {
        self.core.cancellation.retire();
    }

    pub fn capture(&self, target: String, caps: Option<CaptureCaps>) -> CoreResult<DesktopCapture> {
        let target = Target::parse(&target);
        match self.run(|reply| Request::Capture {
            target,
            caps: caps.unwrap_or_default(),
            reply,
        })? {
            Response::Capture(v) => Ok(v),
            _ => unexpected(),
        }
    }

    /// Capture a fresh native-detail region without replacing the full input
    /// coordinate frame.
    pub fn capture_region(
        &self,
        target: String,
        region: CaptureRegion,
        caps: Option<CaptureCaps>,
    ) -> CoreResult<DesktopCapture> {
        let target = Target::parse(&target);
        match self.run(|reply| Request::CaptureRegion {
            target,
            region,
            caps: caps.unwrap_or_default(),
            reply,
        })? {
            Response::Capture(value) => Ok(value),
            _ => unexpected(),
        }
    }

    pub fn click(
        &self,
        target: String,
        x: f64,
        y: f64,
        opts: Option<PointerOptions>,
    ) -> CoreResult<()> {
        let options = ParsedPointerOptions::parse(opts)?;
        self.unit(move |reply| Request::Click {
            target: Target::parse(&target),
            x,
            y,
            options,
            reply,
        })
    }

    pub fn move_mouse(
        &self,
        target: String,
        x: f64,
        y: f64,
        opts: Option<PointerOptions>,
    ) -> CoreResult<()> {
        let takeover = ParsedPointerOptions::parse(opts)?.takeover;
        self.unit(move |reply| Request::MoveMouse {
            target: Target::parse(&target),
            x,
            y,
            takeover,
            reply,
        })
    }

    pub fn drag(
        &self,
        target: String,
        path: Vec<DesktopPoint>,
        opts: Option<PointerOptions>,
    ) -> CoreResult<()> {
        let options = ParsedPointerOptions::parse(opts)?;
        let path = path.into_iter().map(|p| (p.x, p.y)).collect();
        self.unit(move |reply| Request::Drag {
            target: Target::parse(&target),
            path,
            options,
            reply,
        })
    }

    pub fn scroll(
        &self,
        target: String,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        opts: Option<PointerOptions>,
    ) -> CoreResult<()> {
        let takeover = ParsedPointerOptions::parse(opts)?.takeover;
        self.unit(move |reply| Request::Scroll {
            target: Target::parse(&target),
            x,
            y,
            dx,
            dy,
            takeover,
            reply,
        })
    }

    pub fn type_text(
        &self,
        target: String,
        text: String,
        opts: Option<PointerOptions>,
    ) -> CoreResult<()> {
        let takeover = ParsedPointerOptions::parse(opts)?.takeover;
        self.unit(move |reply| Request::TypeText {
            target: Target::parse(&target),
            text,
            takeover,
            reply,
        })
    }

    pub fn key_chord(
        &self,
        target: String,
        keys: Vec<String>,
        opts: Option<PointerOptions>,
    ) -> CoreResult<()> {
        let keys = parse_keys(&keys)?;
        let takeover = ParsedPointerOptions::parse(opts)?.takeover;
        self.unit(move |reply| Request::KeyChord {
            target: Target::parse(&target),
            keys,
            takeover,
            reply,
        })
    }

    pub fn raise_window(&self, window_id: String) -> CoreResult<()> {
        self.unit(move |reply| Request::RaiseWindow {
            id: window_id,
            reply,
        })
    }

    pub fn ax_snapshot(
        &self,
        target: String,
        opts: Option<AxSnapshotOptions>,
    ) -> CoreResult<AxSnapshot> {
        match self.run(|reply| Request::AxSnapshot {
            target: Target::parse(&target),
            options: opts.unwrap_or_default(),
            reply,
        })? {
            Response::Snapshot(v) => Ok(v),
            _ => unexpected(),
        }
    }

    pub fn ax_query(&self, target: String, query: AxQuery) -> CoreResult<Vec<AxNode>> {
        self.nodes(move |reply| Request::AxQuery {
            target: Target::parse(&target),
            query,
            reply,
        })
    }

    /// Accessibility hit-test at global logical desktop coordinates; needs no
    /// prior capture.
    pub fn ax_element_at(&self, target: String, x: f64, y: f64) -> CoreResult<Option<AxNode>> {
        self.node(move |reply| Request::AxElementAt {
            target: Target::parse(&target),
            x,
            y,
            reply,
        })
    }

    pub fn ax_focused(&self) -> CoreResult<Option<AxNode>> {
        self.node(|reply| Request::AxFocused { reply })
    }

    pub fn ax_node(&self, reference: String) -> CoreResult<AxNode> {
        match self.run(|reply| Request::AxNode { reference, reply })? {
            Response::Node(Some(v)) => Ok(v),
            _ => unexpected(),
        }
    }

    pub fn ax_attributes(&self, reference: String) -> CoreResult<Vec<(String, String)>> {
        match self.run(|reply| Request::AxAttributes { reference, reply })? {
            Response::Attributes(v) => Ok(v),
            _ => unexpected(),
        }
    }

    pub fn ax_children(&self, reference: String) -> CoreResult<Vec<AxNode>> {
        self.nodes(move |reply| Request::AxChildren { reference, reply })
    }

    pub fn ax_parent(&self, reference: String) -> CoreResult<Option<AxNode>> {
        self.node(move |reply| Request::AxParent { reference, reply })
    }

    pub fn ax_perform(&self, reference: String, action: String) -> CoreResult<()> {
        self.unit(move |reply| Request::AxPerform {
            reference,
            action,
            reply,
        })
    }

    pub fn ax_set_value(&self, reference: String, value: String) -> CoreResult<()> {
        self.unit(move |reply| Request::AxSetValue {
            reference,
            value,
            reply,
        })
    }

    pub fn ax_focus(&self, reference: String) -> CoreResult<()> {
        self.unit(move |reply| Request::AxFocus { reference, reply })
    }

    pub fn ax_click(&self, reference: String, opts: Option<PointerOptions>) -> CoreResult<()> {
        let options = ParsedPointerOptions::parse(opts)?;
        self.unit(move |reply| Request::AxClick {
            reference,
            options,
            reply,
        })
    }

    /// Immediately cancel operations submitted before this call. Later
    /// operations may proceed.
    pub fn cancel(&self) {
        self.cancel_for(CancelReason::Cancelled);
    }

    /// Like [`Self::cancel`], recording who ended the work so cancelled
    /// operations can report it.
    pub(crate) fn cancel_for(&self, reason: CancelReason) {
        self.core.cancellation.cancel_with(reason);
    }

    pub fn close(&self) -> CoreResult<()> {
        self.cancel_for(CancelReason::SessionClosed);
        self.core.close()
    }
}

#[cfg(all(test, not(feature = "desktop")))]
mod native_absent_tests;
