//! A minimal module system (Phase 3). `from module import name, …` loads a
//! referenced `.mojo` file and **hoists its top-level declarations** (`def` /
//! `struct` / `trait` / `comptime`) into a single flat program, with the import
//! statement removed. Because the result is an ordinary `Vec<Stmt>`, the rest of
//! the pipeline (checker → ownership → VM) is unchanged — a module is just more
//! declarations spliced in ahead of the code that uses them.
//!
//! Scope of this first increment:
//! - **`from module import Name, …`** and **`from module import *`** are resolved.
//!   A plain **`import module`** is left as a no-op (qualified `module.Name` access
//!   isn't modeled yet).
//! - Relative module paths resolve from the importing file's directory:
//!   `from .m import X` / `from ..pkg import X` climb from the importing directory
//!   (each leading dot beyond the first goes up one level). Absolute imports first
//!   try the importing directory, then configured search roots such as the bundled
//!   `stdlib/`, so `from std.collections.list import List` works without
//!   repository-relative dot paths.
//! - A module's imports are resolved first (its dependencies' declarations land
//!   ahead of its own), with **dedup + cycle-breaking** by canonical path. A
//!   module's top-level executable statements and its `main()` are **not** hoisted.
//! - Deferred: `as` aliases on imported names, plain `import module` (qualified
//!   access), and running a module's top-level code.

use crate::ast::{Stmt, StmtKind};
use crate::error::ParseError;
use crate::parse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// An error from resolving/loading modules.
#[derive(Debug)]
pub enum ModuleError {
    /// A module file could not be read.
    Io { module: String, err: std::io::Error },
    /// A module file failed to parse.
    Parse { module: String, err: ParseError },
    /// `from module import Name` where `Name` isn't a top-level declaration of it.
    NameNotFound { module: String, name: String },
    /// An import with no module path (e.g. `from . import x`), not yet supported.
    EmptyModulePath,
}

/// Options for module linking.
#[derive(Debug, Clone)]
pub struct LinkOptions {
    /// Additional roots searched for absolute imports after the importing file's
    /// own directory. Each root is a directory containing module files/packages.
    pub search_roots: Vec<PathBuf>,
}

impl Default for LinkOptions {
    fn default() -> Self {
        let mut search_roots = Vec::new();
        if let Some(root) = option_env!("CARGO_MANIFEST_DIR") {
            search_roots.push(Path::new(root).join("stdlib"));
        }
        LinkOptions { search_roots }
    }
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleError::Io { module, err } => {
                write!(f, "cannot load module '{module}': {err}")
            }
            ModuleError::Parse { module, err } => {
                write!(f, "in module '{module}': {err}")
            }
            ModuleError::NameNotFound { module, name } => {
                write!(f, "module '{module}' has no declaration named '{name}'")
            }
            ModuleError::EmptyModulePath => {
                write!(
                    f,
                    "'from . import …' (empty module path) is not supported yet"
                )
            }
        }
    }
}

/// Link `entry_path` and its transitively-imported modules into one flat program:
/// the imported declarations (dependencies first), then the entry file's own
/// statements (with its `from … import …` statements removed).
pub fn link(entry_path: &Path) -> Result<Vec<Stmt>, ModuleError> {
    link_with_options(entry_path, LinkOptions::default())
}

/// Link with explicit module search options. The entry file's directory is
/// always searched first for absolute imports; `options.search_roots` are tried
/// after that.
pub fn link_with_options(
    entry_path: &Path,
    options: LinkOptions,
) -> Result<Vec<Stmt>, ModuleError> {
    let mut linker = Linker::new(options);
    let program = read_and_parse(entry_path)?;
    let dir = entry_path.parent().unwrap_or_else(|| Path::new("."));
    let body = linker.resolve_entry(program, dir)?;
    let mut result = linker.decls;
    result.extend(body);
    Ok(result)
}

/// Link a program that was already read from `source` at `entry_path` (so the CLI
/// doesn't read the file twice). Equivalent to [`link`] otherwise.
pub fn link_source(source: &str, entry_path: &Path) -> Result<Vec<Stmt>, ModuleError> {
    link_source_with_options(source, entry_path, LinkOptions::default())
}

/// Link an already-read source string with explicit module search options.
pub fn link_source_with_options(
    source: &str,
    entry_path: &Path,
    options: LinkOptions,
) -> Result<Vec<Stmt>, ModuleError> {
    let program = parse(source).map_err(|err| ModuleError::Parse {
        module: display(entry_path),
        err,
    })?;
    let mut linker = Linker::new(options);
    let dir = entry_path.parent().unwrap_or_else(|| Path::new("."));
    let body = linker.resolve_entry(program, dir)?;
    let mut result = linker.decls;
    result.extend(body);
    Ok(result)
}

struct Linker {
    options: LinkOptions,
    /// Module files already hoisted (canonical path) — dedup + cycle break.
    loaded: HashSet<PathBuf>,
    /// The top-level declaration names each loaded module exposes (for validating
    /// `from module import Name`).
    exports: HashMap<PathBuf, HashSet<String>>,
    /// Hoisted declarations from all loaded modules, in dependency order.
    decls: Vec<Stmt>,
}

