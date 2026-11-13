//! Pairing: two nodes that share a four-word code become members of one cortex.
//!
//! The joiner opens a `cortex/pair/2` connection to a candidate (found on the LAN by mDNS, or
//! named by the invitation's ticket) and both run SPAKE2 over the code: the short code becomes a
//! strong shared key, and a wrong code leaves the two with different keys, which the confirmation
//! step shows. The confirmation also covers the two connection keys, each as the other side of
//! this very connection presents it: a computer passing the exchange along between the two has a
//! key of its own on both connections, so the two confirmations differ and nobody is admitted.
//! Then the joiner sends its signed record (id, name), which must be the key of the connection it
//! arrives on, and the inviter answers with the membership log extended by it.
//!
//! One invitation takes three attempts and admits one joiner, however many connections open at
//! once. A connection claims an attempt before anything is exchanged (`Mesh::claim_pairing`): while
//! the window is open and fewer than three attempts have failed or are under way it gets the code,
//! else it is closed as not pairing. An attempt that ends without a joiner admitted counts as
//! failed, whatever ended it (a wrong code, an error, a connection that went away, the deadline of
//! the whole exchange on the inviter's side, `net::PairHandler`), and the third failure closes the
//! window. A joiner that proves the code closes the window in the same step that admits it
//! (`Mesh::pairing_admit`), so a second exchange still under way on the same code is refused
//! before anything is added to the log.

use crate::identity::{Identity, verify};
use crate::members::{Entry, Log};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity as PakeIdentity, Password, Spake2};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const ALPN: &[u8] = b"cortex/pair/2";
const JOINER: &[u8] = b"cortex-joiner";
const INVITER: &[u8] = b"cortex-inviter";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinerRecord {
    pub node: String,
    pub name: String,
    pub addrs: Vec<String>,
    /// Hex signature by `node` over `node|name`.
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviterAnswer {
    pub ok: bool,
    pub reason: String,
    pub entries: Vec<Entry>,
}

async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, bytes: &[u8]) -> Result<(), String> {
    writer.write_all(&(bytes.len() as u32).to_be_bytes()).await.map_err(|e| e.to_string())?;
    writer.write_all(bytes).await.map_err(|e| e.to_string())
}

async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R, limit: usize) -> Result<Vec<u8>, String> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).await.map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(header) as usize;
    if len > limit {
        return Err("the other side sent too much".to_owned());
    }
    let mut buf = vec![0_u8; len];
    reader.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    Ok(buf)
}

/// One side's proof of the shared key, for this connection only: `joiner` and `inviter` are the
/// two connection keys in hex, each length-prefixed so the pair reads one way only.
fn confirmation(key: &[u8], role: &[u8], joiner: &str, inviter: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"cortex-pair-confirm");
    hasher.update(key);
    hasher.update(role);
    for id in [joiner, inviter] {
        hasher.update((id.len() as u32).to_be_bytes());
        hasher.update(id.as_bytes());
    }
    hasher.finalize().to_vec()
}

/// The joiner's side on an open stream: PAKE, confirmation, record, answer. `inviter` is the key
/// of the connection's other end, in hex.
pub async fn join<S: AsyncReadExt + AsyncWriteExt + Unpin>(stream: &mut S, code: &str, me: &Identity, inviter: &str, name: &str, addrs: Vec<String>) -> Result<Vec<Entry>, String> {
    let (state, outbound) = Spake2::<Ed25519Group>::start_a(&Password::new(code.as_bytes()), &PakeIdentity::new(JOINER), &PakeIdentity::new(INVITER));
    write_frame(stream, &outbound).await?;
    let inbound = read_frame(stream, 64).await?;
    let key = state.finish(&inbound).map_err(|_| "the pairing exchange failed".to_owned())?;
    let node = me.hex();
    write_frame(stream, &confirmation(&key, JOINER, &node, inviter)).await?;
    let theirs = read_frame(stream, 64).await?;
    if theirs != confirmation(&key, INVITER, &node, inviter) {
        return Err("the code did not match".to_owned());
    }
    let sig = hex::encode(me.sign(format!("{node}|{name}").as_bytes()));
    let record = JoinerRecord { node, name: name.to_owned(), addrs, sig };
    write_frame(stream, &serde_json::to_vec(&record).map_err(|e| e.to_string())?).await?;
    let answer: InviterAnswer = serde_json::from_slice(&read_frame(stream, 4 * 1024 * 1024).await?).map_err(|e| e.to_string())?;
    if !answer.ok {
        return Err(answer.reason);
    }
    Ok(answer.entries)
}

/// The inviter's side: PAKE and confirmation with the code of the attempt this connection
/// claimed, then the joiner's record is checked and `admit` extends the log (or refuses, when the
/// invitation closed meanwhile). Returns the joiner's record once admitted. `me` is this node's
/// key and `joiner` the key of the connection's other end, both in hex.
pub async fn invite<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    code: &str,
    me: &str,
    joiner: &str,
    admit: impl FnOnce(&JoinerRecord) -> Result<Vec<Entry>, String>,
) -> Result<JoinerRecord, String> {
    let inbound = read_frame(stream, 64).await?;
    let (state, outbound) = Spake2::<Ed25519Group>::start_b(&Password::new(code.as_bytes()), &PakeIdentity::new(JOINER), &PakeIdentity::new(INVITER));
    write_frame(stream, &outbound).await?;
    let key = state.finish(&inbound).map_err(|_| "the pairing exchange failed".to_owned())?;
    let theirs = read_frame(stream, 64).await?;
    if theirs != confirmation(&key, JOINER, joiner, me) {
        // The joiner typed another code: it learns nothing (our confirmation is never sent).
        return Err("the code did not match".to_owned());
    }
    write_frame(stream, &confirmation(&key, INVITER, joiner, me)).await?;
    let record: JoinerRecord = serde_json::from_slice(&read_frame(stream, 16 * 1024).await?).map_err(|e| e.to_string())?;
    if record.node != joiner {
        return Err("the joiner's record names another key than its connection".to_owned());
    }
    let Ok(sig) = hex::decode(&record.sig) else { return Err("the joiner's record has no readable signature".to_owned()) };
    if !verify(&record.node, format!("{}|{}", record.node, record.name).as_bytes(), &sig) {
        return Err("the joiner's record is not signed by its key".to_owned());
    }
    let answer = match admit(&record) {
        Ok(entries) => InviterAnswer { ok: true, reason: String::new(), entries },
        Err(reason) => InviterAnswer { ok: false, reason, entries: Vec::new() },
    };
    write_frame(stream, &serde_json::to_vec(&answer).map_err(|e| e.to_string())?).await?;
    if !answer.ok {
        return Err(answer.reason);
    }
    Ok(record)
}

/// Whether a log already holds the node (a second pairing of the same computer changes nothing).
pub fn already_member(log: &Log, node: &str) -> bool {
    log.is_member(node)
}
