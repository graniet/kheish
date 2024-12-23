use super::TaskManager;
use crate::core::TaskWorker;
use crate::event::Event;
use tracing::{error, info};

impl TaskManager {
    /// Runs the task manager, handling workers and processing events
    ///
    /// # Arguments
    ///
    /// * `workers` - Vector of TaskWorker instances to spawn and manage
    ///
    /// # Details
    ///
    /// This method manages the main event loop of the task manager:
    ///
    /// - Spawns the provided workers as async tasks
    /// - Sets up interval timers for checking new and ready tasks
    /// - Processes incoming events:
    ///   - `NewMessage`: Handles task messages and logging
    ///   - `TaskStateUpdated`: Updates task states in the system
    ///   - `CreateTask`: Creates and initializes new tasks
    ///   - `NewOutput`: Processes task outputs and results
    /// - Runs periodic checks for new and ready tasks on configured intervals
    pub async fn run(&mut self, workers: Vec<TaskWorker>) {
        for worker in workers {
            tokio::spawn(worker.run());
        }

        let mut new_tasks_interval = tokio::time::interval(std::time::Duration::from_secs(10));
        let mut ready_tasks_interval = tokio::time::interval(std::time::Duration::from_secs(10));

        loop {
            tokio::select! {
                Some(msg) = self.self_rx.recv() => {
                    match msg {
                        Event::NewMessage(task_id, message) => {
                            if ! self.api_enabled {
                                if let Err(e) = self.handle_new_message(task_id, message).await {
                                    error!("Error handling new message: {}", e);
                                }
                            } else {
                                info!("{}", message);
                            }
                        }
                        Event::TaskStateUpdated(task_id, state) => {
                            if let Err(e) = self.handle_task_state_updated(task_id, state).await {
                                error!("Error updating task state: {}", e);
                            }
                        }
                        Event::CreateTask(task) => {
                            if let Err(e) = self.handle_create_task(task).await {
                                error!("Error creating task: {}", e);
                            }
                        }
                        Event::NewOutput(task_id, output) => {
                            info!("New output: {}", output);
                            if let Err(e) = self.handle_new_output(task_id, output).await {
                                error!("Error handling new output: {}", e);
                            }
                        }
                        _ => {}
                    }
                }
                _ = new_tasks_interval.tick() => {
                    if let Err(e) = self.handle_new_tasks_interval().await {
                        error!("Error handling new tasks interval: {}", e);
                    }
                }
                _ = ready_tasks_interval.tick() => {
                    if let Err(e) = self.handle_ready_tasks_interval().await {
                        error!("Error handling ready tasks interval: {}", e);
                    }
                }
            }
        }
    }
}
