use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{atomic, Arc};
use std::time;

use ahash::AHashMap;
use anyhow::{self, Context as AnyhowContext};
use dataflow::payload::{MaterializedState, SourceChannelIdentifier};
use dataflow::prelude::Executor;
use dataflow::{Domain, DomainReceiver, DomainRequest, DualTcpStream, Packet};
use futures_util::sink::{Sink, SinkExt};
use futures_util::stream::StreamExt;
use futures_util::FutureExt;
use readyset_client::internal::ReplicaAddress;
use readyset_client::{KeyComparison, PacketData, PacketPayload, Tagged, CONNECTION_FROM_BASE};
use readyset_errors::ReadySetResult;
use strawpoll::Strawpoll;
use time::Duration;
use tokio::io::{AsyncReadExt, BufReader, BufStream, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_stream::wrappers::IntervalStream;
use tracing::{debug, error, info, info_span, instrument, trace, warn, Span};

use super::ChannelCoordinator;

type Outputs =
    AHashMap<ReplicaAddress, Box<dyn Sink<Packet, Error = bincode::Error> + Send + Unpin>>;

/// Generates a monotonically incrementing u64 value to be used as a token for our connections
fn next_token() -> u64 {
    static NEXT_TOKEN: atomic::AtomicU64 = atomic::AtomicU64::new(0);
    NEXT_TOKEN.fetch_add(1, atomic::Ordering::Relaxed)
}

/// A domain request wrapped together with the sender to which to send the processing result
pub struct WrappedDomainRequest {
    pub req: DomainRequest,
    pub done_tx: oneshot::Sender<ReadySetResult<Option<Vec<u8>>>>,
}

/// [`Replica`] is a wrapper for a [`Domain`], handling intra Domain communication and coordination
pub struct Replica {
    /// Wrapped domain
    domain: Domain,

    coord: Arc<ChannelCoordinator>,

    /// How often to update state sizes (hardcoded to 500 ms ATM, should probably change)
    /// NOTE: if `aggressively_update_state_sizes` updates will happen every packet
    refresh_sizes: IntervalStream,

    /// Incoming TCP connections, usually from other Domains
    incoming: Strawpoll<TcpListener>,

    /// A receiver for locally sent messages
    locals: DomainReceiver,

    /// A receiver for domain messages
    requests: mpsc::Receiver<WrappedDomainRequest>,

    init_state_reqs: mpsc::Receiver<MaterializedState>,

    /// Stores pending outgoing messages
    out: Outboxes,
}

impl Replica {
    pub(super) fn new(
        domain: Domain,
        on: TcpListener,
        locals: DomainReceiver,
        requests: mpsc::Receiver<WrappedDomainRequest>,
        init_state_reqs: mpsc::Receiver<MaterializedState>,
        cc: Arc<ChannelCoordinator>,
    ) -> Self {
        Replica {
            coord: cc,
            domain,
            incoming: Strawpoll::from(on),
            locals,
            out: Outboxes::new(),
            refresh_sizes: IntervalStream::new(tokio::time::interval(Duration::from_millis(500))),
            requests,
            init_state_reqs,
        }
    }
}

struct Outboxes {
    /// messages for other domains
    domains: AHashMap<ReplicaAddress, VecDeque<Packet>>,
}

impl Outboxes {
    fn new() -> Self {
        Outboxes {
            domains: Default::default(),
        }
    }
}

impl Executor for Outboxes {
    fn send(&mut self, dest: ReplicaAddress, m: Packet) {
        self.domains.entry(dest).or_default().push_back(m);
    }
}

/// Merge multiple [`RequestReaderReplay`] packets into a single packet
fn flatten_request_reader_replay(
    n: readyset_client::internal::LocalNodeIndex,
    c: &[usize],
    unique_keys: &mut HashSet<KeyComparison>,
    packets: &mut VecDeque<Packet>,
) {
    // Sadly no drain filter for VecDeque yet
    let mut i = 0;
    while i < packets.len() {
        match packets.get_mut(i) {
            Some(Packet::RequestReaderReplay { node, cols, keys }) if *node == n && *cols == c => {
                unique_keys.extend(keys.drain(..));
                packets.remove(i);
            }
            _ => i += 1,
        }
    }
}

impl Replica {
    fn span(&self) -> Span {
        info_span!(
            // Target readyset_dataflow::domain rather than noria_server::worker::replica so that a log
            // level of `readyset_dataflow=trace` still logs the domain address
            target: "readyset_dataflow::domain",
            "domain",
            address = %self.domain.address(),
        )
    }

    /// Read the first byte of a connection to determine if it is from a base node, and convert
    /// it to a DualTcpStream, returning a unique token for the connection together with the
    /// upgraded connection
    async fn handle_new_connection(
        mut stream: TcpStream,
    ) -> Result<(u64, DualTcpStream), anyhow::Error> {
        let mut tag: u8 = 0;
        stream.read_exact(std::slice::from_mut(&mut tag)).await?;
        let is_base = tag == CONNECTION_FROM_BASE;

        debug!(base = is_base, "established new connection");

        let token = next_token();
        let _ = stream.set_nodelay(true);

        let tcp = if is_base {
            DualTcpStream::upgrade(BufStream::new(stream), move |Tagged { v, tag }| {
                let input: PacketData = v;
                // Peek at its type.
                match input.data {
                    PacketPayload::Input(_) => Packet::Input {
                        inner: input,
                        src: SourceChannelIdentifier { token, tag },
                    },
                    PacketPayload::Timestamp(_) => Packet::Timestamp {
                        // The link values propagated to the base table are not used.
                        link: None,
                        src: SourceChannelIdentifier { token, tag },
                        timestamp: input,
                    },
                }
            })
        } else {
            BufStream::from(BufReader::with_capacity(
                2 * 1024 * 1024,
                BufWriter::with_capacity(4 * 1024, stream),
            ))
            .into()
        };

        Ok((token, tcp))
    }

    /// Receive packets from local and remote connections
    async fn receive_packets(
        locals: &mut DomainReceiver,
        connections: &mut tokio_stream::StreamMap<u64, DualTcpStream>,
    ) -> ReadySetResult<Option<VecDeque<Packet>>> {
        const MAX_PACKETS_PER_CALL: usize = 64;

        let mut packets = VecDeque::with_capacity(MAX_PACKETS_PER_CALL);

        // Read packets from either the local connection or the remote connections, tokio
        // select order is random, so if both are ready some degree of forward progress will
        // happen for both
        tokio::select! {
            local_req = locals.recv() => match local_req {
                None => return Ok(None),
                Some(packet) => {
                    packets.push_back(packet);
                    // Try to read more packets without waiting
                    while let Some(packet) = locals.recv().now_or_never() {
                        match packet {
                            None => {
                                return Ok(None)
                            }
                            Some(packet) => {
                                packets.push_back(packet);
                                if packets.len() >= MAX_PACKETS_PER_CALL {
                                    break;
                                }
                            }
                        }
                    }
                }
            },

            Some((_, packet)) = connections.next() => {
                let packet = packet?;
                packets.push_back(packet);
                // Try to read more packets without waiting
                while let Some(Some((_, packet))) = connections.next().now_or_never() {
                    packets.push_back(packet?);
                    if packets.len() > MAX_PACKETS_PER_CALL {
                        break;
                    }
                }
            }
        }

        Ok(Some(packets))
    }

    /// Sends response packets asynchronously, the future takes ownership of a set of packets and
    /// their destination, therefore it can't be dropped before completion without risking some
    /// packets being lost
    #[instrument(level = "debug", name = "send_packets", skip_all)]
    async fn send_packets(
        to_send: Vec<(ReplicaAddress, VecDeque<Packet>)>,
        connections: &tokio::sync::Mutex<Outputs>,
        coord: &ChannelCoordinator,
        failed: &Mutex<HashSet<SocketAddr>>,
    ) -> ReadySetResult<()> {
        let mut lock = connections.lock().await;

        let connections = &mut *lock;
        for (replica_address, mut messages) in to_send {
            if messages.is_empty() {
                continue;
            }

            let tx = match connections.entry(replica_address) {
                Occupied(entry) => {
                    trace!(%replica_address, "Reusing existing domain connection");
                    entry.into_mut()
                }
                Vacant(entry) => {
                    let Some(addr) = coord.get_addr(&replica_address) else {
                        trace!(
                            target = %replica_address,
                            num_messages = messages.len(),
                            "Missing channel for domain, dropping messages"
                        );
                        continue;
                    };

                    if failed.lock().await.contains(&addr) {
                        warn!(
                            target = %replica_address,
                            num_messages = messages.len(),
                            "Skipping packets to domain as it may have failed"
                        );
                        continue;
                    }

                    debug!(%replica_address, %addr, "Establishing connection to domain");
                    entry.insert(coord.builder_for(&replica_address)?.build_async()?)
                }
            };

            // Send the messages to the sink associated with each target domain.
            // If an error occurs when performing `feed` or `flush`, add the address
            // for the domain to the failed set, remove the channel rom the local
            // channel coordinator, and skip the remaining packets.
            //
            // The next time a batch of packets is sent to a domain, a new connection
            // will be established if the domain has been repaired.
            let mut send_err = false;
            while let Some(m) = messages.pop_front() {
                if let Err(e) = tx.feed(m).await {
                    error!(err = ?e, "Error on feed.");
                    send_err = true;
                    break;
                }
            }

            if !send_err {
                if let Err(e) = tx.flush().await {
                    error!(err = ?e, "Error on flush.");
                    send_err = true;
                }
            }

            if send_err {
                connections.remove(&replica_address);
                if let Some(addr) = coord.get_addr(&replica_address) {
                    failed.lock().await.insert(addr);
                } else {
                    warn!(target = ?replica_address, "Failure on sending packet to domain")
                }
            }
        }
        Ok(())
    }

    /// Start the event loop for a Replica
    pub async fn run(mut self) -> Result<(), anyhow::Error> {
        // Accepted TCP connections being upgraded
        let mut connection_preambles = futures::stream::FuturesUnordered::new();
        // Every established connection goes here
        let mut connections: tokio_stream::StreamMap<u64, DualTcpStream> = Default::default();
        // A cache of established connections to other Replicas we may send messages to
        // sadly have to use Mutex here to make it possible to pass a mutable reference to outputs
        // to an async function
        let outputs = Mutex::new(Outputs::default());
        let failed = Mutex::new(HashSet::new());

        // Will only ever hold a single future that handles sending packets, wrap it for convenience
        // into FuturesUnordered in general we don't want to drop the future for
        // send_packets until it finished, otherwise some packets might get lost. So the
        // solution for that is to do a `pin_mut` on a `Fuse` future (which requires first issuing a
        // fake call) and then keeping it updated once `Fuse` reports `terminated`, or
        // instead simply using `FuturesUnordered` with one entry, and adding the next
        // future when it is empty.
        let mut send_packets = futures::stream::FuturesUnordered::new();
        let span = self.span();

        let Replica {
            domain,
            coord,
            refresh_sizes,
            incoming,
            locals,
            requests,
            out,
            init_state_reqs,
        } = &mut self;

        let mut channel_changes = coord.subscribe();

        loop {
            // we have three logical input sources: receives from local domains, receives from
            // remote domains, and remote mutators.

            // NOTE: If adding new statements, make sure they are cancellation safe according to
            // https://docs.rs/tokio/1.9.0/tokio/macro.select.html
            tokio::select! {
                // Accept incoming connections
                conn = incoming.accept() => {
                    let (conn, addr) = conn.context("listening")?;
                    span.in_scope(|| debug!(from = ?addr, "accepted new connection"));
                    connection_preambles.push(Self::handle_new_connection(conn));
                },

                // Handle any connections that we accepted but still need to preprocess and convert to DualTcpStream
                Some(established_conn) = connection_preambles.next() => {
                    let (token, tcp) = match established_conn {
                        Err(_) => continue, // Ignore the errors on unestablished connections, they don't matter
                        Ok(tcp) => tcp,
                    };

                    connections.insert(token, tcp);
                },

                // Handle changes to the addresses of individual domain replicas
                replica_addr = channel_changes.recv() => {
                    match replica_addr {
                        Ok(replica_addr) => {
                            // We've received a notification that the socket address for a replica
                            // has changed - remove its cached connection from `outputs` so that
                            // when we try to send to it later we re-lookup the addr and reconnect
                            if outputs.lock().await.remove(&replica_addr).is_some() {
                                info!(%replica_addr, "Removed connection for replica");
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            // If we've lagged behind, that means we've missed some changes to
                            // replica addresses, so we need to consider all connections invalid
                            warn!(
                                %skipped,
                                "Coordinator change broadcast receiver lagged behind; reconnecting \
                                to all replicas"
                            );
                            outputs.lock().await.clear();
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            panic!("ChannelCoordinator dropped!");
                        }
                    }
                }

                // Handle domain requests
                domain_req = requests.recv() => match domain_req {
                    Some(req) => {
                        let _guard = span.enter();
                        if req.done_tx.send(domain.domain_request(req.req, out)).is_err() {
                            span.in_scope(|| warn!("domain request sender hung up"));
                        }
                    },
                    None => {
                        span.in_scope(|| warn!("domain request stream ended"));
                        return Ok(())
                    }
                },

                init_state = init_state_reqs.recv() => match init_state {
                    Some(MaterializedState{ node, state }) => domain.process_state_for_node(node, *state)?,
                    None => {
                        span.in_scope(|| warn!("domain state initialization stream ended"));
                        return Ok(())
                    }
                },

                // Handle incoming messages
                packets = Self::receive_packets(locals, &mut connections) => match packets? {
                    None => {
                        span.in_scope(|| warn!("local input stream ended"));
                        return Ok(())
                    },
                    Some(mut packets) => {
                        while let Some(mut packet) = packets.pop_front() {
                            let ack = match &mut packet {
                                Packet::Timestamp { src: SourceChannelIdentifier { token, tag }, .. } |
                                Packet::Input { src: SourceChannelIdentifier { token, tag }, .. } => {
                                    // After processing we need to ack timestamp and input messages from base
                                    connections.iter_mut().find(|(t, _)| *t == *token).map(|(_, conn)| (*tag, conn))
                                }
                                Packet::RequestReaderReplay { node, cols, keys } => {
                                    // We want to batch multiple reader replay requests into a single call while
                                    // deduplicating non unique keys
                                    let mut unique_keys: HashSet<_> = keys.drain(..).collect();
                                    flatten_request_reader_replay(*node, cols, &mut unique_keys, &mut packets);
                                    keys.extend(unique_keys.drain());
                                    None
                                }
                                _ => None,
                            };

                            span.in_scope(|| domain.handle_packet(packet, out))?;

                            if let Some((tag, conn)) = ack {
                                conn.send(Tagged { tag, v: () }).await?;
                            }
                        }
                    },
                },

                // Poll the send packets future and reissue if outstanding packets are present
                Some(res) = send_packets.next() => res?,

                // Update domain sizes when `refresh_sizes` expires
                Some(_) = refresh_sizes.next() => domain.update_state_sizes(),

                // Wait for a possible sleep
                _ = tokio::time::sleep(domain.next_poll_duration().unwrap_or_else(|| Duration::from_secs(3600))) => domain.handle_timeout()?,
            }

            // Check if the previous batch of send packets is done, and issue a new batch if needed
            if send_packets.is_empty() && !out.domains.is_empty() {
                let to_send: Vec<_> = out.domains.drain().collect();
                send_packets.push(Self::send_packets(to_send, &outputs, coord, &failed));
            }
        }
    }
}
