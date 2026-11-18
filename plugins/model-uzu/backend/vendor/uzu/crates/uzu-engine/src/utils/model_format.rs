//! Cortex model-format migration: normalize the explicit0.6 schema to0.7 in memory.
//! Tensor byte ranges and installed files are never rewritten. Unknown fields remain subject to
//! the engine's strict typed deserializer. Every added value is the old engine's fixed operation.
use std::collections::HashSet;

use serde::de::Error as _;
use serde_json::{Map, Value, json};

fn malformed(message: &str) -> serde_json::Error {
    serde_json::Error::custom(message)
}

fn required(object: &mut Map<String, Value>, key: &str) -> Result<Value, serde_json::Error> {
    object.remove(key).ok_or_else(|| malformed(&format!("0.6 model is missing {key}")))
}

/// Normalize a model/config or serialized weight-matrix specification before strict deserialization.
pub(crate) fn config(mut value: Value) -> Result<Value, serde_json::Error> {
    if value.get("type").and_then(Value::as_str) == Some("SpeculatorModelConfig") {
        let object = value.as_object_mut().ok_or_else(|| malformed("speculator wrapper is not an object"))?;
        let speculator = required(object, "speculator_config")?;
        object.remove("type");
        if !object.is_empty() {
            return Err(malformed("unknown fields in0.6 speculator wrapper"));
        }
        value = speculator;
    }
    normalize(&mut value)?;
    Ok(value)
}

fn normalize(value: &mut Value) -> Result<(), serde_json::Error> {
    match value {
        Value::Array(items) => {
            for item in items { normalize(item)?; }
        },
        Value::Object(object) => {
            // Only TransformerConfig, not DFlashDraftConfig, owns a residual_config field.
            let old_transformer = object.contains_key("layer_configs")
                && object.contains_key("model_dim") && object.contains_key("hidden_dim")
                && !object.contains_key("block_size")
                && object.get("layer_configs").and_then(Value::as_array)
                    .is_some_and(|layers| layers.iter().any(|layer| layer.get("mixer_config").is_some()));
            for child in object.values_mut() { normalize(child)?; }
            if object.contains_key("mixer_config") {
                normalize_layer(object)?;
            }
            if old_transformer {
                if object.contains_key("residual_config") {
                    return Err(malformed("mixed0.6/0.7 transformer residual configuration"));
                }
                //0.6 only implements the ordinary additive residual;0.7 makes it explicit.
                object.insert("residual_config".into(), json!({"type": "AdditiveResidualConfig"}));
            }
            let ty = object.get("type").and_then(Value::as_str).map(str::to_owned);
            match ty.as_deref() {
                Some("ChatCodecConfig") if object.contains_key("output_parser_regex")
                    || object.contains_key("end_of_thinking_tag") => {
                    if object.contains_key("response_format") {
                        return Err(malformed("mixed0.6/0.7 chat codec"));
                    }
                    let pattern = object.remove("output_parser_regex").unwrap_or(Value::Null);
                    // This field was only read by the old high-level codec. Cortex renders and
                    // parses from the untouched installed config through its own model-kit.
                    object.remove("end_of_thinking_tag");
                    object.insert("response_format".into(), json!({"reasoning_pattern": pattern,
                        "prefixes": [], "turn_pattern": null, "tool_calls": null}));
                },
                Some("TiedEmbeddingConfig" | "UntiedEmbeddingConfig") if !object.contains_key("permute_indices") => {
                    //0.6 embeddings do not read an index permutation.
                    object.insert("permute_indices".into(), Value::Bool(false));
                },
                Some("DeltaNetConfig") if !object.contains_key("output_gate") => {
                    //0.6 gdn/norm_gate and gdn/update use activate_silu without another alpha.
                    object.insert("output_gate".into(), json!({"type": "SiLU", "alpha": 1.0}));
                },
                Some("HybridSpec") if object.contains_key("incoherence_block_size")
                    || object.contains_key("incoherence_processing_mode") => {
                    if object.contains_key("incoherence_spec") {
                        return Err(malformed("mixed0.6/0.7 incoherence specification"));
                    }
                    let block = object.remove("incoherence_block_size").unwrap_or(Value::Null);
                    let mode = required(object, "incoherence_processing_mode")?;
                    let incoherence = if block.is_null() { Value::Null } else {
                        json!({"type": "HadamardSpec", "block_size": block, "processing_mode": mode})
                    };
                    object.insert("incoherence_spec".into(), incoherence);
                },
                _ => {},
            }
        },
        _ => {},
    }
    Ok(())
}

