use std::{
    collections::HashMap,
    fmt::Write as _,
    hash::{DefaultHasher, Hash, Hasher},
};

use super::{
    backend::AxBackend,
    error::{CoreResult, DesktopError},
    types::{AxNode, AxQuery, AxSnapshot, AxSnapshotOptions, DesktopWindow},
};

#[derive(Clone)]
pub enum AxHandle {
    #[cfg(dial_desktop_macos)]
    Mac(objc2_core_foundation::CFRetained<objc2_application_services::AXUIElement>),
    /// The element and its `RuntimeId`, read once when the handle is made;
    /// `None` when the element reports none.
    #[cfg(dial_desktop_windows)]
    Uia(uiautomation::UIElement, Option<Box<[i32]>>),
    #[cfg(dial_desktop_linux)]
    AtSpi(atspi::ObjectRefOwned),
    #[cfg(test)]
    Test(u64),
    /// A test element without an identity, like a UIA element whose
    /// `RuntimeId` cannot be read: all of them compare equal.
    #[cfg(test)]
    TestUnidentified(u64),
}

impl AxHandle {
    /// Whether the handle carries an identity naming its element across reads.
    /// Handles without one all compare equal, so they never enter the ref
    /// index.
    const fn identified(&self) -> bool {
        match self {
            #[cfg(dial_desktop_macos)]
            Self::Mac(_) => true,
            #[cfg(dial_desktop_windows)]
            Self::Uia(_, runtime_id) => runtime_id.is_some(),
            #[cfg(dial_desktop_linux)]
            Self::AtSpi(_) => true,
            #[cfg(test)]
            Self::Test(_) => true,
            #[cfg(test)]
            Self::TestUnidentified(_) => false,
            #[cfg(not(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux, test)))]
            _ => match *self {},
        }
    }
}

/// Two handles are equal when they name the same live element, however many
/// reads produced them: `CFEqual` on macOS, the `RuntimeId` on Windows, the bus
/// name and object path on Linux.
impl PartialEq for AxHandle {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            #[cfg(dial_desktop_macos)]
            (Self::Mac(a), Self::Mac(b)) => **a == **b,
            #[cfg(dial_desktop_windows)]
            (Self::Uia(_, a), Self::Uia(_, b)) => a == b,
            #[cfg(dial_desktop_linux)]
            (Self::AtSpi(a), Self::AtSpi(b)) => a == b,
            #[cfg(test)]
            (Self::Test(a), Self::Test(b)) => a == b,
            #[cfg(test)]
            (Self::TestUnidentified(_), Self::TestUnidentified(_)) => true,
            #[cfg(test)]
            _ => false,
            #[cfg(not(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux, test)))]
            _ => match *self {},
        }
    }
}

impl Eq for AxHandle {}

impl Hash for AxHandle {
    fn hash<H: Hasher>(&self, _state: &mut H) {
        match self {
            #[cfg(dial_desktop_macos)]
            Self::Mac(element) => (**element).hash(_state),
            #[cfg(dial_desktop_windows)]
            Self::Uia(_, runtime_id) => runtime_id.hash(_state),
            #[cfg(dial_desktop_linux)]
            Self::AtSpi(object) => object.hash(_state),
            #[cfg(test)]
            Self::Test(id) => id.hash(_state),
            #[cfg(test)]
            Self::TestUnidentified(_) => {}
            #[cfg(not(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux, test)))]
            _ => match *self {},
        }
    }
}

#[derive(Debug, Clone)]
pub struct AxProps {
    pub role: String,
    pub native_role: String,
    pub title: Option<String>,
    pub value: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub focused: bool,
    pub bounds: Option<AxBounds>,
    pub actions: Vec<String>,
    pub child_count: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct AxBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

struct Registered {
    handle: AxHandle,
    target_key: String,
    generation: u64,
    /// Role and label at registration. An identity read again with another
    /// role or label gets a new ref.
    fingerprint: u64,
}

pub struct AxRegistry {
    next_ref: u64,
    generations: HashMap<String, u64>,
    entries: HashMap<u64, Registered>,
    /// Ref of each element registered under each target, so an element read
    /// again keeps its ref.
    refs: HashMap<String, HashMap<AxHandle, u64>>,
}

impl Default for AxRegistry {
    fn default() -> Self {
        Self {
            next_ref: 1,
            generations: HashMap::new(),
            entries: HashMap::new(),
            refs: HashMap::new(),
        }
    }
}

impl AxRegistry {
    /// Starts a snapshot of `target`, returning its generation.
    pub(crate) fn begin_snapshot(&mut self, target: &str) -> u64 {
        let generation = self.generations.entry(target.to_string()).or_default();
        *generation = generation.saturating_add(1);
        *generation
    }

