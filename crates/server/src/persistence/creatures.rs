use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde::Deserialize;
use thiserror::Error;

use crate::config::CONFIG;
use crate::entities::agent::{OutfitColors, OutfitId, Pool};
use crate::entities::combat::CombatElement;
use crate::entities::creature::{
    AbilityEffect, BloodType, ConditionAttack, CreatureAbility, CreatureAbilityId, CreatureAttack,
    CreatureAttackDamage, CreatureFlag, CreatureKind, CreatureKindId, CreatureVoices, LootEntry,
};
use crate::entities::effects::{AreaShape, AreaShapeId, EffectId, MissileId};
use crate::entities::items::ItemId;
use crate::entities::support::SupportCast;
use crate::game::TickDelta;
use crate::persistence::areas::AREA_SHAPES;
use crate::persistence::conditions::{RawBounds, RawConditionSpec};
use crate::persistence::support::{
    RawSpeedFormula, RawSupportFields, SupportError, SupportKind, build_support,
};
use crate::persistence::target_mode::{TargetModeError, parse_target_mode, take_type};
use crate::persistence::yaml_files_in;

pub static CREATURE_KINDS: Lazy<Arc<HashMap<CreatureKindId, Arc<CreatureKind>>>> =
    Lazy::new(|| {
        Arc::new(
            load_creatures(&CONFIG.creatures_dir_path, &AREA_SHAPES)
                .expect("failed to load creatures"),
        )
    });

