//! dsh's enforced JSON Schema subset and its validator. Ported from
//! `tools/tests/json-schema.spec.ts`, and the `validateArgs` cases of
//! `tools/tests/tools.spec.ts`, where a case fits plain JSON.
//!
//! Not ported, as a `serde_json::Value` can't hold what they test:
//! `undefined`, `NaN`, `Infinity` and `-0`; sparse, decorated and exotic
//! arrays; forged, revoked and other-realm prototypes; proxies, getters,
//! symbols and hidden keys; cycles; `Date` and `Map`; and the
//! `assertNever` backstop for a forged schema. Nor are the two 5,000-deep
//! `oneOf` cases, which test dsh's explicit stacks: the walk here recurses
//! as deep as the schema, which a tool's author writes.

use karaka_tools::JsonSchemaError;
use karaka_tools::json_schema::{check_object_schema, check_schema, validate};
use serde_json::{Value, json};

fn violations_of(schema: Value) -> Vec<String> {
    match check_schema(&schema) {
        Ok(()) => panic!("expected {schema} to be refused"),
        Err(JsonSchemaError { violations }) => violations,
    }
}

fn object_violations_of(schema: Value) -> Vec<String> {
    match check_object_schema(&schema) {
        Ok(()) => panic!("expected {schema} to be refused"),
        Err(JsonSchemaError { violations }) => violations,
    }
}

/// `validateJsonSchemaValue`'s default root, `value`.
fn check(schema: Value, value: Value) -> Vec<String> {
    check_schema(&schema).expect("the schema is in the subset");
    validate(&schema, &value, "value")
}

/// `validateArgs`: the parameters' root, named `arguments`.
fn args(schema: Value, value: Value) -> Vec<String> {
    check_schema(&schema).expect("the schema is in the subset");
    validate(&schema, &value, "")
}

// the enforced raw JSON Schema subset

// "accepts every JSON root and every supported node"
#[test]
fn every_supported_node_is_accepted() {
    for schema in [
        json!({ "type": "string" }),
        json!({ "type": "number" }),
        json!({ "type": "integer" }),
        json!({ "type": "boolean" }),
        json!({ "type": "null" }),
        json!({ "type": "array", "items": { "type": "string" } }),
        json!({
            "type": "object",
            "properties": {
                "nested": { "type": "object", "properties": {}, "additionalProperties": false },
                "free": {},
            },
            "required": ["nested"],
            "additionalProperties": true,
        }),
        json!({ "oneOf": [{ "type": "string" }, { "type": "number" }] }),
        json!({ "description": "any JSON", "title": "JSON", "default": null, "examples": [1, "x"] }),
    ] {
        assert_eq!(check_schema(&schema), Ok(()), "{schema}");
    }
}

// "retains an object-root guard only at consumers that need it"
#[test]
fn the_object_root_guard() {
    assert_eq!(check_object_schema(&json!({ "type": "object" })), Ok(()));
    for schema in [
        json!({}),
        json!({ "type": "string" }),
        json!({ "type": "array" }),
        json!({ "oneOf": [{ "type": "string" }, { "type": "null" }] }),
    ] {
        assert_eq!(
            object_violations_of(schema),
            ["schema.type must be \"object\" (structured output is object-rooted)"]
        );
    }
}

// "rejects non-schema nodes, unknown types, and type arrays"
#[test]
fn non_schemas_unknown_types_and_type_arrays_are_refused() {
    for schema in [json!(null), json!([]), json!("no")] {
        assert_eq!(violations_of(schema), ["schema must be a schema object"]);
    }
    assert!(violations_of(json!({ "type": "tuple" }))[0].contains("type must be one of"));
    assert_eq!(
        violations_of(json!({ "type": "tuple" })),
        ["schema.type must be one of object/array/string/number/integer/boolean/null"]
    );
    assert_eq!(
        violations_of(json!({ "type": ["string", "null"] })),
        ["schema.type must be a single type string (type arrays are not supported)"]
    );
}

