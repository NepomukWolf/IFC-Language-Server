//! Loading boundary for official and custom EXPRESS schema text.
//! `load_express` converts EXPRESS text into a resolved `SchemaDoc`; the official schemas are
//! bundled into the binary at compile time and parsed from those embedded strings at startup.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;

use crate::schema::parser::parse_express_source;
use crate::schema::resolver::resolve_schema;
use crate::schema::{IfcVersion, SchemaDoc};

#[derive(Debug)]
pub enum LoadExpressError {
    Parse(String),
    MissingSchema,
}

impl fmt::Display for LoadExpressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "failed to parse EXPRESS schema: {message}"),
            Self::MissingSchema => write!(f, "EXPRESS input does not contain a schema"),
        }
    }
}

impl Error for LoadExpressError {}

#[derive(Debug)]
pub(crate) enum SchemaLoadError {
    Parse(LoadExpressError),
}

impl fmt::Display for SchemaLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "{error}"),
        }
    }
}

impl Error for SchemaLoadError {}

pub fn load_express(version: IfcVersion, source: &str) -> Result<SchemaDoc, LoadExpressError> {
    load_express_for_schema_name(version.schema_name(), source)
}

fn load_express_for_schema_name(
    schema_name: &str,
    source: &str,
) -> Result<SchemaDoc, LoadExpressError> {
    let raw = parse_express_source(source)?;
    Ok(resolve_schema(
        IfcVersion::from_schema_name(schema_name),
        raw,
    ))
}

pub(crate) fn load_official_schema(version: IfcVersion) -> Result<SchemaDoc, SchemaLoadError> {
    load_express(version, official_express_source(version)).map_err(SchemaLoadError::Parse)
}

#[derive(Debug)]
pub enum LocalSchemaError {
    Read {
        path: String,
        message: String,
    },
    MissingSchemaName {
        path: String,
    },
    Parse {
        path: String,
        error: LoadExpressError,
    },
}

impl fmt::Display for LocalSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, message } => {
                write!(f, "failed to read local EXPRESS schema `{path}`: {message}")
            }
            Self::MissingSchemaName { path } => {
                write!(
                    f,
                    "local EXPRESS schema `{path}` does not declare `SCHEMA ...;`"
                )
            }
            Self::Parse { path, error } => {
                write!(f, "failed to parse local EXPRESS schema `{path}`: {error}")
            }
        }
    }
}

impl Error for LocalSchemaError {}

pub fn inspect_local_schema_name(path: &Path) -> Result<String, LocalSchemaError> {
    let source = read_local_schema_source(path)?;
    extract_declared_schema_name(&source).ok_or_else(|| LocalSchemaError::MissingSchemaName {
        path: path.display().to_string(),
    })
}

pub fn load_local_schema(path: &Path) -> Result<(String, SchemaDoc), LocalSchemaError> {
    let source = read_local_schema_source(path)?;
    let schema_name = extract_declared_schema_name(&source).ok_or_else(|| {
        LocalSchemaError::MissingSchemaName {
            path: path.display().to_string(),
        }
    })?;
    let schema = load_express_for_schema_name(&schema_name, &source).map_err(|error| {
        LocalSchemaError::Parse {
            path: path.display().to_string(),
            error,
        }
    })?;

    Ok((schema_name, schema))
}

fn official_express_source(version: IfcVersion) -> &'static str {
    match version {
        IfcVersion::Ifc2x3Tc1 => include_str!(concat!(env!("OUT_DIR"), "/express/ifc2x3_tc1.exp")),
        IfcVersion::Ifc4Add2Tc1 => {
            include_str!(concat!(env!("OUT_DIR"), "/express/ifc4_add2_tc1.exp"))
        }
        IfcVersion::Ifc4x3Add2 => {
            include_str!(concat!(env!("OUT_DIR"), "/express/ifc4x3_add2.exp"))
        }
    }
}

fn read_local_schema_source(path: &Path) -> Result<String, LocalSchemaError> {
    fs::read_to_string(path).map_err(|error| LocalSchemaError::Read {
        path: path.display().to_string(),
        message: error.to_string(),
    })
}

fn extract_declared_schema_name(source: &str) -> Option<String> {
    let mut tokens = source.split_whitespace();

    while let Some(token) = tokens.next() {
        if token.eq_ignore_ascii_case("SCHEMA") {
            let schema_name = tokens.next()?;
            return Some(schema_name.trim_end_matches(';').to_ascii_uppercase());
        }
    }

    None
}
