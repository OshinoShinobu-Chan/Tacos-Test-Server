//! # Server Queue
//!
//! This module defines a unified request queue for the server.
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, SyncSender};
use tokio::sync::mpsc as tokio_mpsc;

use super::{ResultCode, TestResult};

#[derive(Debug, Clone, Copy)]
pub enum RequestType {
    Lab1,
    Lab2,
    Lab3,
}

impl std::fmt::Display for RequestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestType::Lab1 => write!(f, "Lab1"),
            RequestType::Lab2 => write!(f, "Lab2"),
            RequestType::Lab3 => write!(f, "Lab3"),
        }
    }
}

#[derive(Debug)]
pub struct RequestItem {
    /// Unique identifier for the request
    pub id: String,
    /// Type of the request
    pub req_type: RequestType,
    /// Body of the request
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub enum RequestStatus {
    /// The rank of the request in the queue
    Queued(usize),
    /// The request is being processed, return intermediate result if available
    Processing,
    /// The request is completed with the result
    Completed,
    /// This request is not found
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    Idle,
    Busy,
}

#[derive(Debug)]
pub struct RequestResponse {
    /// Unique identifier for the request
    pub id: String,
    /// The status of the request
    pub status: RequestStatus,
    /// The detailed output of the request
    pub test_result: Option<TestResult>,
}

#[derive(Debug)]
/// Unified request queue for the server
///
/// This queue holds incoming requests and manages their processing.
/// The main routine of this queue is polling the following channels:
/// - `receiver`: for incoming requests from the server handlers.
///
///   Once a new request is received, the queue checks if there is capacity to hold it.
///
///   If so, it adds the request to the internal queue and sends back an `Ok({request-id})
///   to the request handler via the response_channel.
///
///   Else, it sends back an `Err(())` indicating the queue is full.
/// - `result_channel`: for signals from worker threads indicating they are ready for
/// new requests.
///
///   When a result is received, it indicates that a worker is ready to process a new request.
/// The queue checks if there are pending requests in the internal queue.
///
///   If so, it pops a request from the queue and sends it to the corresponding worker
/// thread via the sender channel.
///
///   Else, it ignores the signal.
/// - `request_response_receiver`: for incoming status queries from the server handlers.
///   
///   When a status query is received, the queue checks the internal state to determine
/// the status of the requested request.
pub struct Queue {
    /// Internal queue to hold requests
    requests: VecDeque<RequestItem>,
    /// Maximum capacity of the queue
    capacity: usize,
    /// Result list
    result: HashMap<String, VecDeque<TestResult>>,
    /// Result capacity
    result_capacity: usize,
    /// Request handler will send a tuple of (RequestItem, SyncSender for response)
    request_receiver: Receiver<(RequestItem, SyncSender<Result<(), ()>>)>,
    /// First element is SyncSender for outgoing requests
    ///
    /// Second element is Receiver for incoming results from worker threads
    worker_channels: Vec<(
        tokio_mpsc::Receiver<TestResult>,
        SyncSender<(RequestItem, tokio_mpsc::Receiver<()>)>,
    )>,
    /// Worker send signal to queue that it is ready for new request
    worker_signal: Vec<Receiver<()>>,
    /// Worker status
    worker_status: Vec<WorkerStatus>,
    /// Request handler will send a tuple of (request id, SyncSender for response)
    monitor_receiver: Receiver<(String, tokio_mpsc::Sender<RequestResponse>)>,
    /// monitor response using this channel, one channel to one request
    monitor_response_channel: HashMap<String, tokio_mpsc::Sender<RequestResponse>>,
    /// recent monitor result
    recent_monitor_result: HashMap<String, RequestStatus>,
    /// request cancel channel
    request_cancel_channel: HashMap<String, tokio_mpsc::Sender<()>>,
}

impl Queue {
    /// Create a new request queue
    pub fn new(
        capacity: usize,
        request_receiver: Receiver<(RequestItem, SyncSender<Result<(), ()>>)>,
        result_capacity: usize,
        request_sender: Vec<SyncSender<(RequestItem, tokio_mpsc::Receiver<()>)>>,
        result_channels: Vec<tokio_mpsc::Receiver<TestResult>>,
        worker_signal: Vec<Receiver<()>>,
        monitor_receiver: Receiver<(String, tokio_mpsc::Sender<RequestResponse>)>,
    ) -> Self {
        let worker_num = request_sender.len();
        Self {
            requests: VecDeque::with_capacity(capacity),
            capacity,
            result: HashMap::with_capacity(result_capacity),
            result_capacity,
            request_receiver,
            worker_channels: result_channels
                .into_iter()
                .zip(request_sender.into_iter())
                .collect(),
            worker_signal,
            worker_status: vec![WorkerStatus::Busy; worker_num],
            monitor_receiver,
            monitor_response_channel: HashMap::with_capacity(result_capacity),
            recent_monitor_result: HashMap::with_capacity(result_capacity),
            request_cancel_channel: HashMap::with_capacity(result_capacity),
        }
    }

    /// poll reqeust receiver
    fn poll_request_channel(&mut self) {
        for (request, sender) in self.request_receiver.try_iter() {
            if self.requests.len() < self.capacity {
                log::info!("Queueing request: {:?}", request.id);
                self.requests.push_back(request);
                let _ = sender.send(Ok(()));
            } else {
                log::warn!("Queue full, rejecting request: {:?}", request.id);
                let _ = sender.send(Err(()));
            }
        }
    }