// "enforces oneOf vocabulary and its minimum branch count"
#[test]
fn one_of_needs_two_branches_and_no_siblings() {
    for one_of in [json!([]), json!([{}]), json!("x")] {
        assert_eq!(
            violations_of(json!({ "oneOf": one_of })),
            ["schema.oneOf must be an array of at least two schemas"]
        );
    }
    assert_eq!(
        violations_of(json!({ "type": "string", "oneOf": [{}, {}] })),
        ["schema cannot declare both type and oneOf"]
    );
    assert_eq!(
        violations_of(
            json!({ "oneOf": [{ "type": "string" }, { "type": "number" }], "items": {} })
        ),
        ["schema.items is not supported beside oneOf"]
    );
    assert!(
        violations_of(json!({ "oneOf": [{ "type": "string" }, { "type": "weird" }] }))[0]
            .contains("schema.oneOf[1].type")
    );
}

// "rejects unknown and misplaced keywords without accepted-then-ignored
// behavior"
#[test]
fn unknown_and_misplaced_keywords_are_refused() {
    for keyword in [
        "anyOf",
        "allOf",
        "not",
        "pattern",
        "minimum",
        "maxLength",
        "$ref",
    ] {
        let mut schema = json!({ "type": "object" });
        schema[keyword] = json!([]);
        assert!(
            violations_of(schema)[0]
                .contains(&format!("schema.{keyword} is not a supported keyword")),
            "{keyword}"
        );
    }
    assert_eq!(
        violations_of(json!({ "type": "object", "minimum": 1 })),
        ["schema.minimum is not a supported keyword (subset: \
          type/oneOf/properties/required/additionalProperties/items/enum/const + annotations)"]
    );
    assert_eq!(
        violations_of(json!({ "type": "object", "items": {} })),
        ["schema.items is not supported on type \"object\""]
    );
    assert_eq!(
        violations_of(json!({ "type": "array", "properties": {} })),
        ["schema.properties is not supported on type \"array\""]
    );
    assert_eq!(
        violations_of(json!({ "type": "object", "enum": ["x"] })),
        ["schema.enum is not supported on type \"object\""]
    );
    assert_eq!(
        violations_of(json!({ "type": "array", "const": null })),
        ["schema.const is not supported on type \"array\""]
    );
    assert_eq!(
        violations_of(json!({
            "properties": {}, "required": [], "additionalProperties": true,
            "items": {}, "enum": [], "const": null,
        })),
        [
            "schema.properties requires type or oneOf",
            "schema.required requires type or oneOf",
            "schema.additionalProperties requires type or oneOf",
            "schema.items requires type or oneOf",
            "schema.enum requires type or oneOf",
            "schema.const requires type or oneOf",
        ]
    );
}

// "reports every independent schema violation"
#[test]
fn every_violation_is_reported() {
    assert_eq!(
        violations_of(json!({
            "type": "object",
            "pattern": "x",
            "properties": { "a": { "type": "weird" }, "b": { "type": "string", "minimum": 1 } },
        }))
        .len(),
        3
    );
}

// "validates object properties, required names, and openness"
#[test]
fn object_keywords_are_checked() {
    assert_eq!(
        violations_of(json!({ "type": "object", "properties": [] })),
        ["schema.properties must be an object of schemas"]
    );
    assert_eq!(
        violations_of(json!({ "type": "object", "properties": { "a": "x" } })),
        ["schema.properties.a must be a schema object"]
    );
    for required in [json!("a"), json!([1])] {
        assert_eq!(
            violations_of(json!({ "type": "object", "required": required })),
            ["schema.required must be an array of strings"]
        );
    }
    assert_eq!(
        violations_of(json!({ "type": "object", "properties": {}, "required": ["missing"] })),
        ["schema.required names \"missing\" which is not in properties"]
    );
    assert_eq!(
        violations_of(json!({ "type": "object", "additionalProperties": "yes" })),
        ["schema.additionalProperties must be a boolean"]
    );
    // dsh's `properties: undefined`; JSON's nearest is `null`.
    assert_eq!(
        violations_of(json!({ "type": "object", "properties": null, "required": ["missing"] })),
        [
            "schema.properties must be an object of schemas",
            "schema.required names \"missing\" which is not in properties",
        ]
    );
}

