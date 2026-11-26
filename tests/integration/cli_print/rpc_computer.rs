//! Desktop uses ordinary saved settings and agent approvals, not a privileged input RPC.
use super::*;

fn start(env: &Env, server: &Server) -> RpcChild {
    // Pin the offer so apply_settings reports whether the tool is actually registered,
    // independent of the model's deferred-tool discovery budget.
    std::fs::write(
        env.home.path().join("config.toml"),
        "approval = 'ask'\nintegrations = []\n[harness]\ntools = ['read', 'computer']\n",
    )
    .unwrap();
    RpcChild::start(env, server)
}

fn toggle(rpc: &mut RpcChild, enabled: bool, id: u64) {
    let value = if enabled {
        json!(["computer"])
    } else {
        json!([])
    };
    rpc.send(
        id,
        "set_setting",
        json!({"scope":"project","key":"integrations","value":value}),
    );
    assert_eq!(rpc.result(id)["saved"], true);
    rpc.send(id + 1, "apply_settings", json!({}));
    let applied = rpc.result(id + 1);
    assert_eq!(
        applied["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("computer")),
        enabled
    );
    rpc.send(id + 2, "computer_status", json!({}));
    assert_eq!(rpc.result(id + 2)["enabled"], enabled);
}

#[test]
fn settings_enable_and_remove_the_computer_tool_in_the_running_session() {
    let env = Env::new();
    let server = Server::new(vec![
        call("computer", json!({"action":"capabilities"})),
        text("done"),
    ]);
    let mut rpc = start(&env, &server);
    toggle(&mut rpc, true, 1);
    rpc.send(4, "send", json!({"text":"Report computer capabilities."}));
    let (_, events) = rpc.result_with_events(4);
    let result = events
        .iter()
        .find(|e| e["event"] == "tool_result" && e["name"] == "computer")
        .unwrap();
    assert!(result["data"]["backend"].is_string(), "{result}");
    toggle(&mut rpc, false, 5);
    rpc.send(8, "shutdown", json!({}));
    rpc.result(8);
}

#[cfg(windows)]
#[test]
#[ignore = "requires an interactive Windows desktop; opens its own disposable window"]
fn approved_computer_run_captures_clicks_and_types_into_its_own_window() {
    use std::time::Instant;

    struct Fixture(std::process::Child);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let env = Env::new();
    let dir = tempfile::tempdir().unwrap();
    let fixture = Fixture(Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", r"
            $ErrorActionPreference = 'Stop'
            Add-Type -AssemblyName System.Windows.Forms
            $form = New-Object Windows.Forms.Form
            $form.Text = 'Dial computer test'
            $box = New-Object Windows.Forms.TextBox
            $box.Multiline = $true
            $box.Dock = 'Fill'
            $form.Controls.Add($box)
            $box.Add_MouseDown({ [IO.File]::WriteAllText('clicked', 'yes') })
            $box.Add_TextChanged({ [IO.File]::WriteAllText('received', $box.Text) })
            $form.Add_Shown({ $box.Focus(); [IO.File]::WriteAllText('hwnd', $form.Handle.ToInt64().ToString()) })
            [Windows.Forms.Application]::Run($form)
        "])
        .current_dir(dir.path()).stdout(Stdio::null()).spawn().unwrap());
    let wait_file = |name: &str, ready: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(s) = std::fs::read_to_string(dir.path().join(name))
                && ready(&s)
            {
                break s;
            }
            assert!(Instant::now() < deadline, "fixture never reported {name}");
            thread::sleep(Duration::from_millis(50));
        }
    };
    let hwnd = wait_file("hwnd", &|v| v.parse::<usize>().is_ok());
    let expected = "Dial Windows check";
    let server = Server::new(vec![
        call(
            "computer",
            json!({
                "action":"run", "args":[hwnd,fixture.0.id(),expected],
                "fn":"async ({desktop}, id, pid, text) => {
            const w = await desktop.window(id);
            if (w.pid !== pid || w.title !== 'Dial computer test') throw Error('wrong window');
            const shot = await w.screenshot({silent:true});
            await w.click(Math.floor(shot.width/2), Math.floor(shot.height/2), {takeover:false});
            await w.type(text, {takeover:false});
            return shot;
        }"
            }),
        ),
        text("done"),
    ]);
    let mut rpc = start(&env, &server);
    toggle(&mut rpc, true, 1);
    rpc.send(
        4,
        "send",
        json!({"text":"Exercise the disposable test window."}),
    );
    let approval = loop {
        let event = rpc.next();
        assert_ne!(event["id"], 4, "turn ended before requesting approval");
        if event["event"] == "approval" {
            break event;
        }
    };
    assert_eq!(approval["tool"], "computer");
    assert!(!dir.path().join("clicked").exists() && !dir.path().join("received").exists());
    rpc.send(
        5,
        "approve",
        json!({"approval_id":approval["approval_id"],"decision":"allow"}),
    );
    let (_, events) = rpc.result_with_events(4);
    let result = events
        .iter()
        .find(|e| e["event"] == "tool_result" && e["name"] == "computer")
        .unwrap();
    assert_eq!(
        result["data"]["capabilities"]["backend"], "win32",
        "{}",
        result["text"]
    );
    let shot = &result["data"]["value"];
    let path = shot["path"].as_str().unwrap();
    let png = std::fs::read(path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(&png[..16], b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
    for (axis, bytes) in [("width", &png[16..20]), ("height", &png[20..24])] {
        let size = u32::from_be_bytes(bytes.try_into().unwrap());
        assert!(size > 1);
        assert_eq!(shot[axis], size);
    }
    wait_file("clicked", &|v| v == "yes");
    wait_file("received", &|v| v == expected);
    rpc.send(6, "shutdown", json!({}));
    rpc.result(6);
}
