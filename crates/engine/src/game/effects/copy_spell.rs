use crate::types::ability::{
    Effect, EffectError, EffectKind, ResolvedAbility, TargetFilter, TargetRef,
};
use crate::types::events::GameEvent;
use crate::types::game_state::{CopyTargetSlot, GameState, StackEntry, StackEntryKind, WaitingFor};
use crate::types::identifiers::ObjectId;
use crate::types::player::PlayerId;
use crate::types::statics::StaticMode;
use crate::types::zones::Zone;

/// CR 707.10: Copy a spell or ability by putting a copy onto the stack with the
/// same characteristics and choices.
/// CR 707.10c: Some copy effects let the controller choose new targets before
/// the copy is put onto the stack.
pub fn resolve(
    state: &mut GameState,
    ability: &ResolvedAbility,
    events: &mut Vec<GameEvent>,
) -> Result<(), EffectError> {
    let top_entry = copy_source_entry(state, ability).ok_or_else(|| {
        EffectError::MissingParam("No spell or ability on stack to copy".to_string())
    })?;

    if stack_entry_cant_be_copied(state, &top_entry) {
        events.push(GameEvent::EffectResolved {
            kind: EffectKind::from(&ability.effect),
            source_id: ability.source_id,
        });
        return Ok(());
    }

    // Allocate a new stack ID for the copy.
    let copy_id = ObjectId(state.next_object_id);
    state.next_object_id += 1;

    // CR 707.10: A spell copy is itself a spell on the stack. Ability stack
    // entries are objects too, but this engine does not store GameObjects for
    // activated/triggered ability entries; clone a GameObject only when the
    // copied stack entry already has one.
    if let Some(source_obj) = state.objects.get(&top_entry.id) {
        let mut copy_obj = source_obj.clone();
        copy_obj.id = copy_id;
        copy_obj.controller = ability.controller;
        copy_obj.zone = Zone::Stack;
        copy_obj.is_token = true;
        state.objects.insert(copy_id, copy_obj);
    }

    let mut copy_kind = top_entry.kind.clone();
    set_copied_kind_controller(&mut copy_kind, ability.controller);

    // Create the copy with a new ID but same kind.
    let copy_entry = crate::types::game_state::StackEntry {
        id: copy_id,
        source_id: top_entry.source_id,
        controller: ability.controller,
        kind: copy_kind,
    };

    state.stack.push_back(copy_entry);
    events.push(GameEvent::StackPushed { object_id: copy_id });

    // CR 707.10c: If the copy has targets, allow the controller to choose new ones.
    let copy_targets = top_entry
        .ability()
        .map(|a| a.targets.clone())
        .unwrap_or_default();

    if !copy_targets.is_empty() {
        // Build target slots — each slot shows current target. Legal alternatives
        // are not computed here (the engine handler validates at selection time).
        let target_slots: Vec<CopyTargetSlot> = copy_targets
            .iter()
            .map(|t| CopyTargetSlot {
                current: t.clone(),
                legal_alternatives: Vec::new(),
            })
            .collect();

        state.waiting_for = WaitingFor::CopyRetarget {
            player: ability.controller,
            copy_id,
            target_slots,
        };
        // EffectResolved deferred until after retarget choice completes.
        return Ok(());
    }

    events.push(GameEvent::EffectResolved {
        kind: EffectKind::from(&ability.effect),
        source_id: ability.source_id,
    });

    Ok(())
}

fn copy_source_entry(state: &GameState, ability: &ResolvedAbility) -> Option<StackEntry> {
    let target_id = ability.targets.iter().find_map(|target| match target {
        TargetRef::Object(id) => Some(*id),
        TargetRef::Player(_) => None,
    });
    if let Some(target_id) = target_id {
        return state
            .stack
            .iter()
            .find(|entry| entry.id == target_id)
            .cloned();
    }
    if matches!(
        &ability.effect,
        Effect::CopySpell {
            target: TargetFilter::SelfRef
        }
    ) {
        return state
            .stack
            .iter()
            .find(|entry| entry.id == ability.source_id)
            .cloned();
    }
    state.stack.last().cloned()
}

