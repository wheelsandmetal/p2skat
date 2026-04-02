/// Standard Skat bid values in ascending order.
pub const BID_VALUES: &[u32] = &[
    18, 20, 22, 23, 24, 27, 30, 33, 35, 36, 40, 44, 45, 46, 48, 50, 54, 55, 59, 60, 63, 66, 70,
    72, 77, 80, 81, 84, 88, 90, 96, 99, 100, 108, 110, 117, 120, 121, 126, 130, 132, 135, 140,
    143, 144, 150, 153, 154, 156, 160, 162, 165, 168, 170, 176, 180, 187, 192, 198, 204, 216, 240,
    264,
];

/// Seat positions at the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Seat {
    Forehand,  // first to lead
    Middlehand,
    Rearhand,
}

impl Seat {
    pub fn index(self) -> usize {
        match self {
            Seat::Forehand => 0,
            Seat::Middlehand => 1,
            Seat::Rearhand => 2,
        }
    }

    pub fn from_index(i: usize) -> Self {
        match i % 3 {
            0 => Seat::Forehand,
            1 => Seat::Middlehand,
            _ => Seat::Rearhand,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Seat::Forehand => "Forehand",
            Seat::Middlehand => "Middlehand",
            Seat::Rearhand => "Rearhand",
        }
    }
}

/// A player's response during bidding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BidAction {
    /// Make a bid at the given value.
    Bid(u32),
    /// Accept/hold the current bid (Forehand says "yes").
    Hold,
    /// Pass — drop out of bidding.
    Pass,
}

/// Phase of the bidding process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BidPhase {
    /// Middlehand bids against Forehand.
    MiddleVsFore,
    /// Winner of first phase bids against Rearhand.
    WinnerVsRear,
    /// Bidding complete.
    Done,
}

/// Bidding state machine.
#[derive(Debug, Clone)]
pub struct BiddingState {
    phase: BidPhase,
    /// Current bid value (index into BID_VALUES).
    current_bid_index: Option<usize>,
    /// Who is currently the "caller" (the one making bids).
    caller: Seat,
    /// Who is currently the "responder" (the one accepting/passing).
    pub responder: Seat,
    /// Who has passed.
    passed: [bool; 3],
    /// The seat whose turn it is.
    pub turn: Seat,
    /// The winning bidder (set when bidding concludes).
    pub winner: Option<Seat>,
    /// The final bid value.
    pub final_bid: Option<u32>,
}

impl BiddingState {
    pub fn new() -> Self {
        // Middlehand bids first, Forehand responds.
        Self {
            phase: BidPhase::MiddleVsFore,
            current_bid_index: None,
            caller: Seat::Middlehand,
            responder: Seat::Forehand,
            passed: [false; 3],
            turn: Seat::Middlehand,
            winner: None,
            final_bid: None,
        }
    }

    /// Current bid value, if any.
    pub fn current_bid(&self) -> Option<u32> {
        self.current_bid_index.map(|i| BID_VALUES[i])
    }

    /// The next valid bid value (one step above current).
    pub fn next_bid_value(&self) -> Option<u32> {
        let next_idx = match self.current_bid_index {
            Some(i) => i + 1,
            None => 0,
        };
        BID_VALUES.get(next_idx).copied()
    }

    /// Is bidding complete?
    pub fn is_done(&self) -> bool {
        self.phase == BidPhase::Done
    }

