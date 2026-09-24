// @generated automatically by Diesel CLI.

diesel::table! {
    activity_events (id) {
        id -> BigInt,
        repo_id -> BigInt,
        pr_number -> BigInt,
        source -> Text,
        kind -> Text,
        occurred_at -> Text,
        recorded_at -> Text,
        actor -> Nullable<Text>,
        head_sha -> Nullable<Text>,
        external_id -> Nullable<Text>,
        permalink -> Nullable<Text>,
        payload -> Text,
        observed_transition -> Bool,
        relation -> Text,
    }
}

diesel::table! {
    activity_retention (singleton) {
        singleton -> BigInt,
        cutoff -> Text,
    }
}

diesel::table! {
    activity_sync_state (repo_id, pr_number) {
        repo_id -> BigInt,
        pr_number -> BigInt,
        cursor -> Nullable<Text>,
        completed_at -> Nullable<Text>,
        next_rate_limit -> Nullable<Text>,
        incremental_cursor -> Nullable<Text>,
        incremental_next_rate_limit -> Nullable<Text>,
        requested_generation -> BigInt,
        completed_generation -> BigInt,
        incremental_generation -> Nullable<BigInt>,
        incremental_revision -> BigInt,
        incremental_stop_at -> Nullable<Text>,
        covered_through -> Nullable<Text>,
        backfill_started_at -> Nullable<Text>,
        incremental_started_at -> Nullable<Text>,
    }
}

diesel::table! {
    attention (repo_id, pr_number, reason) {
        repo_id -> BigInt,
        pr_number -> BigInt,
        reason -> Text,
        since -> Text,
        payload -> Text,
        priority -> Bool,
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
        last_review_event_id -> Nullable<BigInt>,
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
        state_changed_at -> Nullable<Text>,
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
    team_memberships (host, organization, team) {
        host -> Text,
        organization -> Text,
        team -> Text,
        members -> Text,
        refreshed_at -> Text,
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
        resolution_event_id -> Nullable<BigInt>,
    }
}

diesel::joinable!(labels -> repos (repo_id));
diesel::joinable!(my_state -> activity_events (last_review_event_id));
diesel::joinable!(prs -> repos (repo_id));
diesel::joinable!(sync_meta -> repos (repo_id));
diesel::joinable!(threads -> activity_events (resolution_event_id));

diesel::allow_tables_to_appear_in_same_query!(
    activity_events,
    activity_retention,
    activity_sync_state,
    attention,
    labels,
    my_state,
    prs,
    repos,
    reviewers,
    sync_meta,
    team_memberships,
    threads,
);
