//
// Copyright (c) Pirmin Kalberer. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root for full license information.
//

use datasource::postgis::PostgisInput;
use core::grid::Grid;
use mvt::tile::Tile;
use mvt::vector_tile;
use service::mvt::{MvtService, Tileset};
use core::{Config, read_config, parse_config};
use toml;
use serde_json;
use cache::{Tilecache, Nocache, Filecache};

use nickel::{Nickel, Options, HttpRouter, MediaType, Request, Responder, Response,
             MiddlewareResult, StaticFilesHandler};
use hyper::header::{CacheControl, CacheDirective, AccessControlAllowOrigin,
                    AccessControlAllowMethods, ContentEncoding, Encoding};
use hyper::method::Method;
use hyper::header;
use std::collections::HashMap;
use std::str::FromStr;
use clap::ArgMatches;
use std::str;
use std::process;
use open;


fn log_request<'mw>(req: &mut Request<MvtService>,
                    res: Response<'mw, MvtService>)
                    -> MiddlewareResult<'mw, MvtService> {
    info!("{} {}", req.origin.method, req.origin.uri);
    res.next_middleware()
}

#[allow(dead_code)]
fn enable_cors<'mw>(_req: &mut Request, mut res: Response<'mw>) -> MiddlewareResult<'mw> {
    // access-control-allow-methods: GET
    // access-control-allow-origin: *
    // see also https://github.com/nickel-org/nickel.rs/blob/master/examples/enable_cors.rs
    res.set(AccessControlAllowMethods(vec![Method::Get]));
    res.set(AccessControlAllowOrigin::Any);

    res.next_middleware()
}

header! { (ContentType, "Content-Type") => [String] }

impl<D> Responder<D> for vector_tile::Tile {
    fn respond<'a>(self, mut res: Response<'a, D>) -> MiddlewareResult<'a, D> {
        res.set_header_fallback(|| ContentType("application/x-protobuf".to_owned()));
        res.set_header_fallback(|| CacheControl(vec![CacheDirective::MaxAge(43200u32)])); //TODO: from cache settings

        let vec = Tile::binary_tile(&self);
        res.send(vec)
    }
}

#[derive(RustcEncodable)]
struct TilesetInfo {
    name: String,
    layerinfos: String,
    hasviewer: bool,
}

impl TilesetInfo {
    fn from_tileset(set: &Tileset) -> TilesetInfo {
        let mut hasviewer = true;
        let layerinfos: Vec<String> = set.layers
            .iter()
            .map(|l| {
                let geom_type = l.geometry_type.clone().unwrap_or("UNKNOWN".to_string());
                hasviewer = hasviewer &&
                            ["POINT",
                             "LINESTRING",
                             "POLYGON",
                             "MULTPOINT",
                             "MULTILINESTRING",
                             "MULTIPOLYGON"]
                                    .contains(&(&geom_type as &str));
                format!("{} [{}]", &l.name, &geom_type)
            })
            .collect();
        TilesetInfo {
            name: set.name.clone(),
            layerinfos: format!("{}", layerinfos.join(", ")),
            hasviewer: hasviewer,
        }
    }
}

struct StaticFiles {
    files: HashMap<&'static str, (&'static [u8], MediaType)>,
}

impl StaticFiles {
    fn init() -> StaticFiles {
        let mut static_files = StaticFiles { files: HashMap::new() };
        static_files.add("favicon.ico",
                         include_bytes!("static/favicon.ico"),
                         MediaType::Ico);
        static_files.add("index.html",
                         include_bytes!("static/index.html"),
                         MediaType::Html);
        static_files.add("viewer.js",
                         include_bytes!("static/viewer.js"),
                         MediaType::Js);
        static_files.add("viewer.css",
                         include_bytes!("static/viewer.css"),
                         MediaType::Css);
        static_files.add("maputnik.html",
                         include_bytes!("static/maputnik.html"),
                         MediaType::Html);
        static_files.add("maputnik.js",
                         include_bytes!("static/maputnik.js"),
                         MediaType::Js);
        static_files.add("maputnik-vendor.js",
                         include_bytes!("static/maputnik-vendor.js"),
                         MediaType::Js);
        static_files.add("img/maputnik.png",
                         include_bytes!("static/img/maputnik.png"),
                         MediaType::Png);
        static_files.add("fonts/Roboto-Regular.ttf",
                         include_bytes!("static/fonts/Roboto-Regular.ttf"),
                         MediaType::Ttf);
        static_files.add("fonts/Roboto-Medium.ttf",
                         include_bytes!("static/fonts/Roboto-Medium.ttf"),
                         MediaType::Ttf);
        static_files
    }
    fn add(&mut self, name: &'static str, data: &'static [u8], media_type: MediaType) {
        self.files.insert(name, (data, media_type));
    }
    fn content(&self, base: Option<&str>, name: String) -> Option<&(&[u8], MediaType)> {
        let mut key = if name == "." {
            "index.html".to_string()
        } else {
            name
        };
        if let Some(path) = base {
            key = format!("{}/{}", path, key);
        }
        self.files.get(&key as &str)
    }
}


