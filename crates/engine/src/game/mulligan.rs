use crate::game::ability_utils::build_resolved_from_def;
use crate::game::effects::resolve_ability_chain;
use crate::types::ability::AbilityKind;
use crate::types::events::GameEvent;
use crate::types::game_state::{
    GameState, MulliganBottomEntry, MulliganDecisionEntry, PendingBeginGameAbility, WaitingFor,
};
use crate::types::identifiers::ObjectId;
use crate::types::player::PlayerId;
use crate::types::zones::Zone;

use super::turns;
use super::zones;

/// CR 103.4: Starting hand size is seven cards.
const STARTING_HAND_SIZE: usize = 7;
const MAX_MULLIGANS: u8 = 7;

/// CR 103.5c + Commander RC supplement: whether `state` grants a free first
/// mulligan. True for any multiplayer game (≥3 seats), and for duels in
/// formats where `GameFormat::grants_free_first_mulligan()` holds.
fn free_first_mulligan(state: &GameState) -> bool {
    state.seat_order.len() > 2 || state.format_config.format.grants_free_first_mulligan()
}

/// CR 103.5: Cards a player must put on the bottom of their library after
/// keeping with `mulligan_count` mulligans taken (free-first discount applied
/// when the game grants one).
fn bottom_count_for(mulligan_count: u8, free_first: bool) -> u8 {
    if free_first {
        mulligan_count.saturating_sub(1)
    } else {
        mulligan_count
    }
}

/// CR 103.4: Start the mulligan process — shuffle libraries and draw 7 for each player.
///
/// CR 103.5: All players decide simultaneously. The returned
/// `WaitingFor::MulliganDecision` carries every living player in seat order;
/// each may submit `MulliganDecision { keep }` in any arrival order.
///
/// CR 103.5b/103.5d deferred: This implementation collapses CR 103.5's
/// per-round structure into a single per-player loop. The output state is
/// equivalent for the current set of supported cards (no Serum-Powder-class
/// "any time you could mulligan" effects, no Two-Headed Giant team mulligans).
/// If those land in scope, this flow must be split back into explicit rounds.
pub fn start_mulligan(state: &mut GameState, events: &mut Vec<GameEvent>) -> WaitingFor {
    events.push(GameEvent::MulliganStarted);

    // Shuffle every player's library.
    let GameState { players, rng, .. } = &mut *state;
    for player in players.iter_mut() {
        crate::util::im_ext::shuffle_vector(&mut player.library, rng);
    }

    // Draw the opening hand for each player in seat order.
    let seat_order = state.seat_order.clone();
    for &player_id in &seat_order {
        draw_n(state, player_id, STARTING_HAND_SIZE, events);
    }

    // Build the initial pending set: every player, mulligan_count = 0,
    // in seat order so iteration is deterministic.
    let pending = seat_order
        .iter()
        .map(|&player| MulliganDecisionEntry {
            player,
            mulligan_count: 0,
        })
        .collect();

    WaitingFor::MulliganDecision {
        pending,
        free_first_mulligan: free_first_mulligan(state),
    }
}

