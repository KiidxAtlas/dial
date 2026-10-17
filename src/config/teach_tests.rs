use super::*;

fn resolve(text: &str) -> Config {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    Config::from_table(&layout, toml::from_str(text).unwrap()).unwrap()
}

#[test]
fn teaching_needs_the_switch_and_a_local_main_model() {
    for local in [
        "provider='mlx'\nmodel='/models/qwen'\n",
        "provider='ollama'\nmodel='qwen3:8b'\n",
        "model='qwen'\n[endpoint]\nbase_url='http://192.168.1.20:1234/v1'\n",
    ] {
        assert!(!resolve(local).teaching(), "off by default: {local}");
        let on = resolve(&format!("{local}[improve]\nteach=true\n"));
        assert!(on.teaching(), "{local}");
    }
    for hosted in [
        "provider='together'\nmodel='meta-llama/Llama-3.3-70B-Instruct-Turbo'\n[improve]\nteach=true\n",
        "model='m'\n[endpoint]\nbase_url='https://api.example.com/v1'\n[improve]\nteach=true\n",
    ] {
        assert!(!resolve(hosted).teaching(), "{hosted}");
    }
}

#[test]
fn coding_keeps_recording_only_while_teaching() {
    let mut config = resolve("provider='mlx'\nmodel='/models/qwen'\n");
    config.improve.background = true;
    config.training.enabled = true;
    let coding = config.clone().for_coding();
    assert!(!coding.improve.enabled && !coding.training.enabled);
    config.improve.teach = true;
    config.improve.learn_from_sessions = false;
    let coding = config.for_coding();
    assert!(coding.improve.enabled && coding.improve.learn_from_sessions);
    assert!(coding.training.enabled, "saved training preference is kept");
    assert!(
        !coding.improve.background,
        "no background cycles from coding"
    );
    assert!(!coding.laya.enabled && !coding.memory.autolearn);
}