fn stack_entry_cant_be_copied(state: &GameState, entry: &StackEntry) -> bool {
    if entry
        .ability()
        .is_some_and(|ability| ability.cant_be_copied)
    {
        return true;
    }

    state
        .objects
        .get(&entry.id)
        .map(|obj| {
            super::super::functioning_abilities::active_static_definitions(state, obj)
                .any(|sd| sd.mode == StaticMode::CantBeCopied)
        })
        .unwrap_or(false)
}

fn set_copied_kind_controller(kind: &mut StackEntryKind, controller: PlayerId) {
    match kind {
        StackEntryKind::Spell {
            ability: Some(ability),
            ..
        }
        | StackEntryKind::ActivatedAbility { ability, .. } => {
            set_resolved_controller_recursive(ability, controller);
        }
        StackEntryKind::TriggeredAbility { ability, .. } => {
            set_resolved_controller_recursive(ability, controller);
        }
        StackEntryKind::Spell { ability: None, .. } | StackEntryKind::KeywordAction { .. } => {}
    }
}

fn set_resolved_controller_recursive(ability: &mut ResolvedAbility, controller: PlayerId) {
    ability.controller = controller;
    if let Some(sub_ability) = ability.sub_ability.as_mut() {
        set_resolved_controller_recursive(sub_ability, controller);
    }
    if let Some(else_ability) = ability.else_ability.as_mut() {
        set_resolved_controller_recursive(else_ability, controller);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::game_object::GameObject;
    use crate::types::ability::{Effect, QuantityExpr, TargetFilter, TargetRef};
    use crate::types::game_state::{CastingVariant, StackEntry, StackEntryKind};
    use crate::types::identifiers::{CardId, ObjectId};
    use crate::types::player::PlayerId;

    /// Helper: push a spell onto the stack with a matching GameObject.
    fn push_spell(
        state: &mut GameState,
        obj_id: ObjectId,
        card_id: CardId,
        owner: PlayerId,
        name: &str,
        ability: ResolvedAbility,
        variant: CastingVariant,
    ) {
        let obj = GameObject::new(obj_id, card_id, owner, name.to_string(), Zone::Stack);
        state.objects.insert(obj_id, obj);
        state.stack.push_back(StackEntry {
            id: obj_id,
            source_id: obj_id,
            controller: owner,
            kind: StackEntryKind::Spell {
                card_id,
                ability: Some(ability),
                casting_variant: variant,
                actual_mana_spent: 0,
            },
        });
    }

    #[test]
    fn test_copy_spell_duplicates_stack_entry() {
        let mut state = GameState::new_two_player(42);

        let original_ability = ResolvedAbility::new(
            Effect::DealDamage {
                amount: QuantityExpr::Fixed { value: 3 },
                target: TargetFilter::Any,
                damage_source: None,
            },
            vec![],
            ObjectId(10),
            PlayerId(0),
        );

        push_spell(
            &mut state,
            ObjectId(10),
            CardId(1),
            PlayerId(0),
            "Lightning Bolt",
            original_ability.clone(),
            CastingVariant::Normal,
        );

        let copy_ability = ResolvedAbility::new(
            Effect::CopySpell {
                target: TargetFilter::Any,
            },
            vec![],
            ObjectId(20),
            PlayerId(0),
        );
        let mut events = Vec::new();

        resolve(&mut state, &copy_ability, &mut events).unwrap();

        // Stack should have 2 entries now
        assert_eq!(state.stack.len(), 2);
        // Copy should have a different ID
        assert_ne!(state.stack[0].id, state.stack[1].id);

        // Engine bookkeeping: spell copies get a stack GameObject.
        let copy_id = state.stack[1].id;
        let copy_obj = state.objects.get(&copy_id).expect("copy object exists");
        assert!(copy_obj.is_token);
        assert_eq!(copy_obj.zone, Zone::Stack);

        // Same spell kind
        match (&state.stack[0].kind, &state.stack[1].kind) {
            (
                StackEntryKind::Spell {
                    card_id: c1,
                    ability: Some(a1),
                    ..
                },
                StackEntryKind::Spell {
                    card_id: c2,
                    ability: Some(a2),
                    ..
                },
            ) => {
                assert_eq!(c1, c2);
                assert_eq!(
                    crate::types::ability::effect_variant_name(&a1.effect),
                    crate::types::ability::effect_variant_name(&a2.effect)
                );
            }
            _ => panic!("Expected both entries to be Spells with abilities"),
        }
    }

    #[test]
    fn test_copy_spell_empty_stack_returns_error() {
        let mut state = GameState::new_two_player(42);
        assert!(state.stack.is_empty());

        let ability = ResolvedAbility::new(
            Effect::CopySpell {
                target: TargetFilter::Any,
            },
            vec![],
            ObjectId(20),
            PlayerId(0),
        );
        let mut events = Vec::new();

        let result = resolve(&mut state, &ability, &mut events);
        assert!(result.is_err());
    }

    #[test]
    fn test_copy_spell_with_targets_enters_retarget() {
        let mut state = GameState::new_two_player(42);

        let original_ability = ResolvedAbility::new(
            Effect::DealDamage {
                amount: QuantityExpr::Fixed { value: 3 },
                target: TargetFilter::Any,
                damage_source: None,
            },
            vec![TargetRef::Object(ObjectId(50))],
            ObjectId(10),
            PlayerId(0),
        );

        push_spell(
            &mut state,
            ObjectId(10),
            CardId(1),
            PlayerId(0),
            "Lightning Bolt",
            original_ability,
            CastingVariant::Normal,
        );

        let copy_ability = ResolvedAbility::new(
            Effect::CopySpell {
                target: TargetFilter::Any,
            },
            vec![],
            ObjectId(20),
            PlayerId(0),
        );
        let mut events = Vec::new();

        resolve(&mut state, &copy_ability, &mut events).unwrap();

        // CR 707.10c: Copy has targets → should enter CopyRetarget.
        assert!(matches!(state.waiting_for, WaitingFor::CopyRetarget { .. }));
        // Copy should still be on the stack
        assert_eq!(state.stack.len(), 2);
    }

    #[test]
    fn test_copy_spell_without_targets_skips_retarget() {
        let mut state = GameState::new_two_player(42);

        let original_ability = ResolvedAbility::new(
            Effect::Draw {
                count: QuantityExpr::Fixed { value: 2 },
                target: TargetFilter::Controller,
            },
            vec![],
            ObjectId(10),
            PlayerId(0),
        );

        push_spell(
            &mut state,
            ObjectId(10),
            CardId(1),
            PlayerId(0),
            "Divination",
            original_ability,
            CastingVariant::Normal,
        );

        let copy_ability = ResolvedAbility::new(
            Effect::CopySpell {
                target: TargetFilter::Any,
            },
            vec![],
            ObjectId(20),
            PlayerId(0),
        );
        let mut events = Vec::new();

        resolve(&mut state, &copy_ability, &mut events).unwrap();

        // No targets → should NOT enter CopyRetarget, should emit EffectResolved
        assert!(!matches!(
            state.waiting_for,
            WaitingFor::CopyRetarget { .. }
        ));
        assert!(events
            .iter()
            .any(|e| matches!(e, GameEvent::EffectResolved { .. })));
    }

    #[test]
    fn uncopyable_activated_ability_on_stack_is_not_copied_through_stack_resolution() {
        let mut state = GameState::new_two_player(42);
        let gogo_id = ObjectId(20);
        let other_id = ObjectId(21);

        let mut gogo_ability = ResolvedAbility::new(
            Effect::CopySpell {
                target: TargetFilter::StackAbility {
                    controller: Some(crate::types::ability::ControllerRef::You),
                },
            },
            vec![],
            gogo_id,
            PlayerId(0),
        );
        gogo_ability.cant_be_copied = true;

        state.stack.push_back(StackEntry {
            id: ObjectId(40),
            source_id: gogo_id,
            controller: PlayerId(0),
            kind: StackEntryKind::ActivatedAbility {
                source_id: gogo_id,
                ability: gogo_ability,
            },
        });

        let copy_gogo = ResolvedAbility::new(
            Effect::CopySpell {
                target: TargetFilter::StackAbility {
                    controller: Some(crate::types::ability::ControllerRef::You),
                },
            },
            vec![TargetRef::Object(ObjectId(40))],
            other_id,
            PlayerId(0),
        );
        state.stack.push_back(StackEntry {
            id: ObjectId(41),
            source_id: other_id,
            controller: PlayerId(0),
            kind: StackEntryKind::ActivatedAbility {
                source_id: other_id,
                ability: copy_gogo,
            },
        });

        let mut events = Vec::new();
        crate::game::stack::resolve_top(&mut state, &mut events);

        assert_eq!(state.stack.len(), 1);
        assert_eq!(state.stack[0].id, ObjectId(40));
        assert!(!events
            .iter()
            .any(|event| matches!(event, GameEvent::StackPushed { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, GameEvent::EffectResolved { .. })));
    }

    #[test]
    fn copy_targeted_triggered_ability_on_stack_through_stack_resolution() {
        let mut state = GameState::new_two_player(42);
        let hope_id = ObjectId(10);
        let gogo_id = ObjectId(20);
        state.objects.insert(
            hope_id,
            GameObject::new(
                hope_id,
                CardId(10),
                PlayerId(0),
                "Hope Estheim".to_string(),
                Zone::Battlefield,
            ),
        );
        state.objects.insert(
            gogo_id,
            GameObject::new(
                gogo_id,
                CardId(20),
                PlayerId(0),
                "Gogo, Master of Mimicry".to_string(),
                Zone::Battlefield,
            ),
        );

        let hope_trigger_entry = ObjectId(30);
        let hope_trigger = ResolvedAbility::new(
            Effect::Mill {
                count: QuantityExpr::Fixed { value: 2 },
                target: TargetFilter::Typed(
                    crate::types::ability::TypedFilter::default()
                        .controller(crate::types::ability::ControllerRef::Opponent),
                ),
                destination: Zone::Graveyard,
            },
            vec![],
            hope_id,
            PlayerId(0),
        );
        state.stack.push_back(StackEntry {
            id: hope_trigger_entry,
            source_id: hope_id,
            controller: PlayerId(0),
            kind: StackEntryKind::TriggeredAbility {
                source_id: hope_id,
                ability: Box::new(hope_trigger),
                condition: None,
                trigger_event: None,
                description: Some("At the beginning of your end step".to_string()),
                source_name: "Hope Estheim".to_string(),
            },
        });
        state.stack.push_back(StackEntry {
            id: ObjectId(31),
            source_id: ObjectId(31),
            controller: PlayerId(1),
            kind: StackEntryKind::TriggeredAbility {
                source_id: ObjectId(31),
                ability: Box::new(ResolvedAbility::new(
                    Effect::Draw {
                        count: QuantityExpr::Fixed { value: 1 },
                        target: TargetFilter::Controller,
                    },
                    vec![],
                    ObjectId(31),
                    PlayerId(1),
                )),
                condition: None,
                trigger_event: None,
                description: Some("Opponent trigger".to_string()),
                source_name: "Opponent Source".to_string(),
            },
        });

        let gogo_entry = ObjectId(40);
        let gogo_target_filter = TargetFilter::StackAbility {
            controller: Some(crate::types::ability::ControllerRef::You),
        };
        assert_eq!(
            crate::game::targeting::find_legal_targets(
                &state,
                &gogo_target_filter,
                PlayerId(0),
                gogo_id,
            ),
            vec![TargetRef::Object(hope_trigger_entry)]
        );

        let mut gogo_copy = ResolvedAbility::new(
            Effect::CopySpell {
                target: gogo_target_filter,
            },
            vec![TargetRef::Object(hope_trigger_entry)],
            gogo_id,
            PlayerId(0),
        );
        gogo_copy.repeat_for = Some(QuantityExpr::Fixed { value: 2 });
        state.stack.push_back(StackEntry {
            id: gogo_entry,
            source_id: gogo_id,
            controller: PlayerId(0),
            kind: StackEntryKind::ActivatedAbility {
                source_id: gogo_id,
                ability: gogo_copy,
            },
        });

        let mut events = Vec::new();
        crate::game::stack::resolve_top(&mut state, &mut events);

        assert_eq!(state.stack.len(), 4);
        assert_eq!(state.stack[0].id, hope_trigger_entry);
        assert_eq!(state.stack[1].id, ObjectId(31));
        assert!(state.stack.iter().skip(2).all(|entry| matches!(
            &entry.kind,
            StackEntryKind::TriggeredAbility { source_id, .. } if *source_id == hope_id
        )));
        assert!(
            events
                .iter()
                .filter(|event| matches!(event, GameEvent::StackPushed { .. }))
                .count()
                >= 2
        );
    }
}