/// CR 103.5: Resolve one player's `MulliganDecision { keep }` action.
///
/// - `keep: true` removes the player from `pending`. The player has locked in
///   their hand for the game; their bottom-cards selection is deferred to the
///   second phase (CR 103.5 second sentence: "all players who decided to take
///   mulligans do so at the same time" — bottoms happen after every player
///   has kept).
/// - `keep: false` increments that player's `mulligan_count`, shuffles their
///   hand back into their library, and redraws 7. The player remains in
///   `pending` to decide again.
/// - If incrementing reaches the maximum mulligan count (CR 103.5 final
///   sentence: a player may not take a mulligan that would result in a
///   zero-card hand), the player is force-removed from `pending` and will
///   bottom every card in their hand.
///
/// When `pending` becomes empty, advance to `MulliganBottomCards` (or, if no
/// one owes bottoms, directly to `finish_mulligans`).
pub fn handle_mulligan_decision(
    state: &mut GameState,
    player: PlayerId,
    keep: bool,
    events: &mut Vec<GameEvent>,
) -> Result<WaitingFor, String> {
    let free_first = free_first_mulligan(state);

    // Snapshot the current pending list (we own a clone because the engine
    // borrows `state.waiting_for` immutably during match dispatch).
    let WaitingFor::MulliganDecision { pending, .. } = &state.waiting_for else {
        return Err("handle_mulligan_decision called outside MulliganDecision".to_string());
    };
    let mut pending = pending.clone();

    let idx = pending
        .iter()
        .position(|e| e.player == player)
        .ok_or_else(|| format!("Player {:?} is not in the mulligan pending set", player))?;
    let current_count = pending[idx].mulligan_count;

    // Record the final mulligan_count for the bottoms phase. Track in
    // state.final_mulligan_counts indexed by PlayerId — populated as each
    // player locks in their hand.
    if keep {
        record_final_count(state, player, current_count);
        pending.remove(idx);
    } else {
        let new_count = current_count + 1;
        shuffle_hand_into_library(state, player, events);
        draw_n(state, player, STARTING_HAND_SIZE, events);

        if new_count >= MAX_MULLIGANS {
            // CR 103.5: A player may take mulligans until their opening hand
            // would be zero cards. Force-remove from pending; the bottoms
            // phase will bottom every card in their hand.
            record_final_count(state, player, new_count);
            pending.remove(idx);
        } else {
            pending[idx].mulligan_count = new_count;
        }
    }

    Ok(advance_after_decision(state, pending, free_first, events))
}

/// CR 103.5: Stash the locked-in mulligan count for `player` so the bottoms
/// phase knows how many cards they owe.
fn record_final_count(state: &mut GameState, player: PlayerId, count: u8) {
    state.final_mulligan_counts.insert(player, count);
}

/// CR 103.5: After updating `pending`, either re-emit `MulliganDecision` or
/// transition to the bottom-cards phase (or finish entirely).
fn advance_after_decision(
    state: &mut GameState,
    pending: Vec<MulliganDecisionEntry>,
    free_first: bool,
    events: &mut Vec<GameEvent>,
) -> WaitingFor {
    if !pending.is_empty() {
        return WaitingFor::MulliganDecision {
            pending,
            free_first_mulligan: free_first,
        };
    }

    // All players have locked in their hands. Build the bottoms-phase pending
    // list from each player's final mulligan count.
    enter_bottom_phase(state, events)
}

/// CR 103.5: Enter the bottoms phase. Each player who took at least one
/// counted mulligan (after free-first discount) must put N cards on the
/// bottom of their library. Players choose simultaneously.
fn enter_bottom_phase(state: &mut GameState, events: &mut Vec<GameEvent>) -> WaitingFor {
    let free_first = free_first_mulligan(state);
    let pending: Vec<MulliganBottomEntry> = state
        .seat_order
        .iter()
        .filter_map(|&player_id| {
            let count = state
                .final_mulligan_counts
                .get(&player_id)
                .copied()
                .unwrap_or(0);
            let bottom = bottom_count_for(count, free_first);
            if bottom > 0 {
                Some(MulliganBottomEntry {
                    player: player_id,
                    count: bottom,
                })
            } else {
                None
            }
        })
        .collect();

    if pending.is_empty() {
        state.final_mulligan_counts.clear();
        finish_mulligans(state, events)
    } else {
        WaitingFor::MulliganBottomCards { pending }
    }
}

