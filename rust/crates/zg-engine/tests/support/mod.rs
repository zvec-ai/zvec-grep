use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::{Value, json};
use zg_engine::api::{index::IndexOptions, info::InfoOptions};

pub fn native_file_records(index_path: &Path) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    native_documents(&index_path.join("files"))?
        .iter()
        .map(|document| {
            let payload = document.get_string("payload")?.expect("file payload");
            Ok(serde_json::from_str(&payload)?)
        })
        .collect()
}

pub fn native_documents(path: &Path) -> Result<Vec<zvec_rust::Doc>, Box<dyn std::error::Error>> {
    #[cfg(windows)]
    let path = dunce::simplified(path);
    let mut options = zvec_rust::CollectionOptions::new()?;
    options.set_read_only(true)?;
    let collection = zvec_rust::Collection::open(
        path.to_str().expect("UTF-8 collection path"),
        Some(&options),
    )?;
    let documents = collection
        .iter_with_options(None, false)?
        .collect::<Result<Vec<_>, _>>()?;
    collection.close()?;
    Ok(documents)
}

pub fn index_options(root: &Path) -> IndexOptions {
    IndexOptions {
        root: Some(root.to_path_buf()),
        allow_remote: true,
        ..IndexOptions::default()
    }
}

pub fn info_options(root: &Path) -> InfoOptions {
    InfoOptions {
        root: Some(root.to_path_buf()),
        include_status: true,
    }
}

pub fn configure_remote_model(root: &Path, address: SocketAddr) -> std::io::Result<()> {
    let home = root.join(".zvec-grep");
    fs::create_dir_all(&home)?;
    // Seed credentials without changing process-wide environment. Each fixture
    // keeps one globally unique name, including when its root is later moved.
    let name = format!("fixture-{}", uuid::Uuid::new_v4());
    let generation = uuid::Uuid::new_v4().to_string();
    fs::create_dir_all(home.join("generations").join(&generation))?;
    let manifest = json!({
        "manifestVersion": 5, "name": name, "path": home,
        "root": root, "scan": {},
        "indexPolicy": "enabled", "embeddings": [{ "model": { "provider": "qwen", "name": "text-embedding-v4", "endpoint": format!("http://{address}/embeddings") }, "dimension": 1024, "metric": "cosine", "maxBatchSize": 10, "maxInputTokens": 8192, "maxImageBytes": null }],
        "embeddingRoutes": { "text": "qwen/text-embedding-v4" },
        "indexVersion": null, "storageGeneration": generation, "createdTime": 1, "updatedTime": 1,
        "embeddingRuntimes": { "qwen/text-embedding-v4": { "apiKey": "local-test-key", "endpoint": format!("http://{address}/embeddings") } }
    });
    fs::write(home.join("manifest.json"), serde_json::to_vec(&manifest)?)
}

pub struct EmbeddingServer {
    pub address: SocketAddr,
    pub requests: Arc<AtomicUsize>,
    pub inputs: Arc<AtomicUsize>,
    #[allow(dead_code)] // Other integration binaries share the server without image assertions.
    pub multimodal_inputs: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl EmbeddingServer {
    pub fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let inputs = Arc::new(AtomicUsize::new(0));
        let multimodal_inputs = Arc::new(AtomicUsize::new(0));
        let worker = thread::spawn({
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            let inputs = Arc::clone(&inputs);
            let multimodal_inputs = Arc::clone(&multimodal_inputs);
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    requests.fetch_add(1, Ordering::Release);
                    respond(
                        stream.expect("mock HTTP connection"),
                        &inputs,
                        &multimodal_inputs,
                    )
                    .expect("mock embedding response");
                }
            }
        });
        Ok(Self {
            address,
            requests,
            inputs,
            multimodal_inputs,
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                assert!(result.is_ok(), "mock embedding server failed");
            }
        }
    }
}

fn respond(
    mut stream: TcpStream,
    inputs: &AtomicUsize,
    multimodal_inputs: &AtomicUsize,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer local-test-key"));
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .expect("content length")
                .trim()
                .parse::<usize>()
                .expect("valid content length");
            break (end + 4, length);
        }
    };
    while request.len() < header_end + content_length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        request.extend_from_slice(&buffer[..count]);
    }
    let body: Value = serde_json::from_slice(&request[header_end..header_end + content_length])?;
    let multimodal = body["input"].is_object();
    let items = if multimodal {
        body["input"]["contents"]
            .as_array()
            .expect("multimodal inputs")
    } else {
        body["input"].as_array().expect("text inputs")
    };
    inputs.fetch_add(items.len(), Ordering::Release);
    if multimodal {
        multimodal_inputs.fetch_add(items.len(), Ordering::Release);
    }
    let dimension_value = if multimodal {
        &body["parameters"]["dimension"]
    } else {
        &body["dimensions"]
    };
    let dimension =
        usize::try_from(dimension_value.as_u64().expect("dimensions")).expect("usize dimensions");
    let data = items
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let mut vector = vec![0.0_f32; dimension];
            let text = if multimodal {
                text["text"].as_str().unwrap_or("image")
            } else {
                text.as_str().expect("text")
            };
            for word in text
                .split(|character: char| !character.is_alphanumeric())
                .filter(|word| !word.is_empty())
            {
                let hash = word.to_lowercase().bytes().fold(0usize, |hash, byte| {
                    hash.wrapping_mul(31).wrapping_add(usize::from(byte))
                });
                vector[hash % dimension] += 1.0;
            }
            json!({ "index": index, "embedding": vector })
        })
        .collect::<Vec<_>>();
    let response = serde_json::to_vec(&if multimodal {
        json!({ "output": { "embeddings": data } })
    } else {
        json!({ "data": data })
    })?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    stream.write_all(&response)
}
