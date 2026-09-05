// SPDX-License-Identifier: GPL-3.0-or-later
//! WebSocket tracker envelopes. Binary identifiers are JSON Latin-1 strings, not hex.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const MAX_ENVELOPE: usize = 256 * 1024;
pub const MAX_SDP: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity(pub [u8; 20]);
impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}
impl Serialize for Identity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.iter().map(|&b| char::from(b)).collect::<String>())
    }
}
impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let mut bytes = [0; 20];
        let mut chars = encoded.chars();
        for byte in &mut bytes {
            *byte = chars
                .next()
                .and_then(|c| u8::try_from(u32::from(c)).ok())
                .ok_or_else(|| serde::de::Error::custom("expected a 20-byte Latin-1 identifier"))?;
        }
        if chars.next().is_some() {
            return Err(serde::de::Error::custom("identifier exceeds 20 bytes"));
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Description {
    #[serde(rename = "type")]
    pub kind: String,
    pub sdp: String,
}
impl Description {
    pub fn validate(&self, expected: &str) -> Result<(), String> {
        if self.kind != expected || !self.sdp.starts_with("v=0") || self.sdp.len() > MAX_SDP {
            return Err("invalid or oversized session description".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Proposal {
    pub offer_id: Identity,
    pub offer: Description,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    Started,
    Completed,
    Stopped,
}
#[derive(Clone, Copy, Debug)]
pub struct Counters {
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
}

#[derive(Serialize)]
pub struct Announce<'a> {
    pub action: &'static str,
    pub info_hash: Identity,
    pub peer_id: Identity,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub numwant: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<Event>,
    pub offers: &'a [Proposal],
}
impl<'a> Announce<'a> {
    pub fn new(
        hash: Identity,
        peer: Identity,
        counters: Counters,
        event: Option<Event>,
        offers: &'a [Proposal],
    ) -> Self {
        Self {
            action: "announce",
            info_hash: hash,
            peer_id: peer,
            uploaded: counters.uploaded,
            downloaded: counters.downloaded,
            left: counters.left,
            numwant: offers.len(),
            event,
            offers,
        }
    }
}

#[derive(Debug)]
pub enum Notice {
    Schedule(u64),
    Offer {
        peer: Identity,
        token: Identity,
        description: Description,
    },
    Answer {
        peer: Identity,
        token: Identity,
        description: Description,
    },
    Failure(String),
    Ignore,
}
#[derive(Deserialize)]
struct Envelope {
    action: String,
    info_hash: Option<Identity>,
    peer_id: Option<Identity>,
    offer_id: Option<Identity>,
    offer: Option<Description>,
    answer: Option<Description>,
    interval: Option<u64>,
    #[serde(rename = "min interval", alias = "min_interval")]
    minimum: Option<u64>,
    #[serde(rename = "failure reason")]
    failure: Option<String>,
}
pub fn decode(bytes: &[u8], hash: Identity, local: Identity) -> Result<Notice, String> {
    if bytes.len() > MAX_ENVELOPE {
        return Err("tracker envelope too large".into());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let peer_signal = (value.get("offer").is_some() || value.get("answer").is_some())
        && value.get("failure reason").is_none();
    match decode_envelope(value, hash, local) {
        Err(error) if peer_signal => {
            tracing::debug!(%error, "discarding invalid RTC peer signal");
            Ok(Notice::Ignore)
        }
        result => result,
    }
}

fn decode_envelope(
    value: serde_json::Value,
    hash: Identity,
    local: Identity,
) -> Result<Notice, String> {
    let message: Envelope = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if message.action != "announce" {
        return Ok(Notice::Ignore);
    }
    if message.info_hash.is_some_and(|value| value != hash) {
        return Ok(Notice::Ignore);
    }
    if let Some(reason) = message.failure {
        return Ok(Notice::Failure(reason));
    }
    if message.info_hash != Some(hash) {
        return Err("missing swarm identity".into());
    }
    match (message.offer, message.answer) {
        (Some(_), Some(_)) => Err("ambiguous offer and answer".into()),
        (offer, answer) if offer.is_some() || answer.is_some() => {
            let peer = message.peer_id.ok_or("missing peer identity")?;
            if peer == local {
                return Ok(Notice::Ignore);
            }
            let token = message.offer_id.ok_or("missing offer identity")?;
            if let Some(description) = offer {
                description.validate("offer")?;
                Ok(Notice::Offer {
                    peer,
                    token,
                    description,
                })
            } else {
                let description = answer.expect("answer selected above");
                description.validate("answer")?;
                Ok(Notice::Answer {
                    peer,
                    token,
                    description,
                })
            }
        }
        _ => Ok(message.interval.map_or(Notice::Ignore, |seconds| {
            // Bound hostile deadlines before they reach Instant arithmetic in the state machine.
            Notice::Schedule(seconds.max(message.minimum.unwrap_or(0)).clamp(1, 86400))
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_peer_signals_are_isolated_from_tracker_failures() {
        let hash = Identity([41; 20]);
        let local = Identity([42; 20]);
        let valid = serde_json::json!({"action":"announce", "info_hash":hash,
            "peer_id":Identity([43;20]), "offer_id":Identity([44;20]),
            "offer":{"type":"offer", "sdp":"v=0\r\n"}});
        for (field, value) in [
            ("peer_id", serde_json::json!("short")),
            ("offer_id", serde_json::Value::Null),
            ("offer", serde_json::json!({"type":"answer","sdp":"bad"})),
            ("offer", serde_json::json!(4)),
            ("answer", valid["offer"].clone()),
        ] {
            let mut message = valid.clone();
            message[field] = value;
            assert!(matches!(
                decode(&serde_json::to_vec(&message).unwrap(), hash, local).unwrap(),
                Notice::Ignore
            ));
        }
        let failure = serde_json::json!({"action":"announce", "info_hash":hash, "failure reason":"unavailable"});
        assert!(matches!(
            decode(&serde_json::to_vec(&failure).unwrap(), hash, local).unwrap(),
            Notice::Failure(_)
        ));
        assert!(decode(br#"{"action":"announce","interval":30}"#, hash, local).is_err());
        assert!(decode(b"invalid json", hash, local).is_err());
    }
    #[test]
    fn identity_is_exactly_twenty_latin1_scalars() {
        for start in 0..=255u8 {
            let id = Identity(std::array::from_fn(|i| start.wrapping_add(i as u8)));
            assert_eq!(
                serde_json::from_str::<Identity>(&serde_json::to_string(&id).unwrap()).unwrap(),
                id
            );
        }
        for invalid in ["a".repeat(19), "a".repeat(21), "π".repeat(20)] {
            assert!(
                serde_json::from_str::<Identity>(&serde_json::to_string(&invalid).unwrap())
                    .is_err()
            );
        }
    }
    #[test]
    fn schedules_preserve_server_interval_and_minimum() {
        let hash = Identity([23; 20]);
        for (interval, minimum, expected) in [
            (1800, 0, 1800),
            (30, 120, 120),
            (0, 0, 1),
            (u64::MAX, 0, 86400),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({"action":"announce", "info_hash":hash,"interval":interval,"min interval":minimum})).unwrap();
            assert!(
                matches!(decode(&bytes, hash, Identity([7;20])).unwrap(), Notice::Schedule(value) if value == expected)
            );
        }
    }
    #[test]
    fn another_swarm_cannot_deliver_candidates() {
        let bytes = serde_json::to_vec(
            &serde_json::json!({"action":"announce", "info_hash":Identity([4;20]), "interval":12}),
        )
        .unwrap();
        assert!(matches!(
            decode(&bytes, Identity([5; 20]), Identity([6; 20])).unwrap(),
            Notice::Ignore
        ));
        assert!(decode(
            &vec![b' '; MAX_ENVELOPE + 1],
            Identity([5; 20]),
            Identity([6; 20])
        )
        .is_err());
    }
}