fn normalize_layer(object: &mut Map<String, Value>) -> Result<(), serde_json::Error> {
    if object.contains_key("mixer") || object.contains_key("mlp") || object.contains_key("prologue_configs")
        || object.contains_key("epilogue_configs") {
        return Err(malformed("mixed0.6/0.7 transformer layer"));
    }
    let mixer = required(object, "mixer_config")?;
    let mlp = required(object, "mlp_config")?;
    let pre_mixer = object.remove("pre_mixer_norm_config").unwrap_or(Value::Null);
    let mixer_conv = object.remove("mixer_conv_config").unwrap_or(Value::Null);
    let post_mixer = object.remove("post_mixer_norm_config").unwrap_or(Value::Null);
    let pre_mlp = required(object, "pre_mlp_norm_config")?;
    let mlp_conv = object.remove("mlp_conv_config").unwrap_or(Value::Null);
    let post_mlp = object.remove("post_mlp_norm_config").unwrap_or(Value::Null);
    let scalar = required(object, "has_post_layer_scalar")?.as_bool()
        .ok_or_else(|| malformed("0.6 has_post_layer_scalar is not a bool"))?;
    if scalar && post_mlp.is_null() {
        return Err(malformed("0.6 post-layer scalar requires a post-MLP norm"));
    }
    let ple = object.remove("ple_config").unwrap_or(Value::Null);
    let mut epilogue = Vec::new();
    let has_ple = !ple.is_null();
    if has_ple {
        let mut injection = ple.as_object().cloned().ok_or_else(|| malformed("0.6 PLE config is not an object"))?;
        injection.insert("type".into(), Value::String("PerLayerEmbeddingInjectionConfig".into()));
        epilogue.push(Value::Object(injection));
    }
    if scalar && has_ple { epilogue.push(json!({"type": "ResidualScaleConfig"})); }
    object.insert("prologue_configs".into(), json!([]));
    object.insert("mixer".into(), json!({"pre_norm_config": pre_mixer, "conv_config": mixer_conv,
        "post_norm_config": post_mixer, "body_config": mixer,
        "legacy_scale_residual": scalar && !has_ple, "legacy_scale_update": false}));
    object.insert("mlp".into(), json!({"pre_norm_config": pre_mlp, "conv_config": mlp_conv,
        "post_norm_config": post_mlp, "body_config": mlp,
        "legacy_scale_residual": false, "legacy_scale_update": scalar && !has_ple}));
    object.insert("epilogue_configs".into(), Value::Array(epilogue));
    Ok(())
}