#[derive(Error, Debug)]
pub enum CreaturesLoadError {
    #[error("I/O error reading {path}: {source}")]
    ReadError {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("YAML parse error in {path}: {source}")]
    ParseError {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    #[error("{path} has no file stem to take a creature kind id from")]
    UnnamedFile { path: PathBuf },
    #[error("{path} has an ability that {source}")]
    Ability {
        path: PathBuf,
        source: TargetModeError,
    },
    #[error("{path} has an ability of type `{kind}`, which this server cannot run")]
    UnknownAbility { path: PathBuf, kind: String },
    #[error("{path} has an attack ability that {reason}")]
    BadAttack { path: PathBuf, reason: &'static str },
    #[error("{path} has a support ability that {source}")]
    Support { path: PathBuf, source: SupportError },
}

#[derive(Deserialize)]
struct RawOutfit {
    id: OutfitId,
    colors: OutfitColors,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAttackDamage {
    damage: RawBounds,
    element: CombatElement,
    #[serde(default)]
    condition: Option<RawConditionSpec>,
}

impl From<RawAttackDamage> for CreatureAttackDamage {
    fn from(raw: RawAttackDamage) -> Self {
        CreatureAttackDamage {
            element: raw.element,
            value: raw.damage.into(),
            condition: raw.condition.map(|condition| condition.into_spec(true)),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAttackAbility {
    cooldown: TickDelta,
    chance: u32,
    #[serde(default)]
    element: Option<CombatElement>,
    #[serde(default)]
    damage: Option<RawBounds>,
    #[serde(default)]
    condition: Option<RawConditionSpec>,
    target: serde_yaml::Value,
    #[serde(default)]
    effect_id: Option<EffectId>,
    #[serde(default)]
    missile_id: Option<MissileId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHealAbility {
    cooldown: TickDelta,
    chance: u32,
    life: RawBounds,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSupportAbility {
    cooldown: TickDelta,
    chance: u32,
    target: serde_yaml::Value,
    #[serde(default)]
    speed: Option<RawSpeedFormula>,
    #[serde(default)]
    duration_ticks: Option<TickDelta>,
    #[serde(default)]
    element: Option<CombatElement>,
    #[serde(default)]
    effect_id: Option<EffectId>,
    #[serde(default)]
    missile_id: Option<MissileId>,
}

fn default_amount() -> u32 {
    1
}

#[derive(Deserialize)]
struct RawLootEntry {
    item_id: ItemId,
    chance: u32,
    #[serde(default = "default_amount")]
    amount: u32,
}

#[derive(Deserialize)]
struct RawCreatureVoices {
    cooldown: TickDelta,
    chance: u32,
    #[serde(default)]
    sentences: Vec<String>,
}

#[derive(Deserialize)]
struct RawCreature {
    name: String,
    life: u32,
    outfit: RawOutfit,
    melee: RawAttackDamage,
    #[serde(default)]
    abilities: Vec<serde_yaml::Value>,
    speed: u16,
    blood_type: BloodType,
    armor: u16,
    defense: u16,
    experience: u32,
    corpse: ItemId,
    #[serde(default)]
    loot: Vec<RawLootEntry>,
    #[serde(default)]
    flee_threshold: Option<u32>,
    say: RawCreatureVoices,
    #[serde(default)]
    flags: Vec<CreatureFlag>,
}

fn parse_ability(
    index: usize,
    path: &Path,
    mut value: serde_yaml::Value,
    shapes: &HashMap<AreaShapeId, Arc<AreaShape>>,
) -> Result<CreatureAbility, CreaturesLoadError> {
    let parse_error = |source| CreaturesLoadError::ParseError {
        path: path.to_path_buf(),
        source,
    };
    let unknown = |kind: String| CreaturesLoadError::UnknownAbility {
        path: path.to_path_buf(),
        kind,
    };

    let kind =
        take_type(&mut value).ok_or_else(|| unknown("a mapping without a `type`".to_string()))?;

    match kind.as_str() {
        "attack" => {
            let attack: RawAttackAbility = serde_yaml::from_value(value).map_err(parse_error)?;
            let target = parse_target_mode(attack.target, shapes).map_err(|source| {
                CreaturesLoadError::Ability {
                    path: path.to_path_buf(),
                    source,
                }
            })?;

            let bad = |reason| CreaturesLoadError::BadAttack {
                path: path.to_path_buf(),
                reason,
            };
            let effect = match (attack.damage, attack.element, attack.condition) {
                (Some(damage), Some(element), condition) => AbilityEffect::Attack(CreatureAttack {
                    damage: CreatureAttackDamage {
                        element,
                        value: damage.into(),
                        condition: condition.map(|condition| condition.into_spec(true)),
                    },
                    target,
                    effect_id: attack.effect_id,
                    missile_id: attack.missile_id,
                }),
                (Some(_), None, _) => return Err(bad("deals damage of no element")),
                (None, _, Some(condition)) => AbilityEffect::Condition(ConditionAttack {
                    condition: condition.into_spec(true),
                    target,
                    effect_id: attack.effect_id,
                    missile_id: attack.missile_id,
                }),
                (None, _, None) => return Err(bad("deals no damage and applies no condition")),
            };

            Ok(CreatureAbility {
                id: CreatureAbilityId(index as u8),
                cooldown: attack.cooldown,
                chance: attack.chance,
                effect,
            })
        }
        "heal" => {
            let heal: RawHealAbility = serde_yaml::from_value(value).map_err(parse_error)?;

            Ok(CreatureAbility {
                id: CreatureAbilityId(index as u8),
                cooldown: heal.cooldown,
                chance: heal.chance,
                effect: AbilityEffect::Heal(heal.life.into()),
            })
        }
        other => {
            let Some(support_kind) = SupportKind::parse(other) else {
                return Err(unknown(other.to_string()));
            };
            let support: RawSupportAbility = serde_yaml::from_value(value).map_err(parse_error)?;
            let target = parse_target_mode(support.target, shapes).map_err(|source| {
                CreaturesLoadError::Ability {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            let effect = build_support(
                support_kind,
                RawSupportFields {
                    speed: support.speed,
                    duration_ticks: support.duration_ticks,
                    element: support.element,
                },
            )
            .map_err(|source| CreaturesLoadError::Support {
                path: path.to_path_buf(),
                source,
            })?;

            Ok(CreatureAbility {
                id: CreatureAbilityId(index as u8),
                cooldown: support.cooldown,
                chance: support.chance,
                effect: AbilityEffect::Support(SupportCast {
                    target,
                    effect,
                    effect_id: support.effect_id,
                    missile_id: support.missile_id,
                }),
            })
        }
    }
}

impl RawCreature {
    fn into_kind(
        self,
        path: &Path,
        shapes: &HashMap<AreaShapeId, Arc<AreaShape>>,
    ) -> Result<CreatureKind, CreaturesLoadError> {
        let abilities = self
            .abilities
            .into_iter()
            .enumerate()
            .map(|(i, ability)| parse_ability(i, path, ability, shapes))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(CreatureKind {
            name: self.name,
            life: Pool {
                current: self.life,
                maximum: self.life,
            },
            melee: self.melee.into(),
            abilities,
            outfit: (self.outfit.id, self.outfit.colors),
            speed: self.speed,
            blood_type: self.blood_type,
            armor: self.armor,
            defense: self.defense,
            experience: self.experience,
            corpse: self.corpse,
            loot_table: self
                .loot
                .into_iter()
                .map(|loot| LootEntry {
                    item_id: loot.item_id,
                    chance: loot.chance,
                    amount: loot.amount,
                })
                .collect(),
            flee_threshold: self.flee_threshold,
            say: CreatureVoices {
                cooldown: self.say.cooldown,
                chance: self.say.chance,
                sentences: self.say.sentences,
            },
            flags: self.flags,
        })
    }
}

/// Split out from `load_creatures` for the same reason `load_spells_from_str` is: the whole
/// parse path over a document the caller owns. `path` names the file in an error only.
fn parse_creature(
    path: &Path,
    contents: &str,
    shapes: &HashMap<AreaShapeId, Arc<AreaShape>>,
) -> Result<CreatureKind, CreaturesLoadError> {
    let raw: RawCreature =
        serde_yaml::from_str(contents).map_err(|source| CreaturesLoadError::ParseError {
            path: path.to_path_buf(),
            source,
        })?;
    raw.into_kind(path, shapes)
}

/// Each `.yaml` file in `dir` is one creature, and **its file stem is the
/// `CreatureKindId`** — the name `spawns.yaml` refers to. Renaming a file renames the
/// kind, which nothing but a spawn point's `kind:` will notice.
pub fn load_creatures(
    dir: impl AsRef<Path>,
    shapes: &HashMap<AreaShapeId, Arc<AreaShape>>,
) -> Result<HashMap<CreatureKindId, Arc<CreatureKind>>, CreaturesLoadError> {
    let dir = dir.as_ref();
    let paths = yaml_files_in(dir).map_err(|source| CreaturesLoadError::ReadError {
        path: dir.to_path_buf(),
        source,
    })?;

    paths
        .into_iter()
        .map(|path| {
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| CreaturesLoadError::UnnamedFile {
                    path: path.to_path_buf(),
                })?
                .to_string();
            let contents =
                fs::read_to_string(&path).map_err(|source| CreaturesLoadError::ReadError {
                    path: path.to_path_buf(),
                    source,
                })?;
            let kind = parse_creature(&path, &contents, shapes)?;
            Ok((id, Arc::new(kind)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::Bounds;
    use crate::entities::agent::Agent;
    use crate::entities::conditions::SpecSchedule;
    use crate::entities::items::ItemId;
    use crate::entities::position::Position;
    use crate::entities::targeting::{AreaOrigin, TargetMode};
    use crate::persistence::areas::load_areas;
    use crate::persistence::items::ITEM_CONFIGS;

    const A_CREATURE: &str = r#"
name: Test Dragon
life: 1000
speed: 86
blood_type: blood
armor: 25
defense: 25
experience: 700
outfit:
  id: 34
  colors: [0, 0, 0, 0]
melee:
  damage:
    min: 4
    max: 120
  element: physical
abilities:
  - cooldown: 80
    chance: 10000
    type: attack
    element: fire
    damage:
      min: 100
      max: 200
    target:
      type: area
      origin: self
      shape: probe
    effect_id: 16
  - cooldown: 40
    chance: 15000
    type: heal
    life:
      min: 50
      max: 100
corpse: 5973
say:
  cooldown: 100
  chance: 10000
  sentences:
    - GROOAAARRR
"#;

    fn shape(name: &str) -> HashMap<AreaShapeId, Arc<AreaShape>> {
        HashMap::from([(
            name.to_string(),
            Arc::new(AreaShape::new(vec![(0, 0)].into_boxed_slice())),
        )])
    }

    fn a_creature(contents: &str) -> CreatureKind {
        parse_creature(Path::new("test.yaml"), contents, &shape("probe")).unwrap()
    }

    fn shipped() -> HashMap<CreatureKindId, Arc<CreatureKind>> {
        let areas = load_areas(&CONFIG.areas_file_path).unwrap();
        load_creatures(&CONFIG.creatures_dir_path, &areas).unwrap()
    }

    fn attack(ability: &CreatureAbility) -> (&CreatureAttackDamage, &TargetMode) {
        match &ability.effect {
            AbilityEffect::Attack(attk) => (&attk.damage, &attk.target),
            _ => panic!("not an attack"),
        }
    }

    #[test]
    fn a_creature_walks_at_its_tibia_speed() {
        for (speed, expected_ms) in [(128, 500), (86, 700), (95, 650), (109, 550)] {
            let kind = a_creature(&A_CREATURE.replace("speed: 86", &format!("speed: {speed}")));
            let agent = Agent::from_creature_kind(Arc::new(kind), Position::new(1028, 128, 7));

            assert_eq!(
                agent.calculate_walk_ticks(150, false).0 * 50,
                expected_ms,
                "speed {speed} walks a normal tile in {expected_ms}ms in the reference"
            );
        }
    }

    #[test]
    fn every_shipped_loot_and_corpse_id_resolves_to_an_item() {
        let creatures = shipped();
        assert!(
            !creatures.is_empty(),
            "loaded no creatures at all; the rest of this test would pass vacuously"
        );

        let unknown: Vec<ItemId> = creatures
            .values()
            .flat_map(|kind| {
                std::iter::once(kind.corpse).chain(kind.loot_table.iter().map(|l| l.item_id))
            })
            .filter(|id| !ITEM_CONFIGS.contains_key(id))
            .collect();

        assert!(
            unknown.is_empty(),
            "ids missing from assets/items: {unknown:?}"
        );
    }

    /// A melee element is authored per creature rather than assumed physical, and nothing
    /// downstream of the loader would notice a swing that arrived as the wrong element —
    /// it would mitigate, colour and splash as whatever it was given.
    const A_POISONOUS_CREATURE: &str = r#"
name: Test Spider
life: 100
speed: 86
blood_type: blood
armor: 10
defense: 10
experience: 50
outfit:
  id: 34
  colors: [0, 0, 0, 0]
melee:
  damage:
    min: 4
    max: 20
  element: physical
  condition:
    type: decaying
    element: earth
    damage:
      min: 40
      max: 40
    interval: 80
corpse: 5973
say:
  cooldown: 100
  chance: 10000
  sentences:
    - HISSS
"#;

    #[test]
    fn a_melee_condition_is_authored_beside_the_damage() {
        let kind = a_creature(A_POISONOUS_CREATURE);

        let condition = kind.melee.condition.as_ref().unwrap();
        assert_eq!(condition.element, CombatElement::Earth);
        assert_eq!(condition.damage, Bounds { min: 40, max: 40 });
        assert_eq!(condition.interval, TickDelta(80));
        assert!(matches!(
            condition.schedule,
            SpecSchedule::Decaying { start: None }
        ));
    }

    #[test]
    fn a_flat_condition_carries_its_count() {
        let kind = a_creature(&A_POISONOUS_CREATURE.replace(
            "    type: decaying\n    element: earth",
            "    type: flat\n    count: 7\n    element: fire",
        ));

        assert!(matches!(
            kind.melee.condition.as_ref().unwrap().schedule,
            SpecSchedule::Flat { count: 7 }
        ));
    }

    #[test]
    fn a_melee_block_carries_its_element_and_bounds() {
        let kind = a_creature(A_CREATURE);

        assert_eq!(kind.melee.element, CombatElement::Physical);
        assert_eq!(kind.melee.value, Bounds { min: 4, max: 120 });
    }

    #[test]
    fn an_ability_carries_its_roll_and_resolves_its_shape() {
        let kind = a_creature(A_CREATURE);

        let [wave, heal] = &kind.abilities[..] else {
            panic!("expected two abilities, got {}", kind.abilities.len());
        };

        assert_eq!((wave.cooldown, wave.chance), (TickDelta(80), 10000));
        let (damage, target) = attack(wave);
        assert_eq!(damage.element, CombatElement::Fire);
        assert_eq!(damage.value, Bounds { min: 100, max: 200 });
        match target {
            TargetMode::Area {
                origin: AreaOrigin::Caster,
                shape,
            } => assert_eq!(shape.get_delta(), [(0, 0)]),
            other => panic!("expected a caster-centred area, got {other:?}"),
        }

        match heal.effect {
            AbilityEffect::Heal(life) => assert_eq!(life, Bounds { min: 50, max: 100 }),
            _ => panic!("not a heal"),
        }
    }

    /// A creature with no `abilities:` is the common case, and it must load rather than
    /// join the list of fields that fail to parse when unwritten.
    #[test]
    fn a_creature_without_abilities_loads_with_none() {
        let contents = A_CREATURE.split("abilities:").next().unwrap().to_string()
            + "corpse: 5973\nsay:\n  cooldown: 100\n  chance: 10000\n  sentences: []\n";

        assert!(a_creature(&contents).abilities.is_empty());
    }

    /// The loader must refuse an ability it cannot run rather than drop it: a creature that
    /// loads with its abilities missing is a creature that fights with its melee alone, and
    /// nothing at runtime would say why.
    #[test]
    fn an_ability_type_the_server_cannot_run_is_refused() {
        let contents = A_CREATURE.replace("type: heal", "type: summon");
        let error = parse_creature(Path::new("test.yaml"), &contents, &shape("probe"))
            .expect_err("an unknown ability type must not load as no ability");

        assert!(
            matches!(&error, CreaturesLoadError::UnknownAbility { kind, .. } if kind == "summon"),
            "unexpected error: {error}"
        );
    }

    /// `type:` picks which fields the rest of the mapping may carry, so a field belonging to
    /// the other type is a typo rather than a variant this ability also has.
    #[test]
    fn a_field_of_the_other_ability_type_is_refused() {
        let contents = A_CREATURE.replace(
            "    type: attack\n",
            "    type: attack\n    life:\n      min: 1\n      max: 2\n",
        );
        let error = parse_creature(Path::new("test.yaml"), &contents, &shape("probe"))
            .expect_err("a heal field under an attack must not be dropped");

        assert!(
            matches!(error, CreaturesLoadError::ParseError { .. }),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_shape_no_area_file_defines_is_refused() {
        let error = parse_creature(Path::new("test.yaml"), A_CREATURE, &shape("something_else"))
            .expect_err("an ability pointing at nothing would hit nothing");

        assert!(
            matches!(
                error,
                CreaturesLoadError::Ability {
                    source: TargetModeError::UnknownShape { .. },
                    ..
                }
            ),
            "unexpected error: {error}"
        );
    }

    /// The catalogue is the real check: every shape an ability names must be a key in
    /// `areas.yaml`, and no test of a fixture can tell you that.
    #[test]
    fn every_shipped_ability_resolves_its_shape() {
        shipped();
    }

    /// A creature's kind id is its file name, so a rename or a typo severs every spawn
    /// point naming it. The spawner logs and skips, which is quiet enough to ship.
    #[test]
    fn every_shipped_spawn_names_a_creature_that_exists() {
        let creatures = shipped();
        let spawns = crate::persistence::spawns::load_spawns(&CONFIG.spawns_file_path).unwrap();

        let unknown: Vec<&str> = spawns
            .iter()
            .map(|spawn| spawn.kind.as_str())
            .filter(|kind| !creatures.contains_key(*kind))
            .collect();

        assert!(
            unknown.is_empty(),
            "spawns.yaml names creatures with no file in {}: {unknown:?}",
            CONFIG.creatures_dir_path
        );
    }

    use crate::entities::conditions::SpeedEffect;
    use crate::entities::support::SupportEffect;

    #[test]
    fn flags_reach_the_kind_and_default_to_none() {
        assert!(a_creature(A_CREATURE).flags.is_empty());

        let flagged = a_creature(
            &A_CREATURE.replace("speed: 86\n", "speed: 86\nflags: [immune_paralysis]\n"),
        );
        assert!(flagged.has_flag(CreatureFlag::ImmuneParalysis));
    }

    #[test]
    fn a_paralysing_ability_loads_as_support() {
        let contents = A_CREATURE.replace(
            "abilities:\n",
            "abilities:
  - cooldown: 40
    chance: 15000
    type: paralyse
    target:
      type: target
      range: 7
    speed:
      min: { factor: -1.0, flat: 44 }
      max: { factor: -1.0, flat: 149 }
    duration_ticks: 500
",
        );
        let kind = a_creature(&contents);
        let AbilityEffect::Support(cast) = &kind.abilities[0].effect else {
            panic!("not a support ability");
        };

        assert!(matches!(
            cast.effect,
            SupportEffect::Speed {
                effect: SpeedEffect::Paralysis,
                duration: TickDelta(500),
                ..
            }
        ));
        assert!(matches!(cast.target, TargetMode::Target { range: 7 }));
    }

    #[test]
    fn an_attack_with_only_a_condition_loads_as_a_condition_attack() {
        let contents = A_CREATURE.replace(
            "abilities:\n",
            "abilities:
  - cooldown: 40
    chance: 30000
    type: attack
    condition:
      type: decaying
      element: earth
      damage:
        min: 440
        max: 520
      interval: 80
    target:
      type: area
      origin: self
      shape: probe
",
        );
        let AbilityEffect::Condition(attack) = &a_creature(&contents).abilities[0].effect else {
            panic!("not a condition attack");
        };

        assert_eq!(attack.condition.damage, Bounds { min: 440, max: 520 });
    }

    #[test]
    fn an_attack_with_neither_damage_nor_condition_is_refused() {
        let contents = A_CREATURE.replace(
            "abilities:\n",
            "abilities:
  - cooldown: 40
    chance: 30000
    type: attack
    target:
      type: self
",
        );
        let error = parse_creature(Path::new("test.yaml"), &contents, &shape("probe"))
            .expect_err("an attack that does nothing must not load");

        assert!(
            matches!(error, CreaturesLoadError::BadAttack { .. }),
            "unexpected error: {error}"
        );
    }
}
