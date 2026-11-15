//! Runtime validation must work without an upstream schema or network access.
use schlange_github::config::ConfigCheck;

#[test]
fn embedded_validator_accepts_supported_config_and_rejects_unsafe_shapes() {
    let check = ConfigCheck::embedded().unwrap();
    check
        .check(
            "arztagent.yml",
            include_str!("../../schlange-config/tests/golden/arztagent.yml"),
        )
        .unwrap();
    let valid = "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions: [base = main]\npull_request_rules:\n  - name: enqueue\n    conditions: []\n    actions: {queue: null}\n";
    check.check(".mergify.yml", valid).unwrap();
    check
        .check(
            ".mergify.yml",
            &format!("{valid}merge_queue: {{mode: isolated}}\n"),
        )
        .unwrap();
    for invalid in [
        valid.replace("queue_conditions", "queue_conditons"),
        valid.replace("name: default", "name: default\n    batch_size: false"),
        valid.replace(
            "name: default",
            "name: default\n    commit_message_template: custom",
        ),
        valid.replace(
            "name: default",
            "name: default\n    batch_size: {min: nonsense, max: 3}",
        ),
        valid.replace(
            "name: default",
            "name: default\n    allow_queue_branch_edit: 'false'",
        ),
        valid.replace("base = main", "nosuchattribute = main"),
        valid.replace("base = main", "base ~= ("),
        valid.replace(
            "name: default",
            "name: default\n    batch_max_wait_time: 18446744073709551615 days",
        ),
        valid.replace("    conditions: []\n", ""),
        valid.replace("queue: null", "queue: {nmae: default}"),
        valid.replace("queue: null", "queue: false"),
        format!("{valid}unknown_option: true\n"),
        format!("{valid}merge_queue: {{mode: nonsense}}\n"),
        format!("{valid}merge_queue: {{mode: ordered}}\n"),
        valid.replace(
            "name: default",
            "name: default\n    batch_size: {min: 4, max: 2}",
        ),
        format!("{valid}commands_restrictions: {{queue: {{conditons: []}}}}\n"),
        format!("{valid}scopes: {{source: {{manual: true}}}}\n"),
    ] {
        assert!(
            check.check(".mergify.yml", &invalid).is_err(),
            "accepted invalid runtime config: {invalid}"
        );
    }
}

#[test]
fn explicit_missing_schema_does_not_silently_fall_back() {
    assert!(
        ConfigCheck::locate(
            Some(std::path::Path::new("/no-such-schema.json")),
            std::path::Path::new("/tmp")
        )
        .is_err()
    );
}

#[test]
fn inherited_runtime_config_is_also_validated() {
    let check = ConfigCheck::embedded().unwrap();
    let error = check
        .check_extended(
            ".mergify.yml",
            "extends: org/shared\n",
            Some("org/local"),
            |_| Ok("unknown_option: true\n".into()),
        )
        .unwrap_err();
    assert!(error.contains("unknown_option"), "{error}");
}