/// Normalize just safetensors header names/metadata; retain every dtype, shape and byte offset.
pub(crate) fn header(value: Value) -> Result<Value, serde_json::Error> {
    let object = value.as_object().ok_or_else(|| malformed("safetensors header is not an object"))?;
    let old_layers = object.keys().any(|key| legacy_layer_key(key));
    let ple_layers: HashSet<String> = object.keys().filter_map(|key| {
        let parts: Vec<_> = key.split('.').collect();
        let i = layer_index(&parts)?;
        (parts.get(i + 2) == Some(&"ple")).then(|| parts[..i + 2].join("."))
    }).collect();
    let mut mapped = Map::new();
    for (key, value) in object {
        if key == "__metadata__" {
            let metadata = value.as_object().ok_or_else(|| malformed("safetensors metadata is not an object"))?;
            let mut normalized = Map::new();
            for (key, value) in metadata {
                let new_key = tensor_key(key, old_layers, &ple_layers);
                let value = if key.ends_with(".spec") {
                    let text = value.as_str().ok_or_else(|| malformed("weight specification is not JSON text"))?;
                    Value::String(serde_json::to_string(&config(serde_json::from_str(text)?)?)?)
                } else { value.clone() };
                if normalized.insert(new_key, value).is_some() {
                    return Err(malformed("colliding migrated weight metadata names"));
                }
            }
            mapped.insert(key.clone(), Value::Object(normalized));
        } else {
            let key = tensor_key(key, old_layers, &ple_layers);
            if mapped.insert(key, value.clone()).is_some() {
                return Err(malformed("colliding migrated tensor names"));
            }
        }
    }
    Ok(Value::Object(mapped))
}

fn layer_index(parts: &[&str]) -> Option<usize> {
    parts.windows(2).position(|pair| pair[0] == "layers"
        && !pair[1].is_empty() && pair[1].bytes().all(|b| b.is_ascii_digit()))
}

fn legacy_layer_key(key: &str) -> bool {
    let parts: Vec<_> = key.split('.').collect();
    let Some(i) = layer_index(&parts) else { return false; };
    match parts.get(i + 2).copied() {
        Some("pre_mixer_norm" | "post_mixer_norm" | "mixer_conv" | "pre_mlp_norm"
            | "post_mlp_norm" | "mlp_conv" | "ple" | "post_layer_scalar") => true,
        Some("mixer" | "mlp") => parts.get(i + 3).is_some_and(|name|
            !matches!(*name, "body" | "pre_norm" | "post_norm" | "conv" | "connection")),
        _ => false,
    }
}

fn tensor_key(key: &str, old_layers: bool, ple_layers: &HashSet<String>) -> String {
    //0.6 SpeculatorModelConfig stored parameters below speculator.;0.7 loads the config directly.
    let mut parts: Vec<_> = key.split('.').collect();
    let mut result: Vec<String> = Vec::new();
    if old_layers {
        if let Some(i) = layer_index(&parts) {
            let layer = parts[..i + 2].join(".");
            result.extend(parts[..i + 2].iter().map(|part| (*part).to_owned()));
            let suffix = parts.get(i + 3..).unwrap_or(&[]);
            match parts.get(i + 2).copied() {
                Some("mixer") => result.extend(["mixer".into(), "body".into()]),
                Some("mlp") => result.extend(["mlp".into(), "body".into()]),
                Some("pre_mixer_norm") => result.extend(["mixer".into(), "pre_norm".into()]),
                Some("post_mixer_norm") => result.extend(["mixer".into(), "post_norm".into()]),
                Some("mixer_conv") => result.extend(["mixer".into(), "conv".into()]),
                Some("pre_mlp_norm") => result.extend(["mlp".into(), "pre_norm".into()]),
                Some("post_mlp_norm") => result.extend(["mlp".into(), "post_norm".into()]),
                Some("mlp_conv") => result.extend(["mlp".into(), "conv".into()]),
                Some("ple") => result.extend(["epilogue".into(), "0".into()]),
                Some("post_layer_scalar") => {
                    if ple_layers.contains(&layer) {
                        result.extend(["epilogue".into(), "1".into(), "scale".into()]);
                    } else {
                        result.push("legacy_post_layer_scalar".into());
                    }
                },
                _ => return key.strip_prefix("speculator.").unwrap_or(key).to_owned(),
            }
            result.extend(suffix.iter().map(|part| (*part).to_owned()));
            if result.first().map(String::as_str) == Some("speculator") { result.remove(0); }
            return result.join(".");
        }
    }
    if parts.first() == Some(&"speculator") { parts.remove(0); }
    parts.join(".")
}
