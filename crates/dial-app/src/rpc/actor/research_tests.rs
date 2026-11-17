use super::tests::with_runtime;
use super::*;

#[test]
fn research_routing_inspects_live_state_and_restores_compute_after_owner_refusal() {
    with_runtime(
        |config| {
            config.compute_locks = true;
            config.user_dir = config.state_dir.join("research-user");
        },
        |runtime| {
            runtime.acp = false;
            let state = runtime
                .handle_request(&json!({"method":"research_status"}))
                .unwrap();
            assert_eq!(state["session_version"]["id"], runtime.agent.version().id);
            assert!(state["steering"]["runtime"].is_object());
            runtime.compute = Some(crate::learn::auto::compute_lock(runtime.layout).unwrap());
            let request = json!({"method":"research_action","params":{"action":"promote_steering","name":"missing","confirm":true}});
            let error = runtime.handle_request(&request).unwrap_err();
            assert!(
                error.to_string().contains("pending steering vector"),
                "{error:#}"
            );
            assert!(
                runtime.compute.is_some(),
                "a refused owner operation must restore foreground compute"
            );
            let probe = crate::learn::auto::lock_file(&crate::config::paths::compute_lock_file(
                &runtime.layout.user_dir,
            ))
            .unwrap();
            let error = probe.try_lock().map_err(std::io::Error::from).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            let owner: Value = serde_json::from_slice(
                &std::fs::read(runtime.layout.user_dir.join("compute-owner.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(owner["model"], runtime.agent.model());
            assert_eq!(
                owner["state"],
                json!(runtime.agent.effective_config().state_dir)
            );
        },
    );
}

#[test]
fn research_validation_preserves_lease_and_cancellation_releases_it_without_waiting() {
    with_runtime(
        |config| config.compute_locks = true,
        |runtime| {
            runtime.compute = Some(crate::learn::auto::compute_lock(runtime.layout).unwrap());
            assert!(
                runtime
                    .research_action(&json!({"action":"evaluate","candidate":1,"confirm":false}))
                    .is_err()
            );
            assert!(
                runtime.compute.is_some(),
                "validation runs before resource teardown"
            );
            runtime.flag.store(true, Ordering::SeqCst);
            let error = runtime
                .research_action(&json!({"action":"evaluate","candidate":1,"confirm":true}))
                .unwrap_err();
            assert!(error.is::<crate::llm::Interrupted>());
            assert!(runtime.compute.is_none());
            let probe = crate::learn::auto::lock_file(&crate::config::paths::compute_lock_file(
                &runtime.layout.user_dir,
            ))
            .unwrap();
            probe.try_lock().unwrap();
        },
    );
}

#[test]
fn joined_guest_refuses_local_turn_mutations_before_goal_or_attachment_work() {
    use crate::collab::{
        guest::Guest,
        room::{self, Room},
    };
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = room::leave();
        }
    }
    with_runtime(
        |config| config.compute_locks = true,
        |runtime| {
            let _cleanup = Cleanup;
            room::with(|slot| {
                assert!(slot.is_none());
                *slot = Some(Room::Guest(Guest::fixture(&[])));
            });
            runtime.acp = false;
            let goal = serde_json::to_value(&runtime.agent.modes().goal).unwrap();
            let messages = runtime.agent.session().message_count();
            for request in [
                json!({"method":"goal","params":{"action":"set","objective":"must not persist"}}),
                json!({"method":"goal","params":{"action":"guided","idea":"must not persist"}}),
                json!({"method":"goal","params":{"action":"resume"}}),
                json!({"method":"send","params":{"text":"/skill:unknown should stay remote"}}),
                json!({"method":"retry","params":{}}),
                json!({"method":"live_listen","params":{}}),
            ] {
                let error = runtime.handle_request(&request).unwrap_err();
                assert_eq!(
                    error.downcast_ref::<RequestError>().unwrap().code,
                    ErrorCode::Refused,
                    "{request}"
                );
                assert_eq!(
                    serde_json::to_value(&runtime.agent.modes().goal).unwrap(),
                    goal
                );
                assert_eq!(runtime.agent.session().message_count(), messages);
                assert!(
                    runtime.compute.is_none(),
                    "remote guest input must not acquire local compute"
                );
            }
            let error = runtime.handle_request(&json!({"method":"send","params":{"text":"remote","images":["nonexistent-private-image.png"]}})).unwrap_err();
            assert_eq!(
                error.downcast_ref::<RequestError>().unwrap().code,
                ErrorCode::InvalidParams
            );
        },
    );
}
