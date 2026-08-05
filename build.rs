use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

struct SchemaSource {
    file_name: &'static str,
    urls: &'static [&'static str],
    expected_sha256: &'static str,
}

const SCHEMAS: &[SchemaSource] = &[
    SchemaSource {
        file_name: "ifc2x3_tc1.exp",
        urls: &[
            "https://standards.buildingsmart.org/IFC/RELEASE/IFC2x3/TC1/EXPRESS/IFC2X3_TC1.exp",
            "https://raw.githubusercontent.com/buildingSMART/IFC4.x-development/master/reference_schemas/IFC2X3_TC1.exp",
        ],
        expected_sha256: "e18a1b2c3e29f5256904c83378ccad0850f52287a8d0122d149aba4a417fe5e5",
    },
    SchemaSource {
        file_name: "ifc4_add2_tc1.exp",
        urls: &[
            "https://standards.buildingsmart.org/IFC/RELEASE/IFC4/ADD2_TC1/EXPRESS/IFC4.exp",
            "https://raw.githubusercontent.com/buildingSMART/IFC4.x-development/master/reference_schemas/IFC4_ADD2_TC1.exp",
        ],
        expected_sha256: "a2704ba20a1b3d0b7d9b61d6fd37d0baa3b4996ba3e90d968a1d2ca2819d1046",
    },
    SchemaSource {
        file_name: "ifc4x3_add2.exp",
        urls: &[
            "https://standards.buildingsmart.org/IFC/RELEASE/IFC4_3/HTML/IFC4X3_ADD2.exp",
            "https://raw.githubusercontent.com/Autodesk/revit-ifc/master/Source/RevitIFCTools/IFC4X3_ADD2.exp",
        ],
        expected_sha256: "f67c8762b13a099c28082061e6f16b9ef1284ceec34069792afc702725675860",
    },
];

const IFC2X3_ENTITY_INDEX_URL: &str = "https://standards.buildingsmart.org/IFC/RELEASE/IFC2x3/TC1/HTML/alphabeticalorder_entities.htm";
const IFC2X3_ENTITY_METADATA_URL: &str = "https://raw.githubusercontent.com/IfcOpenShell/IfcOpenShell/master/src/ifcopenshell-python/ifcopenshell/util/schema/ifc2x3_entities.json";
const IFC2X3_DOC_BASE_URL: &str =
    "https://standards.buildingsmart.org/IFC/RELEASE/IFC2x3/TC1/HTML/";
const IFC2X3_ENTITY_COUNT: usize = 653;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let express_dir = out_dir.join("express");
    fs::create_dir_all(&express_dir)?;

    for schema in SCHEMAS {
        let source = fetch_schema(schema)?;
        fs::write(express_dir.join(schema.file_name), source)?;
    }

    let entity_index = fetch_url(IFC2X3_ENTITY_INDEX_URL);
    let links = match entity_index {
        Ok(entity_index) => {
            let entity_index = String::from_utf8(entity_index)?;
            parse_ifc2x3_entity_links(&entity_index)?
        }
        Err(_) => {
            let entity_metadata = String::from_utf8(fetch_url(IFC2X3_ENTITY_METADATA_URL)?)?;
            parse_ifc2x3_entity_metadata_links(&entity_metadata)?
        }
    };

    fs::write(
        out_dir.join("ifc2x3_entity_doc_links.rs"),
        render_ifc2x3_entity_links(&links),
    )?;

    Ok(())
}

fn fetch_schema(schema: &SchemaSource) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut errors = Vec::new();

    for url in schema.urls {
        match fetch_url(url) {
            Ok(source) => {
                let actual_sha256 = format!("{:x}", Sha256::digest(&source));

                if actual_sha256 == schema.expected_sha256 {
                    return Ok(source);
                }

                errors.push(format!(
                    "{url}: checksum mismatch: expected {}, got {actual_sha256}",
                    schema.expected_sha256
                ));
            }
            Err(error) => errors.push(format!("{url}: {error}")),
        }
    }

    Err(format!(
        "failed to fetch a valid {}:\n{}",
        schema.file_name,
        errors.join("\n")
    )
    .into())
}

fn fetch_url(url: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    Ok(ureq::get(url).call()?.body_mut().read_to_vec()?)
}

fn parse_ifc2x3_entity_links(html: &str) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let mut links = Vec::new();

    for line in html.lines() {
        let Some((_, after_prefix)) = line.split_once("<A HREF=\"") else {
            continue;
        };
        let Some((href, after_href)) = after_prefix.split_once('"') else {
            return Err(format!("malformed IFC2x3 entity link: {line}").into());
        };
        let Some((_, after_tag)) = after_href.split_once('>') else {
            return Err(format!("malformed IFC2x3 entity link: {line}").into());
        };
        let Some((entity_name, _)) = after_tag.split_once("</A>") else {
            return Err(format!("malformed IFC2x3 entity link: {line}").into());
        };

        if entity_name.starts_with("Ifc") && href.ends_with(".htm") {
            links.push((entity_name.to_ascii_uppercase(), href.to_string()));
        }
    }

    if links.len() != IFC2X3_ENTITY_COUNT {
        return Err(format!(
            "expected {IFC2X3_ENTITY_COUNT} IFC2x3 entity links, found {}",
            links.len()
        )
        .into());
    }

    Ok(links)
}

fn parse_ifc2x3_entity_metadata_links(
    metadata: &str,
) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let mut links = Vec::new();
    let mut entity_name = None;

    for line in metadata.lines() {
        let trimmed = line.trim();

        if line.starts_with("    \"")
            && !line.starts_with("        ")
            && let Some(name) = parse_json_object_key(trimmed)
        {
            entity_name = Some(name);
            continue;
        }

        if !trimmed.starts_with("\"spec_url\"") {
            continue;
        }

        let Some(entity_name) = entity_name.take() else {
            return Err(format!("IFC2x3 metadata spec_url without entity: {line}").into());
        };
        let Some(url) = parse_json_string_value(trimmed) else {
            return Err(format!("malformed IFC2x3 metadata spec_url: {line}").into());
        };
        let Some(href) = url.strip_prefix(IFC2X3_DOC_BASE_URL) else {
            return Err(format!("unexpected IFC2x3 metadata spec_url: {url}").into());
        };

        links.push((entity_name.to_ascii_uppercase(), href.to_string()));
    }

    if links.len() != IFC2X3_ENTITY_COUNT {
        return Err(format!(
            "expected {IFC2X3_ENTITY_COUNT} IFC2x3 entity links, found {}",
            links.len()
        )
        .into());
    }

    Ok(links)
}

fn parse_json_object_key(line: &str) -> Option<String> {
    let (key, rest) = parse_json_string(line)?;
    (rest == ": {").then_some(key)
}

fn parse_json_string_value(line: &str) -> Option<String> {
    let (_, rest) = line.split_once(':')?;
    let (value, _) = parse_json_string(rest.trim())?;
    Some(value)
}

fn parse_json_string(line: &str) -> Option<(String, &str)> {
    let after_open_quote = line.strip_prefix('"')?;
    let closing_quote = after_open_quote.find('"')?;
    let value = after_open_quote[..closing_quote].to_string();
    let rest = &after_open_quote[closing_quote + 1..];

    Some((value, rest))
}

fn render_ifc2x3_entity_links(links: &[(String, String)]) -> String {
    let mut output = String::from("const IFC2X3_ENTITY_DOC_LINKS: &[(&str, &str)] = &[\n");

    for (entity_name, href) in links {
        output.push_str(&format!("    (\"{entity_name}\", \"{href}\"),\n"));
    }

    output.push_str("];\n");
    output
}