const DEFAULT_CONFIG: &'static str = r#"
[service.mvt]
viewer = true

[webserver]
bind = "127.0.0.1"
port = 6767
threads = 4
"#;

pub fn service_from_args(args: &ArgMatches) -> (MvtService, toml::Value) {
    if let Some(cfgpath) = args.value_of("config") {
        info!("Reading configuration from '{}'", cfgpath);
        let config = read_config(cfgpath).unwrap_or_else(|err| {
                                                             println!("Error reading configuration - {} ", err);
                                                             process::exit(1)
                                                         });
        let mut svc =
            MvtService::from_config(&config).unwrap_or_else(|err| {
                                                                println!("Error reading configuration - {} ", err);
                                                                process::exit(1)
                                                            });
        svc.connect();
        (svc, config)
    } else {
        let config = parse_config(DEFAULT_CONFIG.to_string(), "").unwrap();
        let cache = match args.value_of("cache") {
            None => Tilecache::Nocache(Nocache),
            Some(dir) => Tilecache::Filecache(Filecache { basepath: dir.to_string() }),
        };
        let simplify = bool::from_str(args.value_of("simplify").unwrap_or("true")).unwrap_or(false);
        let clip = bool::from_str(args.value_of("clip").unwrap_or("true")).unwrap_or(false);
        if let Some(dbconn) = args.value_of("dbconn") {
            let pg = PostgisInput::new(dbconn).connected();
            let grid = Grid::web_mercator();
            let detect_geometry_types = true; //TODO: add option (maybe slow for many geometries)
            let mut layers = pg.detect_layers(detect_geometry_types);
            let mut tilesets = Vec::new();
            while let Some(mut l) = layers.pop() {
                l.simplify = Some(simplify);
                if simplify {
                    // Limit features by default unless simplify is set to false
                    l.query_limit = Some(1000);
                }
                l.buffer_size = match l.geometry_type {
                    Some(ref geom) => {
                        let types =
                            vec!["LINESTRING", "MULTILINESTRING", "POLYGON", "MULTIPOLYGON"];
                        if clip && types.contains(&(geom as &str)) {
                            Some(1)
                        } else {
                            None
                        }
                    }
                    None => None,
                };
                let tileset = Tileset {
                    name: l.name.clone(),
                    layers: vec![l],
                };
                tilesets.push(tileset);
            }
            let svc = MvtService {
                input: pg,
                grid: grid,
                tilesets: tilesets,
                cache: cache,
            };
            (svc, config)
        } else {
            println!("Either 'config' or 'dbconn' is required");
            process::exit(1)
        }
    }
}

