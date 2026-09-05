// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io;

use serde::Serialize;
use serde_json::{Map, Value};

use super::{MAX_OFFERS_PER_ANNOUNCE, MAX_SDP_SIZE, MAX_SIGNALING_MESSAGE_SIZE};

pub type WebTorrentId = [u8; 20];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebTorrentAnnounceEvent {
    Started,
    Completed,
    Stopped,
}

impl WebTorrentAnnounceEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingOffer {
    pub offer_id: WebTorrentId,
    pub sdp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTorrentAnnounce {
    pub info_hash: WebTorrentId,
    pub peer_id: WebTorrentId,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub numwant: usize,
    pub key: u32,
    pub event: Option<WebTorrentAnnounceEvent>,
    pub offers: Vec<OutgoingOffer>,
}

impl WebTorrentAnnounce {
    pub fn to_json(&self) -> io::Result<String> {
        if self.numwant > MAX_OFFERS_PER_ANNOUNCE
            || self.offers.len() > MAX_OFFERS_PER_ANNOUNCE
            || self.offers.len() > self.numwant
        {
            return Err(invalid_data("WebTorrent announce exceeds the offer quota"));
        }
        for offer in &self.offers {
            validate_sdp(&offer.sdp)?;
        }

        let wire = WireAnnounce {
            action: "announce",
            info_hash: encode_latin1_id(&self.info_hash),
            peer_id: encode_latin1_id(&self.peer_id),
            uploaded: self.uploaded,
            downloaded: self.downloaded,
            left: self.left,
            numwant: self.numwant,
            key: format!("{:08X}", self.key),
            event: self.event.map(WebTorrentAnnounceEvent::as_str),
            offers: self
                .offers
                .iter()
                .map(|offer| WireOfferEnvelope {
                    offer_id: encode_latin1_id(&offer.offer_id),
                    offer: WireDescription {
                        kind: "offer",
                        sdp: &offer.sdp,
                    },
                })
                .collect(),
        };
        encode_bounded_json(&wire)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTorrentAnswer {
    pub info_hash: WebTorrentId,
    pub local_peer_id: WebTorrentId,
    pub remote_peer_id: WebTorrentId,
    pub offer_id: WebTorrentId,
    pub sdp: String,
}

impl WebTorrentAnswer {
    pub fn to_json(&self) -> io::Result<String> {
        validate_sdp(&self.sdp)?;
        encode_bounded_json(&WireAnswerAnnounce {
            action: "announce",
            info_hash: encode_latin1_id(&self.info_hash),
            offer_id: encode_latin1_id(&self.offer_id),
            to_peer_id: encode_latin1_id(&self.remote_peer_id),
            peer_id: encode_latin1_id(&self.local_peer_id),
            answer: WireDescription {
                kind: "answer",
                sdp: &self.sdp,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingOffer {
    pub offer_id: WebTorrentId,
    pub peer_id: WebTorrentId,
    pub sdp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingAnswer {
    pub offer_id: WebTorrentId,
    pub peer_id: WebTorrentId,
    pub sdp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerInterval {
    pub interval_secs: u64,
    pub min_interval_secs: Option<u64>,
    pub complete: Option<u64>,
    pub incomplete: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTorrentTrackerMessage {
    pub info_hash: WebTorrentId,
    pub offer: Option<IncomingOffer>,
    pub answer: Option<IncomingAnswer>,
    pub interval: Option<TrackerInterval>,
    pub failure_reason: Option<String>,
}

pub fn parse_tracker_message(text: &str) -> io::Result<WebTorrentTrackerMessage> {
    if text.len() > MAX_SIGNALING_MESSAGE_SIZE {
        return Err(invalid_data("WebTorrent tracker message exceeds 128 KiB"));
    }
    let value: Value = serde_json::from_str(text).map_err(invalid_json)?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_data("WebTorrent tracker message must be an object"))?;
    let info_hash = decode_required_id(object, "info_hash")?;
    let failure_reason = optional_string(object, "failure reason")?
        .or(optional_string(object, "failure_reason")?)
        .map(str::to_owned);

    let offer = match object.get("offer") {
        Some(value) => Some(parse_description::<IncomingOffer>(object, value, "offer")?),
        None => None,
    };
    let answer = match object.get("answer") {
        Some(value) => Some(parse_description::<IncomingAnswer>(
            object, value, "answer",
        )?),
        None => None,
    };
    let interval = match object.get("interval") {
        Some(value) => {
            let interval_secs = bounded_interval(value, "interval")?;
            Some(TrackerInterval {
                interval_secs,
                min_interval_secs: optional_u64(object, "min_interval")?
                    .map(|value| validate_interval(value, "min_interval"))
                    .transpose()?,
                complete: optional_u64(object, "complete")?,
                incomplete: optional_u64(object, "incomplete")?,
            })
        }
        None => None,
    };

    if offer.is_none() && answer.is_none() && interval.is_none() && failure_reason.is_none() {
        return Err(invalid_data(
            "WebTorrent tracker message has no recognized payload",
        ));
    }

    Ok(WebTorrentTrackerMessage {
        info_hash,
        offer,
        answer,
        interval,
        failure_reason,
    })
}

trait IncomingDescription: Sized {
    fn new(offer_id: WebTorrentId, peer_id: WebTorrentId, sdp: String) -> Self;
}

impl IncomingDescription for IncomingOffer {
    fn new(offer_id: WebTorrentId, peer_id: WebTorrentId, sdp: String) -> Self {
        Self {
            offer_id,
            peer_id,
            sdp,
        }
    }
}

impl IncomingDescription for IncomingAnswer {
    fn new(offer_id: WebTorrentId, peer_id: WebTorrentId, sdp: String) -> Self {
        Self {
            offer_id,
            peer_id,
            sdp,
        }
    }
}

fn parse_description<T: IncomingDescription>(
    object: &Map<String, Value>,
    value: &Value,
    expected_type: &str,
) -> io::Result<T> {
    let description = value
        .as_object()
        .ok_or_else(|| invalid_data("WebRTC description must be an object"))?;
    let kind = required_string(description, "type")?;
    if kind != expected_type {
        return Err(invalid_data("WebRTC description has the wrong type"));
    }
    let sdp = required_string(description, "sdp")?.to_string();
    validate_sdp(&sdp)?;
    Ok(T::new(
        decode_required_id(object, "offer_id")?,
        decode_required_id(object, "peer_id")?,
        sdp,
    ))
}

fn encode_latin1_id(id: &WebTorrentId) -> String {
    id.iter().map(|byte| char::from(*byte)).collect()
}

fn decode_required_id(
    object: &Map<String, Value>,
    field: &'static str,
) -> io::Result<WebTorrentId> {
    let value = required_string(object, field)?;
    let mut id = [0_u8; 20];
    let mut count = 0_usize;
    for character in value.chars() {
        let codepoint = u32::from(character);
        if codepoint > u32::from(u8::MAX) || count == id.len() {
            return Err(invalid_data(
                "WebTorrent ID must contain exactly 20 Latin-1 bytes",
            ));
        }
        id[count] = codepoint as u8;
        count += 1;
    }
    if count != id.len() {
        return Err(invalid_data(
            "WebTorrent ID must contain exactly 20 Latin-1 bytes",
        ));
    }
    Ok(id)
}

fn required_string<'a>(object: &'a Map<String, Value>, field: &'static str) -> io::Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_data("required WebTorrent tracker string is missing"))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> io::Result<Option<&'a str>> {
    match object.get(field) {
        Some(Value::String(value)) => Ok(Some(value)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(invalid_data("WebTorrent tracker string has the wrong type")),
    }
}

fn optional_u64(object: &Map<String, Value>, field: &'static str) -> io::Result<Option<u64>> {
    match object.get(field) {
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid_data("WebTorrent tracker integer is negative or invalid")),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(invalid_data(
            "WebTorrent tracker integer has the wrong type",
        )),
    }
}

fn bounded_interval(value: &Value, field: &'static str) -> io::Result<u64> {
    let value = value
        .as_u64()
        .ok_or_else(|| invalid_data("WebTorrent tracker interval is invalid"))?;
    validate_interval(value, field)
}

fn validate_interval(value: u64, _field: &'static str) -> io::Result<u64> {
    if !(1..=86_400).contains(&value) {
        return Err(invalid_data(
            "WebTorrent tracker interval is outside supported bounds",
        ));
    }
    Ok(value)
}

fn validate_sdp(sdp: &str) -> io::Result<()> {
    if sdp.is_empty() || sdp.len() > MAX_SDP_SIZE {
        return Err(invalid_data("WebRTC SDP is empty or exceeds 64 KiB"));
    }
    Ok(())
}

fn encode_bounded_json(value: &impl Serialize) -> io::Result<String> {
    let json = serde_json::to_string(value).map_err(invalid_json)?;
    if json.len() > MAX_SIGNALING_MESSAGE_SIZE {
        return Err(invalid_data("WebTorrent tracker message exceeds 128 KiB"));
    }
    Ok(json)
}

fn invalid_json(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Serialize)]
struct WireAnnounce<'a> {
    action: &'static str,
    info_hash: String,
    peer_id: String,
    uploaded: u64,
    downloaded: u64,
    left: u64,
    numwant: usize,
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'static str>,
    offers: Vec<WireOfferEnvelope<'a>>,
}

#[derive(Serialize)]
struct WireOfferEnvelope<'a> {
    offer_id: String,
    offer: WireDescription<'a>,
}

#[derive(Serialize)]
struct WireAnswerAnnounce<'a> {
    action: &'static str,
    info_hash: String,
    offer_id: String,
    to_peer_id: String,
    peer_id: String,
    answer: WireDescription<'a>,
}

#[derive(Serialize)]
struct WireDescription<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    sdp: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(seed: u8) -> WebTorrentId {
        std::array::from_fn(|index| seed.wrapping_add(index as u8))
    }

    #[test]
    fn announce_preserves_all_twenty_id_bytes() {
        let announce = WebTorrentAnnounce {
            info_hash: id(0x80),
            peer_id: id(0x20),
            uploaded: 11,
            downloaded: 22,
            left: 33,
            numwant: 1,
            key: 0x1020_3040,
            event: Some(WebTorrentAnnounceEvent::Started),
            offers: vec![OutgoingOffer {
                offer_id: id(0x40),
                sdp: "v=0\r\ns=synthetic-offer\r\n".to_string(),
            }],
        };

        let json = announce.to_json().expect("encode announce");
        let value: Value = serde_json::from_str(&json).expect("parse encoded announce");
        let object = value.as_object().expect("announce object");
        assert_eq!(decode_required_id(object, "info_hash").unwrap(), id(0x80));
        assert_eq!(decode_required_id(object, "peer_id").unwrap(), id(0x20));
        assert_eq!(object["key"], "10203040");
        assert_eq!(object["offers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn incoming_offer_and_interval_are_strictly_parsed() {
        let info_hash = encode_latin1_id(&id(1));
        let peer_id = encode_latin1_id(&id(2));
        let offer_id = encode_latin1_id(&id(3));
        let json = serde_json::json!({
            "info_hash": info_hash,
            "peer_id": peer_id,
            "offer_id": offer_id,
            "offer": {"type": "offer", "sdp": "v=0\r\ns=synthetic\r\n"},
            "interval": 120,
            "min_interval": 60,
            "complete": 4,
            "incomplete": 5
        })
        .to_string();

        let parsed = parse_tracker_message(&json).expect("parse tracker message");
        assert_eq!(parsed.info_hash, id(1));
        assert_eq!(parsed.offer.unwrap().offer_id, id(3));
        assert_eq!(parsed.interval.unwrap().interval_secs, 120);
    }

    #[test]
    fn oversized_sdp_and_non_latin1_ids_are_rejected() {
        let oversized = serde_json::json!({
            "info_hash": encode_latin1_id(&id(1)),
            "peer_id": encode_latin1_id(&id(2)),
            "offer_id": encode_latin1_id(&id(3)),
            "answer": {"type": "answer", "sdp": "x".repeat(MAX_SDP_SIZE + 1)}
        })
        .to_string();
        assert!(parse_tracker_message(&oversized).is_err());

        let invalid = serde_json::json!({
            "info_hash": "😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀",
            "interval": 120
        })
        .to_string();
        assert!(parse_tracker_message(&invalid).is_err());
    }
}
