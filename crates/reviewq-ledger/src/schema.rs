// @generated automatically by Diesel CLI.

diesel::table! {
    attention (repo_id, pr_number, reason) {
        repo_id -> BigInt,
        pr_number -> BigInt,
        reason -> Text,
        since -> Text,
        payload -> Text,
    }
}

diesel::table! {
    labels (repo_id, name) {
        repo_id -> BigInt,
        name -> Text,
        color -> Text,
    }
}

diesel::table! {
    my_state (repo_id, number) {
        repo_id -> BigInt,
        number -> BigInt,
        last_reviewed_sha -> Nullable<Text>,
        last_verdict -> Nullable<Text>,
        last_action_at -> Nullable<Text>,
        done_sha -> Nullable<Text>,
        snoozed_until -> Nullable<Text>,
        muted -> Bool,
        deferred_at -> Nullable<Text>,
        done_at -> Nullable<Text>,
    }
}

diesel::table! {
    prs (repo_id, number) {
        repo_id -> BigInt,
        number -> BigInt,
        title -> Text,
        author -> Text,
        author_association -> Text,
        head_sha -> Text,
        is_draft -> Bool,
        state -> Text,
        updated_at -> Text,
        labels -> Text,
        milestone -> Nullable<Text>,
        files -> Nullable<Text>,
        files_truncated -> Bool,
        tracked_reason -> Nullable<Text>,
        first_seen_at -> Text,
        detail_synced_at -> Nullable<Text>,
        body -> Nullable<Text>,
        base_ref -> Text,
        after_merge -> Bool,
        untracked_at -> Nullable<Text>,
        created_at -> Nullable<Text>,
    }
}

diesel::table! {
    repos (id) {
        id -> BigInt,
        host -> Text,
        owner -> Text,
        name -> Text,
    }
}

diesel::table! {
    reviewers (repo_id, pr_number, login) {
        repo_id -> BigInt,
        pr_number -> BigInt,
        login -> Text,
        verdict -> Text,
        submitted_at -> Text,
    }
}

diesel::table! {
    sync_meta (repo_id, key) {
        repo_id -> BigInt,
        key -> Text,
        value -> Text,
    }
}

diesel::table! {
    threads (thread_id) {
        thread_id -> Text,
        repo_id -> BigInt,
        pr_number -> BigInt,
        i_own -> Bool,
        is_resolved -> Bool,
        resolved_by -> Nullable<Text>,
        last_comment_author -> Nullable<Text>,
        last_comment_at -> Nullable<Text>,
        my_last_comment_at -> Nullable<Text>,
    }
}

diesel::joinable!(labels -> repos (repo_id));
diesel::joinable!(prs -> repos (repo_id));
diesel::joinable!(sync_meta -> repos (repo_id));

diesel::allow_tables_to_appear_in_same_query!(
    attention, labels, my_state, prs, repos, reviewers, sync_meta, threads,
);
