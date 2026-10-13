#[cfg(unix)]
struct BarkServer {
    url: String,
    messages: std::sync::mpsc::Receiver<serde_json::Value>,
    code: std::sync::Arc<std::sync::atomic::AtomicU16>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl BarkServer {
    fn new() -> Self {
        use std::io::Read;
        use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/fixture-bark-device-secret/test-body",
            listener.local_addr().unwrap()
        );
        let (send, messages) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let code = std::sync::Arc::new(AtomicU16::new(200));
        let stopped = stop.clone();
        let response_code = code.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let (header_end, length) = loop {
                    let n = socket.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        assert!(headers.starts_with("post /push http/1.1"));
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse::<usize>()
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while bytes.len() < header_end + length {
                    let n = socket.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let payload: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                let response = format!("{{\"code\":{}}}", response_code.load(Ordering::Relaxed));
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                send.send(payload).unwrap();
            }
        });
        Self {
            url,
            messages,
            code,
            stop,
            thread: Some(thread),
        }
    }

    fn receive(&self) -> serde_json::Value {
        self.messages
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
    }
}

#[cfg(unix)]
impl Drop for BarkServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}