impl Linker {
    fn new(options: LinkOptions) -> Self {
        Linker {
            options,
            loaded: HashSet::new(),
            exports: HashMap::new(),
            decls: Vec::new(),
        }
    }

    /// Resolve the entry program's imports (loading their modules) and return its
    /// own non-import statements (declarations + top-level code + `main`).
    fn resolve_entry(&mut self, program: Vec<Stmt>, dir: &Path) -> Result<Vec<Stmt>, ModuleError> {
        let mut body = Vec::new();
        for stmt in program {
            match &stmt.kind {
                StmtKind::FromImport { level, path, names } => {
                    let (module_path, module_name) = self.resolve_module(dir, *level, path)?;
                    self.load_module(&module_path, &module_name)?;
                    self.check_names(&module_path, &module_name, names)?;
                }
                // A plain `import module` stays a no-op (qualified access deferred).
                _ => body.push(stmt),
            }
        }
        Ok(body)
    }

    /// Load a module file (once): resolve its own imports first, then hoist its
    /// top-level declarations (excluding `main`).
    fn load_module(&mut self, path: &Path, module_name: &str) -> Result<(), ModuleError> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !self.loaded.insert(canon.clone()) {
            return Ok(()); // already loaded (or a cycle) — declarations are in place
        }
        let program = read_and_parse_named(path, module_name)?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        // Resolve this module's imports first, so a dependency's declarations are
        // spliced in ahead of this module's (the checker binds names in order).
        for stmt in &program {
            if let StmtKind::FromImport {
                level, path: mpath, ..
            } = &stmt.kind
            {
                let (dep_path, dep_name) = self.resolve_module(dir, *level, mpath)?;
                self.load_module(&dep_path, &dep_name)?;
            }
        }
        let mut names = HashSet::new();
        for stmt in program {
            if let Some(name) = declared_name(&stmt) {
                if name == "main" {
                    continue; // a module's `main` is not part of its API
                }
                names.insert(name.to_string());
                self.decls.push(stmt);
            }
        }
        self.exports.insert(canon, names);
        Ok(())
    }

    /// Verify each `from module import Name` names a declaration the module exposes
    /// (a wildcard `*` is unconditional).
    fn check_names(
        &self,
        path: &Path,
        module_name: &str,
        names: &crate::ast::ImportNames,
    ) -> Result<(), ModuleError> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let exports = self.exports.get(&canon);
        if let crate::ast::ImportNames::Names(list) = names {
            for item in list {
                let known = exports.is_some_and(|e| e.contains(&item.name));
                if !known {
                    return Err(ModuleError::NameNotFound {
                        module: module_name.to_string(),
                        name: item.name.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    fn resolve_module(
        &self,
        from_dir: &Path,
        level: usize,
        path: &[String],
    ) -> Result<(PathBuf, String), ModuleError> {
        module_file(from_dir, level, path, &self.options.search_roots)
    }
}

/// The declared name of a top-level declaration statement (`def`/`struct`/`trait`/
/// `comptime`), or `None` for anything else.
fn declared_name(stmt: &Stmt) -> Option<&str> {
    match &stmt.kind {
        StmtKind::Def { name, .. }
        | StmtKind::Struct { name, .. }
        | StmtKind::Trait { name, .. }
        | StmtKind::Comptime { name, .. } => Some(name),
        _ => None,
    }
}

/// Resolve an import's module to a file path, returning `(path, display_name)`.
/// `level` is the leading-dot count (0 = relative to `from_dir`; 1 = same package,
/// i.e. also `from_dir`; each extra level climbs one directory).
fn module_file(
    from_dir: &Path,
    level: usize,
    path: &[String],
    search_roots: &[PathBuf],
) -> Result<(PathBuf, String), ModuleError> {
    if path.is_empty() {
        return Err(ModuleError::EmptyModulePath);
    }
    let display_name = path.join(".");
    if level > 0 {
        let mut base = from_dir.to_path_buf();
        for _ in 1..level {
            base.pop();
        }
        return Ok((module_path_under(base, path), display_name));
    }

    let local = module_path_under(from_dir.to_path_buf(), path);
    if local.exists() {
        return Ok((local, display_name));
    }
    for root in search_roots {
        let candidate = module_path_under(root.clone(), path);
        if candidate.exists() {
            return Ok((candidate, display_name));
        }
    }
    Ok((local, display_name))
}

fn module_path_under(mut base: PathBuf, path: &[String]) -> PathBuf {
    for part in path {
        base.push(part);
    }
    base.set_extension("mojo");
    base
}

fn read_and_parse(path: &Path) -> Result<Vec<Stmt>, ModuleError> {
    read_and_parse_named(path, &display(path))
}

fn read_and_parse_named(path: &Path, module_name: &str) -> Result<Vec<Stmt>, ModuleError> {
    let src = std::fs::read_to_string(path).map_err(|err| ModuleError::Io {
        module: module_name.to_string(),
        err,
    })?;
    parse(&src).map_err(|err| ModuleError::Parse {
        module: module_name.to_string(),
        err,
    })
}

fn display(path: &Path) -> String {
    path.display().to_string()
}
