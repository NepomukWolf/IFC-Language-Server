//! In-process LSP session over `Backend` and a generated property-heavy model.

use std::path::Path;

use futures::StreamExt;
use ifc_language_server::Backend;
use serde_json::json;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tower::{Service, ServiceExt};
use tower_lsp::jsonrpc::Request;
use tower_lsp::lsp_types::*;
use tower_lsp::{LanguageServer, LspService};

pub struct Model {
    pub name: String,
    pub text: String,
}

impl Model {
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read model {}: {error}", path.display()));
        let name = path
            .file_name()
            .expect("model path has a file name")
            .to_string_lossy()
            .into_owned();
        Self { name, text }
    }

    pub fn synthetic_properties() -> Self {
        let megabytes = match std::env::var("IFC_BENCH_SYNTHETIC_MB") {
            Ok(value) => match value.parse::<usize>() {
                Ok(megabytes) if megabytes > 0 => megabytes,
                _ => panic!(
                    "IFC_BENCH_SYNTHETIC_MB must be a positive whole number of MB, got {value:?}"
                ),
            },
            Err(std::env::VarError::NotPresent) => 18,
            Err(error) => panic!("IFC_BENCH_SYNTHETIC_MB: {error}"),
        };
        Self {
            name: format!("synthetic-properties-{megabytes}mb"),
            text: PropertyModel::generate(megabytes * 1_000_000),
        }
    }

    pub fn uri(&self) -> Url {
        Url::parse(&format!("file:///bench/{}", self.name)).expect("model name forms a valid URI")
    }

    pub fn position(&self, offset: usize) -> Position {
        let line_start = self.text[..offset].rfind('\n').map_or(0, |index| index + 1);
        let line = self.text[..line_start].matches('\n').count();
        let character = self.text[line_start..offset].encode_utf16().count();
        Position::new(line as u32, character as u32)
    }

    /// Offset of the first digit of the first `IFCCARTESIANPOINT((` past the middle of the file,
    /// or of the first one in the file.
    pub fn cartesian_point_digit(&self) -> usize {
        let needle = "IFCCARTESIANPOINT((";
        let middle = self.text.len() / 2;
        let mut points = self.text.match_indices(needle).map(|(index, _)| index);
        let first = points
            .next()
            .unwrap_or_else(|| panic!("{} has no IFCCARTESIANPOINT", self.name));
        let start = std::iter::once(first)
            .chain(points)
            .find(|&index| index >= middle)
            .unwrap_or(first)
            + needle.len();
        start
            + self.text[start..]
                .find(|c: char| c.is_ascii_digit())
                .expect("cartesian point has a digit")
    }
}

/// IFC4 walls, each with placement, a swept-solid body and a property set of 100–160
/// `IFCPROPERTYSINGLEVALUE`s, grouped by storey, material and type like a Revit export.
struct PropertyModel {
    text: String,
    last_id: usize,
    seed: u64,
}

const STOREYS: usize = 10;
const MATERIALS: usize = 40;

