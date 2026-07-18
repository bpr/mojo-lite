//! Stage 1 source-module and package linking. Imports load `.mojo` modules or
//! package `__init__.mojo` files, assign declarations collision-free internal
//! names, resolve qualified and selective aliases, and erase module objects before
//! the flat checked-program pipeline. Imports are lexical, dependencies are
//! deduplicated by canonical path, and source provenance survives rewriting.

use crate::ast::{Expr, ExprKind, ImportNames, ParamArg, Stmt, StmtKind, TStringPart, Type};
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
    /// An empty module path used with a form other than named sibling imports.
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
                    "an empty relative module path requires named sibling imports"
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
    let mut body = linker.resolve_entry(program, dir)?;
    let entry_module = display(entry_path);
    crate::ast::stamp_source(&mut body, &entry_module);
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
    let mut body = linker.resolve_entry(program, dir)?;
    let entry_module = display(entry_path);
    crate::ast::stamp_source(&mut body, &entry_module);
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
    exports: HashMap<PathBuf, HashMap<String, String>>,
    /// Namespace-valued imports exposed by each module. Keys are relative
    /// qualified paths (`sub`, `sub.nested`) and values are the declarations
    /// visible through that namespace. This is what lets a package initializer
    /// deliberately re-export a submodule without making every file below the
    /// package directory an implicit member.
    namespace_exports: HashMap<PathBuf, HashMap<String, HashMap<String, String>>>,
    /// Hoisted declarations from all loaded modules, in dependency order.
    decls: Vec<Stmt>,
}

impl Linker {
    fn new(options: LinkOptions) -> Self {
        Linker {
            options,
            loaded: HashSet::new(),
            exports: HashMap::new(),
            namespace_exports: HashMap::new(),
            decls: Vec::new(),
        }
    }

    /// Resolve the entry program's imports (loading their modules) and return its
    /// own non-import statements (declarations + top-level code + `main`).
    fn resolve_entry(&mut self, program: Vec<Stmt>, dir: &Path) -> Result<Vec<Stmt>, ModuleError> {
        let uses_kwargs = program_uses_kwargs(&program);
        if uses_kwargs && let Some(root) = option_env!("CARGO_MANIFEST_DIR") {
            let runtime = Path::new(root).join("stdlib/std/collections/string_dict.mojo");
            self.load_module(&runtime, "std.collections.string_dict")?;
        }
        let mut bindings = HashMap::new();
        let mut namespaces = HashMap::new();
        if uses_kwargs && let Some(root) = option_env!("CARGO_MANIFEST_DIR") {
            let runtime = Path::new(root).join("stdlib/std/collections/string_dict.mojo");
            if let Some(target) = self.exports[&canonical(&runtime)].get("StringDict") {
                bindings.insert("StringDict".to_string(), target.clone());
            }
        }
        let mut body = Vec::new();
        for stmt in program {
            match &stmt.kind {
                StmtKind::FromImport { level, path, names } => {
                    self.apply_from_import(
                        dir,
                        *level,
                        path,
                        names,
                        &mut bindings,
                        &mut namespaces,
                    )?;
                }
                StmtKind::Import { path, alias } => {
                    self.apply_import(dir, path, alias.as_deref(), &mut namespaces)?;
                }
                _ => body.push(stmt),
            }
        }
        self.resolve_scoped_imports(&mut body, dir, &bindings, &namespaces)?;
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
        let mut bindings = HashMap::new();
        let mut namespaces = HashMap::new();
        if module_name != "std.collections.string_dict"
            && program_uses_kwargs(&program)
            && let Some(root) = option_env!("CARGO_MANIFEST_DIR")
        {
            let runtime = Path::new(root).join("stdlib/std/collections/string_dict.mojo");
            self.load_module(&runtime, "std.collections.string_dict")?;
            if let Some(target) = self.exports[&canonical(&runtime)].get("StringDict") {
                bindings.insert("StringDict".to_string(), target.clone());
            }
        }
        for stmt in &program {
            match &stmt.kind {
                StmtKind::FromImport {
                    level,
                    path: mpath,
                    names,
                } => {
                    self.apply_from_import(
                        dir,
                        *level,
                        mpath,
                        names,
                        &mut bindings,
                        &mut namespaces,
                    )?;
                }
                StmtKind::Import { path: mpath, alias } => {
                    self.apply_import(dir, mpath, alias.as_deref(), &mut namespaces)?;
                }
                _ => {}
            }
        }
        let mut local = HashMap::new();
        for stmt in &program {
            if let Some(name) = declared_name(stmt) {
                if name == "main" {
                    continue;
                }
                let linked_name =
                    if module_name == "std.collections.string_dict" && name == "StringDict" {
                        name.to_string()
                    } else {
                        qualified(module_name, name)
                    };
                local.insert(name.to_string(), linked_name);
            }
        }
        bindings.extend(local.clone());
        let mut declarations: Vec<_> = program
            .into_iter()
            .filter(|stmt| declared_name(stmt).is_some_and(|name| name != "main"))
            .collect();
        self.resolve_scoped_imports(&mut declarations, dir, &bindings, &namespaces)?;
        rewrite_program(&mut declarations, &bindings, &namespaces);
        for mut stmt in declarations {
            crate::ast::stamp_source(std::slice::from_mut(&mut stmt), &display(path));
            self.decls.push(stmt);
        }
        let mut exports = local;
        for (name, target) in bindings {
            if !name.starts_with('_') && !exports.contains_key(&name) {
                exports.insert(name, target);
            }
        }
        self.exports.insert(canon, exports);
        let visible_namespaces = namespaces
            .into_iter()
            .filter(|(name, _)| !name.split('.').next().is_some_and(|p| p.starts_with('_')))
            .collect();
        self.namespace_exports
            .insert(canonical(path), visible_namespaces);
        Ok(())
    }