/// CR 103.5: Resolve one player's `SelectCards { cards }` during the bottoms
/// phase. Validates the count and contents, moves cards to the bottom of the
/// library, removes the player from `pending`. When `pending` is empty,
/// advance to `finish_mulligans`.
pub fn handle_mulligan_bottom(
    state: &mut GameState,
    player: PlayerId,
    cards: Vec<ObjectId>,
    events: &mut Vec<GameEvent>,
) -> Result<WaitingFor, String> {
    let WaitingFor::MulliganBottomCards { pending } = &state.waiting_for else {
        return Err("handle_mulligan_bottom called outside MulliganBottomCards".to_string());
    };
    let mut pending = pending.clone();

    let idx = pending
        .iter()
        .position(|e| e.player == player)
        .ok_or_else(|| format!("Player {:?} is not in the bottoms pending set", player))?;
    let expected_count = pending[idx].count;

    if cards.len() != expected_count as usize {
        return Err(format!(
            "Expected {} cards to bottom, got {}",
            expected_count,
            cards.len()
        ));
    }

    let player_data = state
        .players
        .iter()
        .find(|p| p.id == player)
        .expect("player exists");
    for &card_id in &cards {
        if !player_data.hand.contains(&card_id) {
            return Err(format!("Card {:?} is not in player's hand", card_id));
        }
    }

    for card_id in cards {
        zones::move_to_library_position(state, card_id, false, events);
    }

    pending.remove(idx);

    if pending.is_empty() {
        state.final_mulligan_counts.clear();
        Ok(finish_mulligans(state, events))
    } else {
        Ok(WaitingFor::MulliganBottomCards { pending })
    }
}