impl PropertyModel {
    fn generate(target_bytes: usize) -> String {
        let mut model = Self {
            text: String::with_capacity(target_bytes + 4096),
            last_id: 0,
            seed: 7,
        };
        model.text.push_str(
            "ISO-10303-21;\nHEADER;\n\
             FILE_DESCRIPTION(('ViewDefinition [DesignTransferView]'),'2;1');\n\
             FILE_NAME('model.ifc','2024-01-01T00:00:00',('Jane Doe'),('ACME'),'Generator','Generator 2024','');\n\
             FILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n",
        );
        model.emit("IFCPERSON($,'Doe','Jane',$,$,$,$,$)".into());
        model.emit("IFCOWNERHISTORY(#3,#5,$,.ADDED.,$,$,$,1700000000)".into());
        model.emit("IFCPERSONANDORGANIZATION(#1,#4,$)".into());
        model.emit("IFCORGANIZATION($,'ACME',$,$,$)".into());
        model.emit("IFCAPPLICATION(#4,'2024','Generator','gen')".into());
        let origin = model.emit("IFCCARTESIANPOINT((0.,0.,0.))".into());
        let dir_z = model.emit("IFCDIRECTION((0.,0.,1.))".into());
        let dir_x = model.emit("IFCDIRECTION((1.,0.,0.))".into());
        let dir_x2 = model.emit("IFCDIRECTION((1.,0.))".into());
        let world = model.emit(format!("IFCAXIS2PLACEMENT3D(#{origin},#{dir_z},#{dir_x})"));
        let context = model.emit(format!(
            "IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-05,#{world},$)"
        ));
        let body_context = model.emit(format!(
            "IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#{context},$,.MODEL_VIEW.,$)"
        ));
        let length = model.emit("IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.)".into());
        let area = model.emit("IFCSIUNIT(*,.AREAUNIT.,$,.SQUARE_METRE.)".into());
        let volume = model.emit("IFCSIUNIT(*,.VOLUMEUNIT.,$,.CUBIC_METRE.)".into());
        let units = model.emit(format!("IFCUNITASSIGNMENT((#{length},#{area},#{volume}))"));
        let guid = model.guid();
        let project = model.emit(format!(
            "IFCPROJECT({guid},#2,'Project',$,$,$,$,(#{context}),#{units})"
        ));
        let site_placement = model.emit(format!("IFCLOCALPLACEMENT($,#{world})"));
        let guid = model.guid();
        let site = model.emit(format!(
            "IFCSITE({guid},#2,'Site',$,$,#{site_placement},$,$,.ELEMENT.,$,$,$,$,$)"
        ));
        let building_placement =
            model.emit(format!("IFCLOCALPLACEMENT(#{site_placement},#{world})"));
        let guid = model.guid();
        let building = model.emit(format!(
            "IFCBUILDING({guid},#2,'Building',$,$,#{building_placement},$,$,.ELEMENT.,$,$,$)"
        ));
        model.aggregate(project, &[site]);
        model.aggregate(site, &[building]);
        let mut storeys = Vec::new();
        let mut storey_placements = Vec::new();
        for storey in 0..STOREYS {
            let elevation = storey as f64 * 3.2;
            let point = model.emit(format!("IFCCARTESIANPOINT((0.,0.,{elevation:.1}))"));
            let axis = model.emit(format!("IFCAXIS2PLACEMENT3D(#{point},$,$)"));
            let placement = model.emit(format!("IFCLOCALPLACEMENT(#{building_placement},#{axis})"));
            let guid = model.guid();
            storeys.push(model.emit(format!(
                "IFCBUILDINGSTOREY({guid},#2,'Level {storey}',$,$,#{placement},$,$,.ELEMENT.,{elevation:.1})"
            )));
            storey_placements.push(placement);
        }
        model.aggregate(building, &storeys);
        let materials: Vec<usize> = (0..MATERIALS)
            .map(|material| model.emit(format!("IFCMATERIAL('Material {material}',$,$)")))
            .collect();
        let guid = model.guid();
        let wall_type = model.emit(format!(
            "IFCWALLTYPE({guid},#2,'Basic Wall:Generic - 200mm',$,$,$,$,$,$,.STANDARD.)"
        ));

        let mut per_storey = vec![Vec::new(); STOREYS];
        let mut per_material = vec![Vec::new(); MATERIALS];
        let mut walls = Vec::new();
        while model.text.len() + 600 * (STOREYS + 1) < target_bytes {
            let index = walls.len();
            let (x, y) = (model.uniform(0.0, 100.0), model.uniform(0.0, 100.0));
            let point = model.emit(format!("IFCCARTESIANPOINT(({x:.4},{y:.4},0.))"));
            let axis = model.emit(format!("IFCAXIS2PLACEMENT3D(#{point},$,$)"));
            let placement = model.emit(format!(
                "IFCLOCALPLACEMENT(#{},#{axis})",
                storey_placements[index % STOREYS]
            ));
            let profile_point = model.emit("IFCCARTESIANPOINT((0.,0.))".into());
            let profile_axis =
                model.emit(format!("IFCAXIS2PLACEMENT2D(#{profile_point},#{dir_x2})"));
            let width = model.uniform(1.0, 20.0);
            let profile = model.emit(format!(
                "IFCRECTANGLEPROFILEDEF(.AREA.,$,#{profile_axis},{width:.4},0.2)"
            ));
            let solid = model.emit(format!(
                "IFCEXTRUDEDAREASOLID(#{profile},#{world},#{dir_z},3.)"
            ));
            let body = model.emit(format!(
                "IFCSHAPEREPRESENTATION(#{body_context},'Body','SweptSolid',(#{solid}))"
            ));
            let shape = model.emit(format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{body}))"));
            let tag = 100_000 + index;
            let guid = model.guid();
            let wall = model.emit(format!(
                "IFCWALL({guid},#2,'Basic Wall:Generic - 200mm:{tag}',$,'Basic Wall:Generic - 200mm',#{placement},#{shape},'{tag}',.STANDARD.)"
            ));
            walls.push(wall);
            per_storey[index % STOREYS].push(wall);
            per_material[index % MATERIALS].push(wall);

            let properties: Vec<usize> = (0..model.int(100, 160))
                .map(|property| model.property(property))
                .collect();
            let guid = model.guid();
            let property_set = model.emit(format!(
                "IFCPROPERTYSET({guid},#2,'Pset_WallCommon',$,{})",
                references(&properties)
            ));
            model.relate("IFCRELDEFINESBYPROPERTIES", &[wall], property_set);
        }
        for (storey, walls) in storeys.iter().zip(&per_storey) {
            model.relate("IFCRELCONTAINEDINSPATIALSTRUCTURE", walls, *storey);
        }
        for (material, walls) in materials.iter().zip(&per_material) {
            model.relate("IFCRELASSOCIATESMATERIAL", walls, *material);
        }
        for walls in walls.chunks(50) {
            model.relate("IFCRELDEFINESBYTYPE", walls, wall_type);
        }
        model.text.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
        model.text
    }

    fn property(&mut self, index: usize) -> usize {
        const NAMES: [&str; 11] = [
            "IsExternal",
            "LoadBearing",
            "Reference",
            "Length",
            "Area",
            "Volume",
            "Comments",
            "FireRating",
            "ThermalTransmittance",
            "Count",
            "Ratio",
        ];
        let kind = index % NAMES.len();
        let value = match kind {
            0 => "IFCBOOLEAN(.T.)".to_string(),
            1 => "IFCBOOLEAN(.F.)".to_string(),
            2 => "IFCIDENTIFIER('Generic - 200mm')".to_string(),
            3 => format!("IFCLENGTHMEASURE({:.4})", self.uniform(1.0, 20.0)),
            4 => format!("IFCAREAMEASURE({:.4})", self.uniform(1.0, 60.0)),
            5 => format!("IFCVOLUMEMEASURE({:.4})", self.uniform(0.2, 12.0)),
            6 => "IFCTEXT('Exported')".to_string(),
            7 => "IFCLABEL('REI 60')".to_string(),
            8 => format!(
                "IFCTHERMALTRANSMITTANCEMEASURE({:.4})",
                self.uniform(0.1, 2.0)
            ),
            9 => format!("IFCINTEGER({})", self.int(1, 99)),
            _ => format!("IFCREAL({:.4})", self.uniform(0.0, 1.0)),
        };
        let name = if index < NAMES.len() {
            NAMES[kind].to_string()
        } else {
            format!("{}_{index}", NAMES[kind])
        };
        self.emit(format!("IFCPROPERTYSINGLEVALUE('{name}',$,{value},$)"))
    }

    fn aggregate(&mut self, relating: usize, related: &[usize]) {
        let guid = self.guid();
        self.emit(format!(
            "IFCRELAGGREGATES({guid},#2,$,$,#{relating},{})",
            references(related)
        ));
    }

    fn relate(&mut self, entity: &str, related: &[usize], relating: usize) {
        if related.is_empty() {
            return;
        }
        let guid = self.guid();
        self.emit(format!(
            "{entity}({guid},#2,$,$,{},#{relating})",
            references(related)
        ));
    }

    fn emit(&mut self, body: String) -> usize {
        self.last_id += 1;
        let id = self.last_id;
        self.text.push_str(&format!("#{id}={body};\n"));
        id
    }

    fn guid(&mut self) -> String {
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_$";
        let mut guid = String::from("'");
        guid.push(ALPHABET[self.int(0, 3)] as char);
        for _ in 0..21 {
            guid.push(ALPHABET[self.int(0, 63)] as char);
        }
        guid.push('\'');
        guid
    }

    fn next(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    fn uniform(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn int(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() % (high - low + 1) as u64) as usize
    }
}

