use std::{
    collections::HashMap,
    fmt,
    iter::Peekable,
    path::{Path, PathBuf},
    str::Lines,
};

use crate::compare::assert_e2e;
use walkdir::WalkDir;

#[derive(Debug)]
pub struct LayoutTree {
    root: LayoutTreeNode,
}

#[derive(Debug, Clone)]
pub struct LayoutTreeNode {
    path: PathBuf,
    children: Vec<LayoutTreeNode>,
}

impl LayoutTree {
    /// Parses a string formatted like the output of the `tree` command into a `LayoutTree`.
    pub fn parse(input: &str) -> Self {
        // Create a peekable iterator over the lines of the input string.
        let mut lines = input.trim().lines().peekable();

        // The first line is always the root directory path.
        let root_line = lines.next().expect("Input string cannot be empty.");
        let root_path = PathBuf::from(root_line);

        // Create the root node of our tree.
        let mut root = LayoutTreeNode {
            path: root_path,
            children: Vec::new(),
        };

        // Begin the recursive parsing process for all children of the root.
        // The root is at level -1, so its direct children are at level 0.
        Self::parse_level(&mut root, &mut lines, -1);

        LayoutTree { root }
    }

    /// Recursively parses lines to build up the tree structure for a given parent node.
    ///
    /// - `parent`: The directory node to which children (files/dirs) will be added.
    /// - `lines`: The peekable iterator over the input lines.
    /// - `parent_level`: The indentation level of the `parent` node.
    fn parse_level(
        parent: &mut LayoutTreeNode,
        lines: &mut Peekable<Lines<'_>>,
        parent_level: isize,
    ) {
        // Keep processing lines as long as they are direct children of the current parent node.
        while let Some(line) = lines.peek() {
            let (level, _, _) = Self::get_line_info(line);

            // If the current line's level is not one greater than the parent's,
            // it's not a direct child, so we stop parsing for this parent.
            if level as isize <= parent_level {
                break;
            }

            // This line is a child, so we must consume it from the iterator.
            let line = lines.next().unwrap();
            let (level, name, active) = Self::get_line_info(&line); // Re-parse the consumed line
            if !active {
                continue;
            }
            let current_path = parent.path.join(name);

            // To determine if the current line is a file or a directory, we peek at the *next* line.
            // If the next line is more indented, the current line must be a directory.
            let is_directory = if let Some(next_line) = lines.peek() {
                let (next_level, _, _) = Self::get_line_info(&next_line);
                next_level > level
            } else {
                false // No more lines, so it must be a file.
            };

            if is_directory {
                let mut dir_node = LayoutTreeNode {
                    path: current_path,
                    children: Vec::new(),
                };
                Self::parse_level(&mut dir_node, lines, level as isize);
                parent.children.push(dir_node);
            } else {
                parent.children.push(LayoutTreeNode {
                    path: current_path,
                    children: Vec::new(),
                });
            }
        }
    }

