use super::*;

#[test]
fn computer_explicit_release_uses_native_rpc_and_waits_for_acknowledgement() {
    let result = run_worker_mode(
        "const before = await desktop.control.state();
         await desktop.control.release();
         const after = await desktop.control.state();
         return { before: before.active, after: after.active };",
        |_| json!([]),
        false,
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(
        result["payload"]["returnValue"],
        json!({"before": true, "after": false})
    );
}
