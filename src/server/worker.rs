//! # Server Worker
//!
//! This module defines the worker threads for processing requests from the server queue.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::time::{Duration, timeout};

use futures::StreamExt;
use futures::stream;
use tokio_stream::wrappers::LinesStream;

use super::RequestItem;

#[derive(Debug, PartialEq, Eq, Clone, Copy, serde::Serialize)]
pub enum ResultCode {
    Processing,
    Completed,
    Canceled,
    TimeLimitExceeded,
    RuntimeError,
    ServerError,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TestResult {
    /// Unique identifier for the request
    pub id: String,
    /// The point of the test
    pub point: u32,
    /// The total point of the test
    pub total: u32,
    /// The detailed output of the test
    pub output: String,
    /// The time taken for the test in milliseconds
    pub time_ms: u64,
    /// The result code of the test
    pub result_code: ResultCode,
}

#[derive(Debug)]
/// Worker to process requests from the queue
///
/// Each worker listens on a channel for incoming requests,
/// processes them according to the specified time limit,
/// and sends back the results through another channel.
pub struct Worker {
    /// Worker id
    id: usize,
    /// Channel to receive requests from the queue
    ///
    /// First element is the receiver for requests,
    ///
    /// Second element is the sender to send back results.
    queue_channel: (
        Receiver<(RequestItem, tokio_mpsc::Receiver<()>)>,
        tokio_mpsc::Sender<TestResult>,
    ),
    /// Worker signal sender
    worker_signal: SyncSender<()>,
    /// Time limit for each test in milliseconds
    time_limit_ms: u64,
    /// lab command mapping
    lab_command: HashMap<String, Vec<String>>,
}

impl Worker {
    pub fn new(
        id: usize,
        queue_channel: (
            Receiver<(RequestItem, tokio_mpsc::Receiver<()>)>,
            tokio_mpsc::Sender<TestResult>,
        ),
        worker_signal: SyncSender<()>,
        time_limit_ms: u64,
        lab_command: HashMap<String, Vec<String>>,
    ) -> Self {
        Worker {
            id,
            queue_channel,
            worker_signal,
            time_limit_ms,
            lab_command,
        }
    }

    pub fn poll(&mut self) {
        // Notify queue that worker is ready
        log::info!("Worker {} is ready to receive request", self.id);
        let _ = self.worker_signal.send(());
        let request = self.queue_channel.0.recv();
        if request.is_err() {
            log::error!(
                "Worker {} failed to receive request: {}",
                self.id,
                request.err().unwrap()
            );
            panic!();
        }
        let (request, cancel_signal_receiver) = request.unwrap();
        log::info!("Worker {} received request: {:?}", self.id, request.id);
        // Determine the lab command to run
        let lab_name = request.req_type.to_string();
        if !self.lab_command.contains_key(&lab_name) {
            log::error!(
                "Worker {} request {}: Unknown lab command: {}",
                self.id,
                request.id,
                lab_name
            );
            self.run_test_with_timeout(
                request.id,
                "echo".to_string(),
                vec!["Unknown lab command.".to_string()],
                self.queue_channel.1.clone(),
                cancel_signal_receiver,
            );
            return;
        }
        let mut command_args = self.lab_command.get(&lab_name).unwrap().clone();
        if command_args.is_empty() {
            log::error!(
                "Worker {} request {}: Empty command for lab {}",
                self.id,
                request.id,
                lab_name
            );
            self.run_test_with_timeout(
                request.id,
                "echo".to_string(),
                vec!["Empty command for lab.".to_string()],
                self.queue_channel.1.clone(),
                cancel_signal_receiver,
            );
            return;
        }
        let command = command_args.remove(0);
        let mut args = command_args;
        for arg in &mut args.iter_mut() {
            if arg == "$FILE" {
                arg.replace_range(.., &String::from_utf8_lossy(&request.body));
            }
        }
        log::info!(
            "Worker {} request {}: Running command: {} {:?}",
            self.id,
            request.id,
            command,
            args
        );
        self.run_test_with_timeout(
            request.id,
            command,
            args,
            self.queue_channel.1.clone(),
            cancel_signal_receiver,
        );
        crate::Server::cleanup_tmp_file(&String::from_utf8_lossy(&request.body).to_string());
    }

    fn run_test_with_timeout(
        &self,
        request_id: String,
        command: String,
        args: Vec<String>,
        sender: tokio_mpsc::Sender<TestResult>,
        cancel_receiver: tokio_mpsc::Receiver<()>,
    ) {
        let rt = self.create_runtime(request_id.clone());
        if rt.is_err() {
            return;
        }
        let rt = rt.unwrap();
        let time_limit = Duration::from_millis(self.time_limit_ms);
        // block on run_test with timeout, and send each result to sender
        let worker_id = self.id;
        let _ = rt.block_on(async move {
            let start_time = chrono::Local::now();
            // Run test and stream results through sender
            let test_result = Worker::run_test(
                worker_id,
                request_id.clone(),
                command,
                args,
                sender.clone(),
                cancel_receiver,
            )
            .await;
            // Set timeout for the test
            let timed_result = timeout(time_limit, async { test_result }).await;
            let end_time = chrono::Local::now();
            let duration = (end_time - start_time).num_milliseconds() as u64;
            // Handle timeout and send appropriate result
            if timed_result.is_err() {
                send_tle_result(&sender, request_id.clone(), duration).await;
            } else if let Ok(Err(err_result)) = timed_result {
                send_test_result(&sender, err_result).await;
            } else {
                send_completed_result(&sender, request_id.clone(), duration).await;
            }
        });
    }