fn references(ids: &[usize]) -> String {
    let ids: Vec<String> = ids.iter().map(|id| format!("#{id}")).collect();
    format!("({})", ids.join(","))
}

pub struct Session {
    runtime: Runtime,
    service: LspService<Backend>,
    published: mpsc::UnboundedReceiver<PublishDiagnosticsParams>,
    version: i32,
}

impl Session {
    pub fn new(initialization_options: serde_json::Value) -> Self {
        let runtime = Runtime::new().expect("tokio runtime starts");
        let (published_tx, published) = mpsc::unbounded_channel();
        let mut service = runtime.block_on(async {
            let (mut service, mut socket) = LspService::new(Backend::new);
            tokio::spawn(async move {
                while let Some(request) = socket.next().await {
                    if request.method() == "textDocument/publishDiagnostics" {
                        let params = serde_json::from_value(
                            request
                                .params()
                                .cloned()
                                .expect("publishDiagnostics has params"),
                        )
                        .expect("publishDiagnostics params deserialize");
                        if published_tx.send(params).is_err() {
                            break;
                        }
                    }
                }
            });
            let initialize = Request::build("initialize")
                .params(json!({
                    "capabilities": {},
                    "initializationOptions": initialization_options,
                }))
                .id(1)
                .finish();
            call(&mut service, initialize).await;
            call(
                &mut service,
                Request::build("initialized").params(json!({})).finish(),
            )
            .await;
            service
        });
        runtime.block_on(service.ready()).expect("service is ready");
        Self {
            runtime,
            service,
            published,
            version: 0,
        }
    }

    pub fn backend(&self) -> &Backend {
        self.service.inner()
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    pub fn open(&mut self, uri: &Url, text: String) -> Vec<Diagnostic> {
        self.version += 1;
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                uri.clone(),
                "ifc".to_string(),
                self.version,
                text,
            ),
        };
        self.runtime.block_on(self.service.inner().did_open(params));
        self.next_diagnostics(uri)
    }

    pub fn change(&mut self, uri: &Url, range: Range, text: &str) -> Vec<Diagnostic> {
        self.version += 1;
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier::new(uri.clone(), self.version),
            content_changes: vec![TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: text.to_string(),
            }],
        };
        self.runtime
            .block_on(self.service.inner().did_change(params));
        self.next_diagnostics(uri)
    }

    fn next_diagnostics(&mut self, uri: &Url) -> Vec<Diagnostic> {
        loop {
            let params = self
                .published
                .blocking_recv()
                .expect("diagnostics channel stays open");
            if &params.uri == uri {
                return params.diagnostics;
            }
        }
    }
}

async fn call(service: &mut LspService<Backend>, request: Request) {
    service
        .ready()
        .await
        .expect("service is ready")
        .call(request)
        .await
        .expect("request succeeds");
}
