use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;

use crate::entities::targeting::{AreaOrigin, TargetMode};
use crate::entities::effects::{AreaShape, AreaShapeId};

/// Fragments rather than sentences: each caller wraps these into an error of its own that
/// names the spell or the creature file the target was authored in.
#[derive(Error, Debug)]
pub enum TargetModeError {
    #[error("targets `{target}`, not `self`, `target` or `area`")]
    UnknownTarget { target: String },
    #[error("names the area shape `{shape}`, which `areas.yaml` has not")]
    UnknownShape { shape: AreaShapeId },
    #[error("is malformed: {0}")]
    Malformed(#[from] serde_yaml::Error),
}

/// `self` carries no fields of its own, and this is what refuses one written under it rather
/// than dropping it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCaster {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTargeted {
    range: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArea {
    origin: RawOrigin,
    #[serde(default)]
    shape: AreaShapeId,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawOrigin {
    #[serde(rename = "self")]
    Caster,
    Target,
}

/// Takes the `type:` tag out of a mapping. What is left is exactly the fields of the variant
/// the tag names, so `deny_unknown_fields` still catches a typo among them. Hand-rolled rather
/// than an internally tagged serde enum because a `serde_yaml` error raised off a `Value`
/// carries no line, and this way the caller's error names what failed to load.
pub fn take_type(value: &mut serde_yaml::Value) -> Option<String> {
    let tag = value.as_mapping_mut()?.remove("type")?;
    Some(tag.as_str()?.to_string())
}

fn parse_origin(origin: RawOrigin) -> AreaOrigin {
    match origin {
        RawOrigin::Caster => AreaOrigin::Caster,
        RawOrigin::Target => AreaOrigin::Target,
    }
}

pub fn parse_target_mode(
    mut value: serde_yaml::Value,
    shapes: &HashMap<AreaShapeId, Arc<AreaShape>>,
) -> Result<TargetMode, TargetModeError> {
    let unknown = |target: String| TargetModeError::UnknownTarget { target };

    let kind =
        take_type(&mut value).ok_or_else(|| unknown("a mapping without a `type`".to_string()))?;

    match kind.as_str() {
        "self" => {
            let RawCaster {} = serde_yaml::from_value(value)?;
            Ok(TargetMode::Caster)
        }
        "target" => {
            let targeted: RawTargeted = serde_yaml::from_value(value)?;
            Ok(TargetMode::Target {
                range: targeted.range,
            })
        }
        "area" => {
            let area: RawArea = serde_yaml::from_value(value)?;
            let shape =
                shapes
                    .get(&area.shape)
                    .cloned()
                    .ok_or_else(|| TargetModeError::UnknownShape {
                        shape: area.shape.clone(),
                    })?;

            Ok(TargetMode::Area {
                origin: parse_origin(area.origin),
                shape,
            })
        }
        other => Err(unknown(other.to_string())),
    }
}
