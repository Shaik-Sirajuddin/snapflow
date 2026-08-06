//! Small, authoritative schemas for the MLT services whose properties have
//! caused silent no-op edits. Unknown/plugin services remain pass-through for
//! compatibility; this module only rejects keys for services we can describe.

use serde_json::{json, Map, Value};

fn known(service: &str, property: &str) -> Option<bool> {
    let ok = match service {
        "brightness" => matches!(
            property,
            "level" | "alpha" | "start" | "end" | "threads" | "rgb_only"
        ),
        "qtcrop" => property == "rect",
        // MLT's core `crop` filter removes edge pixels. It does not use the
        // commonly assumed x/y/width/height rectangle API; those keys are
        // silently retained by MLT's generic property bag but have no
        // rendering effect. Use left/right/top/bottom (or center) instead.
        "crop" => matches!(
            property,
            "active" | "left" | "right" | "top" | "bottom" | "center" | "center_bias" | "use_profile"
        ),
        "volume" => matches!(property, "level" | "gain"),
        "panner" => matches!(property, "start" | "end" | "channel"),
        "affine" => {
            matches!(property, "background" | "use_normalized" | "use_normalised")
                || property.starts_with("producer.")
                || property.strip_prefix("transition.").is_some_and(|p| {
                    matches!(
                        p,
                        "rect"
                            | "ox"
                            | "oy"
                            | "scale_x"
                            | "scale_y"
                            | "rotate_x"
                            | "rotate_y"
                            | "rotate_z"
                            | "shear_x"
                            | "shear_y"
                            | "shear_z"
                            | "fix_rotate_x"
                            | "fix_rotate_y"
                            | "fix_rotate_z"
                            | "fix_shear_x"
                            | "fix_shear_y"
                            | "fix_shear_z"
                            | "distort"
                            | "fill"
                            | "repeat_off"
                            | "mirror_off"
                            | "cycle"
                            | "keyed"
                            | "mirror"
                            | "invert_scale"
                            | "b_alpha"
                            | "halign"
                            | "valign"
                            | "threads"
                            | "scale"
                            | "b_scaled"
                    )
                })
        }
        _ => return None,
    };
    Some(ok)
}

pub fn validate_properties(service: &str, properties: &Value) -> Result<(), String> {
    let Some(obj) = properties.as_object() else {
        return Err("filter properties must be a JSON object".into());
    };
    for (property, value) in obj {
        validate_property(service, property, value, false)?;
    }
    Ok(())
}

pub fn validate_property(
    service: &str,
    property: &str,
    value: &Value,
    keyframe: bool,
) -> Result<(), String> {
    let Some(allowed) = known(service, property) else {
        return Ok(());
    };
    if !allowed {
        return Err(format!(
            "unsupported property {property:?} for MLT service {service:?}"
        ));
    }
    if keyframe
        && matches!(
            (service, property),
            ("brightness", "threads") | ("brightness", "rgb_only")
        )
    {
        return Err(format!(
            "property {property:?} for {service:?} is not animatable"
        ));
    }
    if service == "brightness"
        && matches!(property, "level" | "alpha" | "start" | "end" | "threads")
        && !value.is_number()
    {
        return Err(format!(
            "property {property:?} for brightness requires a number"
        ));
    }
    if service == "brightness" && property == "rgb_only" && !value.is_boolean() {
        return Err("brightness.rgb_only requires a boolean".into());
    }
    Ok(())
}

pub fn describe(service: &str) -> Value {
    let properties: Map<String, Value> = match service {
        "brightness" => [
            (
                "level",
                json!({"type":"number","minimum":0,"maximum":15,"animatable":true}),
            ),
            (
                "alpha",
                json!({"type":"number","minimum":-1,"animatable":true}),
            ),
            ("start", json!({"type":"number","minimum":0,"maximum":15})),
            ("end", json!({"type":"number","minimum":0,"maximum":15})),
            ("threads", json!({"type":"integer","minimum":0})),
            ("rgb_only", json!({"type":"boolean"})),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect(),
        "affine" => [
            "background",
            "use_normalized",
            "use_normalised",
            "transition.rect",
            "transition.ox",
            "transition.oy",
            "transition.scale_x",
            "transition.scale_y",
            "transition.repeat_off",
            "transition.mirror_off",
            "transition.keyed",
            "transition.rotate_x",
            "transition.rotate_y",
            "transition.rotate_z",
        ]
        .into_iter()
        .map(|k| (k.into(), json!({"type":"scalar"})))
        .collect(),
        "crop" => [
            ("active", json!({"type":"integer","minimum":0,"maximum":1})),
            ("left", json!({"type":"integer","minimum":0,"unit":"pixels"})),
            ("right", json!({"type":"integer","minimum":0,"unit":"pixels"})),
            ("top", json!({"type":"integer","minimum":0,"unit":"pixels"})),
            ("bottom", json!({"type":"integer","minimum":0,"unit":"pixels"})),
            ("center", json!({"type":"integer","minimum":0,"maximum":1})),
            ("center_bias", json!({"type":"integer","minimum":0,"unit":"pixels"})),
            ("use_profile", json!({"type":"integer","minimum":0,"maximum":1})),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect(),
        _ => Map::new(),
    };
    json!({"mltService": service, "schemaAvailable": !properties.is_empty(), "properties": properties})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_silent_noop_properties() {
        assert!(validate_property("brightness", "opacity", &json!(0.5), false).is_err());
        assert!(validate_property("affine", "transition.geometry", &json!("x"), false).is_err());
        assert!(validate_property("crop", "width", &json!(100), false).is_err());
        assert!(validate_property("crop", "left", &json!(10), false).is_ok());
        assert!(validate_property("brightness", "alpha", &json!(0.5), true).is_ok());
        assert!(validate_property("frei0r.any", "plugin_property", &json!(1), false).is_ok());
    }
}
