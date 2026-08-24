//! Tickets for `iroh-docs` documents.

use iroh::EndpointAddr;
use iroh_tickets::{ParseError, Ticket};
use serde::{Deserialize, Serialize};

use crate::Capability;

/// Contains both a key (either secret or public) to a document, and a list of peers to join.
#[derive(Serialize, Deserialize, Clone, Debug, derive_more::Display)]
#[display("{}", Ticket::encode_string(self))]
pub struct DocTicket {
    /// either a public or private key
    pub capability: Capability,
    /// A list of nodes to contact.
    pub nodes: Vec<EndpointAddr>,
}

/// Wire format for [`DocTicket`].
///
/// In the future we might have multiple variants (not versions, since they
/// might be both equally valid), so this is a single variant enum to force
/// postcard to add a discriminator.
#[derive(Serialize, Deserialize)]
enum TicketWireFormat {
    Variant0(DocTicket),
}

impl Ticket for DocTicket {
    const KIND: &'static str = "doc";

    fn encode_bytes(&self) -> Vec<u8> {
        let data = TicketWireFormat::Variant0(self.clone());
        postcard::to_stdvec(&data).expect("postcard serialization failed")
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        let res: TicketWireFormat = postcard::from_bytes(bytes)?;
        let TicketWireFormat::Variant0(res) = res;
        if res.nodes.is_empty() {
            return Err(ParseError::verification_failed(
                "addressing info cannot be empty",
            ));
        }
        Ok(res)
    }
}

impl DocTicket {
    /// Create a new doc ticket
    pub fn new(capability: Capability, peers: Vec<EndpointAddr>) -> Self {
        Self {
            capability,
            nodes: peers,
        }
    }
}

impl std::str::FromStr for DocTicket {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ticket::decode_string(s)
    }
}


