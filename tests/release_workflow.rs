use serde_yaml::Value;
#[test]
fn release_workflow_is_parseable_opt_in_and_uses_the_checked_commit() {
    let release: Value =
        serde_yaml::from_str(include_str!("../.github/workflows/release.yml")).unwrap();
    let npm: Value = serde_yaml::from_str(include_str!("../.github/workflows/npm.yml")).unwrap();
    let trigger = &release["on"];
    assert_eq!(trigger.as_mapping().unwrap().len(), 1);
    assert_eq!(
        trigger["workflow_dispatch"]["inputs"]["publish"]["default"],
        false
    );
    assert_eq!(
        trigger["workflow_dispatch"]["inputs"]["confirm_ownership"]["default"],
        false
    );
    assert_eq!(
        trigger["workflow_dispatch"]["inputs"]["channel"]["default"],
        "next"
    );
    let jobs = &release["jobs"];
    assert_eq!(jobs["publish"]["environment"], "npm-release");
    assert_eq!(jobs["publish"]["permissions"]["id-token"], "write");
    assert_eq!(
        jobs["build"]["with"]["ref"],
        "${{ needs.prepare.outputs.commit }}"
    );
    assert!(npm["on"]["workflow_call"].is_mapping());
    assert_eq!(npm["permissions"]["contents"], "read");
}
