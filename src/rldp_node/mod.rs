use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use dashmap::DashMap;
use parking_lot::Mutex;
use rand::Rng;
use tokio::sync::mpsc;
use ton_api::{ton, IntoBoxed};

use self::incoming_transfer::*;
use self::outgoing_transfer::*;
use self::peer::*;
use crate::adnl_node::AdnlNode;
use crate::subscriber::*;
use crate::utils::*;

mod decoder;
mod encoder;
mod incoming_transfer;
mod outgoing_transfer;
mod peer;

pub struct RldpNode {
    adnl: Arc<AdnlNode>,
    subscribers: Arc<Vec<Arc<dyn Subscriber>>>,
    peers: DashMap<AdnlNodeIdShort, Arc<RldpPeer>>,
    transfers: Arc<DashMap<TransferId, RldpTransfer>>,
}

impl RldpNode {
    pub fn with_adnl_node(adnl: Arc<AdnlNode>, subscribers: Vec<Arc<dyn Subscriber>>) -> Arc<Self> {
        Arc::new(Self {
            adnl,
            subscribers: Arc::new(subscribers),
            peers: Default::default(),
            transfers: Arc::new(Default::default()),
        })
    }

    pub async fn query(
        &self,
        local_id: &AdnlNodeIdShort,
        peer_id: &AdnlNodeIdShort,
        data: &[u8],
        max_answer_size: Option<i64>,
        roundtrip: Option<u64>,
    ) -> Result<(Option<Vec<u8>>, u64)> {
        use dashmap::mapref::entry::Entry;

        let query_id: QueryId = rand::thread_rng().gen();
        let message = ton::rldp::message::Query {
            query_id: ton::int256(query_id),
            max_answer_size: max_answer_size.unwrap_or(128 * 1024),
            timeout: now() + MAX_TIMEOUT as i32 / 1000,
            data: ton::bytes(data.to_vec()),
        }
        .into_boxed();

        let data = serialize(&message)?;
        let peer = match self.peers.entry(*peer_id) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => entry.insert(Default::default()).value().clone(),
        };

        peer.begin_query().await;

        let mut outgoing_transfer = OutgoingTransfer::new(data.as_slice(), None);
        let outgoing_transfer_id = outgoing_transfer.message().transfer_id.0;
        self.transfers.insert(
            outgoing_transfer_id,
            RldpTransfer::Outgoing(outgoing_transfer.state().clone()),
        );

        let mut incoming_transfer_id = outgoing_transfer_id;
        for symbol in &mut incoming_transfer_id {
            *symbol ^= 0xFF;
        }

        let incoming_transfer = IncomingTransfer::new(incoming_transfer_id);
        let (parts_tx, parts_rx) = mpsc::unbounded_channel();
        self.transfers
            .insert(incoming_transfer_id, RldpTransfer::Incoming(parts_tx));

        let outgoing_context = OutgoingContext {
            adnl: self.adnl.clone(),
            local_id: *local_id,
            peer_id: *peer_id,
            transfer: outgoing_transfer,
            transfer_id: outgoing_transfer_id,
        };

        let incoming_context = IncomingContext {
            adnl: self.adnl.clone(),
            local_id: *local_id,
            peer_id: *peer_id,
            parts_rx,
            transfer: incoming_transfer,
            transfer_id: outgoing_transfer_id,
        };

        let result = self
            .query_transfer_loop(outgoing_context, incoming_context, roundtrip)
            .await;

        if result.is_err() {
            self.transfers
                .insert(outgoing_transfer_id, RldpTransfer::Done);
        }
        self.transfers
            .insert(incoming_transfer_id, RldpTransfer::Done);

        let transfers = self.transfers.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(MAX_TIMEOUT * 2)).await;
            transfers.remove(&outgoing_transfer_id);
            transfers.remove(&incoming_transfer_id);
        });

        peer.end_query().await;

        match result? {
            (Some(answer), roundtrip) => {
                match deserialize(answer.as_slice())?.downcast::<ton::rldp::Message>() {
                    Ok(ton::rldp::Message::Rldp_Answer(answer))
                        if answer.query_id.0 == query_id =>
                    {
                        Ok((Some(answer.data.to_vec()), roundtrip))
                    }
                    Ok(ton::rldp::Message::Rldp_Answer(_)) => {
                        Err(RldpNodeError::QueryIdMismatch.into())
                    }
                    _ => Err(RldpNodeError::UnexpectedAnswer.into()),
                }
            }
            (None, roundtrip) => Ok((None, roundtrip)),
        }
    }

    async fn query_transfer_loop(
        &self,
        outgoing_context: OutgoingContext<'_>,
        mut incoming_context: IncomingContext,
        roundtrip: Option<u64>,
    ) -> Result<(Option<Vec<u8>>, u64)> {
        let barrier: Arc<Mutex<Option<IncomingTransfer>>> = Arc::new(Mutex::new(None));

        let incoming_transfer_state = incoming_context.transfer.state().clone();
        let outgoing_transfer_state = outgoing_context.transfer.state().clone();
        let outgoing_transfer_id = outgoing_context.transfer_id;

        tokio::spawn({
            let barrier = barrier.clone();
            async move {
                receive_loop(&mut incoming_context, Some(outgoing_transfer_state)).await;
                *barrier.lock() = Some(incoming_context.transfer);
            }
        });

        let (ok, mut roundtrip) = send_loop(outgoing_context, roundtrip).await?;

        let mut timeout = calc_timeout(Some(roundtrip));
        self.transfers
            .insert(outgoing_transfer_id, RldpTransfer::Done);

        if !ok {
            return Ok((None, roundtrip));
        }

        let mut start = Instant::now();
        let mut updates = incoming_transfer_state.updates();
        loop {
            tokio::time::sleep(Duration::from_millis(TRANSFER_LOOP_INTERVAL)).await;
            let new_updates = incoming_transfer_state.updates();
            if new_updates > updates {
                timeout = update_roundtrip(&mut roundtrip, &start);
                updates = new_updates;
                start = Instant::now();
            } else if is_timed_out(&start, timeout, updates) {
                break;
            }

            if let Some(reply) = barrier.lock().take() {
                update_roundtrip(&mut roundtrip, &start);
                return Ok((Some(reply.into_data()), roundtrip));
            }
        }
        Ok((None, roundtrip))
    }

    // TODO
}

