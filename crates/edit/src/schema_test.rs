use std::sync::Arc;

use crate::syntax::PathSeg;

use crate::schema::*;

fn index() -> SchemaIndex {
    let mut index = SchemaIndex::new();
    index
        .add_openapi_document(fixtures::APPS_V1_DOC)
        .expect("the fixture parses");
    index
        .add_crd_list(fixtures::CRD_LIST)
        .expect("the fixture parses");
    index
}

// The converted `spec` of a one-version CRD, so a conversion question can
// be asked of one fixed path.
fn crd_spec(spec_schema: &str) -> Arc<SchemaNode> {
    let mut index = SchemaIndex::new();
    index
        .add_crd_list(&format!(
            r#"{{"items":[{{"spec":{{
                "group":"example.com",
                "names":{{"kind":"Probe"}},
                "versions":[{{"name":"v1","served":true,"schema":{{"openAPIV3Schema":{{
                    "type":"object",
                    "properties":{{"spec":{spec_schema}}}
                }}}}}}]
            }}}}]}}"#
        ))
        .expect("the fixture parses");
    let root = index
        .resolve_gvk("example.com/v1", "Probe")
        .expect("the version indexes");
    let Shape::Object { properties, .. } = &index.deref(&root).shape else {
        panic!("the CRD root is an object");
    };
    properties.get("spec").cloned().expect("spec is declared")
}

fn path(segments: &[&str]) -> Vec<PathSeg> {
    segments
        .iter()
        .map(|segment| match segment.strip_prefix('[') {
            Some(rest) => PathSeg::Index(
                rest.trim_end_matches(']')
                    .parse()
                    .expect("test indices are numbers"),
            ),
            None => PathSeg::Key((*segment).to_string()),
        })
        .collect()
}

#[test]
fn a_gvk_resolves_through_the_annotation_to_its_type() {
    let index = index();
    let deployment = index
        .resolve_gvk("apps/v1", "Deployment")
        .expect("the annotation maps apps/v1 Deployment");
    assert!(deployment.description.starts_with("Deployment enables"));
}

#[test]
fn a_deep_path_crosses_refs_arrays_and_enums() {
    let index = index();
    let root = index.resolve_gvk("apps/v1", "Deployment").expect("fixture");
    let policy = index
        .lookup(
            &root,
            &path(&[
                "spec",
                "template",
                "spec",
                "containers",
                "[0]",
                "imagePullPolicy",
            ]),
        )
        .expect("the path resolves across four refs and an array");
    let Shape::Scalar { kind, values } = &policy.shape else {
        panic!("imagePullPolicy is an enum scalar, got {policy:?}");
    };
    assert_eq!(*kind, ScalarKind::Str);
    assert_eq!(values, &["Always", "Never", "IfNotPresent"]);
}

#[test]
fn all_of_wrapped_refs_keep_the_outer_description() {
    let index = index();
    let root = index.resolve_gvk("apps/v1", "Deployment").expect("fixture");
    let Shape::Object { properties, .. } = &index.deref(&root).shape else {
        panic!("a Deployment is an object");
    };
    let spec = properties.get("spec").expect("spec exists");
    assert!(
        spec.description.starts_with("Specification of the desired"),
        "the allOf wrapper's description survives: {:?}",
        spec.description
    );
    let resolved = index.deref(spec);
    assert!(matches!(resolved.shape, Shape::Object { .. }));
    assert!(
        resolved.description.starts_with("Specification"),
        "deref carries the wrapper description onto the bare target"
    );
}

#[test]
fn additional_properties_answer_arbitrary_label_keys() {
    let index = index();
    let root = index.resolve_gvk("apps/v1", "Deployment").expect("fixture");
    let label = index
        .lookup(&root, &path(&["metadata", "labels", "app"]))
        .expect("labels take arbitrary keys");
    assert!(matches!(
        label.shape,
        Shape::Scalar {
            kind: ScalarKind::Str,
            ..
        }
    ));
}

#[test]
fn a_missing_property_is_none_not_any() {
    let index = index();
    let root = index.resolve_gvk("apps/v1", "Deployment").expect("fixture");
    assert_eq!(index.lookup(&root, &path(&["spec", "replicaCount"])), None);
}

#[test]
fn a_served_crd_version_indexes_and_an_unserved_one_does_not() {
    let index = index();
    let widget = index
        .resolve_gvk("example.com/v1", "Widget")
        .expect("the served version indexes");
    assert!(
        index
            .resolve_gvk("example.com/v2alpha1", "Widget")
            .is_none()
    );
    let size = index
        .lookup(&widget, &path(&["spec", "size"]))
        .expect("the structural schema resolves");
    assert!(size.description.starts_with("How many"));
    let Shape::Object { properties, .. } = &index.deref(&widget).shape else {
        panic!("the CRD root is an object");
    };
    assert!(
        properties.contains_key("apiVersion") && properties.contains_key("metadata"),
        "the CRD root is augmented with the implicit object fields"
    );
}