    /// Process a bid action. Returns Ok(()) or an error message.
    pub fn process(&mut self, seat: Seat, action: BidAction) -> Result<(), String> {
        if self.is_done() {
            return Err("Bidding is already complete".to_string());
        }
        if seat != self.turn {
            return Err(format!(
                "Not {}'s turn, waiting for {}",
                seat.name(),
                self.turn.name()
            ));
        }

        match action {
            BidAction::Bid(value) => {
                if seat != self.caller {
                    return Err("Only the caller can make bids".to_string());
                }
                let next = self.next_bid_value();
                match next {
                    Some(v) if value == v => {}
                    _ => {
                        // Allow bidding any valid value >= next
                        let min = self.next_bid_value().unwrap_or(18);
                        if value < min || !BID_VALUES.contains(&value) {
                            return Err(format!(
                                "Invalid bid {}. Minimum is {}, must be a standard bid value",
                                value, min
                            ));
                        }
                        // Find the index of this bid value
                        let idx = BID_VALUES.iter().position(|&v| v == value).unwrap();
                        self.current_bid_index = Some(idx);
                        self.turn = self.responder;
                        return Ok(());
                    }
                }
                self.current_bid_index = Some(
                    BID_VALUES
                        .iter()
                        .position(|&v| v == value)
                        .unwrap(),
                );
                self.turn = self.responder;
            }
            BidAction::Hold => {
                if seat != self.responder {
                    return Err("Only the responder can hold".to_string());
                }
                if self.current_bid_index.is_none() {
                    return Err("Nothing to hold — no bid has been made".to_string());
                }
                // Responder holds, caller must bid higher.
                self.turn = self.caller;
            }
            BidAction::Pass => {
                self.passed[seat.index()] = true;
                self.advance_phase();
            }
        }

        Ok(())
    }

    fn advance_phase(&mut self) {
        match self.phase {
            BidPhase::MiddleVsFore => {
                if self.passed[self.caller.index()] {
                    // Middlehand passed. Rearhand bids against Forehand.
                    self.phase = BidPhase::WinnerVsRear;
                    self.caller = Seat::Rearhand;
                    self.responder = Seat::Forehand;
                    self.turn = Seat::Rearhand;
                } else if self.passed[self.responder.index()] {
                    // Forehand passed. Rearhand bids against Middlehand.
                    self.phase = BidPhase::WinnerVsRear;
                    self.caller = Seat::Rearhand;
                    self.responder = Seat::Middlehand;
                    self.turn = Seat::Rearhand;
                }
            }
            BidPhase::WinnerVsRear => {
                if self.passed[self.caller.index()] {
                    // Rearhand passed. Responder wins.
                    self.winner = Some(self.responder);
                    self.final_bid = self.current_bid().or(Some(18));
                    self.phase = BidPhase::Done;
                } else if self.passed[self.responder.index()] {
                    // Responder passed. Rearhand wins.
                    self.winner = Some(self.caller);
                    self.final_bid = self.current_bid().or(Some(18));
                    self.phase = BidPhase::Done;
                }

                // Special case: if everyone passed (Ramsch), forehand "wins" at 0.
                if self.passed.iter().all(|&p| p) {
                    // All passed — in strict Skat, Forehand must play.
                    // For now, forehand becomes declarer at 18.
                    self.winner = Some(Seat::Forehand);
                    self.final_bid = Some(18);
                    self.phase = BidPhase::Done;
                }
            }
            BidPhase::Done => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_bidding() {
        let mut state = BiddingState::new();

        // Middlehand bids 18
        state.process(Seat::Middlehand, BidAction::Bid(18)).unwrap();
        // Forehand holds
        state.process(Seat::Forehand, BidAction::Hold).unwrap();
        // Middlehand bids 20
        state.process(Seat::Middlehand, BidAction::Bid(20)).unwrap();
        // Forehand passes
        state.process(Seat::Forehand, BidAction::Pass).unwrap();

        // Now Rearhand vs Middlehand
        assert!(!state.is_done());
        // Rearhand passes
        state.process(Seat::Rearhand, BidAction::Pass).unwrap();

        assert!(state.is_done());
        assert_eq!(state.winner, Some(Seat::Middlehand));
        assert_eq!(state.final_bid, Some(20));
    }

    #[test]
    fn all_pass() {
        let mut state = BiddingState::new();
        // Middlehand passes immediately
        state.process(Seat::Middlehand, BidAction::Pass).unwrap();
        // Rearhand passes
        state.process(Seat::Rearhand, BidAction::Pass).unwrap();

        // Forehand gets to play but passes too — forehand must play
        // Actually per the state machine: Middle passed -> Rear vs Fore -> Rear passes -> Fore wins
        assert!(state.is_done());
        assert_eq!(state.winner, Some(Seat::Forehand));
    }

    #[test]
    fn bid_values_sorted() {
        for w in BID_VALUES.windows(2) {
            assert!(w[0] < w[1], "{} should be < {}", w[0], w[1]);
        }
    }
}
