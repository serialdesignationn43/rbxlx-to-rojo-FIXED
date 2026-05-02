use log::info;
use rbxlx_to_rojo::{filesystem::FileSystem, process_instructions};
use std::{
    borrow::Cow,
    fmt, fs,
    io::{self, BufReader, Read, Write},
    path::PathBuf,
    sync::{Arc, RwLock},
};
use quick_xml::Reader;
use quick_xml::Writer;
use quick_xml::events::{Event, BytesText};
use std::io::Cursor;
use xmltree::{Element, XMLNode};

#[derive(Debug)]
enum Problem {
    BinaryDecodeError(rbx_binary::DecodeError),
    InvalidFile,
    IoError(&'static str, io::Error),
    NFDCancel,
    NFDError(String),
    XMLDecodeError(rbx_xml::DecodeError),
}

impl fmt::Display for Problem {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Problem::BinaryDecodeError(error) => write!(
                formatter,
                "While attempting to decode the place file, at {} rbx_binary didn't know what to do",
                error,
            ),

            Problem::InvalidFile => {
                write!(formatter, "The file provided does not have a recognized file extension")
            }

            Problem::IoError(doing_what, error) => {
                write!(formatter, "While attempting to {}, {}", doing_what, error)
            }

            Problem::NFDCancel => write!(formatter, "Didn't choose a file."),

            Problem::NFDError(error) => write!(
                formatter,
                "Something went wrong when choosing a file: {}",
                error,
            ),

            Problem::XMLDecodeError(error) => write!(
                formatter,
                "While attempting to decode the place file, at {} rbx_xml didn't know what to do",
                error,
            ),
        }
    }
}

struct WrappedLogger {
    log: env_logger::Logger,
    log_file: Arc<RwLock<Option<fs::File>>>,
}

impl log::Log for WrappedLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.log.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            self.log.log(record);

            if let Some(ref mut log_file) = &mut *self.log_file.write().unwrap() {
                log_file
                    .write(format!("{}\r\n", record.args()).as_bytes())
                    .ok();
            }
        }
    }

    fn flush(&self) {}
}

fn routine() -> Result<(), Problem> {
    let env_logger = env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .build();

    let log_file = Arc::new(RwLock::new(None));
    let logger = WrappedLogger {
        log: env_logger,
        log_file: Arc::clone(&log_file),
    };

    log::set_boxed_logger(Box::new(logger)).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    info!("rbxlx-to-rojo {}", env!("CARGO_PKG_VERSION"));

    info!("Select a place file.");
    let file_path = PathBuf::from(match std::env::args().nth(1) {
        Some(text) => text,
        None => match nfd::open_file_dialog(Some("rbxl,rbxm,rbxlx,rbxmx"), None)
            .map_err(|error| Problem::NFDError(error.to_string()))?
        {
            nfd::Response::Okay(path) => path,
            nfd::Response::Cancel => Err(Problem::NFDCancel)?,
            _ => unreachable!(),
        },
    });

    info!("Opening place file");
    info!("Decoding place file, this is the longest part...");

    let tree = match file_path.extension().map(|extension| extension.to_string_lossy()) {
        Some(Cow::Borrowed("rbxmx")) | Some(Cow::Borrowed("rbxlx")) => {
            // Read file into a string so we can try a sanitizing pass
            let mut file = fs::File::open(&file_path)
                .map_err(|error| Problem::IoError("read the place file", error))?;
            let mut contents = String::new();
            file.read_to_string(&mut contents)
                .map_err(|error| Problem::IoError("read the place file", error))?;

            // Try normal parsing first
            match rbx_xml::from_str_default(&contents) {
                Ok(tree) => Ok(tree),
                Err(_) => {
                    // attempt to sanitize newer rbxlx styles then retry
                    let sanitized = sanitize_rbxlx(&contents);
                    rbx_xml::from_str_default(&sanitized).map_err(Problem::XMLDecodeError)
                }
            }
        }
        Some(Cow::Borrowed("rbxm")) | Some(Cow::Borrowed("rbxl")) => {
            let file_source = BufReader::new(
                fs::File::open(&file_path)
                    .map_err(|error| Problem::IoError("read the place file", error))?,
            );
            rbx_binary::from_reader_default(file_source).map_err(Problem::BinaryDecodeError)
        }
        _ => Err(Problem::InvalidFile),
    }?;

    info!("Select the path to put your Rojo project in.");
    let root = PathBuf::from(match std::env::args().nth(2) {
        Some(text) => text,
        None => match nfd::open_pick_folder(Some(&file_path.parent().unwrap().to_string_lossy()))
            .map_err(|error| Problem::NFDError(error.to_string()))?
        {
            nfd::Response::Okay(path) => path,
            nfd::Response::Cancel => Err(Problem::NFDCancel)?,
            _ => unreachable!(),
        },
    });

    let mut filesystem = FileSystem::from_root(root.join(file_path.file_stem().unwrap()).into());

    log_file.write().unwrap().replace(
        fs::File::create(root.join("rbxlx-to-rojo.log"))
            .map_err(|error| Problem::IoError("couldn't create log file", error))?,
    );

    info!("Starting processing, please wait a bit...");
    process_instructions(&tree, &mut filesystem);
    info!("Done! Check rbxlx-to-rojo.log for a full log.");
    Ok(())
}

