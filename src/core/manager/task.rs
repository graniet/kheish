use super::TaskManager;
use crate::core::manager::utils;
use crate::core::task_generation::generate_task_config_from_task;
use crate::core::TaskState;
use crate::core::{Task, TaskWorker, Workflow};
use crate::db::TaskRepository;
use crate::errors::Error;
use crate::event::Event;
use serde_json::Value;
use tracing::{error, info};

impl TaskManager {
    /// Updates the state of a task in the database and displays a status message
    ///
    /// # Arguments
    /// * `task_id` - The ID of the task to update
    /// * `state` - The new state to set for the task
    pub async fn handle_task_state_updated(
        &mut self,
        task_id: String,
        state: TaskState,
    ) -> Result<(), Error> {
        TaskRepository::new(&mut self.database.get_conn()).update_task_state(&task_id, &state)?;

        let message = format!("Task {} state updated: {}", task_id, state.to_string());
        if let Err(e) = self.self_tx.send(Event::NewMessage(task_id, message)) {
            error!("Error sending event: {}", e);
        }
        Ok(())
    }

    /// Handles the creation of a new task
    ///
    /// # Arguments
    /// * `task` - The task object containing all task details to create
    pub async fn handle_create_task(&mut self, task: Task) -> Result<(), Error> {
        let mut conn = self.database.get_conn();
        let mut task_repo = TaskRepository::new(&mut conn);

        let proposal_history = serde_json::to_string(&task.proposal_history).unwrap_or_default();
        let feedback_history = serde_json::to_string(&task.feedback_history).unwrap_or_default();
        let module_execution_history =
            serde_json::to_string(&task.module_execution_history).unwrap_or_default();
        let conversation = serde_json::to_string(&task.conversation).unwrap_or_default();
        let context = serde_json::to_string(&task.context).unwrap_or_default();
        let current_proposal = task.current_proposal.as_ref().map(|v| v.to_string());

        let task_id = task.task_id.clone();
        let name = task.name.clone();

        let last_run_at = None;

        let db_task_id = task_repo.insert_task(
            task_id.clone(),
            Some(name.clone()),
            Some(task.description),
            task.state.to_string(),
            Some(context),
            Some(proposal_history),
            current_proposal,
            Some(feedback_history),
            Some(module_execution_history),
            Some(conversation),
            last_run_at,
            task.interval,
        )?;

        let message = format!("Task {} created with id {}", name, db_task_id);
        if let Err(e) = self.self_tx.send(Event::NewMessage(task_id, message)) {
            error!("Error sending event: {}", e);
        }

        Ok(())
    }

    /// Creates a new output for a task in the database and displays a status message
    ///
    /// # Arguments
    /// * `task_id` - The ID of the task to create output for
    /// * `output` - The output value to create for the task as a JSON Value
    pub async fn handle_create_task_output(
        &mut self,
        task_id: String,
        output: Value,
    ) -> Result<(), Error> {
        let output_str = output.to_string();
        TaskRepository::new(&mut self.database.get_conn())
            .update_task_output(&task_id, &output_str)?;

        let message = format!("Task {} output updated", task_id);
        if let Err(e) = self
            .self_tx
            .send(Event::NewMessage(task_id.clone(), message))
        {
            error!("Error sending event: {}", e);
        }
        Ok(())
    }

    /// Updates the output of a task in the database and displays a status message
    ///
    /// # Arguments
    /// * `task_id` - The ID of the task to update
    /// * `output` - The new output value to set for the task as a JSON Value
    pub async fn handle_new_output(&mut self, task_id: String, output: Value) -> Result<(), Error> {
        let output_str = output.to_string();
        info!("New output inserting: {}", output_str);
        TaskRepository::new(&mut self.database.get_conn())
            .update_task_output(&task_id, &output_str)?;
        let message = format!("Task {} output updated", task_id);
        if let Err(e) = self
            .self_tx
            .send(Event::NewMessage(task_id.clone(), message))
        {
            error!("Error sending event: {}", e);
        }
        Ok(())
    }

    /// Handles periodic interval ticks for task monitoring by checking for new tasks and processing them
    ///
    /// Retrieves tasks in "New" state and generates their configurations to move them to "Ready" state
    pub async fn handle_new_tasks_interval(&mut self) -> Result<(), Error> {
        let mut conn = self.database.get_conn();
        let mut task_repo: TaskRepository<'_> = TaskRepository::new(&mut conn);
        if let Ok(tasks) = task_repo.get_tasks_by_state(&TaskState::New) {
            for task in tasks {
                if let Ok(task_config) =
                    generate_task_config_from_task(&task, &self.llm_client).await
                {
                    task_repo.update_task_config(&task.task_id, &task_config)?;
                    task_repo.update_task_state(&task.task_id, &TaskState::Ready)?;
                }
            }
        }
        Ok(())
    }

    /// Handles periodic interval ticks for processing ready tasks
    ///
    /// Retrieves tasks in "Ready" state and spawns task workers to execute them
    pub async fn handle_ready_tasks_interval(&mut self) -> Result<(), Error> {
        let mut conn = self.database.get_conn();
        let mut task_repo: TaskRepository<'_> = TaskRepository::new(&mut conn);

        let tasks = task_repo.get_tasks_by_states(&[TaskState::Ready, TaskState::WaitingWakeUp])?;

        let _handles: Vec<_> = tasks
            .into_iter()
            .filter_map(|task| {
                let task_id = task.task_id.clone();
                let last_run_at = task.last_run_at;
                let interval = task.interval.clone();

                if let Some(interval) = interval {
                    if let Some(last_run_at) = last_run_at {
                        if !utils::check_should_execute_now(&interval, last_run_at) {
                            return None;
                        }
                    }
                }

                if let Err(e) = task_repo.update_task_last_run_at(&task_id) {
                    error!("Error updating task last run at: {}", e);
                } else {
                    info!("Task {} last run at updated", task_id);
                }

                match task_repo.get_task_config(&task.task_id) {
                    Ok(task_config) => {
                        let vector_store = Self::initialize_vector_store(&task_config);
                        let manager_task = Task::from((task, task_config.clone()));
                        let workflow = Workflow::new(task_config.workflow.steps.clone());
                        let task_worker = TaskWorker::new(
                            task_id.clone(),
                            manager_task,
                            workflow,
                            task_config,
                            vector_store,
                            self.self_tx.clone(),
                        );
                        if let Err(e) =
                            task_repo.update_task_state(&task_id, &TaskState::InProgress)
                        {
                            error!("Error updating task state: {}", e);
                        }
                        Some(tokio::spawn(task_worker.run()))
                    }
                    Err(_) => None,
                }
            })
            .collect();

        Ok(())
    }

}
