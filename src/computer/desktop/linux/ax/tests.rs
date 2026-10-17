use super::*;

#[test]
fn generic_press_never_selects_an_unrelated_editing_action() {
    let actions = ["buffer.delete-line", "clipboard.cut", "activate"];
    assert_eq!(action_index(actions.into_iter(), "press"), Some(2));
    assert_eq!(action_index(actions[..2].iter().copied(), "press"), None);
    assert_eq!(
        action_index(actions.into_iter(), "buffer.delete-line"),
        Some(0)
    );
    assert_eq!(action_index(std::iter::empty(), "press"), None);
}

#[test]
fn frame_ownership_requires_unique_process_and_title() {
    let window = |id: &str, pid: u32| DesktopWindow {
        id: id.to_owned(),
        pid: Some(pid),
        title: "Document".into(),
        app: "Editor".into(),
        x: 0,
        y: 0,
        width: 100,
        height: 100,
        focused: false,
    };
    let mut windows = vec![window("1", 10), window("2", 20)];
    assert_eq!(
        native_window_for_frame(&windows, 20, "Document").unwrap(),
        "2"
    );
    assert!(native_window_for_frame(&windows, 30, "Document").is_err());
    windows.push(window("3", 20));
    assert!(native_window_for_frame(&windows, 20, "Document").is_err());
}

#[test]
fn focused_search_skips_null_child_refs() {
    use atspi::zbus::{
        connection::Builder as ConnectionBuilder, interface, zvariant::OwnedObjectPath,
    };

    /// Minimal `org.a11y.atspi.Accessible` node: a state set plus `(name,
    /// path)` children.
    struct Node {
        state: Vec<u32>,
        children: Vec<(String, OwnedObjectPath)>,
    }

    #[interface(name = "org.a11y.atspi.Accessible")]
    impl Node {
        fn get_state(&self) -> Vec<u32> {
            self.state.clone()
        }

        fn get_children(&self) -> Vec<(String, OwnedObjectPath)> {
            self.children.clone()
        }
    }

    const PEER: &str = ":1.7";
    let path = |p: &str| OwnedObjectPath::try_from(p).unwrap();
    let focused = State::Focused as u64;
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let found = rt.block_on(async {
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let root = Node {
            state: vec![0, 0],
            // Chromium frames report children that are all the AT-SPI null object.
            children: vec![
                (PEER.to_owned(), path("/org/a11y/atspi/null")),
                (PEER.to_owned(), path("/focus")),
            ],
        };
        let leaf = Node {
            state: vec![focused as u32, (focused >> 32) as u32],
            children: Vec::new(),
        };
        let guid = atspi::zbus::Guid::generate();
        let server = ConnectionBuilder::unix_stream(server)
            .server(guid)
            .unwrap()
            .p2p()
            .serve_at("/root", root)
            .unwrap()
            .serve_at("/focus", leaf)
            .unwrap()
            .build();
        let client = ConnectionBuilder::unix_stream(client).p2p().build();
        let (server, client) = tokio::try_join!(server, client).unwrap();
        let root = ObjectRefOwned::from_static_str_unchecked(PEER, "/root");
        let found = AtSpiAx::find_focused(&client, root, 0).await;
        drop(server);
        found
    });
    let found = found.expect("null child refs must not fail the focused search");
    assert_eq!(
        found
            .map(|object| object.path_as_str().to_owned())
            .as_deref(),
        Some("/focus")
    );
}