    /// Bind an ordinary `import a.b.c` namespace. Current Mojo binds every
    /// prefix for an unaliased dotted import; an aliased import binds only the
    /// alias. Prefixes which are ordinary directories are still valid namespace
    /// objects even when they do not contain an `__init__.mojo` file.
    fn apply_import(
        &mut self,
        dir: &Path,
        path: &[String],
        alias: Option<&str>,
        namespaces: &mut HashMap<String, HashMap<String, String>>,
    ) -> Result<(), ModuleError> {
        let (module_path, module_name) = self.resolve_module(dir, 0, path)?;
        self.load_module(&module_path, &module_name)?;
        if let Some(alias) = alias {
            self.bind_namespace_tree(&module_path, alias, namespaces);
            return Ok(());
        }

        for end in 1..path.len() {
            let prefix = &path[..end];
            let (prefix_path, prefix_name) = self.resolve_module(dir, 0, prefix)?;
            if prefix_path.exists() {
                self.load_module(&prefix_path, &prefix_name)?;
                self.bind_namespace_tree(&prefix_path, &prefix.join("."), namespaces);
            } else {
                namespaces.entry(prefix.join(".")).or_default();
            }
        }
        self.bind_namespace_tree(&module_path, &path.join("."), namespaces);
        Ok(())
    }

    fn bind_namespace_tree(
        &self,
        path: &Path,
        local: &str,
        namespaces: &mut HashMap<String, HashMap<String, String>>,
    ) {
        let canon = canonical(path);
        namespaces.insert(local.to_string(), self.exports[&canon].clone());
        if let Some(children) = self.namespace_exports.get(&canon) {
            for (suffix, exports) in children {
                namespaces.insert(format!("{local}.{suffix}"), exports.clone());
            }
        }
    }

