//! Access to the chain over the Bitcoin peer-to-peer network, with compact block filters.
//!
//! [`client::ChainClient`] is the only implementation: one long-running client that the wallet
//! sync and the trade engine share.

pub mod client;

use std::net::{Ipv4Addr, SocketAddr};

use bdk_kyoto::TrustedPeer;
use bdk_wallet::bitcoin::{Transaction, Txid};
use thiserror::Error;

#[derive(Debug, Error)]
#[error(transparent)]
#[non_exhaustive]
pub enum ChainApiError {
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl ChainApiError {
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

/// A peer address that cannot be used.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid peer address {peer:?}: {reason}; expected host:port")]
pub struct PeerAddressError {
    pub peer: String,
    pub reason: String,
}

/// Parses a peer address of the form `host:port`. The host is an IP address (IPv6 in brackets),
/// a host name, or `localhost`.
pub fn parse_peer(peer: &str) -> Result<TrustedPeer, PeerAddressError> {
    let peer = peer.trim();
    let invalid = |reason: String| PeerAddressError {
        peer: peer.to_owned(),
        reason,
    };
    // IP literal with port (v4 or bracketed v6): no hostname handling needed
    if let Ok(addr) = peer.parse::<SocketAddr>() {
        return Ok(TrustedPeer::from_socket_addr(addr));
    }
    let (host, port) = peer
        .rsplit_once(':')
        .ok_or_else(|| invalid("the port is missing".to_owned()))?;
    if host.is_empty() {
        return Err(invalid("the host is missing".to_owned()));
    }
    let port: u16 = port
        .parse()
        .map_err(|e| invalid(format!("the port is no number below 65536 ({e})")))?;
    if host.eq_ignore_ascii_case("localhost") {
        Ok(TrustedPeer::from_socket_addr(SocketAddr::new(
            Ipv4Addr::LOCALHOST.into(),
            port,
        )))
    } else {
        Ok(TrustedPeer::from_hostname(host, port))
    }
}

/// Parses a list of peer addresses. See [`parse_peer`].
pub fn parse_peers(peers: &[String]) -> Result<Vec<TrustedPeer>, PeerAddressError> {
    peers.iter().map(|peer| parse_peer(peer)).collect()
}

/// Minimal abstraction over blockchain interaction for broadcasting transactions.
pub trait ChainApi: Send + Sync {
    fn transaction_broadcast(&self, tx: &Transaction) -> Result<Txid, ChainApiError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F22: a peer address that cannot be used gives an error, not a panic.
    #[test]
    fn f22_peer_addresses_are_parsed_without_panic() {
        for valid in [
            "127.0.0.1:18444",
            "[::1]:18444",
            "localhost:18444",
            "LocalHost:18444",
            "node.example.org:8333",
            " 127.0.0.1:18444 ",
        ] {
            assert!(parse_peer(valid).is_ok(), "{valid}");
        }
        for (invalid, reason) in [
            ("127.0.0.1", "the port is missing"),
            ("node.example.org", "the port is missing"),
            ("", "the port is missing"),
            (":8333", "the host is missing"),
            ("node.example.org:port", "the port is no number"),
            ("node.example.org:70000", "the port is no number"),
        ] {
            let error = parse_peer(invalid).expect_err(invalid);
            assert!(error.reason.contains(reason), "{invalid}: {error}");
        }
        let peers = ["127.0.0.1:18444".to_owned(), "nonsense".to_owned()];
        assert_eq!(
            parse_peers(&peers).expect_err("one invalid peer").peer,
            "nonsense"
        );
        assert_eq!(parse_peers(&peers[..1]).map(|peers| peers.len()), Ok(1));
    }
}
