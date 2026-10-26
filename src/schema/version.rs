//! Supported IFC schema versions and version-specific metadata.
//! These mappings provide schema names and hover documentation links for the bundled official
//! EXPRESS definitions.

use std::fmt;

include!(concat!(env!("OUT_DIR"), "/ifc2x3_entity_doc_links.rs"));

const IFC2X3_DOC_BASE_URL: &str = "https://standards.buildingsmart.org/IFC/RELEASE/IFC2x3/TC1/HTML";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IfcVersion {
    Ifc2x3Tc1,
    Ifc4Add2Tc1,
    Ifc4x3Add2,
}

impl IfcVersion {
    pub const SUPPORTED_COUNT: usize = 3;

    pub fn supported() -> [Self; Self::SUPPORTED_COUNT] {
        [Self::Ifc2x3Tc1, Self::Ifc4Add2Tc1, Self::Ifc4x3Add2]
    }

    pub fn from_schema_name(schema_name: &str) -> Option<Self> {
        match schema_name {
            "IFC2X3" => Some(Self::Ifc2x3Tc1),
            "IFC4" => Some(Self::Ifc4Add2Tc1),
            "IFC4X3_ADD2" => Some(Self::Ifc4x3Add2),
            _ => None,
        }
    }

    pub fn schema_name(self) -> &'static str {
        match self {
            Self::Ifc2x3Tc1 => "IFC2X3",
            Self::Ifc4Add2Tc1 => "IFC4",
            Self::Ifc4x3Add2 => "IFC4X3_ADD2",
        }
    }

    pub(crate) fn documentation_url(self, entity_name: &str) -> String {
        match self {
            Self::Ifc2x3Tc1 => ifc2x3_documentation_url(entity_name),
            Self::Ifc4Add2Tc1 => format!(
                "https://standards.buildingsmart.org/IFC/RELEASE/IFC4/ADD2_TC1/HTML/link/{}.htm",
                entity_name.to_ascii_lowercase()
            ),
            Self::Ifc4x3Add2 => format!(
                "https://standards.buildingsmart.org/IFC/RELEASE/IFC4_3/HTML/lexical/{}.htm",
                entity_name
            ),
        }
    }
}

fn ifc2x3_documentation_url(entity_name: &str) -> String {
    let normalized_entity_name = entity_name.to_ascii_uppercase();
    let href = IFC2X3_ENTITY_DOC_LINKS
        .iter()
        .find_map(|(name, href)| (*name == normalized_entity_name.as_str()).then_some(*href))
        .unwrap_or("alphabeticalorder_entities.htm");

    format!("{IFC2X3_DOC_BASE_URL}/{href}")
}

impl fmt::Display for IfcVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.schema_name())
    }
}

//*----- TESTS BEGIN HERE -----*
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ifc2x3_documentation_url_uses_entity_page_from_index() {
        assert_eq!(
            IfcVersion::Ifc2x3Tc1.documentation_url("IfcWall"),
            "https://standards.buildingsmart.org/IFC/RELEASE/IFC2x3/TC1/HTML/ifcsharedbldgelements/lexical/ifcwall.htm"
        );
        assert_eq!(
            IfcVersion::Ifc2x3Tc1.documentation_url("IfcProject"),
            "https://standards.buildingsmart.org/IFC/RELEASE/IFC2x3/TC1/HTML/ifckernel/lexical/ifcproject.htm"
        );
        assert_eq!(
            IfcVersion::Ifc2x3Tc1.documentation_url("IfcTextureVertex"),
            "https://standards.buildingsmart.org/IFC/RELEASE/IFC2x3/TC1/HTML/ifcpresentationdefinitionresource/lexical/ifctexturevertex.htm"
        );
    }
}
