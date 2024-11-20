use crate::messaging::{MavrikRequest, MavrikResponse, Task, TaskId, TaskResult};
use crate::service::MavrikService;
use crate::store::{PullStore, PushStore, QueryStore};
use crate::tcp::util::{read_deserialized, write_serialized};
use anyhow::Context;
use log::trace;
use tokio::net::TcpStream;
use tokio::select;
use tokio::task::JoinSet;

#[derive(Debug)]
pub enum TaskOutputKind {
    Request(MavrikRequest),
    TaskResult(TaskResult),
}

pub struct TcpClientHandler<Store> {
    stream: TcpStream,
    store: Store,
    task_results: JoinSet<Result<TaskResult, anyhow::Error>>
}

impl<Store> TcpClientHandler<Store>
where
    Store: PushStore<Id = TaskId, Error = anyhow::Error> 
        + PullStore<Id = TaskId, Error = anyhow::Error>
        + QueryStore<Error = anyhow::Error>
        + Clone + Send + Sync + 'static,
    
{
    pub fn new(stream: TcpStream, store: Store) -> Self {
        let task_results = JoinSet::new();
        Self { stream, store, task_results }
    }
    
    async fn handle_request(&mut self, request: MavrikRequest) -> Result<(), anyhow::Error> {
        match request {
            MavrikRequest::NewTask(new_task) => {
                let task = Task::from(new_task);
                let task_id = self.store.push(task).await.context("store push failed")?;
                let response = MavrikResponse::NewTaskId(task_id);

                trace!(response:?; "Sending response over TCP");
                write_serialized(&mut self.stream, &response)
                    .await
                    .context("sending new task ID over TCP failed")?;
            },

            MavrikRequest::AwaitTask { task_id } => {
                let store = self.store.clone();
                self.task_results.spawn(async move { store.pull(task_id).await });
            },

            MavrikRequest::GetStoreState => {
                let state = self.store.state().await?;
                let response = MavrikResponse::StoreState(state);
                write_serialized(&mut self.stream, &response)
                    .await
                    .context("sending state over TCP failed")?;
            }
        };
        Ok(())
    }
    
    async fn handle_task_result(&mut self, task_result: TaskResult) -> Result<(), anyhow::Error> {
        let response = MavrikResponse::CompletedTask(task_result);

        trace!(response:?; "Sending response over TCP");
        write_serialized(&mut self.stream, &response)
            .await
            .context("failed to send Mavrik response over TCP")?;
        
        Ok(())
    }
}

impl<Store> MavrikService for TcpClientHandler<Store>
where
    Store: PushStore<Id = TaskId, Error = anyhow::Error>
        + PullStore<Id = TaskId, Error = anyhow::Error>
        + QueryStore<Error = anyhow::Error>
        + Clone + Send + Sync + 'static,
{
    type TaskOutput = Result<TaskOutputKind, anyhow::Error>;

    async fn poll_task(&mut self) -> Self::TaskOutput {
        select! {
            result = read_deserialized(&mut self.stream) => {
                let request = result.context("receiving Mavrik request over TCP failed")?;
                Ok(TaskOutputKind::Request(request))
            },
            
            Some(result) = self.task_results.join_next(), if self.task_results.len() > 0 => {
                let task_result = result
                    .context("joining task result failed")?
                    .context("awaiting task failed")?;
                Ok(TaskOutputKind::TaskResult(task_result))
            }
        }
    }

    async fn on_task_ready(&mut self, output: Self::TaskOutput) -> Result<(), anyhow::Error> {
        match output? {
            TaskOutputKind::Request(request) => self.handle_request(request).await,
            TaskOutputKind::TaskResult(task_result) => self.handle_task_result(task_result).await
        }
    }
}
