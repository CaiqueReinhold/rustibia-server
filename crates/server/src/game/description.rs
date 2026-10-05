use crate::{
    entities::{
        agent::Agent,
        items::{Item, ItemConfig},
        map::GameMap,
        position::{ItemPlacement, Position},
        vocation::Vocation,
    },
    game::map_query::{TileEntity, get_top_entity, item_at_placement},
};
use std::fmt::{Error, Write};

pub fn get_look_description(
    map: &GameMap,
    placement: &ItemPlacement,
    player_pos: &Position,
) -> String {
    let desc = match placement {
        // A tile names no item of its own, so a look there describes whatever is on top.
        ItemPlacement::Map(look_pos) => get_top_entity(map, look_pos).map(|entity| match entity {
            TileEntity::Agent(agent_key) => map
                .get_agent(agent_key)
                .map(get_agent_description)
                .unwrap_or(Ok("".to_owned())),
            TileEntity::Item(item) => get_item_description(item, player_pos.is_within(look_pos, 1)),
        }),
        ItemPlacement::Inventory(..) | ItemPlacement::Container { .. } => {
            item_at_placement(map, placement).map(|item| get_item_description(item, true))
        }
    };

    desc.and_then(|e| e.ok())
        .filter(|s| !String::is_empty(s))
        .unwrap_or("You see nothing".to_owned())
}

fn get_item_description(item: &Item, show_weight: bool) -> Result<String, Error> {
    let mut buff = String::new();
    write!(buff, "You see ")?;

    if item.amount == 1
        && let Some(article) = &item.config.article
    {
        write!(buff, "{} ", article)?;
    } else if item.amount > 1 {
        write!(buff, "{} ", item.amount)?;
    }

    write!(buff, "{}.", item.config.name)?;

    if let Some(desc) = &item.config.description {
        write!(buff, "\n{}", desc)?;
    }

    if let Some(requirement) = requirement_text(&item.config) {
        write!(buff, "\n{}", requirement)?;
    }

    if show_weight {
        let weight = item.total_weight();
        if weight > 0 {
            write!(buff, "\nIt weights {:.2}oz.", (weight as f32) / 100.0)?;
        }
    }

    Ok(buff)
}

fn requirement_text(config: &ItemConfig) -> Option<String> {
    let level = config.attr_required_level();
    let vocations = config.attr_vocations();
    if level.is_none() && vocations.is_none() {
        return None;
    }
    let who = match vocations {
        Some(set) => {
            let names: Vec<&str> = set.iter().map(plural).collect();
            match names.split_last() {
                Some((last, [])) => last.to_string(),
                Some((last, rest)) => format!("{} and {}", rest.join(", "), last),
                None => "players".to_string(),
            }
        }
        None => "players".to_string(),
    };
    let level = level
        .map(|level| format!(" of level {level} or higher"))
        .unwrap_or_default();
    Some(format!("It can only be wielded properly by {who}{level}."))
}

fn plural(vocation: Vocation) -> &'static str {
    match vocation {
        Vocation::Knight => "knights",
        Vocation::Paladin => "paladins",
        Vocation::Sorcerer => "sorcerers",
        Vocation::Druid => "druids",
    }
}

fn get_agent_description(agent: &Agent) -> Result<String, Error> {
    if agent.is_creature() {
        Ok(format!("You see a {}", agent.name()))
    } else {
        Ok(format!("You see {}", agent.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::items::{ItemAttribute, ItemConfig, ItemId};
    use crate::entities::vocation::Vocation;
    use std::sync::Arc;

    fn a_sword(attributes: Vec<ItemAttribute>) -> Item {
        Item::new(
            Arc::new(ItemConfig::new(
                ItemId(1),
                "sword".to_string(),
                None,
                Some("a".to_string()),
                [],
                attributes,
            )),
            1,
        )
    }

    fn look(attributes: Vec<ItemAttribute>) -> String {
        get_item_description(&a_sword(attributes), false).unwrap()
    }

    #[test]
    fn level_and_vocations_are_both_stated() {
        assert_eq!(
            look(vec![
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
            look(vec![ItemAttribute::RequiredLevel(20)]),
            "You see a sword.\nIt can only be wielded properly by players of level 20 or higher."
        );
    }

    #[test]
    fn vocations_alone_name_no_level() {
        assert_eq!(
            look(vec![ItemAttribute::Vocations(
                [Vocation::Sorcerer, Vocation::Druid].into_iter().collect()
            )]),
            "You see a sword.\nIt can only be wielded properly by sorcerers and druids."
        );
    }

    #[test]
    fn an_ungated_item_states_nothing() {
        assert_eq!(look(vec![]), "You see a sword.");
    }
}
