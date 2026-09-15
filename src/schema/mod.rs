//! Runtime EXPRESS loading and schema lookup.
//! Schema docs are generated in memory from official EXPRESS definitions bundled into the binary.
//! Submodules keep version metadata, loading, parsing, resolution, and data types separate.

mod collection;
mod loader;
mod parser;
mod resolver;
mod types;
mod version;

pub use collection::SchemaDocCollection;

pub use loader::{LoadExpressError, inspect_local_schema_name, load_local_schema};
// `load_express` is only reached directly from tests; production code goes through
// `load_official_schema`/`load_local_schema`.
#[cfg(test)]
pub use loader::load_express;
pub use types::{
    AggregateBounds, AggregateKind, AggregateTypeRef, AliasTypeDef, BoundValue, EntityAttributeDoc,
    EntityDoc, EnumerationTypeDef, NamedTypeKind, NamedTypeRef, PrimitiveType, SchemaDoc,
    SelectTypeDef, TypeDoc, TypeRef, normalize_name,
};
pub use version::IfcVersion;

//*----- TESTS BEGIN HERE -----*
#[cfg(test)]
mod tests {
    use super::*;

    /// Fake schema for testing purposes.
    fn fixture_schema() -> &'static str {
        r#"
        SCHEMA DEMO;
          TYPE IfcLabel = STRING(255);
          END_TYPE;

          TYPE IfcWallTypeEnum = ENUMERATION OF (MOVABLE, USERDEFINED);
          END_TYPE;

          TYPE IfcObjectReferenceSelect = SELECT (IfcRoot, IfcLabel);
          END_TYPE;

          ENTITY IfcRoot;
            GlobalId : IfcLabel;
          END_ENTITY;

          ENTITY IfcElement
            SUBTYPE OF (IfcRoot);
            Tag : OPTIONAL IfcLabel;
          END_ENTITY;

          ENTITY IfcWall
            SUBTYPE OF (IfcElement);
            PredefinedType : OPTIONAL IfcWallTypeEnum;
            RelatedObjects : SET [1:?] OF IfcRoot;
          END_ENTITY;
        END_SCHEMA;
        "#
    }

    /// Test that the schema doc is built correctly from the fixture schema.
    #[test]
    fn load_express_builds_schema_doc_from_fixture() {
        let schema =
            load_express(IfcVersion::Ifc4Add2Tc1, fixture_schema()).expect("schema should parse");
        let wall = schema.entity("IFCWALL").expect("IfcWall should resolve");

        assert_eq!(wall.name, "IfcWall");
        assert_eq!(wall.attributes.len(), 4);
        assert_eq!(wall.attributes[0].name, "GlobalId");
        assert_eq!(wall.attributes[0].declared_in, "IfcRoot");
        assert_eq!(wall.attributes[2].type_name, "OPTIONAL IfcWallTypeEnum");
        assert_eq!(wall.attributes[3].type_name, "SET [1:?] OF IfcRoot");
        assert!(wall.url.contains("ifcwall.htm"));

        let TypeDoc::Select(select) = schema
            .type_decl("IfcObjectReferenceSelect")
            .expect("select type should resolve")
        else {
            panic!("expected select type");
        };
        assert_eq!(select.options.len(), 2);
    }

    /// Test that the schema doc knows subtype compatibility.
    /// E.g., `IFCWALL` is a subtype of `IFCROOT` and `IFCELEMENT`.
    #[test]
    fn schema_doc_knows_subtype_compatibility() {
        let schema =
            load_express(IfcVersion::Ifc4Add2Tc1, fixture_schema()).expect("schema should parse");

        assert!(schema.is_entity_compatible("IFCWALL", "IFCROOT"));
        assert!(schema.is_entity_compatible("IFCWALL", "IFCELEMENT"));
        assert!(!schema.is_entity_compatible("IFCROOT", "IFCWALL"));
    }

    /// Test that the schema doc preserves derived attribute overrides.
    #[test]
    fn load_express_preserves_derived_attribute_overrides() {
        let source = r#"
        SCHEMA DEMO;
          TYPE IfcUnitEnum = ENUMERATION OF (LENGTHUNIT);
          END_TYPE;
          TYPE IfcSIPrefix = ENUMERATION OF (MILLI);
          END_TYPE;
          TYPE IfcSIUnitName = ENUMERATION OF (METRE);
          END_TYPE;
          ENTITY IfcDimensionalExponents;
          END_ENTITY;
          ENTITY IfcNamedUnit;
            Dimensions : IfcDimensionalExponents;
            UnitType : IfcUnitEnum;
          END_ENTITY;
          ENTITY IfcSIUnit
            SUBTYPE OF (IfcNamedUnit);
            Prefix : OPTIONAL IfcSIPrefix;
            Name : IfcSIUnitName;
          DERIVE
            SELF\IfcNamedUnit.Dimensions : IfcDimensionalExponents := ?;
          END_ENTITY;
        END_SCHEMA;
        "#;

        let schema = load_express(IfcVersion::Ifc4Add2Tc1, source).expect("schema should parse");
        let unit = schema
            .entity("IFCSIUNIT")
            .expect("IfcSIUnit should resolve");
        let dimensions = unit
            .attributes
            .iter()
            .find(|attribute| attribute.name == "Dimensions")
            .expect("Dimensions should be inherited");

        assert!(dimensions.allows_omitted);
    }

    /// Test that the schema doc sanitizes global algorithm blocks.
    /// While algorithm blocks may be included in the schema, they are not supported.
    /// Yet, these schemas should still be read and parsed successfully.
    #[test]
    fn sanitize_drops_global_algorithm_blocks() {
        let source = r#"
        SCHEMA DEMO;
          TYPE IfcLabel = STRING;
          END_TYPE;
          FUNCTION UnsupportedFunction(Value : IfcLabel) : BOOLEAN;
            RETURN(TRUE);
          END_FUNCTION;
          RULE UnsupportedRule FOR (IfcRoot);
            WHERE WR1 : TRUE;
          END_RULE;
          ENTITY IfcRoot;
            GlobalId : IfcLabel;
          END_ENTITY;
        END_SCHEMA;
        "#;

        let schema = load_express(IfcVersion::Ifc4Add2Tc1, source).expect("schema should parse");

        assert!(schema.entity("IfcRoot").is_some());
    }

    /// Test that the schema doc loads bundled official schemas successfully.
    #[test]
    fn loads_bundled_official_schemas() {
        let collection = SchemaDocCollection::new();

        assert!(
            collection.load_errors().is_empty(),
            "{:?}",
            collection.load_errors()
        );
        for version in IfcVersion::supported() {
            assert!(
                collection
                    .get(version.schema_name())
                    .and_then(|schema| schema.entity("IFCWALL"))
                    .is_some(),
                "missing IfcWall for {version}"
            );
        }
    }
}