// "requires type-correct scalar enum and const values"
#[test]
fn scalar_enums_and_consts_fit_their_type() {
    for schema in [
        json!({ "type": "string", "enum": ["a"], "const": "a" }),
        json!({ "type": "number", "enum": [1.5], "const": 1.5 }),
        json!({ "type": "integer", "enum": [1], "const": 1 }),
        json!({ "type": "boolean", "enum": [true], "const": true }),
        json!({ "type": "null", "enum": [null], "const": null }),
    ] {
        assert_eq!(check_schema(&schema), Ok(()), "{schema}");
    }
    for (schema, violation) in [
        (
            json!({ "type": "string", "enum": [] }),
            "schema.enum must be a non-empty array of string values",
        ),
        (
            json!({ "type": "number", "enum": ["1"] }),
            "schema.enum must be a non-empty array of number values",
        ),
        (
            json!({ "type": "integer", "enum": [1.5] }),
            "schema.enum must be a non-empty array of integer values",
        ),
        (
            json!({ "type": "boolean", "const": 1 }),
            "schema.const must be a boolean value",
        ),
        (
            json!({ "type": "string", "enum": null }),
            "schema.enum must be a non-empty array of string values",
        ),
        (
            json!({ "type": "string", "enum": ["a"], "const": "b" }),
            "schema.const must be one of schema.enum when both are declared",
        ),
    ] {
        assert_eq!(violations_of(schema), [violation]);
    }
    // `Number.isInteger(1.0)`.
    assert_eq!(
        check_schema(&json!({ "type": "integer", "enum": [1.0] })),
        Ok(())
    );
}

// "validates annotation types and lossless JSON payloads": the types.
#[test]
fn annotations_are_typed() {
    assert_eq!(
        violations_of(json!({ "description": 1 })),
        ["schema.description must be a string"]
    );
    assert_eq!(
        violations_of(json!({ "title": 1 })),
        ["schema.title must be a string"]
    );
}

// "rejects cyclic/exotic schema structure but permits sibling reuse": the
// sibling reuse.
#[test]
fn a_schema_may_repeat_a_node() {
    let leaf = json!({ "type": "string" });
    assert_eq!(
        check_schema(&json!({ "type": "object", "properties": { "a": leaf, "b": leaf } })),
        Ok(())
    );
}

// "uses own-property semantics for required declarations"
#[test]
fn required_names_must_be_declared_properties() {
    assert_eq!(
        violations_of(json!({ "type": "object", "properties": {}, "required": ["toString"] })),
        ["schema.required names \"toString\" which is not in properties"]
    );
}

// validateJsonSchemaValue

// "validates scalar, array, object, and null roots"
#[test]
fn each_root_validates() {
    for (schema, value) in [
        (json!({ "type": "string" }), json!("x")),
        (json!({ "type": "number" }), json!(1.5)),
        (json!({ "type": "integer" }), json!(2)),
        (json!({ "type": "integer" }), json!(2.0)),
        (json!({ "type": "boolean" }), json!(true)),
        (json!({ "type": "null" }), json!(null)),
        (
            json!({ "type": "array", "items": { "type": "string" } }),
            json!(["x"]),
        ),
        (json!({ "type": "object" }), json!({ "x": 1 })),
    ] {
        assert_eq!(
            check(schema.clone(), value),
            Vec::<String>::new(),
            "{schema}"
        );
    }
}

// "rejects wrong scalar types and lossy numbers": the types.
#[test]
fn wrong_scalar_types_are_named() {
    for (schema, value, violation) in [
        (
            json!({ "type": "string" }),
            json!(1),
            "\"value\" must be a string",
        ),
        (
            json!({ "type": "number" }),
            json!("1"),
            "\"value\" must be a number",
        ),
        (
            json!({ "type": "integer" }),
            json!(1.5),
            "\"value\" must be an integer",
        ),
        (
            json!({ "type": "boolean" }),
            json!("true"),
            "\"value\" must be a boolean",
        ),
        (
            json!({ "type": "null" }),
            json!(0),
            "\"value\" must be null",
        ),
    ] {
        assert_eq!(check(schema, value), [violation]);
    }
}