    /// Ends `target`'s snapshot `generation` once it has registered its
    /// elements: refs that neither it nor the previous snapshot registered
    /// expire, so an element missing from a single snapshot keeps its ref.
    pub(crate) fn end_snapshot(&mut self, target: &str, generation: u64) {
        self.evict(|entry| {
            entry.target_key == target && entry.generation.saturating_add(1) < generation
        });
    }

    pub(crate) fn current_generation(&mut self, target: &str) -> u64 {
        *self.generations.entry(target.to_string()).or_insert(1)
    }

    /// Returns the element's ref under `target`, minting one the first time it
    /// is seen there, and renews it for `target`'s `generation`. An identity
    /// read again with another role or label, or whose element from the last
    /// read is gone, gets a new ref; the old ref keeps the element it was read
    /// from until it expires like any ref not read again. An element without an
    /// identity gets a new ref on every read.
    pub(crate) fn register(
        &mut self,
        backend: &mut dyn AxBackend,
        target: &str,
        generation: u64,
        handle: AxHandle,
        props: &AxProps,
    ) -> String {
        let mut hasher = DefaultHasher::new();
        props.role.hash(&mut hasher);
        label(props).hash(&mut hasher);
        let fingerprint = hasher.finish();
        let identified = handle.identified();
        let known = if identified {
            self.refs
                .get(target)
                .and_then(|refs| refs.get(&handle))
                .copied()
        } else {
            None
        };
        let renewed = known.filter(|id| {
            self.entries.get(id).is_some_and(|entry| {
                entry.fingerprint == fingerprint && backend.alive(&entry.handle)
            })
        });
        let id = renewed.unwrap_or_else(|| {
            let id = self.next_ref;
            self.next_ref = self.next_ref.saturating_add(1);
            if identified {
                self.refs
                    .entry(target.to_string())
                    .or_default()
                    .insert(handle.clone(), id);
            }
            id
        });
        self.entries.insert(
            id,
            Registered {
                handle,
                target_key: target.to_string(),
                generation,
                fingerprint,
            },
        );
        self.enforce_cap();
        format!("e{id}")
    }

    pub(crate) fn resolve(&self, reference: &str) -> CoreResult<AxHandle> {
        let id = reference
            .strip_prefix('e')
            .and_then(|id| id.parse::<u64>().ok());
        id.and_then(|id| self.entries.get(&id))
            .map(|entry| entry.handle.clone())
            .ok_or_else(|| {
                DesktopError::stale_ref(format!("{reference} expired; re-run ax()/find()"))
            })
    }

    pub(crate) fn target(&self, reference: &str) -> CoreResult<String> {
        let id = reference
            .strip_prefix('e')
            .and_then(|id| id.parse::<u64>().ok());
        id.and_then(|id| self.entries.get(&id))
            .map(|entry| entry.target_key.clone())
            .ok_or_else(|| {
                DesktopError::stale_ref(format!("{reference} expired; re-run ax()/find()"))
            })
    }

    /// Drops expired refs. An identity a newer ref took over keeps pointing at
    /// that one.
    fn evict(&mut self, mut expired: impl FnMut(&Registered) -> bool) {
        let refs = &mut self.refs;
        self.entries.retain(|id, entry| {
            let expired = expired(entry);
            if expired
                && let Some(target_refs) = refs.get_mut(&entry.target_key)
                && target_refs.get(&entry.handle) == Some(id)
            {
                target_refs.remove(&entry.handle);
            }
            !expired
        });
    }

