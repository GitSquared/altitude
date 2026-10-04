//! A two-sided demonstration of the altitude link: a node and a client in one
//! process, talking over Iroh.
//!
//!     cargo run --example iroh_link
//!
//! The crate is binary-only, so there is no `altitude::net` library path to
//! import; the module is compiled into this example directly. Once the link is
//! wired into the app, add a library target and switch this to a normal import.

#[path = "../src/net.rs"]
mod net;

use std::{
    env,
    time::{Duration, Instant},
};

use net::{
    BlobHash, LinkClient, LinkError, LinkNode, LinkReply, LinkRequest, LinkResponse, NodeIdentity,
};
use tokio::{runtime::Builder, time::timeout};

/// How long to wait for a relay handshake, and for address discovery on a dial
/// by bare public key. Generous, because a cold relay handshake is the slowest
/// thing here.
const WAIT_LIMIT: Duration = Duration::from_secs(15);

fn main() -> Result<(), LinkError> {
    // The application's runtime is deliberately `current_thread`; this example
    // builds its own so nothing about `src/main.rs` has to change.
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(run())
}

async fn run() -> Result<(), LinkError> {
    let dir = env::temp_dir().join("altitude-iroh-link");
    let identity = NodeIdentity::load_or_create(dir.join("node.key"))?;
    println!("node key file:   {}", identity.path().display());

    let node = LinkNode::serve(identity).await?;
    println!("node public key: {}", node.id());

    if node.wait_online(WAIT_LIMIT).await {
        println!("node online:     relay handshake completed");
    } else {
        println!("node online:     not within {WAIT_LIMIT:?} (direct addresses only)");
    }
    println!("node address:    {:?}", node.addr());

    let client = LinkClient::bind().await?;
    println!("client key:      {}", client.id());

    println!();
    println!("-- request/response --");
    let started = Instant::now();
    let reply = identify(&client, &node).await?;
    println!("elapsed:         {:?}", started.elapsed());
    println!("answered by:     {}", reply.remote);
    println!("path:            {}", reply.path);
    match &reply.value {
        LinkResponse::Identified { node } => println!("node says it is: {node}"),
        other => println!("unexpected answer: {other:?}"),
    }
    println!(
        "authenticated:   {}",
        if reply.remote == node.id() {
            "yes, the peer holds the key we dialled"
        } else {
            "NO — dialled one key and reached another"
        }
    );

    println!();
    println!("-- blob transfer --");
    // Not a multiple of the 64 KiB chunk size, so the last chunk is short.
    let blob = make_blob(3 * 1024 * 1024 + 1234);
    let hash = BlobHash::of(&blob);
    println!("blob:            {} bytes, blake3 {hash}", blob.len());

    let started = Instant::now();
    let stored = client.put_blob(node.addr(), &blob).await?;
    let upload = started.elapsed();
    println!(
        "stored:          {} bytes, node confirmed {hash}",
        blob.len()
    );
    println!("                 (the node hashed what it received)");
    println!("confirmed by:    {} via {}", stored.remote, stored.path);
    println!("node holds:      {} blob(s)", node.blob_count());
    println!("upload rate:     {}", rate(blob.len(), upload));

    client.close().await?;
    node.shutdown().await?;
    println!();
    println!("both sides closed cleanly");
    Ok(())
}

/// Dial by public key alone first. That is the shape the design wants — no
/// URL, no port — but it only works if the key can be discovered, so it is
/// bounded and falls back to the node's advertised address.
async fn identify(
    client: &LinkClient,
    node: &LinkNode,
) -> Result<LinkReply<LinkResponse>, LinkError> {
    match timeout(
        WAIT_LIMIT,
        client.request(node.id(), &LinkRequest::Identify),
    )
    .await
    {
        Ok(Ok(reply)) => {
            println!("dial by key:     resolved through address discovery");
            Ok(reply)
        }
        Ok(Err(err)) => {
            println!("dial by key:     failed ({err})");
            println!("                 retrying with the node's advertised address");
            Ok(client.request(node.addr(), &LinkRequest::Identify).await?)
        }
        Err(_) => {
            println!("dial by key:     timed out after {WAIT_LIMIT:?}");
            println!("                 retrying with the node's advertised address");
            Ok(client.request(node.addr(), &LinkRequest::Identify).await?)
        }
    }
}

/// Megabytes per second, for the report as much as the demo.
fn rate(bytes: usize, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return "unmeasurable".to_owned();
    }
    format!("{:.1} MB/s", bytes as f64 / seconds / 1_048_576.0)
}

/// Deterministic pseudo-random bytes: an xorshift, so the demo needs no
/// randomness dependency and two runs are comparable.
fn make_blob(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}