    /// A helper function to extract the indentation level and name from a single line.
    ///
    /// Example: `│   ├── .cargo-lock` -> `(1, ".cargo-lock")`
    fn get_line_info(line: &str) -> (usize, &str, bool) {
        // Find the index where the name begins. It's after the tree prefix (`├── ` or `└── `).
        let name_start_index = line.rfind(' ').map_or(0, |i| i + 1);
        let name = &line[name_start_index..];
        let mut active = true;

        // The indentation level is calculated by the character length of the prefix.
        // Each level of depth adds 4 characters (e.g., `│   ` or `    `).
        let prefix = &line[..name_start_index];
        let level = prefix.chars().count() / 4;

        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let target =
            RE.get_or_init(|| regex::Regex::new(r#"\[target_os=(?<target>[a-z,]+)\]"#).unwrap());

        if let Some(cap) = target.captures(line) {
            macro_rules! t {
                ($name:literal) => {
                    if cfg!(target_os = $name) { true } else { false }
                };
            }
            active = cap["target"].split(",").any(|target| match target {
                "windows" => t!("windows"),
                "linux" => t!("linux"),
                "macos" => t!("macos"),
                _ => panic!("Unsupported target_os {target}"),
            });
        }

        (level, name, active)
    }

    /// Creates a `LayoutTree` by recursively walking a directory structure from a given path.
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        // Canonicalize the root path to ensure consistent, absolute paths.
        let root_path = path.canonicalize()?;

        // This map stores fully constructed directory nodes.
        // Key: The canonical path of a directory.
        // Value: The LayoutTreeNode for that directory.
        let mut completed_nodes: HashMap<PathBuf, LayoutTreeNode> = HashMap::new();

        // Use a post-order traversal (`contents_first`). This ensures that when we
        // visit a directory, all of its descendant nodes have already been built
        // and placed in the `completed_nodes` map.
        for entry in WalkDir::new(&root_path).contents_first(true) {
            let entry = entry?;
            let current_path = entry.path();

            // We only need to construct nodes for directories.
            // Files are collected when their parent directory is processed.
            if !entry.file_type().is_dir() {
                continue;
            }

            // Create a new node for the current directory.
            let mut current_node = LayoutTreeNode {
                path: current_path.to_path_buf(),
                children: Vec::new(),
            };

            // Now, find the children of the current directory. We do this by
            // iterating through its contents one level deep.
            for child_entry in std::fs::read_dir(current_path)? {
                let child_entry = child_entry?;
                // Use canonicalize to match the keys in our map.
                let child_path = child_entry.path().canonicalize()?;

                if child_path.is_dir() {
                    // If the child is a directory, its node must already be in our map.
                    // We remove it and add it to the current node's `dirs`.
                    if let Some(child_node) = completed_nodes.remove(&child_path) {
                        current_node.children.push(child_node);
                    }
                } else if child_path.is_file() {
                    // If the child is a file, add its path to the current node's `files`.
                    current_node.children.push(LayoutTreeNode {
                        path: child_path,
                        children: Vec::new(),
                    });
                }
            }

            // Insert the fully constructed node for the current directory into the map.
            completed_nodes.insert(current_path.to_path_buf(), current_node);
        }

        // After the walk, the map should contain exactly one node: the root.
        let mut root_node = completed_nodes.remove(&root_path).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Root node not found after walk; the directory may be empty or invalid.",
            )
        })?;

        fn redact_node(node: &mut LayoutTreeNode) {
            let e2e = assert_e2e();
            let redactions = e2e.redactions();
            let redact_path = |path: &mut PathBuf| {
                let r = redactions.redact(&path.to_string_lossy());
                *path = PathBuf::from(r)
            };

            redact_path(&mut node.path);
            for dir in node.children.iter_mut() {
                redact_node(dir);
            }
        }

        // Walk the tree and add redactions
        redact_node(&mut root_node);

        Ok(LayoutTree { root: root_node })
    }

    pub fn matches_snapshot(&self, snapshot: &Self) -> bool {
        fn matches(n: &LayoutTreeNode, snap: &LayoutTreeNode) -> bool {
            if snap.children.len() != n.children.len() {
                return false;
            }

            let preprocess = |mut path: PathBuf| -> PathBuf {
                // HACK: It would be nice if we could handle redactions in a cleaner way.
                if cfg!(not(target_os = "windows")) {
                    if path.to_str().unwrap_or_default().ends_with("[EXE]") {
                        let mut p = path.to_string_lossy().to_string();
                        p.truncate(p.len() - "[EXE]".len());
                        path = PathBuf::from(p);
                    }
                }

                path
            };

            // TODO: Check for children with no children

            for d in &n.children {
                let mut found = false;
                for potential_match in snap
                    .children
                    .iter()
                    .filter(|p| preprocess(p.path.clone()) == preprocess(d.path.clone()))
                {
                    // println!(
                    //     "checking potential match {}",
                    //     potential_match.path.display()
                    // );
                    if matches(&d, potential_match) {
                        found = true;
                        // TODO: Maybe mark this "match" as used
                        break;
                    }
                }

                if !found {
                    // println!("missing {:?} -- {:#?}", d.path, snap);
                    return false;
                }
            }

            return true;
        }

        return matches(&self.root, &snapshot.root);
    }
}

