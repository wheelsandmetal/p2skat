use serde::{Deserialize, Serialize};

use crate::game::bidding::BidAction;
use crate::game::card::Card;

/// All protocol messages exchanged between peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Initial handshake — announce player name.
    Hello { name: String },

    /// Acknowledge all 3 players are connected, assign seats.
    GameStart {
        seats: Vec<String>, // names in seat order [Forehand, Middlehand, Rearhand]
        your_seat: usize,
    },

    /// Seat 1 tells host its listen port, and optionally a full address override
    /// (required for Tor, where the host can't derive the reachable address).
    PeerListen { port: u16, addr_override: Option<String> },

    /// Host tells seat 2 the full address to connect to for seat 1.
    PeerConnect { addr: String },

    /// Shuffled and encrypted deck from one player to the next.
    ShuffledDeck {
        /// Encrypted card values as hex strings (BigUint serialization).
        cards: Vec<String>,
    },

    /// Partial decryption of specific card positions for a target player.
    PartialDecrypt {
        /// The target player seat index who should receive these.
        target: usize,
        /// Partially decrypted card values (hex strings).
        cards: Vec<String>,
        /// Which positions in the deck these correspond to.
        indices: Vec<usize>,
    },

    /// A bid action during the bidding phase.
    BidMsg {
        seat: usize,
        action: BidActionMsg,
    },

    /// Declarer picks up the Skat (requests decryption of Skat cards).
    SkatRequest,

    /// Declarer puts down cards and declares the contract.
    Declaration {
        game_type: GameTypeMsg,
        hand: bool,
        schneider_announced: bool,
        schwarz_announced: bool,
        ouvert: bool,
        /// If ouvert, the declarer's hand is revealed.
        revealed_hand: Option<Vec<CardMsg>>,
    },

    /// Play a card in a trick.
    PlayCard { card: CardMsg },

    /// End-of-hand: reveal encryption keys for verification.
    RevealKeys {
        /// Encryption key `e` as hex string.
        e: String,
        /// Decryption key `d` as hex string.
        d: String,
    },

    /// Score summary for the completed hand.
    HandResult {
        declarer: usize,
        game_value: i32,
        scores: [i32; 3],
    },

    /// Request to start the next round.
    NextRound,

    /// Error / protocol violation.
    Error { message: String },

    /// A player is leaving the game.
    Quit,
}

/// Serializable bid action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BidActionMsg {
    Bid(u32),
    Hold,
    Pass,
}

impl From<BidAction> for BidActionMsg {
    fn from(action: BidAction) -> Self {
        match action {
            BidAction::Bid(v) => BidActionMsg::Bid(v),
            BidAction::Hold => BidActionMsg::Hold,
            BidAction::Pass => BidActionMsg::Pass,
        }
    }
}

impl From<BidActionMsg> for BidAction {
    fn from(msg: BidActionMsg) -> Self {
        match msg {
            BidActionMsg::Bid(v) => BidAction::Bid(v),
            BidActionMsg::Hold => BidAction::Hold,
            BidActionMsg::Pass => BidAction::Pass,
        }
    }
}

/// Serializable card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardMsg {
    pub suit: u8,
    pub rank: u8,
}

impl From<Card> for CardMsg {
    fn from(card: Card) -> Self {
        CardMsg {
            suit: card.suit as u8,
            rank: card.rank as u8,
        }
    }
}

impl CardMsg {
    pub fn to_card(&self) -> Card {
        use crate::game::card::{Rank, Suit};
        let suit = match self.suit {
            0 => Suit::Diamonds,
            1 => Suit::Hearts,
            2 => Suit::Spades,
            3 => Suit::Clubs,
            _ => panic!("Invalid suit index"),
        };
        let rank = match self.rank {
            0 => Rank::Seven,
            1 => Rank::Eight,
            2 => Rank::Nine,
            3 => Rank::Queen,
            4 => Rank::King,
            5 => Rank::Ten,
            6 => Rank::Ace,
            7 => Rank::Jack,
            _ => panic!("Invalid rank index"),
        };
        Card::new(suit, rank)
    }
}

/// Serializable game type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GameTypeMsg {
    Suit(u8), // suit index
    Grand,
    Null,
}

/// Framing: length-prefixed JSON messages over TCP.
/// Format: [4 bytes big-endian length][JSON payload]
pub mod framing {
    use super::Message;
    use anyhow::Result;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub async fn send<W: AsyncWriteExt + Unpin>(writer: &mut W, msg: &Message) -> Result<()> {
        let json = serde_json::to_vec(msg)?;
        let len = (json.len() as u32).to_be_bytes();
        writer.write_all(&len).await?;
        writer.write_all(&json).await?;
        writer.flush().await?;
        Ok(())
    }

    pub async fn recv<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Message> {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > 10 * 1024 * 1024 {
            anyhow::bail!("Message too large: {} bytes", len);
        }

        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await?;
        let msg: Message = serde_json::from_slice(&buf)?;
        Ok(msg)
    }
}
