use crate::config::TaskConfig;
use crate::core::{process_task_context, TaskState};
use crate::db::models::{Task, TaskOutput};
use crate::errors::Error;
use chrono::Utc;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sqlite::SqliteConnection;
use uuid::Uuid;

/// Repository for managing task records in the SQLite database
pub struct TaskRepository<'a> {
    /// Database connection
    pub conn: &'a mut SqliteConnection,
}

impl<'a> TaskRepository<'a> {
    /// Creates a new TaskRepository instance
    ///
    /// # Arguments
    ///
    /// * `conn` - Mutable reference to SQLite database connection
    ///
    /// # Returns
    ///
    /// A new TaskRepository instance
    pub fn new(conn: &'a mut SqliteConnection) -> Self {
        TaskRepository { conn }
    }

    /// Inserts a new task record into the database, or returns existing ID if `task_id` already exists
    ///
    /// # Arguments
    ///
    /// * `task_id` - Unique identifier for the task
    /// * `name` - Optional name of the task
    /// * `description` - Optional description of the task
    /// * `state` - Current state of the task
    /// * `context` - Optional JSON serialized task context
    /// * `proposal_history` - Optional JSON serialized proposal history
    /// * `current_proposal` - Optional JSON serialized current proposal
    /// * `feedback_history` - Optional JSON serialized feedback history
    /// * `module_execution_history` - Optional JSON serialized module execution history
    /// * `conversation` - Optional JSON serialized conversation history
    ///
    /// # Returns
    ///
    /// The database ID of the inserted or existing task
    ///
    /// # Errors
    ///
    /// Returns an Error if database operations fail
    #[allow(clippy::too_many_arguments)]
    pub fn insert_task(
        &mut self,
        task_id: String,
        name: Option<String>,
        description: Option<String>,
        state: String,
        context: Option<String>,
        proposal_history: Option<String>,
        current_proposal: Option<String>,
        feedback_history: Option<String>,
        module_execution_history: Option<String>,
        conversation: Option<String>,
        last_run_at: Option<chrono::NaiveDateTime>,
        interval: Option<String>,
    ) -> Result<String, Error> {
        use crate::schema::tasks;

        let existing_task = tasks::table
            .filter(tasks::task_id.eq(&task_id))
            .first::<Task>(self.conn)
            .optional()?;

        if let Some(found) = existing_task {
            return Ok(found.id.unwrap_or_default());
        }

        let db_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let new_task = Task {
            id: Some(db_id.clone()),
            task_id,
            name,
            description,
            state,
            context,
            proposal_history,
            current_proposal,
            feedback_history,
            module_execution_history,
            conversation,
            created_at: now.clone(),
            updated_at: now,
            config: None,
            interval,
            last_run_at,
        };

        diesel::insert_into(tasks::table)
            .values(&new_task)
            .execute(self.conn)?;

        Ok(db_id)
    }

    /// Retrieves all tasks by a specific TaskState
    ///
    /// # Arguments
    ///
    /// * `filter_state` - The TaskState to filter by
    ///
    /// # Returns
    ///
    /// A vector of tasks matching the specified state
    ///
    /// # Errors
    ///
    /// Returns an Error if database operations fail
    pub fn get_tasks_by_state(&mut self, filter_state: &TaskState) -> Result<Vec<Task>, Error> {
        use crate::schema::tasks::dsl::*;
        let filter_state_str = filter_state.to_string();

        let found_tasks = tasks
            .filter(state.eq(&filter_state_str))
            .load::<Task>(self.conn)?;

        Ok(found_tasks)
    }

    /// Retrieves all tasks matching any of the specified TaskStates
    ///
    /// # Arguments
    ///
    /// * `filter_states` - Array of TaskState values to filter by
    ///
    /// # Returns
    ///
    /// A vector of tasks matching any of the specified states
    ///
    /// # Errors
    ///
    /// Returns an Error if database operations fail
    pub fn get_tasks_by_states(&mut self, filter_states: &[TaskState]) -> Result<Vec<Task>, Error> {
        use crate::schema::tasks::dsl::*;
        let filter_states_str = filter_states.iter().map(|s| s.to_string()).collect::<Vec<String>>();

        let found_tasks = tasks
            .filter(state.eq_any(filter_states_str))
            .load::<Task>(self.conn)?;

        Ok(found_tasks)
    }

    /// Updates the last run at timestamp of a task by logical `task_id`
    ///
    /// # Arguments
    ///
    /// * `the_task_id` - The task ID to update
    /// * `last_run_at` - The new last run at timestamp
    ///
    /// # Returns
    ///
    /// Unit type if successful
    ///
    /// # Errors
    ///
    /// Returns an Error if database operations fail
    pub fn update_task_last_run_at(&mut self, the_task_id: &str) -> Result<(), Error> {
        use crate::schema::tasks;
        let now = Utc::now().to_rfc3339();

        diesel::update(tasks::table.filter(tasks::task_id.eq(the_task_id)))
            .set((tasks::last_run_at.eq(&now), tasks::updated_at.eq(&now)))
            .execute(self.conn)?;
        Ok(())
    }

    /// Updates the configuration of a task by logical `task_id`
    ///
    /// # Arguments
    ///
    /// * `the_task_id` - The task ID to update
    /// * `config` - The new task configuration
    ///
    /// # Returns
    ///
    /// Unit type if successful
    ///
    /// # Errors
    ///
    /// Returns an Error if database operations fail
    pub fn update_task_config(
        &mut self,
        the_task_id: &str,
        config: &TaskConfig,
    ) -> Result<(), Error> {
        use crate::schema::tasks::dsl::{config as cfg, context, task_id, tasks};

        let new_context = process_task_context(config);
        let combined_context = new_context.combined_context();
        let config_str = serde_json::to_string(config).unwrap_or_default();

        diesel::update(tasks.filter(task_id.eq(the_task_id)))
            .set((cfg.eq(config_str), context.eq(combined_context)))
            .execute(self.conn)?;
        Ok(())
    }

