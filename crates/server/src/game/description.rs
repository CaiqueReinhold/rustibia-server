use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    entities::{
        agent::{Agent, AgentKey},
        combat::WeaponType,
        items::{FluidType, Item, ItemConfig, ItemFlag, ItemMultiAction},
        map::GameMap,
        position::{ItemPlacement, Position},
        sex::Sex,
        spells::{Spell, SpellId},
        vocation::{Vocation, VocationSet},
    },
    game::map_query::{TileEntity, get_top_entity, item_at_placement},
};

const CARRIED: i32 = -1;
const OTHER_FLOOR: i32 = 15;

/// `None` when `viewer` is no longer on the map.
pub fn get_look_description(
    map: &GameMap,
    placement: &ItemPlacement,
    viewer: AgentKey,
    spells: &HashMap<SpellId, Arc<Spell>>,
) -> Option<String> {
    let viewer_pos = map.agent_position(viewer)?;
    let desc = match placement {
        ItemPlacement::Map(look_pos) => {
            get_top_entity(map, look_pos).and_then(|entity| match entity {
                TileEntity::Agent(key) => map
                    .get_agent(key)
                    .map(|agent| agent_description(agent, key == viewer)),
                TileEntity::Item(item) => Some(item_description(
                    item,
                    look_distance(viewer_pos, look_pos),
                    spells,
                )),
            })
        }
        ItemPlacement::Inventory(..) | ItemPlacement::Container { .. } => {
            item_at_placement(map, placement).map(|item| item_description(item, CARRIED, spells))
        }
    };
    Some(desc.unwrap_or_else(|| "You see nothing".to_owned()))
}

fn look_distance(viewer: &Position, target: &Position) -> i32 {
    let floors = if viewer.z == target.z { 0 } else { OTHER_FLOOR };
    i32::from(viewer.distance(target)) + floors
}

fn item_description(item: &Item, distance: i32, spells: &HashMap<SpellId, Arc<Spell>>) -> String {
    let config = &item.config;
    let rune = match config.attr_multi_action() {
        Some(ItemMultiAction::Rune { spell }) => Some(spells.get(&spell)),
        _ => None,
    };

    let mut text = format!("You see {}", name_phrase(item));
    match rune {
        Some(Some(spell)) => text.push_str(&rune_text(spell, item.amount)),
        Some(None) => {}
        None => text.push_str(&attribute_group(config)),
    }
    if config.has_flag(ItemFlag::LiquidPool) {
        text.push_str(" of ");
        text.push_str(fluid_name(item.fluid));
    } else if config.has_flag(ItemFlag::LiquidContainer) {
        match item.fluid.filter(|fluid| *fluid != FluidType::None) {
            Some(fluid) => {
                text.push_str(" of ");
                text.push_str(fluid_name(Some(fluid)));
            }
            None => text.push_str(". It is empty"),
        }
    }
    text.push('.');

    if rune.is_none()
        && let Some(line) = requirement_line(config)
    {
        text.push('\n');
        text.push_str(&line);
    }
    if distance <= 1 {
        if let Some(line) = weight_line(item) {
            text.push('\n');
            text.push_str(&line);
        }
        if let Some(desc) = &config.description {
            text.push('\n');
            text.push_str(desc);
        }
    }
    text
}

fn is_counted_stack(item: &Item) -> bool {
    item.config.has_flag(ItemFlag::Cumulative) && item.amount > 1
}

fn name_phrase(item: &Item) -> String {
    let config = &item.config;
    if config.name.is_empty() {
        return format!("an item of type {}", config.id.0);
    }
    if is_counted_stack(item) {
        let plural = match (&config.plural, config.show_count) {
            (Some(plural), _) => plural.clone(),
            (None, true) => format!("{}s", config.name),
            (None, false) => config.name.clone(),
        };
        return match config.show_count {
            true => format!("{} {plural}", item.amount),
            false => plural,
        };
    }
    match &config.article {
        Some(article) => format!("{article} {}", config.name),
        None => config.name.clone(),
    }
}

