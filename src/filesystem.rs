use crate::structures::*;
use serde::{ser::SerializeMap, Serialize, Serializer};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Component, Path, PathBuf},
};

fn sanitize_component<S: AsRef<str>>(s: S) -> String {
    let s = s.as_ref();
    // Characters invalid on Windows filenames: <>:"/\\|?* and control chars
    const INVALID: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

    let mut out: String = s
        .chars()
        .filter_map(|c| {
            if (c as u32) < 0x20 {
                // drop control characters
                None
            } else if INVALID.contains(&c) {
                // replace invalid chars with underscore
                Some('_')
            } else {
                Some(c)
            }
        })
        .collect();

    // Trim trailing spaces and dots (Windows doesn't allow names ending with them)
    while out.ends_with(' ') || out.ends_with('.') {
        out.pop();
    }

    if out.is_empty() {
        out.push('_');
    }

    // Avoid reserved device names on Windows (CON, PRN, AUX, NUL, COM1..COM9, LPT1..LPT9)
    let upper = out.to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    if reserved.contains(&upper.as_str()) || (upper.len() >= 4 && &upper[..3] == "COM") || (upper.len() >= 4 && &upper[..3] == "LPT") {
        out.push('_');
    }

    out
}

fn sanitize_path<P: AsRef<Path>>(p: P) -> PathBuf {
    let path = p.as_ref();
    let mut out = PathBuf::new();

    for comp in path.components() {
        match comp {
            Component::Normal(os) => {
                let s = os.to_string_lossy();
                out.push(sanitize_component(&s));
            }
            // Preserve root/prefix components unchanged
            other => out.push(other.as_os_str()),
        }
    }

    out
}

const SRC: &str = "src";

fn serialize_project_tree<S: Serializer>(
    tree: &BTreeMap<String, TreePartition>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(tree.len() + 1))?;
    map.serialize_entry("$className", "DataModel")?;
    for (k, v) in tree {
        map.serialize_entry(k, v)?;
    }
    map.end()
}

#[derive(Clone, Debug, Serialize)]
struct Project {
    name: String,
    #[serde(serialize_with = "serialize_project_tree")]
    tree: BTreeMap<String, TreePartition>,
}

impl Project {
    fn new() -> Self {
        Self {
            name: "project".to_string(),
            tree: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileSystem {
    project: Project,
    root: PathBuf,
    source: PathBuf,
}

impl FileSystem {
    pub fn from_root(root: PathBuf) -> Self {
        let source = root.join(SRC);
        let project = Project::new();

        fs::create_dir(&source).ok(); // It'll error later if it matters

        Self {
            project,
            root,
            source,
        }
    }
}

impl InstructionReader for FileSystem {
    fn read_instruction<'a>(&mut self, instruction: Instruction<'a>) {
        match instruction {
            Instruction::AddToTree {
                name,
                mut partition,
            } => {
                assert!(
                    self.project.tree.get(&name).is_none(),
                    "Duplicate item added to tree! Instances can't have the same name: {}",
                    name
                );

                if let Some(path) = partition.path {
                    partition.path = Some(sanitize_path(Path::new(SRC).join(path)));
                }

                for child in partition.children.values_mut() {
                    if let Some(path) = &child.path {
                        child.path = Some(sanitize_path(Path::new(SRC).join(path)));
                    }
                }

                self.project.tree.insert(name, partition);
            }

            Instruction::CreateFile { filename, contents } => {
                let sanitized = sanitize_path(self.source.join(&filename));
                let mut file = File::create(&sanitized).unwrap_or_else(|error| {
                    panic!("can't create file {:?}: {:?}", sanitized, error)
                });
                file.write_all(&contents).unwrap_or_else(|error| {
                    panic!("can't write to file {:?} due to {:?}", sanitized, error)
                });
            }

            Instruction::CreateFolder { folder } => {
                let sanitized = sanitize_path(self.source.join(&folder));
                fs::create_dir_all(&sanitized).unwrap_or_else(|error| {
                    panic!("can't write to folder {:?}: {:?}", sanitized, error)
                });
            }
        }
    }

    fn finish_instructions(&mut self) {
        let mut file = File::create(self.root.join("default.project.json"))
            .expect("can't create default.project.json");
        file.write_all(
            &serde_json::to_string_pretty(&self.project)
                .expect("couldn't serialize project")
                .as_bytes(),
        )
        .expect("can't write project");
    }
}