    fn bind_from_imports(
        &self,
        path: &Path,
        names: &ImportNames,
        bindings: &mut HashMap<String, String>,
        namespaces: &mut HashMap<String, HashMap<String, String>>,
    ) {
        let canon = canonical(path);
        let exports = &self.exports[&canon];
        let namespace_exports = self.namespace_exports.get(&canon);
        match names {
            ImportNames::Wildcard => {
                bindings.extend(
                    exports
                        .iter()
                        .filter(|(n, _)| !n.starts_with('_'))
                        .map(|(n, t)| (n.clone(), t.clone())),
                );
                if let Some(children) = namespace_exports {
                    for (name, child_exports) in children {
                        if !name.split('.').next().is_some_and(|p| p.starts_with('_')) {
                            namespaces.insert(name.clone(), child_exports.clone());
                        }
                    }
                }
            }
            ImportNames::Names(items) => {
                for item in items {
                    let local = item.alias.clone().unwrap_or_else(|| item.name.clone());
                    if let Some(target) = exports.get(&item.name) {
                        bindings.insert(local, target.clone());
                    } else if let Some(children) = namespace_exports
                        && let Some(child_exports) = children.get(&item.name)
                    {
                        namespaces.insert(local.clone(), child_exports.clone());
                        let prefix = format!("{}.", item.name);
                        for (name, exports) in children {
                            if let Some(suffix) = name.strip_prefix(&prefix) {
                                namespaces.insert(format!("{local}.{suffix}"), exports.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    fn apply_from_import(
        &mut self,
        dir: &Path,
        level: usize,
        path: &[String],
        names: &ImportNames,
        bindings: &mut HashMap<String, String>,
        namespaces: &mut HashMap<String, HashMap<String, String>>,
    ) -> Result<(), ModuleError> {
        if path.is_empty() {
            let ImportNames::Names(items) = names else {
                return Err(ModuleError::EmptyModulePath);
            };
            for item in items {
                let submodule = vec![item.name.clone()];
                let (module_path, module_name) = self.resolve_module(dir, level, &submodule)?;
                self.load_module(&module_path, &module_name)?;
                self.bind_namespace_tree(
                    &module_path,
                    &item.alias.clone().unwrap_or_else(|| item.name.clone()),
                    namespaces,
                );
            }
            return Ok(());
        }
        let (module_path, module_name) = self.resolve_module(dir, level, path)?;
        self.load_module(&module_path, &module_name)?;
        self.check_names(&module_path, &module_name, names)?;
        self.bind_from_imports(&module_path, names, bindings, namespaces);
        Ok(())
    }

    fn resolve_scoped_imports(
        &mut self,
        body: &mut Vec<Stmt>,
        dir: &Path,
        inherited_bindings: &HashMap<String, String>,
        inherited_namespaces: &HashMap<String, HashMap<String, String>>,
    ) -> Result<(), ModuleError> {
        let mut bindings = inherited_bindings.clone();
        let mut namespaces = inherited_namespaces.clone();
        let mut resolved = Vec::with_capacity(body.len());
        for mut statement in std::mem::take(body) {
            match &statement.kind {
                StmtKind::FromImport { level, path, names } => {
                    self.apply_from_import(
                        dir,
                        *level,
                        path,
                        names,
                        &mut bindings,
                        &mut namespaces,
                    )?;
                }
                StmtKind::Import { path, alias } => {
                    self.apply_import(dir, path, alias.as_deref(), &mut namespaces)?;
                }
                _ => {
                    self.resolve_imports_in_statement(&mut statement, dir, &bindings, &namespaces)?;
                    rewrite_stmt(&mut statement, &bindings, &namespaces);
                    if let Some(local) = lexical_binding_name(&statement) {
                        bindings.remove(local);
                        remove_namespace_binding(&mut namespaces, local);
                    }
                    resolved.push(statement);
                }
            }
        }
        *body = resolved;
        Ok(())
    }

    fn resolve_imports_in_statement(
        &mut self,
        statement: &mut Stmt,
        dir: &Path,
        bindings: &HashMap<String, String>,
        namespaces: &HashMap<String, HashMap<String, String>>,
    ) -> Result<(), ModuleError> {
        match &mut statement.kind {
            StmtKind::Def { params, body, .. } => {
                let local_bindings = without_local_bindings(
                    bindings,
                    params.iter().map(|param| param.name.clone()),
                    body,
                );
                let local_namespaces = without_local_namespaces(
                    namespaces,
                    params.iter().map(|param| param.name.clone()),
                    body,
                );
                self.resolve_scoped_imports(body, dir, &local_bindings, &local_namespaces)?
            }
            StmtKind::Struct { methods, .. } => {
                for method in methods {
                    let local_bindings = without_local_bindings(
                        bindings,
                        method.params.iter().map(|param| param.name.clone()),
                        &method.body,
                    );
                    let local_namespaces = without_local_namespaces(
                        namespaces,
                        method.params.iter().map(|param| param.name.clone()),
                        &method.body,
                    );
                    self.resolve_scoped_imports(
                        &mut method.body,
                        dir,
                        &local_bindings,
                        &local_namespaces,
                    )?;
                }
            }
            StmtKind::Trait { methods, .. } => {
                for method in methods {
                    if let Some(body) = &mut method.default_body {
                        self.resolve_scoped_imports(body, dir, bindings, namespaces)?;
                    }
                }
            }
            StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
                for (_, block) in branches {
                    self.resolve_scoped_imports(block, dir, bindings, namespaces)?;
                }
                if let Some(block) = orelse {
                    self.resolve_scoped_imports(block, dir, bindings, namespaces)?;
                }
            }
            StmtKind::While { body, .. }
            | StmtKind::For { body, .. }
            | StmtKind::ComptimeFor { body, .. }
            | StmtKind::With { body, .. } => {
                self.resolve_scoped_imports(body, dir, bindings, namespaces)?
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                self.resolve_scoped_imports(body, dir, bindings, namespaces)?;
                if let Some((_, block)) = except {
                    self.resolve_scoped_imports(block, dir, bindings, namespaces)?;
                }
                if let Some(block) = orelse {
                    self.resolve_scoped_imports(block, dir, bindings, namespaces)?;
                }
                if let Some(block) = finalbody {
                    self.resolve_scoped_imports(block, dir, bindings, namespaces)?;
                }
            }
            _ => {}
        }
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
                let known = exports.is_some_and(|e| e.contains_key(&item.name))
                    || self
                        .namespace_exports
                        .get(&canon)
                        .is_some_and(|e| e.contains_key(&item.name));
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

fn remove_namespace_binding(
    namespaces: &mut HashMap<String, HashMap<String, String>>,
    local: &str,
) {
    namespaces.retain(|name, _| name != local && !name.starts_with(&format!("{local}.")));
}

fn lexical_binding_name(statement: &Stmt) -> Option<&str> {
    match &statement.kind {
        StmtKind::VarDecl { name, .. }
        | StmtKind::RefDecl { name, .. }
        | StmtKind::Def { name, .. }
        | StmtKind::Struct { name, .. }
        | StmtKind::Trait { name, .. }
        | StmtKind::Comptime { name, .. } => Some(name),
        _ => None,
    }
}

fn program_uses_kwargs(program: &[Stmt]) -> bool {
    program.iter().any(|stmt| match &stmt.kind {
        StmtKind::Def { params, body, .. } => {
            params
                .iter()
                .any(|p| p.kind == crate::ast::ParamKind::KwVariadic)
                || program_uses_kwargs(body)
        }
        StmtKind::Struct { methods, .. } => methods.iter().any(|method| {
            method
                .params
                .iter()
                .any(|p| p.kind == crate::ast::ParamKind::KwVariadic)
                || program_uses_kwargs(&method.body)
        }),
        StmtKind::If { branches, orelse } => {
            branches.iter().any(|(_, body)| program_uses_kwargs(body))
                || orelse
                    .as_ref()
                    .is_some_and(|body| program_uses_kwargs(body))
        }
        StmtKind::While { body, .. }
        | StmtKind::For { body, .. }
        | StmtKind::ComptimeFor { body, .. } => program_uses_kwargs(body),
        _ => false,
    })
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
    let package = base.join("__init__.mojo");
    if package.exists() {
        package
    } else {
        base.with_extension("mojo")
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn qualified(module: &str, name: &str) -> String {
    format!(
        "__module${}${name}",
        module.replace(|c: char| !c.is_ascii_alphanumeric(), "$")
    )
}

fn rewrite_program(
    body: &mut [Stmt],
    names: &HashMap<String, String>,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    for stmt in body {
        rewrite_stmt(stmt, names, namespaces);
    }
}

fn rename(name: &mut String, names: &HashMap<String, String>) {
    if let Some(replacement) = names.get(name) {
        *name = replacement.clone();
    }
}

fn expression_path(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Identifier(name) => Some(name.clone()),
        ExprKind::Member { object, field } => {
            Some(format!("{}.{}", expression_path(object)?, field))
        }
        _ => None,
    }
}

fn namespace_member(
    object: &Expr,
    member: &str,
    namespaces: &HashMap<String, HashMap<String, String>>,
) -> Option<String> {
    namespaces
        .get(&expression_path(object)?)
        .and_then(|exports| exports.get(member))
        .cloned()
}

fn without_local_bindings(
    names: &HashMap<String, String>,
    params: impl IntoIterator<Item = String>,
    body: &[Stmt],
) -> HashMap<String, String> {
    let mut visible = names.clone();
    for name in params {
        visible.remove(&name);
    }
    remove_bound_names(body, &mut visible);
    visible
}

fn without_local_namespaces(
    namespaces: &HashMap<String, HashMap<String, String>>,
    params: impl IntoIterator<Item = String>,
    body: &[Stmt],
) -> HashMap<String, HashMap<String, String>> {
    let mut visible = namespaces.clone();
    for name in params {
        remove_namespace_binding(&mut visible, &name);
    }
    let mut bound = HashMap::new();
    collect_bound_names(body, &mut bound);
    for name in bound.keys() {
        remove_namespace_binding(&mut visible, name);
    }
    visible
}

fn collect_bound_names<'a>(body: &'a [Stmt], names: &mut HashMap<&'a str, ()>) {
    for statement in body {
        match &statement.kind {
            StmtKind::VarDecl { name, .. }
            | StmtKind::RefDecl { name, .. }
            | StmtKind::Def { name, .. }
            | StmtKind::Struct { name, .. }
            | StmtKind::Trait { name, .. }
            | StmtKind::Comptime { name, .. } => {
                names.insert(name, ());
            }
            StmtKind::For { var, body, .. } | StmtKind::ComptimeFor { var, body, .. } => {
                names.insert(var, ());
                collect_bound_names(body, names);
            }
            StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
                for (_, block) in branches {
                    collect_bound_names(block, names);
                }
                if let Some(block) = orelse {
                    collect_bound_names(block, names);
                }
            }
            StmtKind::While { body, .. } | StmtKind::With { body, .. } => {
                collect_bound_names(body, names)
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                collect_bound_names(body, names);
                if let Some((binding, block)) = except {
                    if let Some(binding) = binding {
                        names.insert(binding, ());
                    }
                    collect_bound_names(block, names);
                }
                if let Some(block) = orelse {
                    collect_bound_names(block, names);
                }
                if let Some(block) = finalbody {
                    collect_bound_names(block, names);
                }
            }
            _ => {}
        }
    }
}

fn remove_bound_names(body: &[Stmt], names: &mut HashMap<String, String>) {
    for statement in body {
        match &statement.kind {
            StmtKind::VarDecl { name, .. }
            | StmtKind::RefDecl { name, .. }
            | StmtKind::Def { name, .. }
            | StmtKind::Struct { name, .. }
            | StmtKind::Trait { name, .. } => {
                names.remove(name);
            }
            StmtKind::For { var, body, .. } | StmtKind::ComptimeFor { var, body, .. } => {
                names.remove(var);
                remove_bound_names(body, names);
            }
            StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
                for (_, block) in branches {
                    remove_bound_names(block, names);
                }
                if let Some(block) = orelse {
                    remove_bound_names(block, names);
                }
            }
            StmtKind::While { body, .. } | StmtKind::With { body, .. } => {
                remove_bound_names(body, names)
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                remove_bound_names(body, names);
                if let Some((binding, block)) = except {
                    if let Some(binding) = binding {
                        names.remove(binding);
                    }
                    remove_bound_names(block, names);
                }
                if let Some(block) = orelse {
                    remove_bound_names(block, names);
                }
                if let Some(block) = finalbody {
                    remove_bound_names(block, names);
                }
            }
            _ => {}
        }
    }
}

fn rewrite_type(
    ty: &mut Type,
    names: &HashMap<String, String>,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    match ty {
        Type::Named(name, args) => {
            rename(name, names);
            rewrite_args(args, names, namespaces);
        }
        Type::Assoc { base, name } => {
            if let Some(namespace) = type_path(base)
                && let Some(target) = namespaces
                    .get(&namespace)
                    .and_then(|exports| exports.get(name))
            {
                *ty = Type::Named(target.clone(), Vec::new());
            } else {
                rewrite_type(base, names, namespaces);
            }
        }
        Type::Func { params, ret, .. } => {
            for ty in params {
                rewrite_type(ty, names, namespaces);
            }
            rewrite_type(ret, names, namespaces);
        }
        Type::Ref { referent, origin } => {
            rewrite_type(referent, names, namespaces);
            if let Some(origin) = origin {
                for expr in origin {
                    rewrite_expr(expr, names, namespaces);
                }
            }
        }
        _ => {}
    }
}

fn type_path(ty: &Type) -> Option<String> {
    match ty {
        Type::Named(name, args) if args.is_empty() => Some(name.clone()),
        Type::Assoc { base, name } => Some(format!("{}.{}", type_path(base)?, name)),
        _ => None,
    }
}

fn rewrite_args(
    args: &mut [ParamArg],
    names: &HashMap<String, String>,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    for arg in args {
        match arg {
            ParamArg::Type(ty) => rewrite_type(ty, names, namespaces),
            ParamArg::Value(expr) => rewrite_expr(expr, names, namespaces),
            ParamArg::Named { value, .. } => {
                rewrite_args(std::slice::from_mut(value), names, namespaces)
            }
        }
    }
}

fn rewrite_expr(
    expr: &mut Expr,
    names: &HashMap<String, String>,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    match &mut expr.kind {
        ExprKind::Identifier(name) => rename(name, names),
        ExprKind::Call {
            name,
            param_args,
            args,
            kwargs,
        } => {
            rename(name, names);
            rewrite_args(param_args, names, namespaces);
            for arg in args {
                rewrite_expr(arg, names, namespaces);
            }
            for arg in kwargs {
                rewrite_expr(&mut arg.value, names, namespaces);
            }
        }
        ExprKind::MethodCall {
            object,
            method,
            args,
            kwargs,
        } => {
            if let Some(target) = namespace_member(object, method, namespaces) {
                for arg in args.iter_mut() {
                    rewrite_expr(arg, names, namespaces);
                }
                for arg in kwargs.iter_mut() {
                    rewrite_expr(&mut arg.value, names, namespaces);
                }
                expr.kind = ExprKind::Call {
                    name: target,
                    param_args: Vec::new(),
                    args: std::mem::take(args),
                    kwargs: std::mem::take(kwargs),
                };
            } else {
                rewrite_expr(object, names, namespaces);
                for arg in args {
                    rewrite_expr(arg, names, namespaces);
                }
                for arg in kwargs {
                    rewrite_expr(&mut arg.value, names, namespaces);
                }
            }
        }
        ExprKind::Member { object, field } => {
            if let Some(target) = namespace_member(object, field, namespaces) {
                expr.kind = ExprKind::Identifier(target);
            } else {
                rewrite_expr(object, names, namespaces);
            }
        }
        ExprKind::Invoke {
            callee,
            param_args,
            args,
            kwargs,
        } => {
            rewrite_expr(callee, names, namespaces);
            rewrite_args(param_args, names, namespaces);
            for arg in args {
                rewrite_expr(arg, names, namespaces);
            }
            for arg in kwargs {
                rewrite_expr(&mut arg.value, names, namespaces);
            }
        }
        ExprKind::TypeApply { name, args } => {
            rename(name, names);
            rewrite_args(args, names, namespaces);
        }
        ExprKind::Prefix(_, value) | ExprKind::Transfer(value) => {
            rewrite_expr(value, names, namespaces)
        }
        ExprKind::Infix(_, left, right)
        | ExprKind::Index {
            object: left,
            index: right,
        } => {
            rewrite_expr(left, names, namespaces);
            rewrite_expr(right, names, namespaces);
        }
        ExprKind::ListLit(values) | ExprKind::TupleLit(values) => {
            for value in values {
                rewrite_expr(value, names, namespaces);
            }
        }
        ExprKind::BraceLit(entries) => {
            for (key, value) in entries {
                rewrite_expr(key, names, namespaces);
                if let Some(value) = value {
                    rewrite_expr(value, names, namespaces);
                }
            }
        }
        ExprKind::Named { value, .. } => rewrite_expr(value, names, namespaces),
        ExprKind::IfExpr {
            cond,
            then_branch,
            else_branch,
        } => {
            rewrite_expr(cond, names, namespaces);
            rewrite_expr(then_branch, names, namespaces);
            rewrite_expr(else_branch, names, namespaces);
        }
        ExprKind::Compare { first, rest } => {
            rewrite_expr(first, names, namespaces);
            for (_, value) in rest {
                rewrite_expr(value, names, namespaces);
            }
        }
        ExprKind::Slice {
            object,
            lower,
            upper,
            step,
            ..
        } => {
            rewrite_expr(object, names, namespaces);
            for value in [lower, upper, step].into_iter().flatten() {
                rewrite_expr(value, names, namespaces);
            }
        }
        ExprKind::MultiIndex { object, args } => {
            rewrite_expr(object, names, namespaces);
            for argument in args {
                match argument {
                    crate::ast::SubscriptArg::Index(value) => {
                        rewrite_expr(value, names, namespaces)
                    }
                    crate::ast::SubscriptArg::Slice {
                        lower, upper, step, ..
                    } => {
                        for value in [lower, upper, step].into_iter().flatten() {
                            rewrite_expr(value, names, namespaces);
                        }
                    }
                }
            }
        }
        ExprKind::TString { parts, .. } => {
            for part in parts {
                if let TStringPart::Expr(value) = part {
                    rewrite_expr(value, names, namespaces);
                }
            }
        }
        ExprKind::TypeValue(ty) => rewrite_type(ty, names, namespaces),
        _ => {}
    }
}

fn rewrite_stmt(
    stmt: &mut Stmt,
    names: &HashMap<String, String>,
    namespaces: &HashMap<String, HashMap<String, String>>,
) {
    match &mut stmt.kind {
        StmtKind::Def {
            name,
            type_params,
            params,
            raises_type,
            ret,
            body,
            decorators,
            ..
        } => {
            rename(name, names);
            for tp in type_params {
                for bound in &mut tp.bounds {
                    rename(bound, names);
                }
            }
            for p in &mut *params {
                rewrite_type(&mut p.ty, names, namespaces);
                if let Some(v) = &mut p.default {
                    rewrite_expr(v, names, namespaces);
                }
            }
            if let Some(error) = raises_type {
                rewrite_type(error, names, namespaces);
            }
            if let Some(ret) = ret {
                rewrite_type(ret, names, namespaces);
            }
            for d in decorators {
                for a in &mut d.args {
                    rewrite_expr(a, names, namespaces);
                }
            }
            let locals =
                without_local_bindings(names, params.iter().map(|param| param.name.clone()), body);
            let local_namespaces = without_local_namespaces(
                namespaces,
                params.iter().map(|param| param.name.clone()),
                body,
            );
            rewrite_program(body, &locals, &local_namespaces);
        }
        StmtKind::Struct {
            name,
            type_params,
            conforms,
            fields,
            associated,
            methods,
            ..
        } => {
            rename(name, names);
            for tp in type_params {
                for b in &mut tp.bounds {
                    rename(b, names);
                }
            }
            for c in conforms {
                rename(c, names);
            }
            for f in fields {
                rewrite_type(&mut f.ty, names, namespaces);
            }
            for a in associated {
                rewrite_expr(&mut a.value, names, namespaces);
            }
            for m in methods {
                for p in &mut m.params {
                    rewrite_type(&mut p.ty, names, namespaces);
                    if let Some(v) = &mut p.default {
                        rewrite_expr(v, names, namespaces);
                    }
                }
                if let Some(error) = &mut m.raises_type {
                    rewrite_type(error, names, namespaces);
                }
                if let Some(ret) = &mut m.ret {
                    rewrite_type(ret, names, namespaces);
                }
                let locals = without_local_bindings(
                    names,
                    m.params.iter().map(|param| param.name.clone()),
                    &m.body,
                );
                let local_namespaces = without_local_namespaces(
                    namespaces,
                    m.params.iter().map(|param| param.name.clone()),
                    &m.body,
                );
                rewrite_program(&mut m.body, &locals, &local_namespaces);
            }
        }
        StmtKind::Trait {
            name,
            refines,
            methods,
            comptime_members,
        } => {
            rename(name, names);
            for r in refines {
                rename(r, names);
            }
            for m in methods {
                for p in &mut m.params {
                    rewrite_type(&mut p.ty, names, namespaces);
                }
                if let Some(error) = &mut m.raises_type {
                    rewrite_type(error, names, namespaces);
                }
                if let Some(ret) = &mut m.ret {
                    rewrite_type(ret, names, namespaces);
                }
                if let Some(body) = &mut m.default_body {
                    rewrite_program(body, names, namespaces);
                }
            }
            for c in comptime_members {
                rewrite_type(&mut c.ty, names, namespaces);
            }
        }
        StmtKind::VarDecl { ty, value, .. } => {
            if let Some(ty) = ty {
                rewrite_type(ty, names, namespaces);
            }
            rewrite_expr(value, names, namespaces);
        }
        StmtKind::Comptime { name, value } => {
            rename(name, names);
            rewrite_expr(value, names, namespaces);
        }
        StmtKind::RefDecl { value, .. }
        | StmtKind::Assign { value, .. }
        | StmtKind::Raise(value)
        | StmtKind::Expr(value)
        | StmtKind::Return(Some(value)) => rewrite_expr(value, names, namespaces),
        StmtKind::SetPlace { place, value } | StmtKind::AugAssign { place, value, .. } => {
            rewrite_expr(place, names, namespaces);
            rewrite_expr(value, names, namespaces);
        }
        StmtKind::Unpack { targets, value } => {
            for t in targets {
                rewrite_expr(t, names, namespaces);
            }
            rewrite_expr(value, names, namespaces);
        }
        StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
            for (c, b) in branches {
                rewrite_expr(c, names, namespaces);
                rewrite_program(b, names, namespaces);
            }
            if let Some(b) = orelse {
                rewrite_program(b, names, namespaces);
            }
        }
        StmtKind::While { cond, body, .. } => {
            rewrite_expr(cond, names, namespaces);
            rewrite_program(body, names, namespaces);
        }
        StmtKind::For { iter, body, .. } | StmtKind::ComptimeFor { iter, body, .. } => {
            rewrite_expr(iter, names, namespaces);
            rewrite_program(body, names, namespaces);
        }
        StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        } => {
            rewrite_program(body, names, namespaces);
            if let Some((_, b)) = except {
                rewrite_program(b, names, namespaces);
            }
            if let Some(b) = orelse {
                rewrite_program(b, names, namespaces);
            }
            if let Some(b) = finalbody {
                rewrite_program(b, names, namespaces);
            }
        }
        StmtKind::With { items, body } => {
            for i in items {
                rewrite_expr(&mut i.context, names, namespaces);
            }
            rewrite_program(body, names, namespaces);
        }
        _ => {}
    }
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