fn main() {
    if let Err(error) = routine() {
        eprintln!("An error occurred while using rbxlx-to-rojo.");
        eprintln!("{}", error);
    }
}

// Sanitizer: strip processing instructions/comments with quick-xml, then use
// xmltree to massage property-style elements into typed property nodes.
fn sanitize_rbxlx(source: &str) -> String {
    // First, reserialize while dropping comments and processing instructions
    let mut reader = Reader::from_str(source);
    reader.trim_text(false);
    let mut writer = Writer::new(Cursor::new(Vec::new()));

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => { writer.write_event(Event::Start(e.to_owned())).ok(); }
            Ok(Event::Empty(e)) => { writer.write_event(Event::Empty(e.to_owned())).ok(); }
            Ok(Event::End(e)) => { writer.write_event(Event::End(e.to_owned())).ok(); }
            Ok(Event::Text(e)) => {
                // decode text, escape any literal PI sequences, and write
                match e.unescape() {
                    Ok(cow) => {
                        let mut text = cow.into_owned();
                        if text.contains("<?") || text.contains("?>") {
                            text = text.replace("<?", "&lt;?").replace("?>", "?&gt;");
                        }
                        writer.write_event(Event::Text(BytesText::new(&text))).ok();
                    }
                    Err(_) => {
                        writer.write_event(Event::Text(e.to_owned())).ok();
                    }
                }
            }
            Ok(Event::CData(e)) => {
                // convert CDATA to escaped text to avoid introducing PIs
                match e.clone().escape() {
                    Ok(bytes_text) => {
                        writer.write_event(Event::Text(bytes_text)).ok();
                    }
                    Err(_) => {
                        writer.write_event(Event::CData(e.to_owned())).ok();
                    }
                }
            }
            Ok(Event::Decl(e)) => { writer.write_event(Event::Decl(e.to_owned())).ok(); }
            Ok(Event::DocType(e)) => { writer.write_event(Event::DocType(e.to_owned())).ok(); }
            Ok(Event::Comment(_)) => { /* skip comments */ }
            Ok(Event::PI(_)) => { /* skip processing instructions */ }
            Ok(Event::Eof) => break,
            Err(_) => return source.to_string(),
        }
    }

    let out = writer.into_inner().into_inner();
    let cleaned = String::from_utf8(out).unwrap_or_else(|_| source.to_string());

    // Parse with xmltree and perform property element transformations
    let result = match Element::parse(cleaned.as_bytes()) {
        Ok(mut root) => {
            sanitize_element(&mut root, false);
            let mut out: Vec<u8> = Vec::new();
            root.write(&mut out).ok();
            String::from_utf8(out).unwrap_or(cleaned.clone())
        }
        Err(_) => cleaned.clone(),
    };

    // Final pass: escape any stray '?>' sequences except the initial XML declaration
    let final_result = {
        let s = &result;
        // find xml declaration range to preserve
        let mut preserve_range: Option<(usize, usize)> = None;
        if let Some(start) = s.find("<?xml") {
            if let Some(rel_end) = s[start..].find("?>") {
                preserve_range = Some((start, start + rel_end + 2));
            }
        }

        let mut out = String::with_capacity(s.len());
        let mut i = 0usize;
        while i < s.len() {
            if let Some(pos_rel) = s[i..].find("?>") {
                let pos = i + pos_rel;
                let in_preserve = preserve_range
                    .map(|(st, en)| pos >= st && pos < en)
                    .unwrap_or(false);
                if in_preserve {
                    // copy through this token unchanged
                    out.push_str(&s[i..pos + 2]);
                    i = pos + 2;
                } else {
                    out.push_str(&s[i..pos]);
                    out.push_str("?&gt;");
                    i = pos + 2;
                }
            } else {
                out.push_str(&s[i..]);
                break;
            }
        }
        out
    };

    // Write a sanitized copy to temp for debugging (ignore errors)
    let _ = std::fs::write(std::env::temp_dir().join("rbxlx-to-rojo-sanitized.rbxlx"), &final_result);

    final_result
}