// "enforces scalar enum and const together"
#[test]
fn enum_then_const() {
    let schema = json!({ "type": "string", "enum": ["a", "b"], "const": "a" });
    assert_eq!(check(schema.clone(), json!("a")), Vec::<String>::new());
    assert_eq!(
        check(schema.clone(), json!("c")),
        ["\"value\" must be one of [\"a\",\"b\"]"]
    );
    assert_eq!(check(schema, json!("b")), ["\"value\" must be \"a\""]);
}

// "validates object requiredness, nested values, and raw open defaults"
#[test]
fn objects_check_required_nested_and_closed_keys() {
    let open = json!({
        "type": "object",
        "properties": {
            "file": { "type": "string" },
            "nested": {
                "type": "object",
                "properties": { "line": { "type": "integer" } },
                "required": ["line"],
                "additionalProperties": false,
            },
        },
        "required": ["file"],
    });
    assert_eq!(
        check(
            open.clone(),
            json!({ "file": "a", "extra": [1], "nested": { "line": 2 } })
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        check(open.clone(), json!({ "nested": { "line": 1 } })),
        ["missing required property \"value.file\""]
    );
    assert_eq!(
        check(open.clone(), json!({ "file": 1, "nested": {} })),
        [
            "\"value.file\" must be a string",
            "missing required property \"value.nested.line\""
        ]
    );
    assert_eq!(
        check(
            open.clone(),
            json!({ "file": "a", "nested": { "line": 1, "extra": true } })
        ),
        ["\"value.nested.extra\" is not a declared property (additionalProperties: false)"]
    );
    assert_eq!(check(open, json!("x")), ["\"value\" must be an object"]);
}

// "validates dense arrays per index and rejects lossy arrays": per index.
#[test]
fn arrays_check_each_item() {
    let schema = json!({ "type": "array", "items": { "type": "integer" } });
    assert_eq!(check(schema.clone(), json!([1, 2])), Vec::<String>::new());
    assert_eq!(
        check(schema.clone(), json!([1, 1.5])),
        ["\"value[1]\" must be an integer"]
    );
    assert_eq!(check(schema, json!("x")), ["\"value\" must be an array"]);
}

// "validates exact-one oneOf semantics, including overlap"
#[test]
fn one_of_takes_exactly_one_branch() {
    let disjoint = json!({ "oneOf": [{ "type": "string" }, { "type": "number" }] });
    assert_eq!(check(disjoint.clone(), json!("x")), Vec::<String>::new());
    assert_eq!(
        check(disjoint, json!(null)),
        ["\"value\" must match exactly one oneOf branch (matched 0)"]
    );
    let overlap = json!({ "oneOf": [{ "type": "number" }, { "type": "integer" }] });
    assert_eq!(
        check(overlap.clone(), json!(1)),
        ["\"value\" must match exactly one oneOf branch (matched 2)"]
    );
    assert_eq!(check(overlap, json!(1.5)), Vec::<String>::new());
}

// "an unconstrained schema accepts only lossless JSON values": every JSON
// value is.
#[test]
fn an_unconstrained_schema_takes_any_json() {
    for value in [
        json!(null),
        json!(true),
        json!(1),
        json!("x"),
        json!([1]),
        json!({ "x": null }),
    ] {
        assert_eq!(
            check(json!({}), value.clone()),
            Vec::<String>::new(),
            "{value}"
        );
    }
}

// "uses own properties for requiredness, recursion, and closed-object
// checks"
#[test]
fn own_properties_only() {
    assert_eq!(
        check(
            json!({ "type": "object", "properties": { "toString": { "type": "string" } }, "required": ["toString"] }),
            json!({})
        ),
        ["missing required property \"value.toString\""]
    );
    assert_eq!(
        check(
            json!({ "type": "object", "additionalProperties": false }),
            json!({ "toString": 1 })
        ),
        ["\"value.toString\" is not a declared property (additionalProperties: false)"]
    );
    assert_eq!(
        check(
            json!({ "type": "object", "properties": { "constructor": { "type": "string" } } }),
            json!({})
        ),
        Vec::<String>::new()
    );
}

// validateArgs (tools.spec)

// "flags a missing required key"
#[test]
fn a_missing_required_argument_is_named_bare() {
    let schema = json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] });
    assert_eq!(
        args(schema.clone(), json!({})),
        ["missing required property \"path\""]
    );
    // "allows extra keys (no additionalProperties:false) and omitted
    // optionals"
    assert_eq!(
        args(schema, json!({ "path": "/tmp", "extra": 1 })),
        Vec::<String>::new()
    );
    // The root is `arguments`.
    assert_eq!(
        args(json!({ "type": "object" }), json!([])),
        ["\"arguments\" must be an object"]
    );
}

