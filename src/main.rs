use std::collections::VecDeque;
use std::env;
use std::net::SocketAddr;
use std::sync::mpsc::sync_channel;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use log::*;
use tokio::net::TcpListener;
use tokio::sync::mpsc as tokio_mpsc;

mod config;
mod server;

use server::{Queue, Server, Worker};

use env_logger::{Builder, Env};
fn setup_logger() {
    Builder::from_env(Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;

            let level = record.level();
            let level_style = buf.default_level_style(level);

            if level == log::Level::Error {
                writeln!(
                    buf,
                    "[{}{}{:#}]{}:{} - {}  - {}",
                    level_style,
                    level,
                    level_style,
                    record.module_path().unwrap_or(""),
                    record.line().unwrap_or(0),
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                    record.args()
                )
            } else {
                writeln!(
                    buf,
                    "[{}{}{:#}]{} - {}",
                    level_style,
                    level,
                    level_style,
                    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%Z"),
                    record.args()
                )
            }
        })
        .init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    setup_logger();
    info!("Starting...");

    // Read configs
    let config;
    let args: Vec<String> = env::args().collect();
    if args.len() > 2 {
        eprintln!("Usage: {} [config_file]", args[0]);
        std::process::exit(1);
    } else if args.len() == 2 {
        let tmp_config = config::Config::from_file(&args[1]);
        if let Err(e) = &tmp_config {
            eprintln!("Failed to load configuration from file {}: {}", &args[1], e);
            std::process::exit(1);
        }
        config = tmp_config.unwrap();
        info!("Loaded configuration from file: {}", &args[1]);
    } else {
        config = config::Config::default();
        info!("Using default configuration.");
    }

    // Initialize the channels
    let (queue_sender, queue_receiver) = sync_channel(config.server.channel_size);
    let (monitor_sender, monitor_receiver) = sync_channel(config.server.channel_size);
    let mut queue_worker_senders = Vec::with_capacity(config.server.concurrency);
    let mut queue_worker_receiver = VecDeque::with_capacity(config.server.concurrency);
    let mut worker_queue_senders = VecDeque::with_capacity(config.server.concurrency);
    let mut worker_queue_receiver = Vec::with_capacity(config.server.concurrency);
    let mut worker_signal_senders = VecDeque::with_capacity(config.server.concurrency);
    let mut worker_signal_receiver = Vec::with_capacity(config.server.concurrency);
    for _ in 0..config.server.concurrency {
        let (s, r) = sync_channel(1);
        queue_worker_senders.push(s);
        queue_worker_receiver.push_back(r);
        let (s, r) = tokio_mpsc::channel(config.server.channel_size);
        worker_queue_senders.push_back(s);
        worker_queue_receiver.push(r);
        let (s, r) = sync_channel(1);
        worker_signal_senders.push_back(s);
        worker_signal_receiver.push(r);
    }
    // Initialize the server
    let server = Server::new(
        queue_sender,
        monitor_sender,
        config.server.channel_size,
        config.server.tmp_file_dir.clone(),
        config.server.upload_file_size_limit_bytes,
    );
    info!("Server initialized.");
    // Initialize the queue
    let mut queue = Queue::new(
        config.queue.max_requests,
        queue_receiver,
        config.queue.max_results,
        queue_worker_senders,
        worker_queue_receiver,
        worker_signal_receiver,
        monitor_receiver,
    );
    std::thread::spawn(move || {
        loop {
            queue.poll();
        }
    });
    info!("Queue initialized.");
    // Initialize the worker
    for i in 0..config.server.concurrency {
        let mut worker = Worker::new(
            i,
            (
                queue_worker_receiver.pop_front().unwrap(),
                worker_queue_senders.pop_front().unwrap(),
            ),
            worker_signal_senders.pop_front().unwrap(),
            config.test.time_limit_secs * 1000,
            config.test.lab_command.clone(),
        );
        std::thread::spawn(move || {
            loop {
                worker.poll();
            }
        });
        info!(
            "Worker initializing ({}/{})...",
            i, config.server.concurrency
        );
    }
    info!("Worker all initialized.");

    let addr = config.server.get_bind_addr().parse::<SocketAddr>()?;
    // We create a TcpListener and bind it to 127.0.0.1:3000
    let listener = TcpListener::bind(addr).await?;
    info!("Listening on {}", addr);

    // We start a loop to continuously accept incoming connections
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let svc_clone = server.clone();
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new().serve_connection(io, svc_clone).await {
                log::warn!("Error serving connection: {}", err);
            }
        });
    }
}
