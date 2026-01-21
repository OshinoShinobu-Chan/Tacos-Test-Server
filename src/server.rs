//! # Server
//!
//! This is the main server module.
//! It contains the core logic for handling HTTP requests and put them to a unified
//! request queue for processing.
//!
//! ## Requests
//!
//! - `/test/lab1`: POST request for Lab 1 testing.
//! - `/test/lab2`: POST request for Lab 2 testing.
//! - `/test/lab3`: POST request for Lab 3 testing.
//! - `/report`: GET request for the report of the server status.
//! - `/monitor/{request-id}`: GET request for getting the result of a specific request.
//! - (TODO) `/quality`: POST request for code quality testing.
//!
//! ## Request Queue
//!
//! The server uses a unified request queue to manage incoming requests. See the `server::Queue`
//! for more details.

mod queue;
mod worker;

pub use queue::*;
pub use worker::*;

use std::fs::File;
use std::io;
use std::pin::Pin;
use std::sync::mpsc::{SyncSender, sync_channel};

use futures::stream;
use http_body_util::{BodyExt, StreamBody, combinators::BoxBody};
use http_body_util::{Empty, Full};
use hyper::body::{Bytes, Frame};
use hyper::service::Service;
use hyper::{Request, Response, StatusCode, body::Incoming as IncomingBody};
use tokio::sync::mpsc as tokio_mpsc;

#[derive(Debug, Clone)]
pub struct Server {
    queue_sender: SyncSender<(RequestItem, SyncSender<Result<(), ()>>)>,
    monitor_sender: SyncSender<(String, tokio_mpsc::Sender<RequestResponse>)>,
    channel_size: usize,
    tmp_file_dir: String,
    upload_file_size_limit_bytes: usize,
}

impl Server {
    pub fn new(
        queue_sender: SyncSender<(RequestItem, SyncSender<Result<(), ()>>)>,
        monitor_sender: SyncSender<(String, tokio_mpsc::Sender<RequestResponse>)>,
        channel_size: usize,
        tmp_file_dir: String,
        upload_file_size_limit_bytes: usize,
    ) -> Self {
        Server {
            queue_sender,
            monitor_sender,
            channel_size,
            tmp_file_dir,
            upload_file_size_limit_bytes,
        }
    }

    /// Handle monitoring request for specific request-id
    ///
    /// Results are sent as a streaming response
    ///
    /// - You should only monitor a specific request-id once at any time
    fn monitor_task(
        &self,
        path: String,
        monitor_sender: SyncSender<(String, tokio_mpsc::Sender<RequestResponse>)>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
        let request_id = path["/monitor/".len()..].to_string();
        log::info!("[SERVER] HTTP/1.1 GET /monitor/{}", request_id);

        // ask result from queue, and turn it into a streaming response
        let (s, r) = tokio_mpsc::channel(self.channel_size);
        let _ = monitor_sender.send((request_id, s));
        let stream = stream::unfold(r, |mut receiver| async move {
            let res = match receiver.recv().await {
                Some(response_data) => {
                    let data = Self::create_monitor_response(response_data);
                    let bytes = Bytes::from(data);
                    let frame = Frame::data(bytes);
                    Some((Ok::<_, hyper::Error>(frame), receiver))
                }
                None => None,
            };
            res
        });

        // create streaming response
        let streambody = StreamBody::new(stream);
        let body = http_body_util::BodyExt::boxed(streambody);
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .unwrap();
        Ok(response)
    }

    /// Create monitor response JSON string from RequestResponse
    fn create_monitor_response(response_data: RequestResponse) -> String {
        match response_data.status {
            RequestStatus::Queued(q) => {
                log::debug!("Creating QUEUE status with position: {:?}", q);
                format!("{{\"status\": \"QUEUE\", \"position\": {}}}\n", q)
            }
            RequestStatus::Processing => {
                let test_result = response_data.test_result.as_ref().unwrap();
                log::debug!(
                    "Creating PROCESSING status with progress: {:?}",
                    test_result
                );
                format!(
                    "{{\"status\": \"PROCESSING\", \"progress\": {}}}\n",
                    serde_json::to_string(test_result).unwrap()
                )
            }
            RequestStatus::Completed => {
                let test_result = response_data.test_result.as_ref().unwrap();
                log::debug!("Creating COMPLETED status with result: {:?}", test_result);
                format!(
                    "{{\"status\": \"COMPLETED\", \"result\": {}}}\n",
                    serde_json::to_string(&test_result).unwrap()
                )
            }
            RequestStatus::NotFound => {
                log::debug!("Creating NOT_FOUND status");
                format!("{{\"status\": \"NOT_FOUND\"}}\n")
            }
        }
    }

