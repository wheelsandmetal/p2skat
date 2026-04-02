use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use crate::game::bidding::BidAction;
use crate::game::card::{Card, Suit};
use crate::game::contract::{Contract, GameType};
use crate::game::trick::Trick;

/// Error returned when the user presses Ctrl+C.
#[derive(Debug)]
pub struct UserQuit;

impl std::fmt::Display for UserQuit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "User quit")
    }
}

impl std::error::Error for UserQuit {}

/// Async function that polls for Ctrl+C. Use in `tokio::select!` alongside
/// network recv calls so users can quit during opponent turns.
pub async fn poll_quit() -> UserQuit {
    loop {
        let is_quit = tokio::task::spawn_blocking(|| {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    return key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        if is_quit {
            return UserQuit;
        }
    }
}

/// Terminal UI for the Skat game using ratatui.
pub struct TerminalUi {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    my_seat: usize,
    names: [String; 3],

    // Persistent display state
    hand: Vec<Card>,
    scores: [i32; 3],
    contract: Option<Contract>,
    trick_num: u32,
    previous_trick: Option<Vec<(usize, Card)>>,
    current_trick: Vec<(usize, Card)>,
    round: u32,

    // Per-frame rendering state
    action_lines: Vec<Line<'static>>,
    help_line: Line<'static>,
    cursor: Option<usize>,
    legal_cards: Option<Vec<Card>>,
    skat_selected: Vec<usize>,
}

impl TerminalUi {
    pub fn new(my_seat: usize, names: &[String; 3]) -> Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        Ok(Self {
            terminal,
            my_seat,
            names: names.clone(),
            hand: Vec::new(),
            scores: [0; 3],
            contract: None,
            trick_num: 0,
            previous_trick: None,
            current_trick: Vec::new(),
            round: 0,
            action_lines: Vec::new(),
            help_line: Line::from(""),
            cursor: None,
            legal_cards: None,
            skat_selected: Vec::new(),
        })
    }

    pub fn cleanup(&mut self) -> Result<()> {
        terminal::disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    // === State setters ===

    pub fn set_hand(&mut self, hand: &[Card]) {
        self.hand = hand.to_vec();
    }

    pub fn set_trick(&mut self, trick: &Trick) {
        self.current_trick = trick.cards.clone();
    }

    pub fn set_contract(&mut self, contract: &Contract) {
        self.contract = Some(*contract);
    }

    pub fn set_scores(&mut self, scores: &[i32; 3]) {
        self.scores = *scores;
    }

    pub fn set_trick_num(&mut self, n: u32) {
        if n > 1 {
            self.previous_trick = Some(self.current_trick.clone());
        }
        self.trick_num = n;
    }

    pub fn set_round(&mut self, n: u32) {
        self.round = n;
    }

    // === Drawing ===

    fn draw(&mut self) -> Result<()> {
        let info_spans = self.build_info_line();
        let action_lines = self.action_lines.clone();
        let hand_line = self.build_hand_line();
        let help_line = self.help_line.clone();
        let my_seat_name = crate::game::bidding::Seat::from_index(self.my_seat).name();

        self.terminal.draw(|frame| {
            let chunks = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(frame.area());

            // Info bar
            let info = Paragraph::new(Line::from(info_spans))
                .block(Block::default().borders(Borders::ALL).title(" Game "));
            frame.render_widget(info, chunks[0]);

            // Hand
            let hand_title = format!(" Your Hand ({}) ", my_seat_name);
            let hand_widget = Paragraph::new(hand_line)
                .block(Block::default().borders(Borders::ALL).title(hand_title));
            frame.render_widget(hand_widget, chunks[1]);

            // Action area
            let action = Paragraph::new(action_lines)
                .block(Block::default().borders(Borders::ALL).title(" Action "));
            frame.render_widget(action, chunks[2]);

            // Help bar
            let help = Paragraph::new(help_line);
            frame.render_widget(help, chunks[3]);
        })?;

        Ok(())
    }

    // Displays
    pub fn display_tricks(&mut self) -> Result<()> {
        match &self.previous_trick {
            Some(trick) => {
                self.action_lines = vec![Line::from(format!("=== Trick {} ===", self.trick_num - 1))];
                self.action_lines.push(Line::from(""));
                self.action_lines.extend(self.trick_action_lines(&trick));
                self.action_lines.push(Line::from(""));
                self.action_lines.push(Line::from(format!("=== Trick {} ===", self.trick_num)));
            }
            None => {
                self.action_lines = vec![Line::from(format!("=== Trick {} ===", self.trick_num))];
            }
        }

        self.action_lines.push(Line::from(""));
        self.action_lines.extend(self.trick_action_lines(&self.current_trick));
        self.draw()?;
        Ok(())
    }

    fn trick_action_lines(&self, trick: &Vec<(usize, Card)>) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for seat in 0..3 {
            let name = if seat == self.my_seat {
                "You".to_string()
            } else {
                self.names[seat].clone()
            };

            let card_played = trick.iter().find(|(s, _)| *s == seat);
            let line = match card_played {
                Some((_, card)) => {
                    let mut spans = vec![Span::raw(format!("  {}: ", name))];
                    spans.push(card_span(*card));
                    Line::from(spans)
                }
                None => Line::from(format!("  {}: ...", name)),
            };
            lines.push(line);
        }
        lines
    }


    fn build_info_line(&self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();

        spans.push(Span::styled(
            format!("Round {} ", self.round),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::raw("│ "));

        if let Some(contract) = &self.contract {
            spans.push(game_type_display(&contract.game_type));
            if contract.modifiers.hand {
                spans.push(Span::raw(" Hand"));
            }
            spans.push(Span::raw(" │ "));
        }

        if self.trick_num > 0 {
            spans.push(Span::raw(format!("Trick {}/10 │ ", self.trick_num)));
        }

        for (i, name) in self.names.iter().enumerate() {
            let label = if i == self.my_seat { "You" } else { name.as_str() };
            spans.push(Span::raw(format!("{}: {} ", label, self.scores[i])));
        }

        spans
    }

    fn build_hand_line(&self) -> Line<'static> {
        if self.hand.is_empty() {
            return Line::from(Span::raw("  (no cards)"));
        }

        let mut spans = Vec::new();
        spans.push(Span::raw(" "));

        for (i, &card) in self.hand.iter().enumerate() {
            let is_cursor = self.cursor == Some(i);
            let is_skat_selected = self.skat_selected.contains(&i);
            let is_legal = match &self.legal_cards {
                Some(legal) => legal.contains(&card),
                None => true,
            };

            if is_skat_selected {
                spans.push(Span::raw("[X]"));
                spans.push(card_span_selected(card));
                spans.push(Span::raw(" "));
            } else if is_cursor {
                spans.push(Span::raw("["));
                spans.push(card_span_selected(card));
                spans.push(Span::raw("] "));
            } else if !is_legal {
                spans.push(Span::raw(" "));
                spans.push(card_span_dimmed(card));
                spans.push(Span::raw("  "));
            } else {
                spans.push(Span::raw(" "));
                spans.push(card_span(card));
                spans.push(Span::raw("  "));
            }
        }

        Line::from(spans)
    }

    // === Check for Ctrl+C ===

    fn check_quit(key: &KeyEvent) -> bool {
        key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
    }

    // === Public UI methods ===

    pub fn show_status(&mut self, msg: &str) -> Result<()> {
        self.action_lines = vec![Line::from(msg.to_string())];
        self.help_line = Line::from("Ctrl+C quit");
        self.cursor = None;
        self.legal_cards = None;
        self.draw()
    }

    pub fn get_bid_action(
        &mut self,
        hand: &[Card],
        next_bid: Option<u32>,
        is_responder: bool,
    ) -> Result<BidAction> {
        self.hand = hand.to_vec();
        self.cursor = None;
        self.legal_cards = None;

        if is_responder {
            self.help_line = Line::from(" h hold │ p pass │ Ctrl+C quit");
        } else {
            let next_str = match next_bid {
                Some(v) => format!("{}", v),
                None => "??".to_string(),
            };
            self.help_line = Line::from(format!(
                " b bid {} │ p pass │ Ctrl+C quit",
                next_str
            ));
        }

        self.draw()?;

        loop {
            if let Event::Key(key) = event::read()? {
                if Self::check_quit(&key) {
                    return Err(UserQuit.into());
                }
                match key.code {
                    KeyCode::Char('b') if !is_responder => {
                        if let Some(v) = next_bid {
                            self.help_line = Line::from(" You bid │ Ctrl+C quit");
                            return Ok(BidAction::Bid(v));
                        }
                    }
                    KeyCode::Char('h') if is_responder => {
                        self.help_line = Line::from(" You held │ Ctrl+C quit");
                        return Ok(BidAction::Hold);
                    }
                    KeyCode::Char('p') => {
                        self.help_line = Line::from(" You passed │ Ctrl+C quit");
                        return Ok(BidAction::Pass);
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn show_bid_action(&mut self, name: &str, action: &BidAction) -> Result<()> {
        let msg = match action {
            BidAction::Bid(v) => format!("{} bids {}", name, v),
            BidAction::Hold => format!("{} holds", name),
            BidAction::Pass => format!("{} passes", name),
        };
        self.action_lines.push(Line::from(msg));
        self.draw()?;
        std::thread::sleep(std::time::Duration::from_millis(800));
        Ok(())
    }

    pub fn ask_skat_pickup(&mut self, hand: &[Card]) -> Result<bool> {
        self.hand = hand.to_vec();
        self.cursor = None;
        self.legal_cards = None;
        self.action_lines = vec![
            Line::from("=== You won the bidding! ==="),
            Line::from(""),
            Line::from("Pick up the Skat?"),
        ];
        self.help_line = Line::from(" y yes │ n no (Hand game) │ Ctrl+C quit");
        self.draw()?;

        loop {
            if let Event::Key(key) = event::read()? {
                if Self::check_quit(&key) {
                    return Err(UserQuit.into());
                }
                match key.code {
                    KeyCode::Char('y') => return Ok(true),
                    KeyCode::Char('n') => return Ok(false),
                    _ => {}
                }
            }
        }
    }

    pub fn choose_skat_discard(&mut self, hand: &[Card]) -> Result<Vec<Card>> {
        self.hand = hand.to_vec();
        self.skat_selected = Vec::new();
        self.cursor = Some(0);
        self.legal_cards = None;

        self.action_lines = vec![Line::from("Choose 2 cards to put in the Skat:")];
        self.help_line = Line::from(
            " \u{2190} \u{2192} move │ Space select/deselect │ Enter confirm │ Ctrl+C quit",
        );
        self.draw()?;

        loop {
            if let Event::Key(key) = event::read()? {
                if Self::check_quit(&key) {
                    return Err(UserQuit.into());
                }
                let cursor = self.cursor.unwrap_or(0);
                match key.code {
                    KeyCode::Left if cursor > 0 => {
                        self.cursor = Some(cursor - 1);
                    }
                    KeyCode::Right if cursor < self.hand.len() - 1 => {
                        self.cursor = Some(cursor + 1);
                    }
                    KeyCode::Char(' ') => {
                        if self.skat_selected.contains(&cursor) {
                            self.skat_selected.retain(|&x| x != cursor);
                        } else if self.skat_selected.len() < 2 {
                            self.skat_selected.push(cursor);
                        }
                    }
                    KeyCode::Enter if self.skat_selected.len() == 2 => {
                        let cards: Vec<Card> =
                            self.skat_selected.iter().map(|&i| self.hand[i]).collect();
                        self.skat_selected.clear();
                        self.cursor = None;
                        return Ok(cards);
                    }
                    _ => {}
                }

                // Update status in action area
                self.action_lines = vec![Line::from("Choose 2 cards to put in the Skat:")];
                if self.skat_selected.len() == 2 {
                    self.action_lines
                        .push(Line::from("2 cards selected. Press Enter to confirm."));
                }
                self.draw()?;
            }
        }
    }

    pub fn declare_contract(
        &mut self,
        hand: &[Card],
        bid: u32,
        is_hand: bool,
    ) -> Result<Contract> {
        self.hand = hand.to_vec();
        self.legal_cards = None;
        self.skat_selected.clear();

        let options = [
            ("Clubs", "\u{2663}", "12"),
            ("Spades", "\u{2660}", "11"),
            ("Hearts", "\u{2665}", "10"),
            ("Diamonds", "\u{2666}", "9"),
            ("Grand", "", "24"),
            ("Null", "", "23"),
        ];
        let mut menu_cursor = 0usize;

        loop {
            self.cursor = None;
            self.action_lines = vec![Line::from(format!(
                "=== Declare Contract (bid: {}) ===",
                bid
            ))];
            self.action_lines.push(Line::from(""));

            for (i, (name, symbol, base)) in options.iter().enumerate() {
                let marker = if i == menu_cursor { "> " } else { "  " };
                let style = if i == menu_cursor {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let text = if symbol.is_empty() {
                    format!("{}{} ({})", marker, name, base)
                } else {
                    format!("{}{} {} ({})", marker, name, symbol, base)
                };
                self.action_lines.push(Line::from(Span::styled(text, style)));
            }

            self.help_line = Line::from(
                " \u{2191} \u{2193} select │ Enter confirm │ Ctrl+C quit",
            );
            self.draw()?;

            if let Event::Key(key) = event::read()? {
                if Self::check_quit(&key) {
                    return Err(UserQuit.into());
                }
                match key.code {
                    KeyCode::Up if menu_cursor > 0 => menu_cursor -= 1,
                    KeyCode::Down if menu_cursor < options.len() - 1 => menu_cursor += 1,
                    KeyCode::Enter => {
                        let game_type = match menu_cursor {
                            0 => GameType::Suit(Suit::Clubs),
                            1 => GameType::Suit(Suit::Spades),
                            2 => GameType::Suit(Suit::Hearts),
                            3 => GameType::Suit(Suit::Diamonds),
                            4 => GameType::Grand,
                            5 => GameType::Null,
                            _ => unreachable!(),
                        };

                        let modifiers = crate::game::contract::Modifiers {
                            hand: is_hand,
                            ..Default::default()
                        };

                        return Ok(Contract::new(game_type, modifiers));
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn choose_card(
        &mut self,
        hand: &[Card],
        legal: &[Card],
        trick: &Trick,
        _contract: &Contract,
        trick_num: u32,
    ) -> Result<Card> {
        self.hand = hand.to_vec();
        self.legal_cards = Some(legal.to_vec());
        self.current_trick = trick.cards.clone();
        self.skat_selected.clear();

        // Start cursor on first legal card
        let mut cursor = 0usize;
        for (i, card) in hand.iter().enumerate() {
            if legal.contains(card) {
                cursor = i;
                break;
            }
        }
        self.cursor = Some(cursor);

        loop {
            self.help_line = Line::from(
                " \u{2190} \u{2192} move │ Enter play │ Ctrl+C quit",
            );
            self.draw()?;

            if let Event::Key(key) = event::read()? {
                if Self::check_quit(&key) {
                    return Err(UserQuit.into());
                }
                let cur = self.cursor.unwrap_or(0);
                match key.code {
                    KeyCode::Left if cur > 0 => {
                        self.cursor = Some(cur - 1);
                    }
                    KeyCode::Right if cur < hand.len() - 1 => {
                        self.cursor = Some(cur + 1);
                    }
                    KeyCode::Enter => {
                        if legal.contains(&hand[cur]) {
                            self.cursor = None;
                            self.legal_cards = None;
                            return Ok(hand[cur]);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn show_trick_result(&mut self, winner_name: &str, points: u32) -> Result<()> {
        self.action_lines.push(Line::from(""));
        self.action_lines.push(Line::from(format!(
            "{} wins the trick ({} points)",
            winner_name, points
        )));
        self.draw()?;
        std::thread::sleep(std::time::Duration::from_millis(1000));
        Ok(())
    }

    pub fn show_round_result(
        &mut self,
        declarer_name: &str,
        game_value: i32,
        declarer_points: u32,
        scores: &[i32; 3],
        names: &[String; 3],
    ) -> Result<()> {
        self.scores = *scores;

        let result = if game_value > 0 { "WINS" } else { "LOSES" };
        self.action_lines = vec![
            Line::from(format!(
                "{} {} with {} card points (game value: {})",
                declarer_name, result, declarer_points, game_value
            )),
            Line::from(""),
            Line::from("Scores:"),
        ];

        for (i, name) in names.iter().enumerate() {
            self.action_lines
                .push(Line::from(format!("  {}: {}", name, scores[i])));
        }

        self.help_line = Line::from("");
        self.cursor = None;
        self.legal_cards = None;
        self.draw()
    }

    pub fn wait_for_continue(&mut self) -> Result<bool> {
        self.help_line = Line::from(" Enter next round │ q quit │ Ctrl+C quit");
        self.draw()?;

        loop {
            if let Event::Key(key) = event::read()? {
                if Self::check_quit(&key) {
                    return Err(UserQuit.into());
                }
                match key.code {
                    KeyCode::Enter => return Ok(true),
                    KeyCode::Char('q') => return Ok(false),
                    _ => {}
                }
            }
        }
    }
}

impl Drop for TerminalUi {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

// === Card rendering helpers ===

fn card_span(card: Card) -> Span<'static> {
    let text = format!("{}{}", card.rank.short_name(), card.suit.symbol());
    let color = match card.suit {
        Suit::Hearts | Suit::Diamonds => Color::Red,
        _ => Color::White,
    };
    Span::styled(text, Style::default().fg(color))
}

fn card_span_selected(card: Card) -> Span<'static> {
    let text = format!("{}{}", card.rank.short_name(), card.suit.symbol());
    let color = match card.suit {
        Suit::Hearts | Suit::Diamonds => Color::Red,
        _ => Color::White,
    };
    Span::styled(
        text,
        Style::default()
            .fg(color)
            .add_modifier(Modifier::REVERSED),
    )
}

fn card_span_dimmed(card: Card) -> Span<'static> {
    let text = format!("{}{}", card.rank.short_name(), card.suit.symbol());
    Span::styled(text, Style::default().fg(Color::DarkGray))
}

fn game_type_display(gt: &GameType) -> Span<'static> {
    match gt {
        GameType::Suit(suit) => {
            let color = match suit {
                Suit::Hearts | Suit::Diamonds => Color::Red,
                _ => Color::White,
            };
            Span::styled(
                format!("{:?} {}", suit, suit.symbol()),
                Style::default().fg(color),
            )
        }
        GameType::Grand => Span::styled("Grand", Style::default().fg(Color::Yellow)),
        GameType::Null => Span::styled("Null", Style::default().fg(Color::Gray)),
    }
}