    /// Create a new tokio runtime for the worker, and handle errors gracefully
    fn create_runtime(&self, request_id: String) -> Result<Runtime, ()> {
        let rt = Runtime::new();
        if rt.is_err() {
            let error_message = format!("Failed to create tokio runtime: {:?}", rt.err());
            log::error!(
                "Worker {} request {}: {}",
                self.id,
                request_id,
                error_message.clone()
            );
            return Err(());
        }
        return Ok(rt.unwrap());
    }

    async fn run_test(
        id: usize,
        request_id: String,
        command: String,
        args: Vec<String>,
        sender: tokio_mpsc::Sender<TestResult>,
        mut cancel_receiver: tokio_mpsc::Receiver<()>,
    ) -> Result<(), TestResult> {
        let child = Command::new(command)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        if child.is_err() {
            let error_message = format!("Failed to spawn process: {:?}", child.err());
            log::error!(
                "Worker {} reqeust {}: {}",
                id,
                request_id,
                error_message.clone()
            );
            return Err(TestResult {
                id: request_id,
                point: 0,
                total: 0,
                output: error_message,
                time_ms: 0,
                result_code: ResultCode::ServerError,
            });
        }

        let mut child = child.unwrap();
        // Take stdout and stderr
        let stdout = child.stdout.take().unwrap();
        let stdout_stream = LinesStream::new(BufReader::new(stdout).lines());
        let stderr = child.stderr.take().unwrap();
        let stderr_stream = LinesStream::new(BufReader::new(stderr).lines());
        let req_id = request_id.clone();
        let mut combined_stream = stream::select(stdout_stream, stderr_stream);

        loop {
            tokio::select! {
                // If cancel signal is received, kill the child process
                _ = cancel_receiver.recv() => {
                    let _ = child.kill().await;
                    return Err(canceled_result(req_id.clone(), 0).await);
                }
                // Read lines from combined stdout and stderr
                line_result = combined_stream.next() => {
                    match line_result {
                        Some(Ok(line)) => {
                            if send_processing_result(&sender, req_id.clone(), line).await.is_err() {
                                return Ok(())
                            }
                        }
                        Some(Err(e)) => {
                            return Err(runtime_error_result(req_id.clone(), 0, format!("Error reading output: {}", e)).await);
                        }
                        None => return Ok(()),
                    }
                }
            }
        }
    }
}

async fn send_test_result(sender: &tokio_mpsc::Sender<TestResult>, result: TestResult) {
    let reqeust_id = result.id.clone();
    if sender.send(result).await.is_err() {
        log::error!("Failed to send test result for request {}", reqeust_id);
    }
}

async fn send_processing_result(
    sender: &tokio_mpsc::Sender<TestResult>,
    request_id: String,
    output: String,
) -> Result<(), ()> {
    let result = TestResult {
        id: request_id.clone(),
        point: 0,
        total: 0,
        output,
        time_ms: 0,
        result_code: ResultCode::Processing,
    };
    if sender.send(result).await.is_err() {
        log::error!(
            "Failed to send processing result for request {}",
            request_id
        );
        return Err(());
    }
    Ok(())
}

async fn send_tle_result(
    sender: &tokio_mpsc::Sender<TestResult>,
    request_id: String,
    duration: u64,
) {
    log::info!(
        "Request {} exceeded time limit, sending TLE result",
        request_id
    );
    let result = TestResult {
        id: request_id.clone(),
        point: 0,
        total: 0,
        output: "TimeLimitExceeded: Your test exceeded the time limit.".to_string(),
        time_ms: duration,
        result_code: ResultCode::TimeLimitExceeded,
    };
    if sender.send(result).await.is_err() {
        log::error!("Failed to send TLE result for request {}", request_id);
    }
}

async fn send_completed_result(
    sender: &tokio_mpsc::Sender<TestResult>,
    request_id: String,
    duration: u64,
) {
    log::info!("Request {} completed, sending completed result", request_id);
    let result = TestResult {
        id: request_id.clone(),
        point: 0,
        total: 0,
        output: "Completed".to_string(),
        time_ms: duration,
        result_code: ResultCode::Completed,
    };
    if sender.send(result).await.is_err() {
        log::error!("Failed to send completed result for request {}", request_id);
    }
}

async fn canceled_result(request_id: String, duration: u64) -> TestResult {
    log::info!("Request {} canceled, sending canceled result", request_id);
    TestResult {
        id: request_id.clone(),
        point: 0,
        total: 0,
        output: "Canceled".to_string(),
        time_ms: duration,
        result_code: ResultCode::Canceled,
    }
}

async fn runtime_error_result(
    request_id: String,
    duration: u64,
    error_message: String,
) -> TestResult {
    log::info!(
        "Request {} runtime error: {}",
        request_id,
        error_message.clone()
    );
    TestResult {
        id: request_id.clone(),
        point: 0,
        total: 0,
        output: error_message,
        time_ms: duration,
        result_code: ResultCode::RuntimeError,
    }
}
