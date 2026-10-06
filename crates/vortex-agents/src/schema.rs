//! Typed tool parameter specifications with server-side validation.
//!
//! We define parameters with simple typed specs and *generate* the JSON Schema
//! handed to the model. Validation happens against the same specs, so the
//! model can never talk the backend into skipping a check.

use vortex_llm::ToolSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    Str,
    Int,
    Num,
    Bool,
    StrArray,
    Any,
}

impl ParamType {
    fn as_str(self) -> &'static str {
        match self {
            ParamType::Str => "string",
            ParamType::Int => "integer",
            ParamType::Num => "number",
            ParamType::Bool => "boolean",
            ParamType::StrArray => "array",
            ParamType::Any => "object",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    pub name: &'static str,
    pub ty: ParamType,
    pub required: bool,
    pub description: &'static str,
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub params: &'static [ParamSpec],
}

impl ToolDef {
    /// JSON Schema payload for the OpenRouter tools array.
    pub fn json_schema(&self) -> ToolSchema {
        let props: serde_json::Map<String, serde_json::Value> = self
            .params
            .iter()
            .map(|p| {
                (
                    p.name.to_string(),
                    serde_json::json!({
                        "type": p.ty.as_str(),
                        "description": p.description,
                    }),
                )
            })
            .collect();
        let required: Vec<String> = self
            .params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name.to_string())
            .collect();
        ToolSchema::function(
            self.name,
            self.description,
            serde_json::json!({
                "type": "object",
                "properties": props,
                "required": required,
            }),
        )
    }

    /// Validate model-provided arguments against the specs.
    pub fn validate(&self, args: &serde_json::Value) -> Result<(), String> {
        let Some(obj) = args.as_object() else {
            return Err("arguments must be a JSON object".into());
        };
        for p in self.params {
            match obj.get(p.name) {
                None if p.required => {
                    return Err(format!("missing required parameter '{}'", p.name))
                }
                None => {}
                Some(v) => {
                    let ok = match p.ty {
                        ParamType::Str => v.is_string(),
                        ParamType::Int => v.is_i64() || v.is_u64(),
                        ParamType::Num => v.is_number(),
                        ParamType::Bool => v.is_boolean(),
                        ParamType::StrArray => v
                            .as_array()
                            .map(|a| a.iter().all(|x| x.is_string()))
                            .unwrap_or(false),
                        ParamType::Any => true,
                    };
                    if !ok {
                        return Err(format!("parameter '{}' must be {}", p.name, p.ty.as_str()));
                    }
                    if let (ParamType::Str, Some(s)) = (p.ty, v.as_str()) {
                        if s.len() > 200_000 {
                            return Err(format!("parameter '{}' too long", p.name));
                        }
                    }
                }
            }
        }
        // Unknown extra keys are dropped, not trusted.
        Ok(())
    }

    /// Keep only known parameters (defense against extra junk).
    pub fn filter(&self, args: serde_json::Value) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        if let Some(obj) = args.as_object() {
            for p in self.params {
                if let Some(v) = obj.get(p.name) {
                    out.insert(p.name.to_string(), v.clone());
                }
            }
        }
        serde_json::Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEF: ToolDef = ToolDef {
        name: "t",
        description: "test",
        params: &[
            ParamSpec {
                name: "path",
                ty: ParamType::Str,
                required: true,
                description: "p",
            },
            ParamSpec {
                name: "n",
                ty: ParamType::Int,
                required: false,
                description: "n",
            },
        ],
    };

    #[test]
    fn validates_required_and_types() {
        assert!(DEF.validate(&serde_json::json!({"path": "x"})).is_ok());
        assert!(DEF.validate(&serde_json::json!({"n": 3})).is_err()); // missing path
        assert!(DEF.validate(&serde_json::json!({"path": 1})).is_err()); // wrong type
        assert!(DEF
            .validate(&serde_json::json!({"path": "x", "n": "str"}))
            .is_err());
    }

    #[test]
    fn filters_unknown_keys() {
        let out = DEF.filter(serde_json::json!({"path": "x", "evil": true}));
        assert!(out.get("evil").is_none());
        assert_eq!(out["path"], "x");
    }
}