fn sanitize_element(elem: &mut Element, inside_properties: bool) {
    let now_inside = inside_properties || elem.name == "Properties";

    if elem.name == "Properties" {
        let mut new_children: Vec<XMLNode> = Vec::with_capacity(elem.children.len());

        for node in elem.children.drain(..) {
            match node {
                XMLNode::Element(mut child_elem) => {
                    let tag = child_elem.name.as_str();
                    let canonical_types = [
                        "string", "double", "int", "bool", "boolean", "ProtectedString",
                        "BinaryString", "Content", "Faces", "BrickColor", "Color3", "Vector3",
                        "CFrame", "UDim2", "NumberSequence", "ColorSequence",
                    ];

                    if tag == "Item" || canonical_types.iter().any(|t| *t == tag) {
                        sanitize_element(&mut child_elem, now_inside);
                        new_children.push(XMLNode::Element(child_elem));
                    } else {
                        // Unknown property element name; attempt to determine a type
                        let prop_name = child_elem.name.clone();

                        // Prefer an xsi:type-like attribute if present
                        let mut chosen_type: Option<&str> = None;
                        for (k, v) in &child_elem.attributes {
                            let key = k.as_str();
                            if key.ends_with(":type") || key == "type" {
                                let lv = v.to_lowercase();
                                if lv.contains("double") || lv.contains("float") || lv.contains("decimal") {
                                    chosen_type = Some("double");
                                } else if lv.contains("int") || lv.contains("integer") {
                                    chosen_type = Some("int");
                                } else if lv.contains("bool") || lv.contains("boolean") {
                                    chosen_type = Some("bool");
                                } else if lv.contains("string") {
                                    chosen_type = Some("string");
                                }
                                break;
                            }
                        }

                        // If we didn't get a type from attributes, inspect textual content
                        let inner_text = child_elem.get_text().map(|s| s.to_string()).unwrap_or_else(String::new);

                        if chosen_type.is_none() {
                            let trimmed = inner_text.trim();
                            if trimmed.eq_ignore_ascii_case("true") || trimmed.eq_ignore_ascii_case("false") {
                                chosen_type = Some("bool");
                            } else if trimmed.parse::<f64>().is_ok() {
                                chosen_type = Some("double");
                            } else {
                                chosen_type = Some("string");
                            }
                        }

                        let typ = chosen_type.unwrap_or("string").to_string();
                        let mut new_elem = Element::new(typ.as_str());
                        new_elem.attributes.insert("name".to_string(), prop_name.clone());

                        let has_element_children = child_elem.children.iter().any(|n| matches!(n, XMLNode::Element(_)));

                        if !has_element_children {
                            if !inner_text.is_empty() {
                                new_elem.children.push(XMLNode::Text(inner_text));
                            }
                        } else {
                            let mut buf: Vec<u8> = Vec::new();
                            child_elem.write(&mut buf).ok();
                            let serialized = String::from_utf8_lossy(&buf).into_owned();
                            new_elem.children.push(XMLNode::Text(serialized));
                        }

                        new_children.push(XMLNode::Element(new_elem));
                    }
                }
                XMLNode::Text(text) => new_children.push(XMLNode::Text(text)),
                _ => { /* drop comments and other non-element/text nodes */ }
            }
        }

        elem.children = new_children;
    }

    if now_inside || elem.name != "Properties" {
        for node in &mut elem.children {
            if let XMLNode::Element(child) = node {
                sanitize_element(child, now_inside);
            }
        }
    }
}
