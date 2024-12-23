// @generated automatically by Diesel CLI.

diesel::table! {
    task_events (id) {
        id -> Nullable<Text>,
        task_id -> Text,
        agent_role -> Nullable<Text>,
        event_type -> Text,
        content -> Nullable<Text>,
        created_at -> Text,
        updated_at -> Text,
    }
}

diesel::table! {
    task_outputs (id) {
        id -> Nullable<Text>,
        task_id -> Text,
        output -> Text,
        created_at -> Text,
        updated_at -> Text,
    }
}

diesel::table! {
    tasks (id) {
        id -> Nullable<Text>,
        task_id -> Text,
        name -> Nullable<Text>,
        description -> Nullable<Text>,
        state -> Text,
        context -> Nullable<Text>,
        proposal_history -> Nullable<Text>,
        current_proposal -> Nullable<Text>,
        feedback_history -> Nullable<Text>,
        module_execution_history -> Nullable<Text>,
        conversation -> Nullable<Text>,
        config -> Nullable<Text>,
        created_at -> Text,
        updated_at -> Text,
        last_run_at -> Nullable<Timestamp>,
        interval -> Nullable<Text>,
    }
}

diesel::allow_tables_to_appear_in_same_query!(task_events, task_outputs, tasks,);
