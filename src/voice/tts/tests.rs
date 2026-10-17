use super::*;
#[test]
fn safe_backend_args_keep_text_off_argv() {
    let env = VoiceEnv::default();
    let say = command(&TtsBackend::Say("say".into()), &env, Path::new("out.aiff")).unwrap();
    assert_eq!(
        say.arg_strings(),
        ["-f", "-", "-o", "out.aiff", "-r", "200"]
    );
    assert!(!say.arg_strings().iter().any(|a| a.contains("text")));
}
