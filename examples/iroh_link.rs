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
    BlobHash, ClientRecord, KnownNodes, LinkClient, LinkError, LinkNode, LinkReply, LinkRequest,
    LinkResponse, NodeIdentity, NodeTicket, Trust,
};
use tokio::{runtime::Builder, time::timeout};

/// How long to wait for a dial. With no relay and no address lookup there is no
/// bootstrap to wait for, so this is only a guard against a dial that hangs.
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

    // A node records who connects and lets them in. Nobody is refused: the
    // shape of an answer to "who may connect" is not settled, and a half-answer
    // would be worse than none.
    // The caller keeps its own handle on the record, so it can ask what the
    // node has seen without the node having to expose it.
    let clients = ClientRecord::in_memory([]);
    let node = LinkNode::serve(
        identity,
        clients.clone(),
        "0.0.0.0:0".parse().expect("a valid address"),
        "an example node".to_owned(),
    )
    .await?;
    println!("node public key: {}", node.id());
    println!("clients seen:    {}", clients.seen().len());
    println!("relays:          none, and no address lookup");

    // The ticket is the address, carried out of band. Nothing else can resolve
    // a bare key here, which is the trade: no directory, and no way in without
    // something the operator handed over.
    let ticket = node.ticket()?;
    println!("ticket:          {ticket}");
    // Round-trip it through its string form, because that is how a real client
    // would receive it.
    let ticket = NodeTicket::from_str(&ticket.to_string())?;
    println!("ticket dials:    {}", ticket.id());

    let client = LinkClient::bind().await?;
    println!("client key:      {}", client.id());

    println!();
    println!("-- first contact --");
    println!("clients seen:    {}", clients.seen().len());
    println!(
        "before talking:  {}",
        attempt(&client, ticket.clone()).await
    );
    println!("clients seen:    {}", clients.seen().len());
    println!("recorded:        {}", clients.has_seen(&client.id()));

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
    println!("-- reaching a node --");
    // Measured with a client that has never spoken to this node. Paired, so the
    // only thing missing is an address: a bare key has nowhere to resolve from
    // now that there is no directory.
    let fresh = LinkClient::bind().await?;
    println!("fresh, by key:   {}", attempt(&fresh, node.id()).await);
    println!("fresh, ticket:   {}", attempt(&fresh, ticket.clone()).await);
    // The same client again, by key alone. An endpoint remembers the addresses
    // it has learned for a peer, so the ticket is needed once, not every time.
    println!("then by key:     {}", attempt(&fresh, node.id()).await);
    fresh.close().await?;

    // The operator-facing field: an IP or a name, resolved by this machine's own
    // resolver. DNS supplies addresses; the operator supplies the key.
    let port = node.ticket()?.port().expect("the node advertises a port");
    for address in ["localhost", &format!("localhost:{port}"), "127.0.0.1"] {
        let by_address = NodeTicket::from_address(node.id(), address, port).await?;
        println!("dial {address:16} {}", attempt(&client, by_address).await);
    }

    println!();
    println!("-- persistence --");
    // The record belongs to the node, not to a task, so it has to outlive the
    // process. Same seam as the identity key: the caller says where.
    let record_path = dir.join("clients.json");
    // Start clean, so the numbers below mean something on a second run.
    let _ = fs::remove_file(&record_path);
    let record = ClientRecord::open(&record_path)?;
    println!("record file:     {}", record_path.display());
    println!("starts empty:    {}", record.seen().len());
    println!(
        "file created?    {} (nothing is written until a client connects)",
        record_path.exists()
    );
    record.record(client.id())?;
    // Reopened as a fresh process would, with nothing carried over.
    let reopened = ClientRecord::open(&record_path)?;
    println!("after restart:   {} recorded", reopened.seen().len());
    println!("                 (read back from disk, not from the first handle)");
    println!("is the client?   {}", reopened.has_seen(&client.id()));
    println!(
        "file mode:       {:o}",
        fs::metadata(&record_path)?.permissions().mode() & 0o777
    );

    println!();
    println!("-- remembering a node --");
    // The operator was told an address and nothing else. What is remembered is
    // the key that address answered with, so a *different* answer later is
    // noticed — which is the whole protection, since the first answer was not
    // authenticated by anything.
    let known = KnownNodes::open(dir.join("known_nodes.json"))?;
    let listen = format!("127.0.0.1:{port}");
    let first = known.learn(&listen, 0).await?;
    println!("first sighting:  {:?}", first.trust);
    known.remember(&listen, &first)?;
    println!(
        "remembered:      {} as {}",
        listen, first.announcement.label
    );
    let again = known.learn(&listen, 0).await?;
    println!("second sighting: {:?}", again.trust);

    // The same address, answering with a different key. A node that was
    // replaced and a node that was never the one you thought are the same thing
    // from here, so it is reported rather than followed.
    node.shutdown().await?;
    let stranger = LinkNode::serve(
        NodeIdentity::load_or_create(dir.join("stranger.key"))?,
        ClientRecord::in_memory([]),
        listen.parse().expect("a valid address"),
        "a stranger".to_owned(),
    )
    .await?;
    let third = known.learn(&listen, 0).await?;
    match &third.trust {
        Trust::Changed { previous } => println!(
            "third sighting:  CHANGED — remembered {previous}, answered {}",
            third.announcement.id
        ),
        other => println!("third sighting:  {other:?}"),
    }
    println!("known nodes:     {}", known.known().len());
    // Accepting the new key is the same act as meeting a node for the first time.
    known.remember(&listen, &third)?;
    println!(
        "after accepting: {:?}",
        known.verify(&listen, &third.ticket.id())
    );
    // Forgetting means the next meeting is a first sighting again.
    known.forget(&listen)?;
    println!(
        "after forgetting:{:?}",
        known.verify(&listen, &third.ticket.id())
    );
    stranger.shutdown().await?;

    client.close().await?;
    println!();
    println!("both sides closed cleanly");
    Ok(())
}

/// One Identify exchange, reported rather than propagated: a refusal is the
/// thing being demonstrated here, not an error.
async fn attempt(client: &LinkClient, peer: impl Into<EndpointAddr>) -> String {
    match timeout(WAIT_LIMIT, client.request(peer, &LinkRequest::Identify)).await {
        Ok(Ok(_)) => "accepted".to_owned(),
        Ok(Err(err)) => format!("failed: {err}"),
        Err(_) => format!("no answer within {WAIT_LIMIT:?}"),
    }
}

/// The same exchange, but a refusal is a failure.
async fn identify(
    client: &LinkClient,
    peer: impl Into<EndpointAddr>,
) -> Result<LinkReply<LinkResponse>, LinkError> {
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