    /// Handle a lab testing request
    fn lab_task(
        &self,
        req: Request<IncomingBody>,
        queue_sender: SyncSender<(RequestItem, SyncSender<Result<(), ()>>)>,
        lab: RequestType,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
        // Create a temporary file
        let id = uuid::Uuid::new_v4().to_string();
        let tmp_file_path = format!("{}/{}.tar.gz", self.tmp_file_dir, id);
        // Read the entire body, and write to the temporary file
        let content = Vec::new();
        let body = req.into_body();
        let status_code = 0;
        let file_stream = stream::unfold(
            LabTaskStreamStatus {
                body,
                content,
                status_code,
                request_id: id.clone(),
                tmp_file_path: tmp_file_path.clone(),
                queue_sender: queue_sender.clone(),
                lab,
                upload_file_size_limit_bytes: self.upload_file_size_limit_bytes,
            },
            |status| async move {
                if status.status() == 2 {
                    None
                } else if status.status() == 1 {
                    // write content to file
                    let write_result =
                        Self::write_bytes_to_file(&status.tmp_file_path, &status.content);
                    if write_result.is_some() {
                        let frame = write_result.unwrap();
                        return Some((Ok(frame), status.set_status(2)));
                    }
                    // request body is tmp_file_path
                    let request = RequestItem {
                        id: status.request_id.clone(),
                        req_type: status.lab,
                        body: status.tmp_file_path.as_bytes().into(),
                    };
                    let (s, r) = sync_channel(1);
                    let _ = status.queue_sender.send((request, s));
                    let response = r.recv().unwrap();
                    if response.is_ok() {
                        let data = format!(
                            "{{\"status\": \"OK\", \"uuid\": \"{}\"}}",
                            status.request_id.clone()
                        );
                        let frame = Frame::data(Bytes::from(data));
                        Some((Ok(frame), status.set_status(2)))
                    } else {
                        let data = "Too many requests.".to_string();
                        let frame = Frame::data(Bytes::from(data));
                        Self::cleanup_tmp_file(&status.tmp_file_path);
                        Some((Ok(frame), status.set_status(2)))
                    }
                } else {
                    write_body_to_content(status).await
                }
            },
        );

        // create streaming response
        let streambody = StreamBody::new(file_stream);
        let body = http_body_util::BodyExt::boxed(streambody);
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .unwrap();
        Ok(response)
    }

    pub fn cleanup_tmp_file(path: &String) {
        log::info!("Cleaning up temporary file {}", path);
        let result = std::fs::remove_file(path);
        if result.is_err() {
            log::error!(
                "Failed to remove temporary file {}: {:?}",
                path,
                result.err()
            );
        } else {
            log::debug!("Removed temporary file {}", path);
        }
    }

    fn write_bytes_to_file(path: &String, content: &Vec<u8>) -> Option<Frame<Bytes>> {
        let file_result = File::create(path);
        if file_result.is_err() {
            let error_message = format!(
                "Failed to create temporary file {}: {:?}",
                path,
                file_result.err()
            );
            log::error!("{}", error_message.clone());
            let body = format!(
                "{{\"status\": \"ServerError\", \"message\": \"{}\"}}",
                error_message
            );
            let bytes = Bytes::from(body);
            let frame = Frame::data(bytes);
            return Some(frame);
        }
        let mut file = file_result.unwrap();
        let write_result = io::Write::write_all(&mut file, content);
        if write_result.is_err() {
            let error_message = format!(
                "Failed to write to temporary file {}: {:?}",
                path,
                write_result.err()
            );
            log::error!("{}", error_message.clone());
            let body = format!(
                "{{\"status\": \"ServerError\", \"message\": \"{}\"}}",
                error_message
            );
            let bytes = Bytes::from(body);
            let frame = Frame::data(bytes);
            return Some(frame);
        }
        None
    }
}

