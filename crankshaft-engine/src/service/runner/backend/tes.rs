//! A Task Execution Service (TES) backend.
//!
//! Learn more about the TES API specification [here][tes].
//!
//! [tes]: https://www.ga4gh.org/product/task-execution-service-tes/

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use crankshaft_config::backend::tes::Config;
use futures::future::BoxFuture;
use futures::FutureExt as _;
use nonempty::NonEmpty;
use tes::v1::client::tasks::View;
use tes::v1::Client;

use crate::service::runner::backend::TaskResult;
use crate::Task;

/// A backend driven by the Task Execution Service (TES) schema.
#[derive(Debug)]
pub struct Backend {
    /// A handle to the inner TES client.
    client: Arc<Client>,
}

impl Backend {
    /// AttemptsCreates a new [`Backend`].
    pub fn initialize(config: Config) -> Self {
        let mut builder = Client::builder().url(config.url().to_owned());

        if let Some(token) = config.http().basic_auth_token() {
            builder = builder.insert_header("Authorization", format!("Basic {}", token));
        }

        Self {
            // SAFETY: this is manually constructed to always build.
            client: Arc::new(builder.try_build().expect("client did not build")),
        }
    }
}

#[async_trait]
impl crate::Backend for Backend {
    fn default_name(&self) -> &'static str {
        "tes"
    }

    /// Runs a task in a backend.
    fn run(&self, task: Task) -> BoxFuture<'static, TaskResult> {
        run(self, task)
    }
}

/// Translates a [`Task`] to a [TES Task](tes::v1::types::Task) for submission.
fn to_tes_task(task: Task) -> tes::v1::types::Task {
    let name = task.name().map(|v| v.to_owned());
    let description = task.description().map(|v| v.to_owned());

    let executors = task
        .executions()
        .map(|execution| tes::v1::types::task::Executor {
            image: execution.image().to_owned(),
            command: execution.args().into_iter().cloned().collect::<Vec<_>>(),
            ..Default::default()
        })
        .collect::<Vec<_>>();

    tes::v1::types::Task {
        name,
        description,
        executors,
        ..Default::default()
    }
}

/// Runs a [`Task`] in the backend.
fn run(backend: &Backend, task: Task) -> BoxFuture<'static, TaskResult> {
    let client = backend.client.clone();
    let task = to_tes_task(task);

    async move {
        let task_id = client.create_task(task).await.unwrap().id;

        loop {
            if let Ok(task) = client.get_task(&task_id, View::Full).await {
                // SAFETY: `get_task` called with `View::Full` will always
                // return a full [`Task`], so this will always unwrap.
                let task = task.into_task().unwrap();

                if let Some(ref state) = task.state {
                    if !state.is_executing() {
                        let mut results = task
                            .logs
                            .unwrap()
                            .into_iter()
                            .flat_map(|task| task.logs)
                            .map(|log| {
                                let status = log.exit_code.expect("exit code to be present");

                                #[cfg(unix)]
                                let output = Output {
                                    status: ExitStatus::from_raw(status as i32),
                                    stdout: log.stdout.unwrap_or_default().as_bytes().to_vec(),
                                    stderr: log.stderr.unwrap_or_default().as_bytes().to_vec(),
                                };

                                #[cfg(windows)]
                                let output = Output {
                                    status: ExitStatus::from_raw(status),
                                    stdout: log.stdout.unwrap_or_default().as_bytes().to_vec(),
                                    stderr: log.stderr.unwrap_or_default().as_bytes().to_vec(),
                                };

                                output
                            });

                        let mut executions = NonEmpty::new(results.next().unwrap());
                        executions.extend(results);

                        return TaskResult { executions };
                    }

                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }
    .boxed()
}