#[async_trait::async_trait]
impl Subscriber for RldpNode {
    // TODO
}

pub type TransferId = [u8; 32];

enum RldpTransfer {
    Incoming(mpsc::UnboundedSender<Box<ton::rldp::messagepart::MessagePart>>),
    Outgoing(Arc<OutgoingTransferState>),
    Done,
}

struct OutgoingContext<'a> {
    adnl: Arc<AdnlNode>,
    local_id: AdnlNodeIdShort,
    peer_id: AdnlNodeIdShort,
    transfer: OutgoingTransfer<'a>,
    transfer_id: TransferId,
}

struct IncomingContext {
    adnl: Arc<AdnlNode>,
    local_id: AdnlNodeIdShort,
    peer_id: AdnlNodeIdShort,
    parts_rx: mpsc::UnboundedReceiver<Box<ton::rldp::messagepart::MessagePart>>,
    transfer: IncomingTransfer,
    transfer_id: TransferId,
}

async fn receive_loop(
    incoming_context: &mut IncomingContext,
    mut outgoing_transfer_state: Option<Arc<OutgoingTransferState>>,
) {
    while let Some(message) = incoming_context.parts_rx.recv().await {
        match incoming_context.transfer.process_chunk(*message) {
            Ok(Some(reply)) => {
                if let Err(e) = incoming_context.adnl.send_custom_message(
                    &incoming_context.local_id,
                    &incoming_context.peer_id,
                    reply,
                ) {
                    log::warn!("RLDP query error: {}", e);
                }
            }
            Err(e) => log::warn!("RLDP error: {}", e),
            _ => {}
        }

        incoming_context.transfer.state().increase_updates();
        if let Some(outgoing_transfer_state) = outgoing_transfer_state.take() {
            outgoing_transfer_state.set_reply();
        }

        match incoming_context.transfer.total_size() {
            Some(total_size) if total_size == incoming_context.transfer.data().len() => {
                break;
            }
            None => {
                log::warn!("total size mismatch");
            }
            _ => {}
        }
    }

    incoming_context.parts_rx.close();
    while incoming_context.parts_rx.recv().await.is_some() {}
}

async fn send_loop(
    mut outgoing_context: OutgoingContext<'_>,
    roundtrip: Option<u64>,
) -> Result<(bool, u64)> {
    const MAX_TRANSFER_WAVE: u32 = 10;

    let mut timeout = calc_timeout(roundtrip);
    let mut roundtrip = roundtrip.unwrap_or_default();

    while let Some(transfer_wave) = outgoing_context.transfer.start_next_part()? {
        let transfer_wave = std::cmp::min(transfer_wave, MAX_TRANSFER_WAVE);

        let part = outgoing_context.transfer.state().part();
        let mut start = Instant::now();
        let mut incoming_seqno = 0;
        'part: loop {
            for _ in 0..transfer_wave {
                outgoing_context.adnl.send_custom_message(
                    &outgoing_context.local_id,
                    &outgoing_context.peer_id,
                    outgoing_context.transfer.prepare_chunk()?,
                )?;

                if outgoing_context.transfer.is_finished_or_next_part(part)? {
                    break 'part;
                }
            }

            tokio::time::sleep(Duration::from_millis(TRANSFER_LOOP_INTERVAL)).await;

            if outgoing_context.transfer.is_finished_or_next_part(part)? {
                break;
            }

            let new_incoming_seqno = outgoing_context.transfer.state().seqno_in();
            if new_incoming_seqno > incoming_seqno {
                timeout = update_roundtrip(&mut roundtrip, &start);
                incoming_seqno = new_incoming_seqno;
                start = Instant::now();
            } else if is_timed_out(&start, timeout, incoming_seqno) {
                return Ok((false, std::cmp::min(roundtrip * 2, MAX_TIMEOUT)));
            }
        }
        timeout = update_roundtrip(&mut roundtrip, &start);
    }

    Ok((true, roundtrip))
}

fn update_roundtrip(roundtrip: &mut u64, time: &Instant) -> u64 {
    *roundtrip = if *roundtrip == 0 {
        time.elapsed().as_millis() as u64
    } else {
        *roundtrip + (time.elapsed().as_millis() as u64) / 2
    };
    calc_timeout(Some(*roundtrip))
}

fn calc_timeout(roundtrip: Option<u64>) -> u64 {
    std::cmp::max(roundtrip.unwrap_or(MAX_TIMEOUT), MIN_TIMEOUT)
}

fn is_timed_out(time: &Instant, timeout: u64, updates: u32) -> bool {
    time.elapsed().as_millis() as u64 > timeout + timeout * (updates as u64) / 100
}

const MIN_TIMEOUT: u64 = 500;
const MAX_TIMEOUT: u64 = 10000; // Milliseconds
const TRANSFER_LOOP_INTERVAL: u64 = 10; // Milliseconds

#[derive(thiserror::Error, Debug)]
enum RldpNodeError {
    #[error("Unexpected answer")]
    UnexpectedAnswer,
    #[error("Unknown query id")]
    QueryIdMismatch,
}