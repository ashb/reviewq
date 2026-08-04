//! Reason strings are the queue's user interface, so their exact rendering is
//! pinned here. A diff in this snapshot is a deliberate UX change, never an
//! incidental one.

use reviewq_core::model::AttentionReason;

/// Every variant, including the singular/plural branches, in priority order.
fn every_rendering() -> Vec<AttentionReason> {
    vec![
        AttentionReason::Mention {
            by: "potiuk".into(),
        },
        AttentionReason::ThreadReply {
            by: "uranusjr".into(),
            threads: 1,
        },
        AttentionReason::ThreadReply {
            by: "uranusjr".into(),
            threads: 3,
        },
        AttentionReason::ResolvedUnanswered {
            by: "kaxil".into(),
            threads: 1,
        },
        AttentionReason::ResolvedUnanswered {
            by: "kaxil".into(),
            threads: 2,
        },
        AttentionReason::ReReview {
            new_commits: 1,
            since_sha: "abc123f8901234567890123456789012345678ab".into(),
        },
        AttentionReason::ReReview {
            new_commits: 3,
            since_sha: "abc123f8901234567890123456789012345678ab".into(),
        },
        AttentionReason::ReviewRequested { team: None },
        AttentionReason::ReviewRequested {
            team: Some("apache/airflow-committers".into()),
        },
        AttentionReason::NeedsFirstLook {
            rule: "label area:task-sdk".into(),
        },
        AttentionReason::NeedsFirstLook {
            rule: "path task-sdk/**".into(),
        },
        AttentionReason::NeedsFirstLook {
            rule: "author FIRST_TIME_CONTRIBUTOR".into(),
        },
        AttentionReason::NeedsFirstLook {
            rule: "milestone 3.2".into(),
        },
    ]
}

#[test]
fn reason_strings_are_stable() {
    let rendered: Vec<String> = every_rendering()
        .iter()
        .map(|reason| format!("{:<20} {reason}", reason.discriminant()))
        .collect();

    insta::assert_snapshot!(rendered.join("\n"));
}
