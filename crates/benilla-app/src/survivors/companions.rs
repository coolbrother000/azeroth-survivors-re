//! Four local AI companions: a loose formation behind the player, plus class-flavoured attacks.

use benilla_world::world_point::WorldPoint;
use bevy::prelude::*;

use super::abilities::{Hit, PendingHits};
use super::data::{self, AbilityDef, ClassDef, Kind};
use super::enemies::Enemy;
use super::hero::{self, Hero};
use super::units::{self, flat, yaw_toward, Fx, Marks, UnitSpec, COLOR_SPELL};
use super::{Phase, Rng, Run, Selection};
use crate::creature_anim::{move_flags, MovementState};

const COMPANION_DAMAGE: f32 = 0.38;
const FOLLOW_DISTANCE: f32 = 1.15;
const TELEPORT_DISTANCE: f32 = 55.0;

/// Left/right slots behind the leader, in the leader's local XZ plane.
const FORMATION: [Vec2; 4] = [
    Vec2::new(-2.4, -2.2),
    Vec2::new(2.4, -2.2),
    Vec2::new(-4.8, -4.6),
    Vec2::new(4.8, -4.6),
];

/// Prefer a frontline, support and ranged mix while keeping the four AI classes distinct.
const CLASS_ORDER: [usize; 9] = [0, 4, 2, 6, 1, 5, 3, 7, 8];

#[derive(Component)]
pub(super) struct Companion {
    slot: usize,
    ability: &'static AbilityDef,
    attack_cd: f32,
}

fn class_indices_for(leader: usize) -> [usize; 4] {
    let mut result = [0; 4];
    let mut n = 0;
    for index in CLASS_ORDER {
        if index != leader && n < result.len() {
            result[n] = index;
            n += 1;
        }
    }
    result
}

fn same_faction(a: u8, b: u8) -> bool {
    let alliance = |race| matches!(race, 1 | 3 | 4 | 7);
    alliance(a) == alliance(b)
}

/// Spawn exactly four different class companions once the selected hero has entered the run.
pub(super) fn spawn_party(
    mut commands: Commands,
    mut run: ResMut<Run>,
    selection: Res<Selection>,
    creatures: Option<Res<crate::entities::Creatures>>,
    leader: Query<&Transform, (With<Hero>, Without<Companion>)>,
    companions: Query<Entity, With<Companion>>,
    world: WorldPoint,
) {
    if !companions.is_empty() || run.dead_t.is_some() {
        return;
    }
    let (Ok(leader_tf), Some(creatures)) = (leader.single(), creatures.as_deref()) else {
        return;
    };
    let main_class = data::CLASSES
        .iter()
        .position(|class| class.id == run.class.id)
        .unwrap_or(0);
    let main_sex = selection
        .look
        .and_then(|i| selection.looks.get(i))
        .map_or(0, |look| look.2);
    let leader_pos = leader_tf.translation;
    let right = leader_tf.rotation * Vec3::X;
    let forward = leader_tf.rotation * Vec3::NEG_Z;

    for (slot, class_index) in class_indices_for(main_class).into_iter().enumerate() {
        let class: &'static ClassDef = &data::CLASSES[class_index];
        let looks = super::hud::looks_for(class, creatures);
        let chosen = looks
            .iter()
            .find(|(_, race, sex)| *race == run.race && *sex == main_sex)
            .or_else(|| looks.iter().find(|(_, race, _)| *race == run.race))
            .or_else(|| {
                looks
                    .iter()
                    .find(|(_, race, _)| same_faction(*race, run.race))
            })
            .or_else(|| looks.first());
        let Some(&(display, race, sex)) = chosen else {
            continue;
        };

        let offset = FORMATION[slot];
        let mut pos = leader_pos + right * offset.x + forward * offset.y;
        pos.y = world.terrain_height_under(pos).unwrap_or(leader_pos.y);
        let serial = run.guid();
        let bytes0 = u32::from(race)
            | u32::from(class.id) << 8
            | u32::from(sex) << 16
            | u32::from(class.power) << 24;
        let entity = units::spawn_unit(
            &mut commands,
            &UnitSpec {
                serial,
                display,
                scale: 1.0,
                level: 60,
                faction: units::FACTION_FRIENDLY,
                bytes0,
                weapons: class.weapons,
                sheath: class.sheath,
            },
            pos,
            yaw_toward(flat(leader_pos - pos)),
        );
        commands.entity(entity).insert((
            Companion {
                slot,
                ability: data::ability(class.start),
                attack_cd: 0.25 + slot as f32 * 0.18,
            },
            Marks::buffs(),
        ));
    }
}

