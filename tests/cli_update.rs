use kroma::cli::{parse, Command};
use std::process::Command as Process;
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}
#[test]
fn update_flags_are_strict_and_global_options_do_not_become_task_text() {
    assert_eq!(
        parse(&args(&["--quiet", "update", "--check", "--channel=next"])).unwrap(),
        Command::Update {
            check: true,
            channel: "next".into()
        }
    );
    for values in [
        vec!["update", "--channel"],
        vec!["update", "--channel", "evil"],
        vec!["update", "--check", "--check"],
        vec!["update", "--channel=next", "--channel=latest"],
    ] {
        assert_eq!(parse(&args(&values)).unwrap_err().code, 2);
    }
    assert!(matches!(
        parse(&args(&["--print", "update"])).unwrap(),
        Command::Run(_)
    ));
    assert!(matches!(
        parse(&args(&["--", "update"])).unwrap(),
        Command::Run(_)
    ));
}
#[test]
fn update_help_and_native_upgrade_need_no_configuration_or_network() {
    for options in [
        vec!["update", "--help"],
        vec!["help", "update"],
        vec!["update"],
    ] {
        let output = Process::new(env!("CARGO_BIN_EXE_kroma"))
            .args(options)
            .env_remove("KROMA_MODEL")
            .env_remove("KROMA_API_KEY")
            .env("KROMA_BASE_URL", "http://127.0.0.1:1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("kroma update") || text.contains("cannot update itself"));
        assert!(!text.contains("TaskStarted"));
    }
}