// "does not apply defaults (validation only)"
#[test]
fn defaults_are_annotations() {
    assert_eq!(
        args(
            json!({ "type": "object", "properties": { "limit": { "type": "number", "default": 25 } } }),
            json!({})
        ),
        Vec::<String>::new()
    );
}

// "type-checks primitives", and "checks enum membership", and "enforces
// type-correct scalar enum declarations"
#[test]
fn arguments_are_typed_and_enums_checked() {
    let schema = json!({ "type": "object", "properties": {
        "s": { "type": "string" }, "n": { "type": "number" }, "b": { "type": "boolean" },
        "color": { "type": "string", "enum": ["red", "green"] },
        "k": { "type": "number", "enum": [1, 2] },
    } });
    for (value, violation) in [
        (json!({ "s": 1 }), "\"s\" must be a string"),
        (json!({ "n": "x" }), "\"n\" must be a number"),
        (json!({ "b": "x" }), "\"b\" must be a boolean"),
        (
            json!({ "color": "blue" }),
            "\"color\" must be one of [\"red\",\"green\"]",
        ),
        (json!({ "k": 3 }), "\"k\" must be one of [1,2]"),
    ] {
        assert_eq!(args(schema.clone(), value), [violation]);
    }
    assert_eq!(
        args(schema, json!({ "color": "red", "k": 1.0 })),
        Vec::<String>::new()
    );
    assert!(check_schema(&json!({ "type": "number", "enum": ["1", "2"] })).is_err());
}

// "recurses into nested objects (and an object without properties only
// type-checks)"
#[test]
fn nested_objects_are_checked() {
    let schema = json!({
        "type": "object",
        "properties": {
            "config": {
                "type": "object",
                "additionalProperties": true,
                "properties": { "host": { "type": "string" }, "port": { "type": "number" } },
                "required": ["host"],
            },
            "bag": { "type": "object", "additionalProperties": true },
        },
        "required": ["config"],
    });
    assert_eq!(
        args(
            schema.clone(),
            json!({ "config": { "host": "h" }, "bag": { "anything": true } })
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        args(schema, json!({ "config": { "port": 9 }, "bag": 5 })),
        [
            "missing required property \"config.host\"",
            "\"bag\" must be an object"
        ]
    );
}

// "recurses into array items (and an array without items only
// type-checks)", and "validates arrays of objects element-wise"
#[test]
fn array_items_are_checked() {
    let schema = json!({ "type": "object", "properties": {
        "tags": { "type": "array", "items": { "type": "string" } },
        "raw": { "type": "array" },
        "servers": {
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": true,
                "properties": { "host": { "type": "string" } },
                "required": ["host"],
            },
        },
    } });
    assert_eq!(
        args(
            schema.clone(),
            json!({ "tags": ["a", "b"], "raw": [1, {}, "x"] })
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        args(schema.clone(), json!({ "tags": ["a", 2] })),
        ["\"tags[1]\" must be a string"]
    );
    assert_eq!(
        args(schema.clone(), json!({ "tags": "nope" })),
        ["\"tags\" must be an array"]
    );
    assert_eq!(
        args(schema, json!({ "servers": [{ "host": "a" }, {}] })),
        ["missing required property \"servers[1].host\""]
    );
}

// "ToolArgsError carries a stable code and the violation list": here, the
// schema error's text and code.
#[test]
fn the_schema_error_reads_as_dshs() {
    let error = check_schema(&json!({ "type": "weird" })).unwrap_err();
    assert_eq!(
        error.to_string(),
        "unsupported JSON schema: schema.type must be one of \
         object/array/string/number/integer/boolean/null"
    );
    assert_eq!(JsonSchemaError::CODE, "UNSUPPORTED_SCHEMA");
}