/// Keep the four companions in open slots behind the controlled hero.
pub(super) fn follow_leader(
    time: Res<Time>,
    phase: Res<State<Phase>>,
    run: Res<Run>,
    selection: Res<Selection>,
    world: WorldPoint,
    leader: Query<&Transform, (With<Hero>, Without<Companion>)>,
    mut companions: Query<(&mut Transform, &mut MovementState, &Companion), Without<Hero>>,
) {
    if *phase.get() != Phase::Playing || run.dead_t.is_some() {
        return;
    }
    let Ok(leader) = leader.single() else { return };
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let right = leader.rotation * Vec3::X;
    let forward = leader.rotation * Vec3::NEG_Z;
    let center = hero::arena(selection.stage_def());
    let leash = selection.stage_def().leash;
    let speed = run.stats().speed * 1.15;

    for (mut transform, mut movement, companion) in &mut companions {
        let slot = FORMATION[companion.slot.min(FORMATION.len() - 1)];
        let mut target = leader.translation + right * slot.x + forward * slot.y;
        let from_center = flat(target - center);
        if from_center.length() > leash - 2.0 {
            target = center + from_center.normalize_or_zero() * (leash - 2.0);
        }
        let target_y = world
            .terrain_height_under(target)
            .unwrap_or(leader.translation.y);
        target.y = target_y;
        let delta = flat(target - transform.translation);
        let distance = delta.length();

        if distance > TELEPORT_DISTANCE {
            transform.translation = target;
            movement.speed = 0.0;
            movement.flags = 0;
            continue;
        }
        if distance <= FOLLOW_DISTANCE {
            movement.speed = 0.0;
            movement.flags = 0;
            continue;
        }

        let direction = delta / distance;
        let step = (speed * dt).min(distance);
        let mut next = transform.translation + direction * step;
        next.y = world.terrain_height_under(next).unwrap_or(target.y);
        transform.translation = next;
        let facing = Quat::from_rotation_y(yaw_toward(direction));
        transform.rotation = transform.rotation.slerp(facing, (dt * 12.0).min(1.0));
        movement.speed = speed;
        movement.flags = move_flags::FORWARD;
    }
}

/// Fire each companion's signature spell automatically at the nearest enemy in range.
#[allow(clippy::type_complexity)]
pub(super) fn auto_attack(
    time: Res<Time>,
    run: Res<Run>,
    mut rng: ResMut<Rng>,
    spells: Option<Res<crate::ui_action::Spells>>,
    mut companions: Query<(Entity, &mut Transform, &mut Companion), Without<Enemy>>,
    enemies: Query<(Entity, &Transform, &Enemy), Without<Companion>>,
    mut fx: Fx,
    mut pending: ResMut<PendingHits>,
    mut hits: MessageWriter<Hit>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 || run.dead_t.is_some() {
        return;
    }
    let now = run.elapsed;
    let level_scale = 1.0 + (run.level.saturating_sub(1) as f32 * 0.035).min(0.8);
    let speed_of = |spell: u32| {
        spells
            .as_deref()
            .and_then(|catalog| catalog.catalog.get(spell))
            .map_or(0.0, |def| def.speed)
    };

    for (entity, mut transform, mut companion) in &mut companions {
        companion.attack_cd -= dt;
        if companion.attack_cd > 0.0 {
            continue;
        }
        let ability = companion.ability;
        let reach = if ability.kind == Kind::Melee {
            ability.range + 0.5
        } else {
            ability.range
        };
        let target = enemies
            .iter()
            .filter(|(_, _, enemy)| !enemy.dead)
            .map(|(target, tf, enemy)| {
                (
                    hero::reach(transform.translation, tf.translation),
                    target,
                    tf.translation,
                    enemy.radius,
                )
            })
            .filter(|(distance, _, _, radius)| *distance <= reach + radius)
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let Some((distance, target, target_pos, _)) = target else {
            companion.attack_cd = 0.2;
            continue;
        };

        let direction = flat(target_pos - transform.translation);
        if direction.length_squared() > 0.01 {
            let facing = Quat::from_rotation_y(yaw_toward(direction));
            transform.rotation = transform.rotation.slerp(facing, (dt * 16.0).min(1.0));
        }

        let base_damage = ability.damage * COMPANION_DAMAGE * level_scale;
        let crit = rng.f32() < run.stats().crit;
        let damage = if crit { base_damage * 2.0 } else { base_damage };
        if ability.kind == Kind::Melee {
            fx.swing(entity, target, damage);
            fx.impact(target, ability.spell);
        } else {
            fx.cast(entity, ability.spell, &[target], None);
        }

        let delay = if ability.kind == Kind::Melee {
            0.0
        } else {
            let speed = speed_of(ability.spell);
            if speed > 0.0 {
                distance / speed
            } else {
                0.0
            }
        };
        let hit = Hit {
            color: if ability.kind == Kind::Melee {
                units::COLOR_MELEE
            } else {
                COLOR_SPELL
            },
            ..Hit::direct(target, damage, crit)
        };
        if delay <= 0.0 {
            hits.write(hit);
        } else {
            pending.0.push((now + delay, hit));
        }
        companion.attack_cd = ability.cooldown.max(0.35);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn each_leader_gets_four_distinct_ai_classes_other_than_their_own() {
        for leader in 0..data::CLASSES.len() {
            let team = class_indices_for(leader);
            let unique: HashSet<_> = team.into_iter().collect();
            assert_eq!(team.len(), 4);
            assert_eq!(unique.len(), 4);
            assert!(!unique.contains(&leader));
            assert!(team.iter().all(|&index| index < data::CLASSES.len()));
        }
    }
}