fn attribute_group(config: &ItemConfig) -> String {
    let mut entries = Vec::new();
    match config.attr_weapon_type() {
        Some(WeaponType::Bow | WeaponType::Crossbow) => {
            entries.push(format!(
                "Range: {}",
                config.attr_weapon_range().unwrap_or(0)
            ));
            if let Some(attack) = config.attr_weapon_attack().filter(|&a| a != 0) {
                entries.push(format!("Atk +{attack}"));
            }
            if let Some(hit_chance) = config.attr_hit_chance().filter(|&h| h != 0) {
                entries.push(format!("Hit% {hit_chance:+}"));
            }
        }
        _ if config.attr_ammo_type().is_some() => return String::new(),
        _ => {
            if config.has_flag(ItemFlag::Container)
                && let Some(capacity) = config.attr_capacity().filter(|&c| c != 0)
            {
                entries.push(format!("Vol:{capacity}"));
            }
            if let Some(attack) = config.attr_weapon_attack().filter(|&a| a != 0) {
                entries.push(format!("Atk:{attack}"));
            }
            let defense = config.attr_defense().unwrap_or(0);
            match config.attr_extra_def().unwrap_or(0) {
                0 if defense == 0 => {}
                0 => entries.push(format!("Def:{defense}")),
                extra => entries.push(format!("Def:{defense} {extra:+}")),
            }
        }
    }
    if let Some(armor) = config.attr_armor().filter(|&a| a != 0) {
        entries.push(format!("Arm:{armor}"));
    }
    if let Some(speed) = config.attr_speed().filter(|&s| s != 0) {
        entries.push(format!("speed {speed:+}"));
    }

    match entries.is_empty() {
        true => String::new(),
        false => format!(" ({})", entries.join(", ")),
    }
}

fn rune_text(spell: &Spell, amount: u8) -> String {
    let mut text = format!(" (\"{}\")", spell.words);
    let mut levels = Vec::new();
    if spell.level > 0 {
        levels.push(format!("level {}", spell.level));
    }
    if spell.magic_level > 0 {
        levels.push(format!("magic level {}", spell.magic_level));
    }
    if levels.is_empty() {
        return text;
    }
    let subject = if amount > 1 { "They" } else { "It" };
    let vocations: VocationSet = spell.vocations.iter().copied().collect();
    text.push_str(&format!(
        ". {subject} can only be used by {} with {} or higher",
        who(Some(vocations).filter(|set| !set.is_empty())),
        levels.join(" and ")
    ));
    text
}

fn requirement_line(config: &ItemConfig) -> Option<String> {
    let level = config.attr_required_level();
    let vocations = config.attr_vocations();
    if level.is_none() && vocations.is_none() {
        return None;
    }
    let level = level
        .map(|level| format!(" of level {level} or higher"))
        .unwrap_or_default();
    Some(format!(
        "It can only be wielded properly by {}{level}.",
        who(vocations)
    ))
}

fn who(vocations: Option<VocationSet>) -> String {
    let names: Vec<&str> = vocations
        .map(|set| set.iter().map(plural).collect())
        .unwrap_or_default();
    match names.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} and {}", rest.join(", "), last),
        None => "players".to_string(),
    }
}

fn plural(vocation: Vocation) -> &'static str {
    match vocation {
        Vocation::Knight => "knights",
        Vocation::Paladin => "paladins",
        Vocation::Sorcerer => "sorcerers",
        Vocation::Druid => "druids",
    }
}

fn weight_line(item: &Item) -> Option<String> {
    let weight = item.total_weight();
    if !item.config.has_flag(ItemFlag::Take) || weight == 0 {
        return None;
    }
    let verb = match is_counted_stack(item) && item.config.show_count {
        true => "They weigh",
        false => "It weighs",
    };
    Some(format!("{verb} {}.{:02} oz.", weight / 100, weight % 100))
}

fn fluid_name(fluid: Option<FluidType>) -> &'static str {
    match fluid.unwrap_or(FluidType::None) {
        FluidType::None => "unknown",
        FluidType::Water => "water",
        FluidType::Mana => "manafluid",
        FluidType::Beer => "beer",
        FluidType::Oil => "oil",
        FluidType::Blood => "blood",
        FluidType::Slime => "slime",
        FluidType::Mud => "mud",
        FluidType::Lemonade => "lemonade",
        FluidType::Milk => "milk",
        FluidType::Wine => "wine",
        FluidType::Health => "lifefluid",
        FluidType::Urine => "urine",
        FluidType::Rum => "rum",
        FluidType::FruitJuice => "fruit juice",
        FluidType::CoconutMilk => "coconut milk",
        FluidType::Tea => "tea",
        FluidType::Mead => "mead",
        FluidType::Ink => "ink",
        FluidType::Candy => "candyfluid",
        FluidType::Chocolate => "chocolate",
    }
}