    /// poll result channel
    fn poll_worker_channel(&mut self) {
        for (i, worker_channel) in self.worker_channels.iter_mut().enumerate() {
            // handle the result
            while let Ok(result) = worker_channel.0.try_recv() {
                log::debug!("Received result for request: {:?}", result.id);
                if let Some(results) = self.result.get_mut(&result.id) {
                    results.push_back(result);
                    // maintain result capacity
                    if results.len() > self.result_capacity {
                        log::warn!(
                            "Result capacity exceeded, removing oldest result for request: {:?}",
                            results.front().unwrap().id
                        );
                        let result = results.pop_front().unwrap();
                        self.recent_monitor_result.remove(&result.id);
                        self.monitor_response_channel.remove(&result.id);
                    }
                } else {
                    log::debug!("Received result for unknown request: {:?}", result.id);
                }
            }
            // Check signal from workers
            if self.worker_signal[i].try_recv().is_ok() {
                self.worker_status[i] = WorkerStatus::Idle;
            }
            // If worker is busy, skip
            if self.worker_status[i] == WorkerStatus::Busy {
                continue;
            }
            // send new request to the worker if last is completed
            if let Some(request) = self.requests.pop_front() {
                let request_id = request.id.clone();
                let (s, r) = tokio_mpsc::channel(1);
                log::info!("Sending request {:?} to worker.", request.id);
                if worker_channel.1.send((request, r)).is_err() {
                    log::error!("Wroker unexpectedly down.");
                    panic!("");
                }
                self.result.insert(request_id.clone(), VecDeque::new());
                self.request_cancel_channel.insert(request_id, s);
                self.worker_status[i] = WorkerStatus::Busy;
            }
        }
    }

    fn poll_monitor_channel(&mut self) {
        for (request_id, sender) in self.monitor_receiver.try_iter() {
            log::info!("Received status query for request: {:?}", request_id);
            self.monitor_response_channel.insert(request_id, sender);
        }

        let mut completed_id = Vec::new();
        let mut send_data = Vec::new();

        for (request_id, sender) in self.monitor_response_channel.iter() {
            // log::info!("Processing status query for request: {:?}", request_id);

            if self.result.get(request_id).is_some() {
                let results = self.result.get_mut(request_id).unwrap();

                let is_completed = match results.back() {
                    Some(res) => res.result_code != ResultCode::Processing,
                    None => false,
                };
                if results.is_empty() {
                    continue;
                }

                while let Some(res) = results.pop_front() {
                    let response = RequestResponse {
                        id: request_id.clone(),
                        status: if res.result_code == ResultCode::Processing {
                            RequestStatus::Processing
                        } else {
                            RequestStatus::Completed
                        },
                        test_result: Some(res),
                    };
                    log::debug!(
                        "Sending response for request {:?} with status: {:?}",
                        request_id,
                        response.status
                    );
                    send_data.push((sender.clone(), response, request_id));
                }

                if is_completed {
                    log::debug!("Request {:?} completed.", request_id);
                    completed_id.push(request_id.clone());
                } else {
                    self.recent_monitor_result
                        .insert(request_id.clone(), RequestStatus::Processing);
                }
            } else if let Some(pos) = self.requests.iter().position(|req| &req.id == request_id) {
                // If the request's queue rank not changed, skip
                if let Some(RequestStatus::Queued(q)) = self.recent_monitor_result.get(request_id) {
                    if *q == pos + 1 {
                        continue;
                    }
                }
                log::info!("Request {:?} is queued in rank {}.", request_id, pos + 1);
                let response = RequestResponse {
                    id: request_id.clone(),
                    status: RequestStatus::Queued(pos + 1),
                    test_result: None,
                };
                send_data.push((sender.clone(), response, request_id));
                self.recent_monitor_result
                    .insert(request_id.clone(), RequestStatus::Queued(pos + 1));
            } else {
                log::warn!("Request {:?} not found.", request_id);
                let response = RequestResponse {
                    id: request_id.clone(),
                    status: RequestStatus::NotFound,
                    test_result: None,
                };
                completed_id.push(request_id.clone());
                send_data.push((sender.clone(), response, request_id));
                self.recent_monitor_result
                    .insert(request_id.clone(), RequestStatus::NotFound);
            }
        }
        for (sender, response, request_id) in send_data {
            if send_response(&sender, response).is_err() {
                completed_id.push(request_id.clone());
            }
        }
        for id in completed_id {
            self.monitor_response_channel.remove(&id);
            self.result.remove(&id);
            if let Some(index) = self.requests.iter().position(|req| &req.id == &id) {
                self.requests.remove(index);
            }
            let _ = send_cancel_signal(self.request_cancel_channel.remove(&id).as_ref().unwrap());
            log::info!(
                "Cleaned up monitor response channel and result for request: {:?}",
                id
            );
        }
    }

    pub fn poll(&mut self) {
        self.poll_request_channel();
        self.poll_worker_channel();
        self.poll_monitor_channel();
    }
}

/// Send response to the monitor channel, and block on it
fn send_response(
    sender: &tokio_mpsc::Sender<RequestResponse>,
    response: RequestResponse,
) -> Result<(), ()> {
    let sender = sender.clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        if sender.send(response).await.is_err() {
            log::error!("Monitor receiver unexpectedly down.");
            return Err(());
        }
        Ok(())
    })
}

fn send_cancel_signal(sender: &tokio_mpsc::Sender<()>) -> Result<(), ()> {
    let sender = sender.clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        if sender.send(()).await.is_err() {
            log::warn!("Request cancel receiver unexpectedly down.");
            return Err(());
        }
        Ok(())
    })
}