/// Queue all BeginGame abilities for cards in each player's opening hand.
fn queue_begin_game_abilities(state: &mut GameState) {
    let mut begin_game: Vec<PendingBeginGameAbility> = state
        .seat_order
        .clone()
        .into_iter()
        .flat_map(|player_id| {
            let player = state
                .players
                .iter()
                .find(|p| p.id == player_id)
                .expect("player exists");
            player
                .hand
                .iter()
                .filter_map(|&obj_id| {
                    let obj = state.objects.get(&obj_id)?;
                    let ability = obj
                        .abilities
                        .iter()
                        .find(|a| a.kind == AbilityKind::BeginGame)?;
                    Some(PendingBeginGameAbility {
                        ability: build_resolved_from_def(ability, obj_id, player_id),
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    begin_game.reverse();
    state.pending_begin_game_abilities = begin_game;
}

/// CR 103.6: Drain beginning-of-game abilities after mulligans, prompting for
/// optional abilities before the first turn receives priority.
pub fn resume_begin_game_abilities(
    state: &mut GameState,
    events: &mut Vec<GameEvent>,
) -> WaitingFor {
    while let Some(pending) = state.pending_begin_game_abilities.pop() {
        let _ = resolve_ability_chain(state, &pending.ability, events, 0);
        if !matches!(state.waiting_for, WaitingFor::Priority { .. }) {
            return state.waiting_for.clone();
        }
    }

    state.resolving_begin_game_abilities = false;
    turns::auto_advance(state, events)
}

/// CR 103.5 + CR 800.4a: Re-entry point for elimination cleanup — drives the
/// flow to the bottoms phase as if the decision phase had ended naturally.
pub(crate) fn enter_bottom_phase_public(
    state: &mut GameState,
    events: &mut Vec<GameEvent>,
) -> WaitingFor {
    enter_bottom_phase(state, events)
}

/// CR 103.5 + CR 800.4a: Re-entry point for elimination cleanup — drives the
/// flow to game start as if all bottoms had been submitted.
pub(crate) fn finish_mulligans_public(
    state: &mut GameState,
    events: &mut Vec<GameEvent>,
) -> WaitingFor {
    finish_mulligans(state, events)
}

/// All players have kept. Start the game properly.
fn finish_mulligans(state: &mut GameState, events: &mut Vec<GameEvent>) -> WaitingFor {
    queue_begin_game_abilities(state);
    state.resolving_begin_game_abilities = true;
    resume_begin_game_abilities(state, events)
}

fn shuffle_hand_into_library(state: &mut GameState, player: PlayerId, events: &mut Vec<GameEvent>) {
    let hand_ids: Vec<ObjectId> = state
        .players
        .iter()
        .find(|p| p.id == player)
        .expect("player exists")
        .hand
        .iter()
        .copied()
        .collect();

    for card_id in hand_ids {
        zones::move_to_zone(state, card_id, Zone::Library, events);
    }

    // Shuffle library
    let GameState { players, rng, .. } = state;
    let player_data = players
        .iter_mut()
        .find(|p| p.id == player)
        .expect("player exists");
    crate::util::im_ext::shuffle_vector(&mut player_data.library, rng);
}

fn draw_n(state: &mut GameState, player_id: PlayerId, count: usize, events: &mut Vec<GameEvent>) {
    for _ in 0..count {
        let player = state
            .players
            .iter()
            .find(|p| p.id == player_id)
            .expect("player exists");

        if player.library.is_empty() {
            break;
        }

        let top_card = player.library[0];
        zones::move_to_zone(state, top_card, Zone::Hand, events);
    }

    events.push(GameEvent::CardsDrawn {
        player_id,
        count: count as u32,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::zones::create_object;
    use crate::types::ability::{AbilityDefinition, Effect, TargetFilter};
    use crate::types::actions::GameAction;
    use crate::types::identifiers::CardId;

    /// Test helper: decide for `player`, advancing `state.waiting_for` in place.
    /// Mirrors the engine dispatch contract: callers must update `state.waiting_for`
    /// from the returned WaitingFor before the next call.
    fn decide(
        state: &mut GameState,
        player: PlayerId,
        keep: bool,
        events: &mut Vec<GameEvent>,
    ) -> WaitingFor {
        let wf = handle_mulligan_decision(state, player, keep, events)
            .expect("handle_mulligan_decision");
        state.waiting_for = wf.clone();
        wf
    }

    fn bottom(
        state: &mut GameState,
        player: PlayerId,
        cards: Vec<ObjectId>,
        events: &mut Vec<GameEvent>,
    ) -> Result<WaitingFor, String> {
        let wf = handle_mulligan_bottom(state, player, cards, events)?;
        state.waiting_for = wf.clone();
        Ok(wf)
    }

    fn setup_with_libraries(cards_per_player: usize) -> GameState {
        setup_n_player_with_libraries(2, cards_per_player)
    }

    fn setup_n_player_with_libraries(num_players: u8, cards_per_player: usize) -> GameState {
        let mut state = if num_players == 2 {
            GameState::new_two_player(42)
        } else {
            GameState::new(
                crate::types::format::FormatConfig::standard(),
                num_players,
                42,
            )
        };
        state.turn_number = 1;
        state.phase = crate::types::phase::Phase::Untap;

        for player_idx in 0..num_players {
            for i in 0..cards_per_player {
                create_object(
                    &mut state,
                    CardId((player_idx as u64) * 100 + i as u64),
                    PlayerId(player_idx),
                    format!("Card {} P{}", i, player_idx),
                    Zone::Library,
                );
            }
        }

        state
    }

    fn pending_decision_players(wf: &WaitingFor) -> Vec<PlayerId> {
        match wf {
            WaitingFor::MulliganDecision { pending, .. } => {
                pending.iter().map(|e| e.player).collect()
            }
            _ => vec![],
        }
    }

    fn decision_count_for(wf: &WaitingFor, player: PlayerId) -> Option<u8> {
        match wf {
            WaitingFor::MulliganDecision { pending, .. } => pending
                .iter()
                .find(|e| e.player == player)
                .map(|e| e.mulligan_count),
            _ => None,
        }
    }

    fn pending_bottom_for(wf: &WaitingFor, player: PlayerId) -> Option<u8> {
        match wf {
            WaitingFor::MulliganBottomCards { pending } => {
                pending.iter().find(|e| e.player == player).map(|e| e.count)
            }
            _ => None,
        }
    }

    #[test]
    fn start_mulligan_draws_seven_for_each_player() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();

        let waiting = start_mulligan(&mut state, &mut events);

        assert_eq!(state.players[0].hand.len(), 7);
        assert_eq!(state.players[1].hand.len(), 7);
        assert_eq!(state.players[0].library.len(), 13);
        assert_eq!(state.players[1].library.len(), 13);
        assert_eq!(
            pending_decision_players(&waiting),
            vec![PlayerId(0), PlayerId(1)],
            "both players should be pending at game start"
        );
    }

    #[test]
    fn start_mulligan_emits_event() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();

        start_mulligan(&mut state, &mut events);

        assert!(events
            .iter()
            .any(|e| matches!(e, GameEvent::MulliganStarted)));
    }

    #[test]
    fn keep_removes_player_from_pending() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        let waiting = decide(&mut state, PlayerId(0), true, &mut events);
        assert_eq!(
            pending_decision_players(&waiting),
            vec![PlayerId(1)],
            "P0 should be removed; P1 still pending"
        );
    }

    #[test]
    fn mulligan_keeps_player_in_pending_and_increments_count() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        let waiting = decide(&mut state, PlayerId(0), false, &mut events);
        assert_eq!(
            decision_count_for(&waiting, PlayerId(0)),
            Some(1),
            "P0 mulligan_count should increment to 1"
        );
        assert!(
            pending_decision_players(&waiting).contains(&PlayerId(0)),
            "P0 should remain pending after mulligan"
        );
    }

    #[test]
    fn keep_after_mulligan_defers_bottoms_until_all_keep() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        // P0 mulligans once then keeps; P1 still pending → still decision phase.
        decide(&mut state, PlayerId(0), false, &mut events);
        let waiting = decide(&mut state, PlayerId(0), true, &mut events);
        assert!(
            matches!(waiting, WaitingFor::MulliganDecision { .. }),
            "should still be decision phase while P1 is pending, got {:?}",
            waiting
        );

        // P1 keeps → enters bottoms phase for P0 only.
        let waiting = decide(&mut state, PlayerId(1), true, &mut events);
        assert_eq!(
            pending_bottom_for(&waiting, PlayerId(0)),
            Some(1),
            "P0 owes 1 bottom card after 1 mulligan in 2-player Standard"
        );
        assert_eq!(
            pending_bottom_for(&waiting, PlayerId(1)),
            None,
            "P1 owes nothing"
        );
    }

    #[test]
    fn mulligan_redraws_seven() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        assert_eq!(state.players[0].hand.len(), 7);

        decide(&mut state, PlayerId(0), false, &mut events);

        assert_eq!(state.players[0].hand.len(), 7);
    }

    #[test]
    fn handle_bottom_cards_puts_on_bottom() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        // P0 mulligans then keeps; P1 keeps → enter bottoms phase.
        decide(&mut state, PlayerId(0), false, &mut events);
        decide(&mut state, PlayerId(0), true, &mut events);
        decide(&mut state, PlayerId(1), true, &mut events);

        let card_to_bottom = state.players[0].hand[0];
        let result = bottom(&mut state, PlayerId(0), vec![card_to_bottom], &mut events);
        assert!(result.is_ok());
        assert_eq!(state.players[0].hand.len(), 6);
        assert_eq!(*state.players[0].library.back().unwrap(), card_to_bottom);
    }

    #[test]
    fn handle_bottom_cards_wrong_count_errors() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        // Drive into bottoms phase: P0 mulligans+keeps, P1 keeps.
        decide(&mut state, PlayerId(0), false, &mut events);
        decide(&mut state, PlayerId(0), true, &mut events);
        decide(&mut state, PlayerId(1), true, &mut events);

        let result = handle_mulligan_bottom(&mut state, PlayerId(0), vec![], &mut events);
        assert!(result.is_err());
    }

    #[test]
    fn both_players_keep_starts_game() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        let waiting = decide(&mut state, PlayerId(0), true, &mut events);
        assert!(matches!(waiting, WaitingFor::MulliganDecision { .. }));

        let waiting = decide(&mut state, PlayerId(1), true, &mut events);
        assert!(matches!(waiting, WaitingFor::Priority { .. }));
    }

    /// CR 103.5: 4-player pod, every player submits in non-turn order; all keep.
    /// All four mulligan decisions complete simultaneously and the game starts.
    #[test]
    fn four_player_concurrent_keep_in_any_order() {
        let mut state = setup_n_player_with_libraries(4, 20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        // Submit in reverse seat order.
        let _ = decide(&mut state, PlayerId(3), true, &mut events);
        let _ = decide(&mut state, PlayerId(0), true, &mut events);
        let _ = decide(&mut state, PlayerId(2), true, &mut events);
        let waiting = decide(&mut state, PlayerId(1), true, &mut events);

        assert!(
            matches!(waiting, WaitingFor::Priority { .. }),
            "all four players kept → game should start, got {:?}",
            waiting
        );
    }

    /// CR 103.5: 4-player pod, partial — two keep, two mulligan.
    /// Pending shrinks to the mulliganing players only.
    #[test]
    fn four_player_partial_keep_pending_shrinks() {
        let mut state = setup_n_player_with_libraries(4, 20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        decide(&mut state, PlayerId(0), true, &mut events);
        decide(&mut state, PlayerId(1), true, &mut events);
        decide(&mut state, PlayerId(2), false, &mut events);
        let waiting = decide(&mut state, PlayerId(3), false, &mut events);

        let pending = pending_decision_players(&waiting);
        assert_eq!(
            pending,
            vec![PlayerId(2), PlayerId(3)],
            "only mulliganing players should remain pending"
        );
        assert_eq!(decision_count_for(&waiting, PlayerId(2)), Some(1));
        assert_eq!(decision_count_for(&waiting, PlayerId(3)), Some(1));
    }

    /// CR 103.5: 4-player pod bottoms phase — three players owe bottoms,
    /// they submit in non-seat order, all resolve concurrently.
    #[test]
    fn four_player_concurrent_bottom_in_any_order() {
        // Need a 4-player game without free-first-mulligan so all three mulligans
        // produce bottoms. Multiplayer (≥3 seats) always grants free first per
        // CR 103.5c, so a single mulligan is free. Take TWO mulligans per player
        // to ensure each owes one bottom card.
        let mut state = setup_n_player_with_libraries(4, 30);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        // P0, P2, P3 each mulligan twice then keep; P1 keeps immediately.
        for &pid in &[PlayerId(0), PlayerId(2), PlayerId(3)] {
            decide(&mut state, pid, false, &mut events);
            decide(&mut state, pid, false, &mut events);
            decide(&mut state, pid, true, &mut events);
        }
        let waiting = decide(&mut state, PlayerId(1), true, &mut events);

        // Bottoms phase: P0/P2/P3 each owe 1 (2 mulligans - 1 free).
        assert_eq!(pending_bottom_for(&waiting, PlayerId(0)), Some(1));
        assert_eq!(pending_bottom_for(&waiting, PlayerId(2)), Some(1));
        assert_eq!(pending_bottom_for(&waiting, PlayerId(3)), Some(1));
        assert_eq!(pending_bottom_for(&waiting, PlayerId(1)), None);

        // Submit bottom cards in non-seat order.
        let card3 = state.players[3].hand[0];
        let card0 = state.players[0].hand[0];
        let card2 = state.players[2].hand[0];
        bottom(&mut state, PlayerId(3), vec![card3], &mut events).unwrap();
        bottom(&mut state, PlayerId(0), vec![card0], &mut events).unwrap();
        let waiting = bottom(&mut state, PlayerId(2), vec![card2], &mut events).unwrap();

        assert!(
            matches!(waiting, WaitingFor::Priority { .. }),
            "all bottoms submitted → game should start, got {:?}",
            waiting
        );
    }

    #[test]
    fn optional_begin_game_ability_prompts_before_resolving() {
        let mut state = setup_with_libraries(20);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        let leyline_id = state.players[0].hand[0];
        let mut begin_game = AbilityDefinition::new(
            AbilityKind::BeginGame,
            Effect::ChangeZone {
                destination: Zone::Battlefield,
                target: TargetFilter::SelfRef,
                origin: Some(Zone::Hand),
                owner_library: false,
                enter_transformed: false,
                under_your_control: false,
                enter_tapped: false,
                enters_attacking: false,
                up_to: false,
                enter_with_counters: vec![],
            },
        )
        .description("If this card is in your opening hand, you may begin the game with it on the battlefield.".to_string());
        begin_game.optional = true;
        let abilities = &mut state
            .objects
            .get_mut(&leyline_id)
            .expect("opening hand card exists")
            .abilities;
        std::sync::Arc::make_mut(abilities).push(begin_game);

        decide(&mut state, PlayerId(0), true, &mut events);
        decide(&mut state, PlayerId(1), true, &mut events);

        assert!(matches!(
            state.waiting_for,
            WaitingFor::OptionalEffectChoice {
                player: PlayerId(0),
                source_id,
                ..
            } if source_id == leyline_id
        ));
        assert_eq!(state.objects[&leyline_id].zone, Zone::Hand);

        let result = crate::game::engine::apply(
            &mut state,
            PlayerId(0),
            GameAction::DecideOptionalEffect { accept: true },
        )
        .expect("accepting begin-game effect should resolve");

        assert_eq!(state.objects[&leyline_id].zone, Zone::Battlefield);
        assert!(matches!(result.waiting_for, WaitingFor::Priority { .. }));
        assert!(!state.resolving_begin_game_abilities);
    }

    #[test]
    fn multiplayer_first_mulligan_is_free() {
        let mut state = setup_n_player_with_libraries(3, 30);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        // CR 103.5c: First mulligan in multiplayer doesn't count.
        let waiting = decide(&mut state, PlayerId(0), false, &mut events);
        assert_eq!(
            decision_count_for(&waiting, PlayerId(0)),
            Some(1),
            "Mulligan count should increment to 1"
        );

        // Keep after first mulligan — drive into bottoms phase by keeping others too.
        decide(&mut state, PlayerId(0), true, &mut events);
        decide(&mut state, PlayerId(1), true, &mut events);
        let waiting = decide(&mut state, PlayerId(2), true, &mut events);

        assert_eq!(
            pending_bottom_for(&waiting, PlayerId(0)),
            None,
            "P0 had 1 free mulligan → owes 0 bottom cards"
        );
        assert!(
            matches!(waiting, WaitingFor::Priority { .. }),
            "with no bottoms owed, game should start immediately"
        );
    }

    #[test]
    fn multiplayer_two_mulligans_bottoms_one() {
        let mut state = setup_n_player_with_libraries(3, 30);
        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        decide(&mut state, PlayerId(0), false, &mut events);
        decide(&mut state, PlayerId(0), false, &mut events);
        decide(&mut state, PlayerId(0), true, &mut events);
        decide(&mut state, PlayerId(1), true, &mut events);
        let waiting = decide(&mut state, PlayerId(2), true, &mut events);
        assert_eq!(
            pending_bottom_for(&waiting, PlayerId(0)),
            Some(1),
            "After 2 mulligans in multiplayer, P0 should bottom 1 card"
        );
    }

    #[test]
    fn ai_starting_player_can_submit_mulligan_decision() {
        use crate::game::engine::{apply, start_game_with_starting_player};
        use crate::types::actions::GameAction;
        use crate::types::format::FormatConfig;

        let mut state = GameState::new(FormatConfig::commander(), 2, 42);
        for player_idx in 0..2u8 {
            for i in 0..10 {
                create_object(
                    &mut state,
                    CardId((player_idx as u64) * 100 + i as u64),
                    PlayerId(player_idx),
                    format!("Card {} P{}", i, player_idx),
                    Zone::Library,
                );
            }
        }
        let c0 = create_object(
            &mut state,
            CardId(200),
            PlayerId(0),
            "P0 Cmd".to_string(),
            Zone::Command,
        );
        let c1 = create_object(
            &mut state,
            CardId(201),
            PlayerId(1),
            "P1 Cmd".to_string(),
            Zone::Command,
        );
        state.objects.get_mut(&c0).unwrap().is_commander = true;
        state.objects.get_mut(&c1).unwrap().is_commander = true;

        let result = start_game_with_starting_player(&mut state, PlayerId(1));

        // CR 103.5: Both players are pending simultaneously at start.
        assert!(
            matches!(result.waiting_for, WaitingFor::MulliganDecision { .. }),
            "expected MulliganDecision, got {:?}",
            result.waiting_for
        );
        let pending = pending_decision_players(&result.waiting_for);
        assert!(
            pending.contains(&PlayerId(0)) && pending.contains(&PlayerId(1)),
            "both players should be pending, got {:?}",
            pending
        );

        // P1 (AI) is authorized as a member of the pending set.
        assert!(crate::game::turn_control::is_authorized_submitter(
            &state,
            PlayerId(1)
        ));

        let r = apply(
            &mut state,
            PlayerId(1),
            GameAction::MulliganDecision { keep: true },
        );
        assert!(
            r.is_ok(),
            "AI P1 should be authorized to submit MulliganDecision, got {:?}",
            r
        );
    }

    /// Commander Rules Committee free-mulligan rule supplements CR 103.5c
    /// (which covers only multiplayer and Brawl). A 2-player Commander
    /// duel grants a free first mulligan.
    #[test]
    fn commander_first_mulligan_is_free_in_duel() {
        use crate::types::format::FormatConfig;

        let mut state = GameState::new(FormatConfig::commander(), 2, 42);
        state.turn_number = 1;
        state.phase = crate::types::phase::Phase::Untap;
        for player_idx in 0..2u8 {
            for i in 0..20 {
                create_object(
                    &mut state,
                    CardId((player_idx as u64) * 100 + i as u64),
                    PlayerId(player_idx),
                    format!("Card {} P{}", i, player_idx),
                    Zone::Library,
                );
            }
        }

        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        decide(&mut state, PlayerId(0), false, &mut events);
        decide(&mut state, PlayerId(0), true, &mut events);
        let waiting = decide(&mut state, PlayerId(1), true, &mut events);
        assert_eq!(
            pending_bottom_for(&waiting, PlayerId(0)),
            None,
            "Commander duel: first mulligan should be free — no MulliganBottomCards"
        );
    }

    /// CR 103.5c: A Brawl duel grants a free first mulligan.
    #[test]
    fn brawl_first_mulligan_is_free_in_duel() {
        use crate::types::format::FormatConfig;

        let mut state = GameState::new(FormatConfig::brawl(), 2, 42);
        state.turn_number = 1;
        state.phase = crate::types::phase::Phase::Untap;
        for player_idx in 0..2u8 {
            for i in 0..20 {
                create_object(
                    &mut state,
                    CardId((player_idx as u64) * 100 + i as u64),
                    PlayerId(player_idx),
                    format!("Card {} P{}", i, player_idx),
                    Zone::Library,
                );
            }
        }

        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        decide(&mut state, PlayerId(0), false, &mut events);
        decide(&mut state, PlayerId(0), true, &mut events);
        let waiting = decide(&mut state, PlayerId(1), true, &mut events);
        assert_eq!(pending_bottom_for(&waiting, PlayerId(0)), None);
    }

    /// CR 103.5c only applies to multiplayer (3+ players) and Brawl. A
    /// Standard 1v1 duel must require bottoming 1 card after 1 mulligan.
    #[test]
    fn standard_duel_has_no_free_mulligan() {
        use crate::types::format::FormatConfig;

        let mut state = GameState::new(FormatConfig::standard(), 2, 42);
        state.turn_number = 1;
        state.phase = crate::types::phase::Phase::Untap;
        for player_idx in 0..2u8 {
            for i in 0..20 {
                create_object(
                    &mut state,
                    CardId((player_idx as u64) * 100 + i as u64),
                    PlayerId(player_idx),
                    format!("Card {} P{}", i, player_idx),
                    Zone::Library,
                );
            }
        }

        let mut events = Vec::new();
        state.waiting_for = start_mulligan(&mut state, &mut events);

        decide(&mut state, PlayerId(0), false, &mut events);
        decide(&mut state, PlayerId(0), true, &mut events);
        let waiting = decide(&mut state, PlayerId(1), true, &mut events);
        assert_eq!(
            pending_bottom_for(&waiting, PlayerId(0)),
            Some(1),
            "Standard duel: after 1 mulligan, should bottom 1 card"
        );
    }
}