fn agent_description(agent: &Agent, is_viewer: bool) -> String {
    if let Some(player) = agent.get_player() {
        let vocation = vocation_noun(player.vocation());
        return match is_viewer {
            true => format!("You see yourself. You are {vocation}."),
            false => format!(
                "You see {} (Level {}). {} is {vocation}.",
                player.name(),
                player.level(),
                pronoun(player.sex())
            ),
        };
    }
    if let Some(kind) = agent.get_creature_kind() {
        return match &kind.name_description {
            Some(described) => format!("You see {described}."),
            None => format!("You see a {}.", kind.name.to_lowercase()),
        };
    }
    format!("You see {}.", agent.name())
}

fn vocation_noun(vocation: Vocation) -> &'static str {
    match vocation {
        Vocation::Knight => "a knight",
        Vocation::Paladin => "a paladin",
        Vocation::Sorcerer => "a sorcerer",
        Vocation::Druid => "a druid",
    }
}

fn pronoun(sex: Sex) -> &'static str {
    match sex {
        Sex::Male => "He",
        Sex::Female => "She",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::combat::{AmmoType, WeaponType};
    use crate::entities::items::{ItemAttribute, ItemConfig, ItemFlag, ItemId, ItemMultiAction};
    use crate::entities::spells::SpellId;
    use crate::entities::vocation::Vocation;
    use crate::persistence::test_fixtures::a_spell;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn config(
        name: &str,
        article: Option<&str>,
        flags: Vec<ItemFlag>,
        attributes: Vec<ItemAttribute>,
    ) -> ItemConfig {
        ItemConfig::new(
            ItemId(1),
            name.to_string(),
            None,
            article.map(str::to_string),
            flags,
            attributes,
        )
    }

    fn look_at(config: ItemConfig, amount: u8, distance: i32) -> String {
        item_description(
            &Item::new(Arc::new(config), amount),
            distance,
            &HashMap::new(),
        )
    }

    fn a_sword(attributes: Vec<ItemAttribute>) -> String {
        look_at(config("sword", Some("a"), vec![], attributes), 1, 2)
    }

    #[test]
    fn level_and_vocations_are_both_stated() {
        assert_eq!(
            a_sword(vec![
                ItemAttribute::RequiredLevel(35),
                ItemAttribute::Vocations(
                    [Vocation::Paladin, Vocation::Knight].into_iter().collect()
                ),
            ]),
            "You see a sword.\nIt can only be wielded properly by knights and paladins of level 35 or higher."
        );
    }

    #[test]
    fn a_level_alone_names_players() {
        assert_eq!(
            a_sword(vec![ItemAttribute::RequiredLevel(20)]),
            "You see a sword.\nIt can only be wielded properly by players of level 20 or higher."
        );
    }

    #[test]
    fn vocations_alone_name_no_level() {
        assert_eq!(
            a_sword(vec![ItemAttribute::Vocations(
                [Vocation::Sorcerer, Vocation::Druid].into_iter().collect()
            )]),
            "You see a sword.\nIt can only be wielded properly by sorcerers and druids."
        );
    }

    #[test]
    fn an_ungated_item_states_nothing() {
        assert_eq!(a_sword(vec![]), "You see a sword.");
    }

    #[test]
    fn an_item_without_a_name_is_named_by_its_id() {
        assert_eq!(
            look_at(config("", None, vec![], vec![]), 1, 2),
            "You see an item of type 1."
        );
    }

    #[test]
    fn a_stack_uses_the_configured_plural() {
        let mut walnut = config("walnut", Some("a"), vec![ItemFlag::Cumulative], vec![]);
        walnut.plural = Some("walnuts".to_string());
        assert_eq!(look_at(walnut, 3, 2), "You see 3 walnuts.");
    }

    #[test]
    fn a_stack_without_a_plural_adds_an_s() {
        let coin = config("gold coin", Some("a"), vec![ItemFlag::Cumulative], vec![]);
        assert_eq!(look_at(coin, 15, 2), "You see 15 gold coins.");
    }

    #[test]
    fn a_stack_with_a_hidden_count_names_no_number() {
        let mut dust = config("dust", None, vec![ItemFlag::Cumulative], vec![]);
        dust.show_count = false;
        assert_eq!(look_at(dust, 15, 2), "You see dust.");
    }

    #[test]
    fn a_melee_weapon_shows_attack_and_both_defences() {
        assert_eq!(
            a_sword(vec![
                ItemAttribute::WeaponType(WeaponType::Sword),
                ItemAttribute::WeaponAttack(24),
                ItemAttribute::Defense(20),
                ItemAttribute::ExtraDef(1),
            ]),
            "You see a sword (Atk:24, Def:20 +1)."
        );
    }

    #[test]
    fn a_negative_extra_defence_keeps_its_sign() {
        assert_eq!(
            a_sword(vec![
                ItemAttribute::Defense(10),
                ItemAttribute::ExtraDef(-2)
            ]),
            "You see a sword (Def:10 -2)."
        );
    }

    #[test]
    fn a_bow_shows_range_attack_and_hit_chance() {
        let bow = config(
            "bow",
            Some("a"),
            vec![],
            vec![
                ItemAttribute::WeaponType(WeaponType::Bow),
                ItemAttribute::AmmoType(AmmoType::Arrow),
                ItemAttribute::WeaponRange(6),
                ItemAttribute::WeaponAttack(5),
                ItemAttribute::HitChance(5),
            ],
        );
        assert_eq!(
            look_at(bow, 1, 2),
            "You see a bow (Range: 6, Atk +5, Hit% +5)."
        );
    }

    #[test]
    fn a_bow_without_bonuses_shows_only_its_range() {
        let bow = config(
            "bow",
            Some("a"),
            vec![],
            vec![
                ItemAttribute::WeaponType(WeaponType::Bow),
                ItemAttribute::WeaponRange(6),
            ],
        );
        assert_eq!(look_at(bow, 1, 2), "You see a bow (Range: 6).");
    }

    #[test]
    fn ammunition_shows_no_attributes() {
        let arrow = config(
            "arrow",
            Some("an"),
            vec![ItemFlag::Cumulative],
            vec![
                ItemAttribute::AmmoType(AmmoType::Arrow),
                ItemAttribute::WeaponAttack(25),
            ],
        );
        assert_eq!(look_at(arrow, 1, 2), "You see an arrow.");
    }

    #[test]
    fn a_container_shows_its_volume() {
        let backpack = config(
            "backpack",
            Some("a"),
            vec![ItemFlag::Container],
            vec![ItemAttribute::Capacity(20)],
        );
        assert_eq!(look_at(backpack, 1, 2), "You see a backpack (Vol:20).");
    }

    #[test]
    fn armour_and_speed_join_the_same_group() {
        assert_eq!(
            a_sword(vec![
                ItemAttribute::WeaponAttack(10),
                ItemAttribute::Armor(2),
                ItemAttribute::Speed(20),
            ]),
            "You see a sword (Atk:10, Arm:2, speed +20)."
        );
    }

    #[test]
    fn a_pool_names_its_fluid_or_unknown() {
        let pool = Arc::new(config(
            "pool",
            Some("a"),
            vec![ItemFlag::LiquidPool],
            vec![],
        ));
        let blood = Item::new_fluid(Arc::clone(&pool), FluidType::Blood);
        let nothing = Item::new_fluid(pool, FluidType::None);
        assert_eq!(
            item_description(&blood, 2, &HashMap::new()),
            "You see a pool of blood."
        );
        assert_eq!(
            item_description(&nothing, 2, &HashMap::new()),
            "You see a pool of unknown."
        );
    }

    #[test]
    fn a_liquid_container_names_its_fluid_or_says_it_is_empty() {
        let vial = Arc::new(config(
            "vial",
            Some("a"),
            vec![ItemFlag::LiquidContainer],
            vec![],
        ));
        let milk = Item::new_fluid(Arc::clone(&vial), FluidType::Milk);
        let empty = Item::new(vial, 1);

        assert_eq!(
            item_description(&milk, 2, &HashMap::new()),
            "You see a vial of milk."
        );
        assert_eq!(
            item_description(&empty, 2, &HashMap::new()),
            "You see a vial. It is empty."
        );
    }

    fn a_rune(amount: u8, spell: crate::entities::spells::Spell) -> String {
        let rune = config(
            "sudden death rune",
            Some("a"),
            vec![ItemFlag::Cumulative],
            vec![
                ItemAttribute::MultiAction(ItemMultiAction::Rune { spell: SpellId(1) }),
                ItemAttribute::RequiredLevel(99),
            ],
        );
        let spells = HashMap::from([(SpellId(1), Arc::new(spell))]);
        item_description(&Item::new(Arc::new(rune), amount), 2, &spells)
    }

    #[test]
    fn a_rune_states_its_words_and_who_may_use_it() {
        let mut spell = a_spell(45, 15, vec![Vocation::Druid, Vocation::Sorcerer]);
        spell.words = "adori gran mort".to_string();
        assert_eq!(
            a_rune(1, spell),
            "You see a sudden death rune (\"adori gran mort\"). It can only be used by sorcerers and druids with level 45 and magic level 15 or higher."
        );
    }

    #[test]
    fn a_rune_stack_says_they_and_names_players_without_vocations() {
        let mut spell = a_spell(0, 4, vec![]);
        spell.words = "adito grav".to_string();
        assert_eq!(
            a_rune(3, spell),
            "You see 3 sudden death runes (\"adito grav\"). They can only be used by players with magic level 4 or higher."
        );
    }

    #[test]
    fn a_rune_without_requirements_shows_only_its_words() {
        let mut spell = a_spell(0, 0, vec![]);
        spell.words = "adito grav".to_string();
        assert_eq!(
            a_rune(1, spell),
            "You see a sudden death rune (\"adito grav\")."
        );
    }

    fn a_shield(flags: Vec<ItemFlag>, weight: u32, amount: u8, distance: i32) -> String {
        let mut shield = config(
            "shield",
            Some("a"),
            flags,
            vec![ItemAttribute::Weight(weight)],
        );
        shield.description = Some("It is battered.".to_string());
        look_at(shield, amount, distance)
    }

    #[test]
    fn weight_and_description_show_within_one_tile_and_when_carried() {
        let expected = "You see a shield.\nIt weighs 23.00 oz.\nIt is battered.";
        assert_eq!(a_shield(vec![ItemFlag::Take], 2300, 1, 1), expected);
        assert_eq!(a_shield(vec![ItemFlag::Take], 2300, 1, -1), expected);
    }

    #[test]
    fn weight_and_description_are_hidden_two_tiles_away() {
        assert_eq!(
            a_shield(vec![ItemFlag::Take], 2300, 1, 2),
            "You see a shield."
        );
    }

    #[test]
    fn an_item_that_cannot_be_taken_shows_no_weight() {
        assert_eq!(
            a_shield(vec![], 2300, 1, 1),
            "You see a shield.\nIt is battered."
        );
    }

    #[test]
    fn small_weights_keep_two_decimals() {
        assert_eq!(
            a_shield(vec![ItemFlag::Take], 5, 1, 1),
            "You see a shield.\nIt weighs 0.05 oz.\nIt is battered."
        );
    }

    #[test]
    fn a_counted_stack_weighs_in_the_plural() {
        let coin = config(
            "gold coin",
            Some("a"),
            vec![ItemFlag::Cumulative, ItemFlag::Take],
            vec![ItemAttribute::Weight(10)],
        );
        assert_eq!(
            look_at(coin, 15, 1),
            "You see 15 gold coins.\nThey weigh 1.50 oz."
        );
    }

    use crate::entities::agent::{Agent, AgentKey};
    use crate::entities::creature::CreatureKind;
    use crate::entities::map::{GameMap, MapTile};
    use crate::entities::position::{ItemPlacement, Position};
    use crate::entities::sex::Sex;
    use crate::entities::skills::{SkillType, SkillValue};
    use crate::persistence::test_fixtures::{a_creature_kind, a_test_snapshot, an_npc};

    fn a_player(id: u32, sex: Sex, level: u16) -> Agent {
        let mut snapshot = a_test_snapshot(id, 1);
        snapshot.sex = sex;
        snapshot.skills.insert(
            SkillType::Level,
            SkillValue {
                value: level,
                current_ticks: 0,
            },
        );
        Agent::from_player(snapshot)
    }

    fn standing(map: &mut GameMap, agent: Agent, at: Position) -> AgentKey {
        map.insert_tile(at.clone(), MapTile::new());
        map.insert_agent(agent, &at).unwrap()
    }

    fn look(map: &GameMap, viewer: AgentKey, at: Position) -> String {
        get_look_description(map, &ItemPlacement::Map(at), viewer, &HashMap::new()).unwrap()
    }

    #[test]
    fn looking_at_yourself_names_your_vocation() {
        let mut map = GameMap::new();
        let at = Position::new(100, 100, 7);
        let me = standing(&mut map, a_player(1, Sex::Male, 8), at.clone());
        assert_eq!(look(&map, me, at), "You see yourself. You are a knight.");
    }

    #[test]
    fn another_player_is_named_with_level_and_pronoun() {
        let mut map = GameMap::new();
        let me = standing(
            &mut map,
            a_player(1, Sex::Male, 8),
            Position::new(100, 100, 7),
        );
        let him = Position::new(101, 100, 7);
        standing(&mut map, a_player(2, Sex::Male, 8), him.clone());
        let her = Position::new(102, 100, 7);
        standing(&mut map, a_player(3, Sex::Female, 20), her.clone());

        assert_eq!(
            look(&map, me, him),
            "You see Rizael (Level 8). He is a knight."
        );
        assert_eq!(
            look(&map, me, her),
            "You see Rizael (Level 20). She is a knight."
        );
    }

    #[test]
    fn a_creature_uses_its_name_description_or_a_lowercase_fallback() {
        let mut map = GameMap::new();
        let me = standing(
            &mut map,
            a_player(1, Sex::Male, 8),
            Position::new(100, 100, 7),
        );
        let rat = Position::new(101, 100, 7);
        standing(
            &mut map,
            Agent::from_creature_kind(Arc::new(a_creature_kind("Rat")), rat.clone()),
            rat.clone(),
        );
        let boss = Position::new(102, 100, 7);
        standing(
            &mut map,
            Agent::from_creature_kind(
                Arc::new(CreatureKind {
                    name_description: Some("Ferumbras".to_string()),
                    ..a_creature_kind("Ferumbras")
                }),
                boss.clone(),
            ),
            boss.clone(),
        );

        assert_eq!(look(&map, me, rat), "You see a rat.");
        assert_eq!(look(&map, me, boss), "You see Ferumbras.");
    }

    #[test]
    fn an_npc_is_named() {
        let mut map = GameMap::new();
        let me = standing(
            &mut map,
            a_player(1, Sex::Male, 8),
            Position::new(100, 100, 7),
        );
        let at = Position::new(101, 100, 7);
        standing(&mut map, Agent::from_npc(an_npc()), at.clone());
        assert_eq!(look(&map, me, at), "You see Captain Jack.");
    }

    #[test]
    fn an_item_on_another_floor_is_out_of_reach_of_its_weight() {
        let mut map = GameMap::new();
        let me = standing(
            &mut map,
            a_player(1, Sex::Male, 8),
            Position::new(100, 100, 7),
        );
        let below = Position::new(100, 100, 8);
        let mut tile = MapTile::new();
        tile.push_item(Item::new(
            Arc::new(config(
                "shield",
                Some("a"),
                vec![ItemFlag::Take],
                vec![ItemAttribute::Weight(2300)],
            )),
            1,
        ));
        map.insert_tile(below.clone(), tile);

        assert_eq!(look(&map, me, below), "You see a shield.");
    }

    #[test]
    fn an_empty_tile_shows_nothing_and_a_missing_viewer_gets_no_answer() {
        let mut map = GameMap::new();
        let at = Position::new(100, 100, 7);
        let me = standing(&mut map, a_player(1, Sex::Male, 8), at.clone());
        let empty = Position::new(101, 100, 7);
        map.insert_tile(empty.clone(), MapTile::new());
        assert_eq!(look(&map, me, empty.clone()), "You see nothing");

        map.remove_agent(me);
        assert_eq!(
            get_look_description(&map, &ItemPlacement::Map(empty), me, &HashMap::new()),
            None
        );
    }

    #[test]
    fn look_distance_adds_fifteen_across_floors() {
        let viewer = Position::new(100, 100, 7);
        assert_eq!(look_distance(&viewer, &Position::new(101, 102, 7)), 2);
        assert_eq!(look_distance(&viewer, &Position::new(100, 100, 6)), 15);
    }
}
