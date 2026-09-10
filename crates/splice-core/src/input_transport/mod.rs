pub(crate) mod desktop;
mod socket;
pub(crate) mod state;

pub(crate) use socket::{client, token};
pub(crate) use socket::{Connection, Endpoint, Incoming, Packet, MAX_DATAGRAM};

use anyhow::{ensure, Result};
use splice_platform::raw::clock::now_us;
use state::{Position, Sender, Transition, Update};

pub(crate) const INPUT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
pub(crate) const HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(20);

pub(crate) async fn transmit(
    connection: &Connection,
    session: u64,
    sender: &Sender,
    sent_transition: &mut u64,
    repair: bool,
) -> Result<usize> {
    let Some(position) = sender.position() else {
        return Ok(0);
    };
    let now = now_us();
    if let Some(oldest) = sender.pending().front() {
        ensure!(
            now.saturating_sub(oldest.position.captured_us) < INPUT_TIMEOUT.as_micros() as u64,
            "input transitions exceeded the 750 ms delivery limit"
        );
    }
    let transitions: Vec<_> = sender
        .pending()
        .iter()
        .enumerate()
        .filter(|(index, edge)| repair || *index < 4 || edge.id >= *sent_transition)
        .map(|(_, edge)| edge.clone())
        .collect();
    let updates = chunks(position, transitions, session, now)?;
    let mut bytes = 0;
    for packet in updates {
        bytes += connection.send(&packet).await?;
    }
    *sent_transition = position.barrier;
    Ok(bytes)
}

fn chunks(
    position: &Position,
    transitions: Vec<Transition>,
    session: u64,
    sent_us: u64,
) -> Result<Vec<Packet>> {
    let mut packets = Vec::new();
    let mut update = Update {
        position: position.clone(),
        transitions: Vec::new(),
    };
    let base = postcard::to_allocvec(&Packet::Data {
        session,
        sent_us,
        update: update.clone(),
    })?
    .len()
        + 20
        - postcard::to_allocvec(&0usize)?.len();
    let mut entries = 0;
    for transition in transitions {
        let entry = postcard::to_allocvec(&transition)?.len();
        let count = postcard::to_allocvec(&(update.transitions.len() + 1))?.len();
        if base + entries + entry + count > MAX_DATAGRAM {
            ensure!(
                !update.transitions.is_empty(),
                "an input transition exceeds the datagram limit"
            );
            packets.push(Packet::Data {
                session,
                sent_us,
                update,
            });
            update = Update {
                position: position.clone(),
                transitions: Vec::new(),
            };
            entries = 0;
        }
        ensure!(
            base + entry + postcard::to_allocvec(&1usize)?.len() <= MAX_DATAGRAM,
            "an input transition exceeds the datagram limit"
        );
        update.transitions.push(transition);
        entries += entry;
    }
    packets.push(Packet::Data {
        session,
        sent_us,
        update,
    });
    Ok(packets)
}