impl fmt::Display for LayoutTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.root.path.display())?;

        Self::format_children(f, &self.root, "")?;

        Ok(())
    }
}

impl LayoutTree {
    /// A recursive helper function to format the children of a `LayoutTreeNode`.
    ///
    /// - `f`: The formatter to write to.
    /// - `node`: The parent node whose children are being formatted.
    /// - `prefix`: The string prefix (e.g., "│   ") for indentation.
    fn format_children(
        f: &mut fmt::Formatter<'_>,
        node: &LayoutTreeNode,
        prefix: &str,
    ) -> fmt::Result {
        let mut children: Vec<_> = node.children.iter().collect();

        children.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));

        let num_children = children.len();
        for (i, child) in children.iter().enumerate() {
            let is_last = i == num_children - 1;

            let connector = if is_last { "└── " } else { "├── " };
            let next_level_prefix = if is_last { "    " } else { "│   " };

            writeln!(
                f,
                "{}{}{}",
                prefix,
                connector,
                child.path.file_name().unwrap().to_string_lossy()
            )?;

            if !child.children.is_empty() {
                let new_prefix = format!("{}{}", prefix, next_level_prefix);
                Self::format_children(f, child, &new_prefix)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_parse_tree() {
        let input = r#"
[ROOT]/foo/build-dir
├── .rustc_info.json
├── CACHEDIR.TAG
├── debug
│   ├── .cargo-lock
│   └── .fingerprint
│       └── foo-[HASH]
│           ├── dep-test-integration-test-foo
│           └── invoked.timestamp
└── tmp
    └── foo.txt
"#;

        let parsed_tree = LayoutTree::parse(input);

        // Build the expected structure manually for comparison.
        let root = PathBuf::from("[ROOT]/foo/build-dir");
        let expected_tree = LayoutTree {
            root: LayoutTreeNode {
                path: root.clone(),
                files: vec![root.join(".rustc_info.json"), root.join("CACHEDIR.TAG")],
                dirs: vec![
                    LayoutTreeNode {
                        path: root.join("debug"),
                        files: vec![root.join("debug/.cargo-lock")],
                        dirs: vec![LayoutTreeNode {
                            path: root.join("debug/.fingerprint"),
                            files: vec![],
                            dirs: vec![LayoutTreeNode {
                                path: root.join("debug/.fingerprint/foo-[HASH]"),
                                files: vec![
                                root.join(
                                    "debug/.fingerprint/foo-[HASH]/dep-test-integration-test-foo",
                                ),
                                root.join("debug/.fingerprint/foo-[HASH]/invoked.timestamp"),
                            ],
                                dirs: vec![],
                            }],
                        }],
                    },
                    LayoutTreeNode {
                        path: root.join("tmp"),
                        files: vec![root.join("tmp/foo.txt")],
                        dirs: vec![],
                    },
                ],
            },
        };

        println!("{:#?}", parsed_tree);
        assert!(parsed_tree.matches_snapshot(&expected_tree))
    }

    #[test]
    fn test_to_string_round_trip() {
        // An input string where children are NOT alphabetically sorted.
        let input = r#"
[ROOT]/foo/build-dir
├── .rustc_info.json
├── CACHEDIR.TAG
├── debug
│   ├── .cargo-lock
│   └── .fingerprint
└── tmp
    └── foo.txt
"#;

        let parsed_tree = LayoutTree::parse(input);
        let result_string = parsed_tree.to_string();

        // The generated string should match the canonical, sorted version.
        assert_eq!(result_string.trim(), input.trim());
    }
}