impl Service<Request<IncomingBody>> for Server {
    type Response = Response<BoxBody<Bytes, hyper::Error>>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<IncomingBody>) -> Self::Future {
        let queue_sender = self.queue_sender.clone();
        let monitor_sender = self.monitor_sender.clone();
        let res = match (req.method(), req.uri().path()) {
            (&hyper::Method::POST, "/test/lab1") => {
                // Handle Lab 1 testing request
                log::info!("HTTP/1.1 POST /test/lab1");
                self.lab_task(req, queue_sender, RequestType::Lab1)
            }
            (&hyper::Method::POST, "/test/lab2") => {
                // Handle Lab 2 testing request
                log::info!("HTTP/1.1 POST /test/lab2");
                self.lab_task(req, queue_sender, RequestType::Lab2)
            }
            (&hyper::Method::POST, "/test/lab3") => {
                // Handle Lab 3 testing request
                log::info!("HTTP/1.1 POST /test/lab3");
                self.lab_task(req, queue_sender, RequestType::Lab3)
            }
            (&hyper::Method::GET, "/report") => {
                // Handle server report request
                log::info!("HTTP/1.1 GET /report");
                unimplemented!()
            }
            (&hyper::Method::GET, path) if path.starts_with("/monitor/") => {
                self.monitor_task(path.to_string(), monitor_sender.clone())
            }
            _ => {
                log::warn!("HTTP/1.1 {} {} - Not Found", req.method(), req.uri().path());
                let body = empty();
                let response = Response::builder().status(404).body(body).unwrap();
                Ok(response)
            }
        };

        Box::pin(async { res })
    }
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

async fn write_body_to_content(
    mut status: LabTaskStreamStatus,
) -> Option<(Result<Frame<Bytes>, hyper::Error>, LabTaskStreamStatus)> {
    match status.body.frame().await {
        // case 1: get a normal data frame:
        // write to content, and return progress
        Some(Ok(frame)) => {
            // extract bytes from frame
            let bytes = frame.into_data();
            if bytes.is_err() {
                let error_message = format!("Failed to read body frame: {:?}", bytes.err());
                log::error!("{}", error_message.clone());
                let data = format!(
                    "{{\"status\": \"ServerError\", \"message\": \"{}\"}}\n",
                    error_message
                );
                let bytes = Bytes::from(data);
                let frame = Frame::data(bytes);
                return Some((Ok(frame), status.set_status(2)));
            }
            let bytes = bytes.unwrap();
            // write bytes to file
            let len = bytes.len();
            let status = status.push_content(bytes);
            log::debug!("Received {} Bytes", len);
            if status.content.len() > status.upload_file_size_limit_bytes {
                let error_message = format!(
                    "Upload file size exceeds limit of {} bytes",
                    status.upload_file_size_limit_bytes
                );
                log::warn!("{}", error_message.clone());
                let data = format!(
                    "{{\"status\": \"FileSizeExceeded\", \"message\": \"{}\"}}\n",
                    error_message
                );
                let bytes = Bytes::from(data);
                let frame = Frame::data(bytes);
                return Some((Ok(frame), status.set_status(2)));
            }
            // return progress
            let data = format!("{{\"status\": \"Processing\", \"progress\": {}}}\n", len);
            let bytes = Bytes::from(data);
            let frame = Frame::data(bytes);
            return Some((Ok(frame), status.set_status(0)));
        }
        // case 2: get an error frame
        // return error message
        Some(Err(e)) => {
            let error_message = format!("Failed to read body frame: {:?}", e);
            log::error!("{}", error_message.clone());
            let data = format!(
                "{{\"status\": \"ServerError\", \"message\": \"{}\"}}\n",
                error_message
            );
            let bytes = Bytes::from(data);
            let frame = Frame::data(bytes);
            return Some((Ok(frame), status.set_status(2)));
        }
        // case 3: end of body
        // return FileUpdateCompleted
        None => {
            log::info!("Finished writing to file");
        }
    }
    return Some((
        Ok(Frame::data(Bytes::from(
            "{\"status\": \"FileUpdateCompleted\"}\n".to_string(),
        ))),
        status.set_status(1),
    ));
}

struct LabTaskStreamStatus {
    body: IncomingBody,
    content: Vec<u8>,
    status_code: u8,
    request_id: String,
    tmp_file_path: String,
    queue_sender: SyncSender<(RequestItem, SyncSender<Result<(), ()>>)>,
    lab: RequestType,
    upload_file_size_limit_bytes: usize,
}

impl LabTaskStreamStatus {
    pub fn set_status(self, status: u8) -> Self {
        LabTaskStreamStatus {
            status_code: status,
            ..self
        }
    }

    pub fn status(&self) -> u8 {
        self.status_code
    }

    pub fn push_content(mut self, bytes: Bytes) -> Self {
        self.content.extend(bytes.to_vec());
        self
    }
}