    fn enforce_cap(&mut self) {
        while self.entries.len() > 5_000 {
            let mut target_sizes: HashMap<&str, usize> = HashMap::new();
            for entry in self.entries.values() {
                *target_sizes.entry(&entry.target_key).or_default() += 1;
            }
            let Some(target) = target_sizes
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(target, _)| target.to_string())
            else {
                break;
            };
            let Some(oldest) = self
                .entries
                .values()
                .filter(|entry| entry.target_key == target)
                .map(|entry| entry.generation)
                .min()
            else {
                break;
            };
            self.evict(|entry| entry.target_key == target && entry.generation == oldest);
        }
    }
}

#[derive(Clone)]
struct WalkNode {
    handle: AxHandle,
    props: AxProps,
    children: Vec<Self>,
}

struct WalkState {
    visited: u32,
    skipped: u32,
    max_nodes: u32,
    max_depth: u32,
    truncated: bool,
}

fn walk_raw(
    backend: &mut dyn AxBackend,
    handle: AxHandle,
    depth: u32,
    state: &mut WalkState,
) -> CoreResult<Option<WalkNode>> {
    if depth > state.max_depth || state.visited >= state.max_nodes {
        state.truncated = true;
        return Ok(None);
    }
    state.visited += 1;
    let props = match backend.props(&handle) {
        Ok(props) => props,
        Err(_) if depth > 0 => {
            state.skipped = state.skipped.saturating_add(1);
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let child_handles = match backend.children(&handle) {
        Ok(children) => children,
        Err(_) if depth > 0 => {
            state.skipped = state.skipped.saturating_add(1);
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let mut children = Vec::new();
    for child in child_handles {
        if let Some(child) = walk_raw(backend, child, depth + 1, state)? {
            children.push(child);
        }
        if state.truncated && state.visited >= state.max_nodes {
            break;
        }
    }
    Ok(Some(WalkNode {
        handle,
        props,
        children,
    }))
}

fn named(props: &AxProps) -> bool {
    [&props.title, &props.value, &props.description]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty())
}
/// Display and match name for a node. Many toolbar controls — Chrome's
/// Back/Forward/Reload among them — carry no `AXTitle` and name themselves
/// through `AXDescription` alone.
fn label(props: &AxProps) -> Option<&str> {
    [props.title.as_deref(), props.description.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|label| !label.is_empty())
}
fn interactable(props: &AxProps) -> bool {
    !props.actions.is_empty()
        || matches!(
            props.role.as_str(),
            "button"
                | "checkbox"
                | "radio"
                | "textfield"
                | "textarea"
                | "link"
                | "menuitem"
                | "tab"
                | "slider"
                | "combobox"
                | "popupbutton"
                | "listitem"
                | "outlineitem"
                | "cell"
        )
}
/// Containers that keep their own line even around a single survivor: the role
/// itself tells the model where the child sits (a list, a toolbar, a scroll
/// area).
fn structural(role: &str) -> bool {
    matches!(
        role,
        "window"
            | "webarea"
            | "list"
            | "table"
            | "row"
            | "menu"
            | "menubar"
            | "tabgroup"
            | "toolbar"
            | "scrollarea"
            | "outline"
    )
}

/// Drops unnamed, inactionable nodes that carry nothing, while keeping every
/// survivor below them: a container with no survivors disappears, one wrapping
/// a single survivor (any role outside [`structural`], `group` included) gives
/// way to it, and one grouping several survivors stays. A container's role
/// never decides whether its content is shown; `AXSplitGroup`, which holds the
/// list and detail panes of Reminders, Contacts and Notes, is on no role list.
fn filter_node(mut node: WalkNode, all: bool) -> Option<WalkNode> {
    node.children = node
        .children
        .into_iter()
        .filter_map(|child| filter_node(child, all))
        .collect();
    if all || interactable(&node.props) || named(&node.props) {
        return Some(node);
    }
    match node.children.len() {
        0 => None,
        1 if !structural(&node.props.role) => node.children.pop(),
        _ => Some(node),
    }
}

fn escaped_truncated(value: &str, max: usize) -> String {
    let mut out: String = value.chars().take(max).collect();
    if value.chars().count() > max {
        out.push('…');
    }
    out.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

pub fn node_to_ax_node(reference: String, props: AxProps) -> AxNode {
    let (x, y, width, height) = props.bounds.map_or((None, None, None, None), |b| {
        (Some(b.x), Some(b.y), Some(b.width), Some(b.height))
    });
    AxNode {
        ref_: reference,
        role: props.role,
        native_role: props.native_role,
        title: props.title,
        value: props.value,
        description: props.description,
        enabled: props.enabled,
        focused: props.focused,
        x,
        y,
        width,
        height,
        actions: (!props.actions.is_empty()).then_some(props.actions),
        child_count: props.child_count,
    }
}

fn format_tree(
    node: WalkNode,
    depth: usize,
    window: &DesktopWindow,
    backend: &mut dyn AxBackend,
    registry: &mut AxRegistry,
    target: &str,
    generation: u64,
    text: &mut String,
    nodes: &mut u32,
) {
    let reference = registry.register(backend, target, generation, node.handle, &node.props);
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&"  ".repeat(depth));
    text.push_str("- ");
    text.push_str(&node.props.role);
    if let Some(label) = label(&node.props) {
        let _ = write!(text, " \"{}\"", escaped_truncated(label, 80));
    }
    let _ = write!(text, " [ref={reference}]");
    // A role alone is not a promise that AXPress is supported. Expose other
    // advertised actions instead of encouraging a failed press (or treating a
    // context-menu action as activation).
    if !node.props.actions.is_empty()
        && !node
            .props
            .actions
            .iter()
            .any(|action| matches!(action.as_str(), "press" | "AXPress"))
    {
        text.push_str(" actions=[");
        for (index, action) in node.props.actions.iter().take(8).enumerate() {
            if index > 0 {
                text.push_str(", ");
            }
            text.push_str(&escaped_truncated(action, 80));
        }
        if node.props.actions.len() > 8 {
            text.push_str(", …");
        }
        text.push(']');
    }
    if depth == 0 {
        let _ = write!(text, " app={}", window.app);
    }
    if let Some(value) = node
        .props
        .value
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let _ = write!(text, ": \"{}\"", escaped_truncated(value, 80));
    }
    if !node.props.enabled {
        text.push_str(" (disabled)");
    }
    // The root's own AXFocused only reflects app-local focus; report the global
    // roster flag instead.
    let focused = if depth == 0 {
        window.focused
    } else {
        node.props.focused
    };
    if focused {
        text.push_str(" (focused)");
    }
    *nodes += 1;
    for child in node.children {
        format_tree(
            child,
            depth + 1,
            window,
            backend,
            registry,
            target,
            generation,
            text,
            nodes,
        );
    }
}

pub fn snapshot(
    backend: &mut dyn AxBackend,
    registry: &mut AxRegistry,
    window: &DesktopWindow,
    options: &AxSnapshotOptions,
) -> CoreResult<AxSnapshot> {
    let target = &window.id;
    let generation = registry.begin_snapshot(target);
    let root = backend.window_root(window)?;
    let mut state = WalkState {
        visited: 0,
        skipped: 0,
        max_nodes: options.max_nodes.unwrap_or(800).max(1),
        max_depth: options.max_depth.unwrap_or(24),
        truncated: false,
    };
    let root = walk_raw(backend, root, 0, &mut state)?
        .and_then(|node| filter_node(node, options.all.unwrap_or(false)));
    let mut text = String::new();
    let mut node_count = 0;
    if let Some(root) = root {
        format_tree(
            root,
            0,
            window,
            backend,
            registry,
            target,
            generation,
            &mut text,
            &mut node_count,
        );
    }
    registry.end_snapshot(target, generation);
    if state.truncated {
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = write!(text, "… truncated ({} nodes)", state.visited);
    }
    if state.skipped > 0 {
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = write!(text, "… skipped {} unreadable nodes", state.skipped);
    }
    Ok(AxSnapshot {
        text,
        node_count,
        truncated: state.truncated,
    })
}

pub fn query(
    backend: &mut dyn AxBackend,
    registry: &mut AxRegistry,
    window: &DesktopWindow,
    query: &AxQuery,
) -> CoreResult<Vec<AxNode>> {
    let target = &window.id;
    let generation = registry.current_generation(target);
    let root = backend.window_root(window)?;
    let mut state = WalkState {
        visited: 0,
        skipped: 0,
        max_nodes: 5_000,
        max_depth: 24,
        truncated: false,
    };
    let Some(root) = walk_raw(backend, root, 0, &mut state)? else {
        return Ok(Vec::new());
    };
    let role = query.role.as_deref().map(str::to_lowercase);
    let title = query.title.as_deref().map(str::to_lowercase);
    let value = query.value.as_deref().map(str::to_lowercase);
    let limit = query.limit.unwrap_or(100).min(5_000) as usize;
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        stack.extend(node.children.iter().rev().cloned());
        let contains = |actual: Option<&str>, expected: Option<&String>| {
            expected.is_none_or(|needle| {
                actual.is_some_and(|text| text.to_lowercase().contains(needle))
            })
        };
        if role
            .as_ref()
            .is_none_or(|expected| node.props.role.to_lowercase() == *expected)
            && contains(label(&node.props), title.as_ref())
            && contains(node.props.value.as_deref(), value.as_ref())
        {
            if result.len() >= limit {
                break;
            }
            let reference =
                registry.register(backend, target, generation, node.handle, &node.props);
            result.push(node_to_ax_node(reference, node.props));
        }
    }
    Ok(result)
}

pub fn register_node(
    backend: &mut dyn AxBackend,
    registry: &mut AxRegistry,
    target: &str,
    handle: AxHandle,
) -> CoreResult<AxNode> {
    let props = backend.props(&handle)?;
    let generation = registry.current_generation(target);
    let reference = registry.register(backend, target, generation, handle, &props);
    Ok(node_to_ax_node(reference, props))
}

pub fn element_at_node(
    backend: &mut dyn AxBackend,
    registry: &mut AxRegistry,
    target: &str,
    x: f64,
    y: f64,
) -> CoreResult<Option<AxNode>> {
    let Some(handle) = backend.element_at(x, y)? else {
        return Ok(None);
    };
    register_node(backend, registry, target, handle).map(Some)
}

pub fn ax_press(backend: &mut dyn AxBackend, handle: &AxHandle) -> CoreResult<()> {
    backend.perform(handle, "press")
}

/// Maps a raw `AX*` macOS accessibility role onto the cross-platform role
/// vocabulary. Compiled only where it has a caller (macOS backend + tests).
#[cfg(any(dial_desktop_macos, test))]
pub fn normalize_role_macos(native: &str) -> String {
    match native {
        "AXTextArea" => "textarea",
        "AXTextField" => "textfield",
        "AXPopUpButton" => "popupbutton",
        "AXRadioButton" => "radio",
        "AXCheckBox" => "checkbox",
        "AXStaticText" => "statictext",
        "AXScrollArea" => "scrollarea",
        "AXTabGroup" => "tabgroup",
        "AXWebArea" => "webarea",
        "AXRow" => "row",
        "AXCell" => "cell",
        "AXOutline" => "outline",
        _ => native.strip_prefix("AX").unwrap_or(native),
    }
    .to_ascii_lowercase()
}
#[cfg(any(dial_desktop_windows, test))]
pub fn normalize_role_uia(native: &str) -> String {
    match native {
        "Edit" => "textfield",
        "Document" => "textarea",
        "Text" => "statictext",
        "Hyperlink" => "link",
        "Pane" => "group",
        "TabItem" => "tab",
        "Tab" => "tabgroup",
        "DataItem" => "listitem",
        "DataGrid" => "table",
        "SplitButton" => "popupbutton",
        other => return other.to_ascii_lowercase(),
    }
    .to_string()
}
#[cfg(any(dial_desktop_linux, test))]
pub fn normalize_role_atspi(native: &str, multiline: bool) -> String {
    match native.to_ascii_lowercase().as_str() {
        "push button" | "toggle button" => "button".into(),
        "entry" | "text" if multiline => "textarea".into(),
        "entry" | "text" => "textfield".into(),
        "label" => "statictext".into(),
        "page tab" => "tab".into(),
        "page tab list" => "tabgroup".into(),
        "table cell" => "cell".into(),
        "tree" => "outline".into(),
        "tree item" => "outlineitem".into(),
        "frame" | "dialog" => "window".into(),
        other => other.replace(' ', ""),
    }
}

#[cfg(test)]
mod tests;