    /// Retrieves the TaskConfig from the `config` column, by logical `task_id`
    ///
    /// # Arguments
    ///
    /// * `the_task_id` - The task ID to retrieve config for
    ///
    /// # Returns
    ///
    /// The task configuration if found
    ///
    /// # Errors
    ///
    /// Returns an Error if database operations fail or config parsing fails
    pub fn get_task_config(&mut self, the_task_id: &str) -> Result<TaskConfig, Error> {
        use crate::schema::tasks::dsl::{config, task_id, tasks};

        let config_str: Option<String> = tasks
            .filter(task_id.eq(the_task_id))
            .select(config)
            .first::<Option<String>>(self.conn)?;

        let config_str = config_str.unwrap_or_default();
        let parsed: TaskConfig = serde_json::from_str(&config_str)?;
        Ok(parsed)
    }

    /// Updates the state of a task (by `task_id`)
    ///
    /// # Arguments
    ///
    /// * `the_task_id` - The task ID to update
    /// * `new_state` - The new state to set
    ///
    /// # Returns
    ///
    /// Unit type if successful
    ///
    /// # Errors
    ///
    /// Returns an Error if database operations fail
    pub fn update_task_state(
        &mut self,
        the_task_id: &str,
        new_state: &TaskState,
    ) -> Result<(), Error> {
        use crate::schema::tasks::dsl::{state, task_id, tasks, updated_at};
        let now = Utc::now().to_rfc3339();

        diesel::update(tasks.filter(task_id.eq(the_task_id)))
            .set((state.eq(new_state.to_string()), updated_at.eq(&now)))
            .execute(self.conn)?;
        Ok(())
    }

    /// Inserts a new row in `task_outputs` for the given logical `task_id`
    ///
    /// # Arguments
    ///
    /// * `the_task_id` - The task ID to add output for
    /// * `new_output` - The output content to add
    ///
    /// # Returns
    ///
    /// Unit type if successful
    ///
    /// # Errors
    ///
    /// Returns a DieselError if database operations fail
    pub fn update_task_output(
        &mut self,
        the_task_id: &str,
        new_output: &str,
    ) -> Result<(), DieselError> {
        use crate::schema::task_outputs::dsl::*;
        let now = Utc::now().to_rfc3339();
        let row_id = Uuid::new_v4().to_string();

        diesel::insert_into(task_outputs)
            .values((
                id.eq(&row_id),
                task_id.eq(the_task_id),
                output.eq(new_output),
                created_at.eq(&now),
                updated_at.eq(&now),
            ))
            .execute(self.conn)?;

        Ok(())
    }

    /// Gets the latest output for a task by `task_id`
    ///
    /// # Arguments
    ///
    /// * `the_task_id` - The task ID to get output for
    ///
    /// # Returns
    ///
    /// The latest task output if found
    ///
    /// # Errors
    ///
    /// Returns a DieselError if database operations fail
    pub fn get_task_output(
        &mut self,
        the_task_id: &str,
    ) -> Result<Option<TaskOutput>, DieselError> {
        use crate::schema::task_outputs::dsl::*;
        let result = task_outputs
            .filter(task_id.eq(the_task_id))
            .order_by(created_at.desc())
            .first::<TaskOutput>(self.conn)
            .optional()?;
        Ok(result)
    }

    /// Retrieves all outputs for a task by `task_id`
    ///
    /// # Arguments
    ///
    /// * `the_task_id` - The task ID to get outputs for
    ///
    /// # Returns
    ///
    /// A vector of all task outputs
    ///
    /// # Errors
    ///
    /// Returns a DieselError if database operations fail
    pub fn get_task_outputs(&mut self, the_task_id: &str) -> Result<Vec<TaskOutput>, DieselError> {
        use crate::schema::task_outputs::dsl::*;
        let result = task_outputs
            .filter(task_id.eq(the_task_id))
            .order_by(created_at.desc())
            .load::<TaskOutput>(self.conn)?;
        Ok(result)
    }

    /// Retrieves a single task by DB primary key `id`
    ///
    /// # Arguments
    ///
    /// * `db_id` - The database ID to look up
    ///
    /// # Returns
    ///
    /// The task if found
    ///
    /// # Errors
    ///
    /// Returns a DieselError if database operations fail
    pub fn get_task_by_db_id(&mut self, db_id: &str) -> Result<Task, DieselError> {
        use crate::schema::tasks::dsl::*;
        let found = tasks.filter(id.eq(db_id)).first::<Task>(self.conn)?;
        Ok(found)
    }

    /// Retrieves a single task by the logical `task_id`
    ///
    /// # Arguments
    ///
    /// * `the_task_id` - The task ID to look up
    ///
    /// # Returns
    ///
    /// The task if found
    ///
    /// # Errors
    ///
    /// Returns a DieselError if database operations fail
    pub fn get_task_by_task_id(&mut self, the_task_id: &str) -> Result<Task, DieselError> {
        use crate::schema::tasks;
        let found = tasks::table
            .filter(tasks::task_id.eq(the_task_id))
            .first::<Task>(self.conn)?;
        Ok(found)
    }
}
