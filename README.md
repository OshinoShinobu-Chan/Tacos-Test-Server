# Tacos Test Server

This project is an online testing server for [PKU Tacos Lab](https://github.com/PKU-OS/Tacos). You can deploy this server and upload your lab source file to get the score.

## API

### `POST /test/lab{x}`

- Description: Submit a source file to server to start a test of lab x. Here for tacos lab, x can be *1*, *2* or *3*. This is a stream http reqeust.
- Request:
    - Head: 
        1. `Transfer-Encoding: chunked`: This field is necessary for a streaming post method.
        2. `Connection: keep-alive`: This field is highly recommanded.
    - Body: Body is a sequence of bytes, which is the content of the uploaded source file. You should compress your `src` folder under the Tacos lab into `xxx.tar.gz`, and pack it to the body of the this http request.
- Response:

    Every normal response with status `200 OK` will return a json data as follows:
    ```json
    {
        "status": "xxx",
        // Other fields
    }
    ```
    Here are the descriptions of every kind of `status`:
    - `Processing`: 
        - Description: This means the server received a chunk successfully.
        - Additional field: 
            - `progress`: The size received in this chunk.
    - `FileUploadCompleted`:
        - Description: This means the server received the final chunk of upload file successfully. No Additional field.
    - `OK`:
        - Description: This means the server accept this submit.
        - Addtional field:
            - `uuid`: The unique id of this submit. You can use it to track the result of this submit from server later.
    - `TooManyRequests`:
        - Description: This is one of the situation where your submit received `FileUploadCompleted` without `OK`. It means the server has too many test request now, that your submit can't be accept. You should wait for a while and retry.
        - Addtional field:
            - `message`: "Too many requests"
    - `ServerError`:
        - Description: This means some kind of error occurred on the server. You should try to contact with the administrator of the server to handle this error.
        - Additional field:
            - `message`: A breif description of what kind of error occurred.

### `GET /monitor/{uuid}`

- Description: Track the output of a test with `uuid` provided in submit response. This is a stream http request. You should only call this method once on each `uuid`. The result of a test that has never been called will be kept for a while. However, if you called this method and interrupt before the test end, The 
- Response: 

    Every normal response with status `200 OK` will return a json data as follows:
    ```json
    {
        "status": "xxx",
        // Otehr fields
    }
    ```
    Here are the descriptions of every kind of `status`:
    - `PROCESSING`:
        - Description: This means the test is running nornally with some outputs. Note that a running test with no output will not get any response.
        - Additional field:
            - `result`: This field contains a part of result of this test.
    - `QUEUE`:
        - Description: This means the test request is in the queue waiting for an idle worker. Note that only 
        - Additional field:
            - `position`: The current position of this request in queue.
    - `COMPLETED`: 
        - Description: This means the test request is completed.
        - Additional field:
            - `result`: This field contains a part of result of this test.
    - `NOTFOUND`:
        - Description: This means the `uuid` is not found.

    The `result` field above is a json data as follows:
    ```json
    {
        "id": "xxx",
        "point": 0,
        "total": 0,
        "output": "xxx",
        "time_ms": 0.0,
        "result_code": "xxx"
    }
    ```
    Here are detailed descriptions of each field:
    - `id`: This is the `uuid` of the request.
    - `point`: This field is not used.
    - `total`: This field is not used.
    - `output`: This field is the output of the test. If `status` is `PROCESSING`, this field is one line of the test output. If `status` is `COMPLETED`, this field is empty.
    - `time_ms`: If `status` is `COMPLETED`, this field is the running time of the test.
    - `result_code`: The result code is the result of the current test.
        - `Processing`: This means the test is running.
        - `Completed`: This means the test is normally completed.
        - `Canceled`: This means the test is canceled.
        - `TimeLimitExceeded`: This means time of the test is exceeded.
        - `RuntimeError`: This means runtime error occurs in the test.
        - `ServerError`: This means some error occurs in the server.

## Features

This chapter give a brief description of the important features of this server.

### Model of the server

This server is a multi-thread, asynchronous http server. It contains the following thread:
1. server: This is the main thread listening to http requests and handling them asynchronously. You can find the soure code of this part mainly in `./src/main.rs` and `./src/server.rs`
2. queue: This thread acts as a centerized data plane of the server. It queues the test request from the server thread and routes them to worker threads. It also stores the result from worker threads, and send them back to server thread to handle monitor request.
3. worker: These threads run test task. They request new task from queue thread and send test result back.

There is no shared data between these threads. All of the communication between them is implemented by `std::mpsc::sync_channel` or `tokio::mpsc::channel`. It is suppposed not to have any data race and to remain highly performant.

### Details about server

Server thread is implemented using `hyper` crate as an asynchronous http request handler. For a test request, server thread will save the uploaded file to the temporary file diractory set by configurations. And the file will be named by its `uuid`.

### Details about queue

Queue thread holds a request queue and a result map(from reqeust uuid to their output result).

You can set capacity of the request queue and capacity of result map. If the coming request is more than the capacity of request queue, they will be ignored. Result that taken by monitor request will be removed. And if the result (count by request) is more than the capacity of result map, the oldest result will be removed.

Queue thread can also send a cancel signal to a worker to cancel a running task. If a monitor request disconnect, queue will cancel the corresponding task.

### Details about worker

Worker thread runs a test task. You can write a test script, and set the `test.lab_command` in configuration. You can alse set the the time limit of a test task in configuration.

You can find the test scripts for tacos at `lab_script.sh`.

## Configurations

You set the server's config by writing a toml file as configuration file.

Here's an example:
```toml
[server]
# The address of server listening to 
bind_address = "[::]"
# The port of the server
bind_port = 14001
# The number of the workers
concurrency = 16
# The size of the channel used to communicate betwen threads.
# Normally, all the thread send and receive the message at the same pace,
# so set a considerably enough size is ok.
channel_size = 1024
# The directory for temporary files. All the uploaded file from test request will be saved here.
tmp_file_dir = "/mnt/nas/tmp/tacos_test_server"
# The size limitation of the uploaded file in bytes.
upload_file_size_limit_bytes = 5242880 # 5 MB

[queue]
# The capacity of request queue
max_requests = 100
# The capacity of result map
max_results = 1000

[test]
# The time limitaiton of a test task
time_limit_secs = 1800

[test.lab_command]
# The test command run for each lab, `$FILE` will be replaced by the file name of the corresponding uploaded file. 
Lab1 = ["bash", "/root/tacos_test_server/lab_script.sh", "/mnt/nas/tmp/tacos_test_server", "$FILE", "lab1"]
Lab2 = ["bash", "/root/tacos_test_server/lab_script.sh", "/mnt/nas/tmp/tacos_test_server", "$FILE", "lab2"]
Lab3 = ["bash", "/root/tacos_test_server/lab_script.sh", "/mnt/nas/tmp/tacos_test_server", "$FILE", "lab3"]
```

## Run this server as a service

You can run this server as a service, so that it can run quietly at background, restart on failure and start automatically when host starts.

Here's an example service config for `systemd`.

```toml
[Unit]
Description=Tacos-Test-Server
After=network.target
Wants=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/tacos_test_server /root/.tacos-test-server/config.toml
User=root
Restart=on-failure
RestartSec=5s

# These three are important for the server to run docker
NoNewPrivileges=false
AppArmorProfile=unconfined
# Add your tmp file directory here
ReadWritePaths=/root/tacos_test_server /mnt/nas/tmp/tacos_test_server /var/run/docker.sock

PrivateTmp=true
ProtectSystem=strict

[Install]
WantedBy=multi-user.target
```

## TODO list

[ ] Add a way to gracefully stop the server (cleanup all the things before stop).

[ ] Add lines limitation to the test result output.

[ ] Add some system test.