#[test]
fn kinds_and_api_versions_serve_the_completion_lists() {
    let mut index = index();
    index.add_api_version("v1");
    index.add_api_version("batch/v1");
    assert_eq!(index.kinds_for("apps/v1"), ["Deployment"]);
    assert_eq!(index.kinds_for("example.com/v1"), ["Widget"]);
    let versions: Vec<&str> = index.api_versions().collect();
    assert!(versions.contains(&"apps/v1"));
    assert!(versions.contains(&"example.com/v1"));
    assert!(versions.contains(&"batch/v1"));
}

#[test]
fn an_unresolvable_ref_degrades_to_any_never_a_panic() {
    let mut index = SchemaIndex::new();
    index
        .add_openapi_document(
            r##"{"components":{"schemas":{
                "a.b.Loop": {"type":"object","properties":{"next":{"$ref":"#/components/schemas/a.b.Missing"}}}
            }}}"##,
        )
        .expect("parses");
    let root = index.types.get("a.b.Loop").cloned().expect("indexed");
    let next = index
        .lookup(&root, &path(&["next", "anything", "deeper"]))
        .expect("Any absorbs any deeper path");
    assert_eq!(next.shape, Shape::Any);
}

#[test]
fn recursive_schemas_stay_walkable_under_the_hop_bound() {
    let mut index = SchemaIndex::new();
    index
        .add_openapi_document(
            r##"{"components":{"schemas":{
                "a.b.Node": {"type":"object","properties":{"child":{"$ref":"#/components/schemas/a.b.Node"}}}
            }}}"##,
        )
        .expect("parses");
    let root = index.types.get("a.b.Node").cloned().expect("indexed");
    let deep = index.lookup(&root, &path(&["child", "child", "child", "child", "child"]));
    assert!(deep.is_some(), "recursion resolves level by level");
}

#[test]
fn malformed_documents_are_labelled_errors() {
    let mut index = SchemaIndex::new();
    assert!(index.add_openapi_document("not json").is_err());
    assert!(
        index
            .add_openapi_document(r#"{"openapi":"3.0.0"}"#)
            .is_err()
    );
    assert!(index.add_crd_list(r#"{"kind":"List"}"#).is_err());
    assert!(index.is_empty());
}

#[test]
fn descriptions_are_capped_as_untrusted_display_text() {
    let mut index = SchemaIndex::new();
    let long = "x".repeat(10_000);
    index
        .add_openapi_document(&format!(
            r#"{{"components":{{"schemas":{{
                "a.b.C": {{"type":"string","description":"{long}"}}
            }}}}}}"#
        ))
        .expect("parses");
    let node = index.types.get("a.b.C").expect("indexed");
    assert!(node.description.chars().count() <= MAX_DESCRIPTION_CHARS + 1);
}

#[test]
fn an_all_of_with_an_unmergeable_member_stops_claiming_to_be_closed() {
    // A `$ref` member resolves by name at walk time, so the merged property
    // table holds only the inline members' fields. Claiming to be closed
    // from that table named every inherited field an unknown one.
    let spec = crd_spec(
        r##"{"allOf":[
            {"$ref":"#/definitions/Base"},
            {"type":"object","properties":{"extra":{"type":"string"}}}
        ]}"##,
    );
    let Shape::Object {
        properties,
        additional,
        ..
    } = &spec.shape
    else {
        panic!("an allOf with an object member merges to an object, got {spec:?}");
    };
    assert!(
        properties.contains_key("extra"),
        "the inline member's properties still serve completion"
    );
    assert_eq!(
        *additional,
        Additional::Any,
        "and the incomplete table reports nothing rather than strangers"
    );
}

#[test]
fn preserve_unknown_fields_opens_the_object_that_marks_itself() {
    let spec = crd_spec(
        r#"{"type":"object","x-kubernetes-preserve-unknown-fields":true,
            "properties":{"size":{"type":"integer"}}}"#,
    );
    let Shape::Object {
        properties,
        additional,
        ..
    } = &spec.shape
    else {
        panic!("the marked schema is still an object, got {spec:?}");
    };
    assert!(properties.contains_key("size"), "named properties survive");
    assert_eq!(
        *additional,
        Additional::Any,
        "the apiserver's own marker turns pruning off, so extras belong"
    );
}

