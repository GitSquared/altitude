//! A two-sided demonstration of the altitude link: a node and a client in one
//! process, talking over Iroh.
//!
//!     cargo run --example iroh_link
//!
//! The crate is binary-only, so there is no `altitude::net` library path to
//! import; the module is compiled into this example directly. Once the link is
//! wired into the app, add a library target and switch this to a normal import.

// The module root is `mod.rs` rather than `net.rs` so that its child modules
// resolve alongside it: a `#[path]`-loaded single file cannot declare
// submodules, because they would be looked up relative to this directory.
#[path = "../src/net/mod.rs"]
mod net;

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    str::FromStr,
    time::{Duration, Instant},
};

use iroh::EndpointAddr;
use net::{
    BlobHash, Discovery, LinkClient, LinkClientError, LinkError, LinkNode, LinkReply, LinkRequest,
    LinkResponse, NodeAccess, NodeIdentity, NodeTicket,
};
use tokio::{runtime::Builder, time::timeout};

/// How long to wait for a relay handshake, a dial, or address discovery.
/// Generous, because a cold relay handshake is the slowest thing here.
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

    // A node starts paired with nobody. This is the state after a fresh
    // install, before the operator has carried any client key to it.
    let node = LinkNode::serve(identity, NodeAccess::in_memory([]), Discovery::Directory).await?;
    println!("node public key: {}", node.id());
    println!("paired clients:  {}", node.access().paired_keys().len());
    if node.wait_for_relay(WAIT_LIMIT).await {
        println!("relay:           handshake completed (a bare key is dialable)");
    } else {
        println!("relay:           none within {WAIT_LIMIT:?} (ticket addresses only)");
    }

    // The ticket is the address, carried out of band. Nothing here needs a
    // directory to resolve it.
    let ticket = node.ticket()?;
    println!("ticket:          {ticket}");
    // Round-trip it through its string form, because that is how a real client
    // would receive it.
    let ticket = NodeTicket::from_str(&ticket.to_string())?;
    println!("ticket dials:    {}", ticket.id());

    let client = LinkClient::bind(Discovery::Directory).await?;
    println!("client key:      {}", client.id());

    println!();
    println!("-- pairing --");
    println!(
        "unpaired:        {}",
        attempt(&client, ticket.clone()).await
    );

    // The out-of-band gesture: the operator carries the client's public key to
    // the node. There is no enrollment window and no token on the wire.
    node.access().pair(client.id())?;
    println!("paired clients:  {}", node.access().paired_keys().len());
    println!(
        "after pairing:   {}",
        attempt(&client, ticket.clone()).await
    );

    println!();
    println!("-- request/response --");
    let started = Instant::now();
    let reply = identify(&client, ticket.clone()).await?;
    println!("elapsed:         {:?}", started.elapsed());
    println!("answered by:     {}", reply.remote);
    println!("path:            {}", reply.path);
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
    let stored = client.put_blob(ticket.clone(), &blob).await?;
    let upload = started.elapsed();
    println!(
        "stored:          {} bytes, node confirmed {hash}",
        blob.len()
    );
    println!("                 (the node hashed what it received)");
    println!("confirmed by:    {} via {}", stored.remote, stored.path);
    println!("node holds:      {} blob(s)", node.blob_count());
    println!("upload rate:     {}", rate(blob.len(), upload));

    println!();
    println!("-- bare key dial --");
    // What a ticket buys: this works too, but only because n0's relays and DNS
    // resolve the key. The ticket needs neither.
    println!("dial by key:     {}", attempt(&client, node.id()).await);
    println!("                 (resolved through n0's DNS, not the ticket)");

    println!();
    println!("-- without n0 --");
    // The same node and client, wired with no relay and no directory. This is
    // what a ticket is for: the bare key now has nowhere to resolve from.
    let private_identity = NodeIdentity::load_or_create(dir.join("private.key"))?;
    let private = LinkNode::serve(
        private_identity,
        NodeAccess::in_memory([client.id()]),
        Discovery::TicketOnly,
    )
    .await?;
    private.wait_for_relay(Duration::from_secs(2)).await;
    let private_ticket = private.ticket()?;
    println!("node:            {} (no relay, no DNS)", private.id());
    println!("dial by key:     {}", attempt(&client, private.id()).await);
    println!(
        "dial by ticket:  {}",
        attempt(&client, private_ticket).await
    );
    private.shutdown().await?;

    println!();
    println!("-- persistence --");
    // A node's grants belong to the node, not to a task, so they have to
    // outlive the process. Same seam as the identity key: the caller says where.
    let grants_path = dir.join("paired.json");
    // Start clean, so the numbers below mean something on a second run.
    let _ = fs::remove_file(&grants_path);
    let grants = NodeAccess::open(&grants_path)?;
    println!("paired file:     {}", grants_path.display());
    println!("starts paired:   {}", grants.paired_keys().len());
    println!(
        "file created?    {} (nothing is written until the first pairing)",
        grants_path.exists()
    );
    grants.pair(client.id())?;
    // Reopened as a fresh process would, with nothing carried over.
    let reopened = NodeAccess::open(&grants_path)?;
    println!("after restart:   {} paired", reopened.paired_keys().len());
    println!("                 (read back from disk, not from the first handle)");
    println!("is the client?   {}", reopened.is_paired(&client.id()));
    println!(
        "file mode:       {:o}",
        fs::metadata(&grants_path)?.permissions().mode() & 0o777
    );

    println!();
    println!("-- revocation --");
    let removed = node.access().unpair(&client.id())?;
    println!("unpaired {removed}:  {}", attempt(&client, ticket).await);
    println!("paired clients:  {}", node.access().paired_keys().len());

    client.close().await?;
    node.shutdown().await?;
    println!();
    println!("both sides closed cleanly");
    Ok(())
}

/// One Identify exchange, reported rather than propagated: a refusal is the
/// thing being demonstrated here, not an error.
async fn attempt(client: &LinkClient, peer: impl Into<EndpointAddr>) -> String {
    match timeout(WAIT_LIMIT, client.request(peer, &LinkRequest::Identify)).await {
        Ok(Ok(_)) => "accepted".to_owned(),
        Ok(Err(LinkClientError::Refused { reason })) => {
            format!("REFUSED, typed as a refusal: {reason}")
        }
        Ok(Err(err)) => format!("failed: {err}"),
        Err(_) => format!("no answer within {WAIT_LIMIT:?}"),
    }
}

/// The same exchange, but a refusal is a failure.
async fn identify(
    client: &LinkClient,
    peer: impl Into<EndpointAddr>,
) -> Result<LinkReply<LinkResponse>, LinkClientError> {
    client.request(peer, &LinkRequest::Identify).await
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
