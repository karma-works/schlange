//! Fuzzing the command parser: arbitrary Unicode never panics, letter case of the prefix does
//! not matter, text below the command line is ignored, text before it disables it, quoting.

use proptest::prelude::*;

use schlange_core::commands::{Name, Parsed, parse};

fn cases() -> u32 {
    std::env::var("SCHLANGE_PBT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512)
}

fn random_case(s: &str, bits: u64) -> String {
    s.chars()
        .enumerate()
        .map(|(i, c)| {
            if bits & (1 << (i % 64)) != 0 {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn never_panics(body in "\\PC*") {
        let _ = parse(&body);
    }

    #[test]
    fn never_panics_near_a_prefix(rest in "\\PC*", prefix in prop::sample::select(vec!["@mergifyio", "@mergify", "@Mergifyio ", "@MERGIFY queue", "@mergifyio queue \""])) {
        let _ = parse(&format!("{prefix}{rest}"));
    }

    #[test]
    fn prefix_case_does_not_matter(bits in any::<u64>(), io in any::<bool>(), cmd in prop::sample::select(vec!["queue", "dequeue", "refresh", "rebase"])) {
        let prefix = if io { "@mergifyio" } else { "@mergify" };
        let body = format!("{} {cmd}", random_case(prefix, bits));
        prop_assert!(matches!(parse(&body), Parsed::Command { .. }), "{}", body);
    }

    #[test]
    fn lines_below_are_left_alone(below in "\\PC*", newline in prop::sample::select(vec!["\n", "\r\n"])) {
        let body = format!("@mergifyio queue default{newline}{below}");
        prop_assert_eq!(parse(&body), Parsed::Command { name: Name::Queue, args: vec!["default".into()] });
    }

    #[test]
    fn a_line_of_text_before_disables_it(before in "[^\\s]\\PC{0,40}") {
        let first_line = before.lines().next().unwrap_or("").trim();
        prop_assume!(!first_line.to_ascii_lowercase().starts_with("@mergify") && !first_line.to_ascii_lowercase().starts_with("@schlange"));
        let body = format!("{before}\n@mergifyio queue");
        prop_assert_eq!(parse(&body), Parsed::NotACommand);
    }

    #[test]
    fn quoted_queue_names_round_trip(name in "[a-zA-Z0-9 _./äöü€-]{1,20}") {
        let quoted = format!("@mergifyio queue \"{name}\"");
        prop_assert_eq!(parse(&quoted), Parsed::Command { name: Name::Queue, args: vec![name.clone()] });
    }

    #[test]
    fn unicode_lookalikes_are_not_the_prefix(c in prop::sample::select(vec!["＠mergifyio", "@mеrgifyio", "@MERGİFYIO", "@mergifyıo"])) {
        prop_assert_eq!(parse(&format!("{c} queue")), Parsed::NotACommand);
    }
}