#[test]
fn the_most_permissive_all_of_member_decides_in_either_order() {
    let additional_of = |spec: &str| {
        let node = crd_spec(spec);
        let Shape::Object { additional, .. } = &node.shape else {
            panic!("an allOf of objects merges to an object, got {node:?}");
        };
        additional.clone()
    };
    let closed =
        r#"{"type":"object","properties":{"a":{"type":"string"}},"additionalProperties":false}"#;
    let open =
        r#"{"type":"object","properties":{"b":{"type":"string"}},"additionalProperties":true}"#;
    let silent = r#"{"type":"object","properties":{"c":{"type":"string"}}}"#;
    for (first, second) in [(closed, open), (open, closed)] {
        assert_eq!(
            additional_of(&format!(r#"{{"allOf":[{first},{second}]}}"#)),
            Additional::Any,
            "the open member wins whichever order it arrives in"
        );
    }
    for (first, second) in [(closed, silent), (silent, closed)] {
        assert_eq!(
            additional_of(&format!(r#"{{"allOf":[{first},{second}]}}"#)),
            Additional::Deny,
            "and a stated closure outranks silence, which states nothing"
        );
    }
}

#[test]
fn a_type_written_as_a_list_still_states_a_shape() {
    // Hand-written CRDs spell an optional scalar the JSON Schema way. Reading
    // the list for nullability but not for the type left the field indexed
    // and unchecked: every value passed, and nothing completed.
    let spec = crd_spec(
        r#"{"type":"object","properties":{
            "mode":{"type":["string","null"],"enum":["tcp","udp"]},
            "port":{"type":["integer"]}}}"#,
    );
    let Shape::Object { properties, .. } = &spec.shape else {
        panic!("spec is an object, got {spec:?}");
    };
    let mode = properties.get("mode").expect("mode is declared");
    assert!(mode.nullable, "the list's `null` member is the nullability");
    let Shape::Scalar { kind, values } = &mode.shape else {
        panic!("mode is a string, got {mode:?}");
    };
    assert_eq!(*kind, ScalarKind::Str);
    assert_eq!(values, &["tcp".to_string(), "udp".to_string()]);
    let port = properties.get("port").expect("port is declared");
    assert!(!port.nullable, "a list without `null` is not nullable");
    assert!(
        matches!(
            port.shape,
            Shape::Scalar {
                kind: ScalarKind::Integer,
                ..
            }
        ),
        "a single-member list is still a type, got {port:?}"
    );
}

#[test]
fn a_field_two_all_of_members_require_is_named_once() {
    let spec = crd_spec(
        r#"{"allOf":[
            {"type":"object","properties":{"name":{"type":"string"}},"required":["name"]},
            {"type":"object","properties":{"port":{"type":"integer"}},
             "required":["name","port"]}]}"#,
    );
    let Shape::Object { required, .. } = &spec.shape else {
        panic!("the merge is an object, got {spec:?}");
    };
    assert_eq!(
        required,
        &["name".to_string(), "port".to_string()],
        "one requirement, not one diagnostic per member that states it"
    );
}

#[test]
fn nullable_survives_a_ref_hop_the_crd_root_and_array_items() {
    let mut index = SchemaIndex::new();
    for (name, nullable) in [("Plain", false), ("Nullable", true)] {
        index.types.insert(
            name.to_string(),
            Arc::new(SchemaNode {
                description: String::new(),
                shape: Shape::Scalar {
                    kind: ScalarKind::Str,
                    values: Vec::new(),
                },
                nullable,
            }),
        );
    }
    let reference = |name: &str, nullable: bool| {
        Arc::new(SchemaNode {
            description: String::new(),
            shape: Shape::Reference(name.to_string()),
            nullable,
        })
    };
    assert!(
        index.deref(&reference("Plain", true)).nullable,
        "a nullable `$ref` keeps it across the hop"
    );
    assert!(
        index.deref(&reference("Nullable", false)).nullable,
        "and the target's own declaration is not dropped either"
    );

    let spec = crd_spec(
        r#"{"type":"object","nullable":true,"properties":{
            "hosts":{"type":"array","items":{"type":"string","nullable":true}},
            "ports":{"type":"array","items":{"type":"integer"}}}}"#,
    );
    // `spec` is read back out of the augmented root, so its flag surviving
    // is what says the root augmentation rebuilt the object without
    // dropping what its properties declared.
    assert!(spec.nullable, "the object's own declaration survives");
    let Shape::Object { properties, .. } = &spec.shape else {
        panic!("spec is an object, got {spec:?}");
    };
    for (key, expected) in [("hosts", true), ("ports", false)] {
        let node = properties.get(key).expect("the array is declared");
        let Shape::Array { items: Some(items) } = &node.shape else {
            panic!("{key} is an array with items, got {node:?}");
        };
        assert_eq!(
            items.nullable, expected,
            "an array's items carry their own declaration, not the array's"
        );
    }
}
