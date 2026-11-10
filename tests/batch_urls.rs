//! Integration test: a config-file batch runs its download requests
//! concurrently instead of one after another.
//!
//! Two `urls` entries point at a local responder that delays every response
//! by 2s. If the batch ran sequentially the run would take >= 4s; running
//! concurrently it takes ~2s. (Only one test per binary may call
//! `pxdprowl::cli::run`, because it installs the global logger.)

use std::io::Write as _;
use std::net::TcpListener;
use std::time::{Duration, Instant};

use clap::Parser;
use pxdprowl::cli::{Args, run};

/// Binds a local responder that delays each response by `delay` and answers
/// with a 1 KiB body. Returns the port and the accept-loop thread handle.
fn spawn_slow_responder(delay: Duration) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let body = vec![b'x'; 1024];
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            });
        }
    });
    (port, handle)
}

#[tokio::test]
async fn batch_urls_run_concurrently() {
    let delay = Duration::from_secs(2);
    let (port, server) = spawn_slow_responder(delay);

    let dir = std::env::temp_dir().join(format!("pxdprowl-batch-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("pxdprowl.toml");
    std::fs::write(
        &config_path,
        format!(
            "output = \"out\"\nurls = [\"http://127.0.0.1:{port}/a.raw\", \"http://127.0.0.1:{port}/b.raw\"]\n"
        ),
    )
    .unwrap();

    let args = Args::try_parse_from(["pxdprowl", "-c", config_path.to_str().unwrap()]).unwrap();
    let start = Instant::now();
    run(args).await.unwrap();
    let elapsed = start.elapsed();

    // Both files landed with the full body.
    for name in ["a.raw", "b.raw"] {
        let content = std::fs::read(dir.join("out").join(name)).unwrap();
        assert_eq!(content.len(), 1024);
    }
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);

    // Concurrent: ~delay. Sequential would be >= 2 * delay.
    assert!(
        elapsed >= delay && elapsed < delay * 2,
        "batch of two {delay:?}-slow urls took {elapsed:?}; expected ~{delay:?} (concurrent), not >= 2x (sequential)"
    );
}