#[allow(unreachable_code)]
pub fn webserver(args: &ArgMatches) {
    let (mut service, config) = service_from_args(args);

    let mvt_config = config
        .get("service")
        .and_then(|s| s.get("mvt"))
        .ok_or("Missing configuration entry [service.mvt]".to_string())
        .unwrap_or_else(|err| {
                            println!("Error reading configuration - {} ", err);
                            process::exit(1)
                        });
    let mvt_viewer = mvt_config
        .get("viewer")
        .map_or(true, |val| val.as_bool().unwrap_or(true));
    let http_config = config
        .get("webserver")
        .ok_or("Missing configuration entry [webserver]".to_string())
        .unwrap_or_else(|err| {
                            println!("Error reading configuration - {} ", err);
                            process::exit(1)
                        });
    let bind = http_config
        .get("bind")
        .map_or("127.0.0.1", |val| val.as_str().unwrap_or("127.0.0.1"));
    let port = http_config
        .get("port")
        .map_or(6767, |val| val.as_integer().unwrap_or(6767)) as u16;
    let threads = http_config
        .get("threads")
        .map_or(4, |val| val.as_integer().unwrap_or(4)) as usize;

    service.prepare_feature_queries();
    service.init_cache();

    let mut tileset_infos: Vec<TilesetInfo> = service
        .tilesets
        .iter()
        .map(|set| TilesetInfo::from_tileset(&set))
        .collect();
    tileset_infos.sort_by_key(|ti| ti.name.clone());

    let mut server = Nickel::with_data(service);
    server.options = Options::default().thread_count(Some(threads));
    // Avoid thread exhaustion caused by hypers keep_alive handling (https://github.com/hyperium/hyper/issues/368)
    server.keep_alive_timeout(None);
    server.utilize(log_request);

    server.get("/index.json",
               middleware! { |_req, mut res|
        let service: &MvtService = res.server_data();
        res.set(MediaType::Json);
        res.set(AccessControlAllowMethods(vec![Method::Get]));
        res.set(AccessControlAllowOrigin::Any);
        let json = service.get_mvt_metadata().unwrap();
        serde_json::to_vec(&json).unwrap()
    });

    // Font list for Maputnik
    server.get("/fontstacks.json",
               middleware! { |_req, mut res|
        res.set(MediaType::Json);
        res.set(AccessControlAllowMethods(vec![Method::Get]));
        res.set(AccessControlAllowOrigin::Any);
        "[]"
    });

    server.get("/:tileset.json",
               middleware! { |req, mut res|
        let service: &MvtService = res.server_data();
        let tileset = req.param("tileset").unwrap();
        res.set(MediaType::Json);
        res.set(AccessControlAllowMethods(vec![Method::Get]));
        res.set(AccessControlAllowOrigin::Any);
        let host = req.origin.headers.get::<header::Host>().unwrap();
        let baseurl = format!("http://{}:{}", host.hostname, host.port.unwrap_or(80));
        let json = service.get_tilejson(&baseurl, &tileset).unwrap();
        serde_json::to_vec(&json).unwrap()
    });

    server.get("/:tileset.style.json",
               middleware! { |req, mut res|
        let service: &MvtService = res.server_data();
        let tileset = req.param("tileset").unwrap();
        res.set(MediaType::Json);
        res.set(AccessControlAllowMethods(vec![Method::Get]));
        res.set(AccessControlAllowOrigin::Any);
        let host = req.origin.headers.get::<header::Host>().unwrap();
        let baseurl = format!("http://{}:{}", host.hostname, host.port.unwrap_or(80));
        let json = service.get_stylejson(&baseurl, &tileset).unwrap();
        serde_json::to_vec(&json).unwrap()
    });

    server.get("/:tileset/metadata.json",
               middleware! { |req, mut res|
        let service: &MvtService = res.server_data();
        let tileset = req.param("tileset").unwrap();
        res.set(MediaType::Json);
        let json = service.get_mbtiles_metadata(&tileset).unwrap();
        serde_json::to_vec(&json).unwrap()
    });

    server.get("/:tileset/:z/:x/:y.pbf",
               middleware! { |req, mut res|
        let service: &MvtService = res.server_data();

        let tileset = req.param("tileset").unwrap();
        let z = req.param("z").unwrap().parse::<u8>().unwrap();
        let x = req.param("x").unwrap().parse::<u32>().unwrap();
        let y = req.param("y").unwrap().parse::<u32>().unwrap();

        let gzip = true; // TODO: From AcceptEncoding
        let tile = service.tile_cached(tileset, x, y, z, gzip);
        if gzip {
            res.set_header_fallback(|| ContentEncoding(vec![Encoding::Gzip]));
        }
        res.set_header_fallback(|| ContentType("application/x-protobuf".to_owned()));
        res.set_header_fallback(|| CacheControl(vec![CacheDirective::MaxAge(43200u32)])); //TODO: from cache settings
        //res.set_header_fallback(|| ContentLength(tile.len() as u64));
        res.set(AccessControlAllowMethods(vec![Method::Get]));
        res.set(AccessControlAllowOrigin::Any);

        tile
    });

    if mvt_viewer {
        let static_files = StaticFiles::init();
        server.get("/(:base/)?:static",
                   middleware! { |req, mut res|
            let mut name = req.param("static").unwrap().to_string();
            if let Some(format) = req.param("format") {
                name = format!("{}.{}", name, format);
            }
            if let Some(content) = static_files.content(req.param("base"), name) {
                res.set(content.1);
                return res.send(content.0)
            }
        });
    }

    server.get("/**", StaticFilesHandler::new("public/"));

    let _listening = server
        .listen((bind, port))
        .expect("Failed to launch server");

    let openbrowser = bool::from_str(args.value_of("openbrowser").unwrap_or("true"))
        .unwrap_or(false);
    if openbrowser && mvt_viewer {
        let _res = open::that(format!("http://{}:{}", bind, port));
    }
}

pub fn gen_config(args: &ArgMatches) -> String {
    let toml = r#"
[webserver]
# Bind address. Use 0.0.0.0 to listen on all adresses.
bind = "127.0.0.1"
port = 6767
threads = 4
"#;
    let mut config;
    if let Some(_dbconn) = args.value_of("dbconn") {
        let (service, _) = service_from_args(args);
        config = service.gen_runtime_config();
    } else {
        config = MvtService::gen_config();
    }
    config.push_str(toml);
    config
}


#[test]
fn test_gen_config() {
    use core::parse_config;

    let args = ArgMatches::new();
    let toml = gen_config(&args);
    println!("{}", toml);
    assert_eq!(Some("# t-rex configuration"), toml.lines().next());

    let config = parse_config(toml, "").unwrap();
    let service = MvtService::from_config(&config).unwrap();
    assert_eq!(service.input.connection_url, "postgresql://user:pass@host/database");
}

#[test]
#[ignore]
fn test_runtime_config() {
    use std::env;
    use clap::App;
    use core::parse_config;

    if env::var("DBCONN").is_err() {
        panic!("DBCONN undefined");
    }
    let args = App::new("test")
        .args_from_usage("--dbconn=[SPEC] 'PostGIS connection postgresql://USER@HOST/DBNAME'")
        .get_matches_from(vec!["", "--dbconn", &env::var("DBCONN").unwrap()]);
    let toml = gen_config(&args);
    println!("{}", toml);
    assert_eq!(Some("# t-rex configuration"), toml.lines().next());

    let config = parse_config(toml, "").unwrap();
    let service = MvtService::from_config(&config).unwrap();
    assert_eq!(service.input.connection_url, env::var("DBCONN").unwrap());
}
