//! # The cargo-geiger library.
//!
//! ## How Errors implements Display and why
//!
//! Display is required by Error. Errors in cargo-geiger simply forwards the the
//! implementation of the Display trait to the derived Debug trait. In the
//! general case, proper end-user error message formatting and presentation must
//! be done in the UI layer. To separate data and presentation, the error
//! struct/enum should avoid all formatting and instead only provide structured
//! unformatted error information.

#![forbid(unsafe_code)]

// TODO: Investigate how cargo-clippy is implemented. Is it using syn?
// Is is using rustc? Is it implementing a compiler plugin?

extern crate cargo;
extern crate colored;
extern crate env_logger;
extern crate failure;
extern crate petgraph;
extern crate structopt;
extern crate syn;
extern crate walkdir;

use self::format::Pattern;
use self::walkdir::DirEntry;
use self::walkdir::WalkDir;
use cargo::core::compiler::CompileMode;
use cargo::core::compiler::Executor;
use cargo::core::compiler::Unit;
use cargo::core::dependency::Kind;
use cargo::core::package::PackageSet;
use cargo::core::registry::PackageRegistry;
use cargo::core::resolver::Method;
use cargo::core::shell::Verbosity;
use cargo::core::Target;
use cargo::core::{Package, PackageId, Resolve, Workspace};
use cargo::ops;
use cargo::ops::CleanOptions;
use cargo::ops::CompileOptions;
use cargo::util::paths;
use cargo::util::ProcessBuilder;
use cargo::util::{self, important_paths, CargoResult, Cfg};
use cargo::Config;
use colored::Colorize;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::EdgeDirection;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::io::Read;
use std::ops::Add;
use std::path::Path;
use std::path::PathBuf;
use std::str::{self, FromStr};
use std::string::FromUtf8Error;
use std::sync::Arc;
use std::sync::Mutex;
use syn::{visit, Expr, ImplItemMethod, ItemFn, ItemImpl, ItemMod, ItemTrait};

pub mod format;

#[derive(Debug)]
pub enum ScanFileError {
    Io(io::Error, PathBuf),
    Utf8(FromUtf8Error, PathBuf),
    Syn(syn::Error, PathBuf),
}

impl Error for ScanFileError {}

/// Forward Display to Debug. See the crate root documentation.
impl fmt::Display for ScanFileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[derive(Debug)]
pub enum RsResolveError {
    Walkdir(walkdir::Error),

    /// Like io::Error but with the related path.
    Io(io::Error, PathBuf),

    /// Would like cargo::Error here, but it's private, why?
    /// This is still way better than a panic though.
    Cargo(String),

    /// This should not happen unless incorrect assumptions have been made in
    /// cargo-geiger about how the cargo API works.
    ArcUnwrap(),

    /// Failed to get the inner context out of the mutex.
    InnerContextMutex(String),

    /// Failed to parse a .dep file.
    DepParse(String, PathBuf),
}

impl Error for RsResolveError {}

/// Forward Display to Debug. See the crate root documentation.
impl fmt::Display for RsResolveError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl From<PoisonError<CustomExecutorInnerContext>> for RsResolveError {
    fn from(e: PoisonError<CustomExecutorInnerContext>) -> Self {
        RsResolveError::InnerContextMutex(e.to_string())
    }
}

#[derive(Debug, Default, Clone)]
pub struct Count {
    /// Number of safe items, in .rs files used by the build.
    pub safe: u64,

    /// Number of unsafe items, in .rs files used by the build.
    pub unsafe_: u64,
}

impl Count {
    fn count(&mut self, is_unsafe: bool) {
        match is_unsafe {
            true => self.unsafe_ += 1,
            false => self.safe += 1,
        }
    }
}

impl Add for Count {
    type Output = Count;

    fn add(self, other: Count) -> Count {
        Count {
            safe: self.safe + other.safe,
            unsafe_: self.unsafe_ + other.unsafe_,
        }
    }
}

/// Unsafe usage metrics collection.
#[derive(Debug, Default, Clone)]
pub struct CounterBlock {
    pub functions: Count,
    pub exprs: Count,
    pub item_impls: Count,
    pub item_traits: Count,
    pub methods: Count,
}

impl CounterBlock {
    fn has_unsafe(&self) -> bool {
        self.functions.unsafe_ > 0
            || self.exprs.unsafe_ > 0
            || self.item_impls.unsafe_ > 0
            || self.item_traits.unsafe_ > 0
            || self.methods.unsafe_ > 0
    }
}

impl Add for CounterBlock {
    type Output = CounterBlock;

    fn add(self, other: CounterBlock) -> CounterBlock {
        CounterBlock {
            functions: self.functions + other.functions,
            exprs: self.exprs + other.exprs,
            item_impls: self.item_impls + other.item_impls,
            item_traits: self.item_traits + other.item_traits,
            methods: self.methods + other.methods,
        }
    }
}

#[derive(Debug, Default)]
pub struct PackageCounters {
    /// Unsafe usage included by the build.
    pub used: CounterBlock,

    /// Unsafe usage not included by the build.
    pub not_used: CounterBlock,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum IncludeTests {
    Yes,
    No,
}

struct GeigerSynVisitor {
    /// Count unsafe usage inside tests
    include_tests: IncludeTests,

    /// Metrics storage.
    counters: CounterBlock,

    /// Used by the Visit trait implementation to track the traversal state.
    in_unsafe_block: bool,
}

impl GeigerSynVisitor {
    fn new(include_tests: IncludeTests) -> Self {
        GeigerSynVisitor {
            include_tests,
            counters: Default::default(),
            in_unsafe_block: false,
        }
    }
}

/// TODO: Write documentation.
pub struct GeigerContext {
    pub pack_id_to_counters: HashMap<PackageId, PackageCounters>,
    pub rs_files_used: HashMap<PathBuf, u32>,
}

/// Will return true for #[cfg(test)] decodated modules.
///
/// This function is a somewhat of a hack and will probably missinterpret more
/// advanded cfg expressions. A better way to do this would be to let rustc emit
/// every single source file path and span within each source file and use that
/// as a general filter for included code.
/// TODO: Investigate if the needed information can be emitted by rustc today.
fn is_test_mod(i: &ItemMod) -> bool {
    use syn::Meta;
    i.attrs
        .iter()
        .flat_map(|a| a.interpret_meta())
        .any(|m| match m {
            Meta::List(ml) => meta_list_is_cfg_test(&ml),
            _ => false,
        })
}

// MetaList {
//     ident: Ident(
//         cfg
//     ),
//     paren_token: Paren,
//     nested: [
//         Meta(
//             Word(
//                 Ident(
//                     test
//                 )
//             )
//         )
//     ]
// }
fn meta_list_is_cfg_test(ml: &syn::MetaList) -> bool {
    use syn::NestedMeta;
    if ml.ident != "cfg" {
        return false;
    }
    ml.nested.iter().any(|n| match n {
        NestedMeta::Meta(meta) => meta_is_word_test(meta),
        _ => false,
    })
}

fn meta_is_word_test(m: &syn::Meta) -> bool {
    use syn::Meta;
    match m {
        Meta::Word(ident) => ident == "test",
        _ => false,
    }
}

fn is_test_fn(i: &ItemFn) -> bool {
    i.attrs
        .iter()
        .flat_map(|a| a.interpret_meta())
        .any(|m| meta_is_word_test(&m))
}

impl<'ast> visit::Visit<'ast> for GeigerSynVisitor {
    /// Free-standing functions
    fn visit_item_fn(&mut self, i: &ItemFn) {
        if IncludeTests::No == self.include_tests && is_test_fn(i) {
            return;
        }
        self.counters.functions.count(i.unsafety.is_some());
        visit::visit_item_fn(self, i);
    }

    fn visit_expr(&mut self, i: &Expr) {
        // Total number of expressions of any type
        match i {
            Expr::Unsafe(i) => {
                self.in_unsafe_block = true;
                visit::visit_expr_unsafe(self, i);
                self.in_unsafe_block = false;
            }
            Expr::Path(_) | Expr::Lit(_) => {
                // Do not count. The expression `f(x)` should count as one
                // expression, not three.
            }
            other => {
                // TODO: Print something pretty here or gather the data for later
                // printing.
                // if self.verbosity == Verbosity::Verbose && self.in_unsafe_block {
                //     println!("{:#?}", other);
                // }
                self.counters.exprs.count(self.in_unsafe_block);
                visit::visit_expr(self, other);
            }
        }
    }

    fn visit_item_mod(&mut self, i: &ItemMod) {
        if IncludeTests::No == self.include_tests && is_test_mod(i) {
            return;
        }
        visit::visit_item_mod(self, i);
    }

    fn visit_item_impl(&mut self, i: &ItemImpl) {
        // unsafe trait impl's
        self.counters.item_impls.count(i.unsafety.is_some());
        visit::visit_item_impl(self, i);
    }

    fn visit_item_trait(&mut self, i: &ItemTrait) {
        // Unsafe traits
        self.counters.item_traits.count(i.unsafety.is_some());
        visit::visit_item_trait(self, i);
    }

    fn visit_impl_item_method(&mut self, i: &ImplItemMethod) {
        self.counters.methods.count(i.sig.unsafety.is_some());
        visit::visit_impl_item_method(self, i);
    }

    // TODO: Visit macros.
    //
    // TODO: Figure out if there are other visit methods that should be
    // implemented here.
}

pub fn is_file_with_ext(entry: &DirEntry, file_ext: &str) -> bool {
    if !entry.file_type().is_file() {
        return false;
    }
    let p = entry.path();
    let ext = match p.extension() {
        Some(e) => e,
        None => return false,
    };
    // to_string_lossy is ok since we only want to match against an ASCII
    // compatible extension and we do not keep the possibly lossy result
    // around.
    ext.to_string_lossy() == file_ext
}

pub fn find_rs_files_in_dir(dir: &Path) -> impl Iterator<Item = PathBuf> {
    let walker = WalkDir::new(dir).into_iter();
    walker.filter_map(|entry| {
        let entry = entry.expect("walkdir error."); // TODO: Return result.
        if !is_file_with_ext(&entry, "rs") {
            return None;
        }
        Some(
            entry
                .path()
                .canonicalize()
                .expect("Error converting to canonical path"),
        ) // TODO: Return result.
    })
}

pub fn find_unsafe_in_file(
    p: &Path,
    include_tests: IncludeTests,
) -> Result<CounterBlock, ScanFileError> {
    let mut vis = GeigerSynVisitor::new(include_tests);
    let mut file =
        File::open(p).map_err(|e| ScanFileError::Io(e, p.to_path_buf()))?;
    let mut src = vec![];
    file.read_to_end(&mut src)
        .map_err(|e| ScanFileError::Io(e, p.to_path_buf()))?;
    let src = String::from_utf8(src)
        .map_err(|e| ScanFileError::Utf8(e, p.to_path_buf()))?;
    let syntax = syn::parse_file(&src)
        .map_err(|e| ScanFileError::Syn(e, p.to_path_buf()))?;
    syn::visit::visit_file(&mut vis, &syntax);
    Ok(vis.counters)
}

pub fn find_rs_files_in_package<'a>(
    pack: &'a Package,
) -> impl Iterator<Item = PathBuf> + 'a {
    Some(pack)
        .into_iter()
        .flat_map(|p| find_rs_files_in_dir(p.root()))
}

pub fn find_rs_files_in_packages<'a, 'b>(
    packs: &'a Vec<&'b Package>,
) -> impl Iterator<Item = (&'a PackageId, PathBuf)> + 'a {
    packs.iter().flat_map(|pack| {
        find_rs_files_in_package(pack)
            .map(move |path| (pack.package_id(), path))
    })
}

pub fn find_unsafe_in_packages<'a, 'b>(
    packs: &'a PackageSet<'b>,
    mut rs_files_used: HashMap<PathBuf, u32>,
    allow_partial_results: bool,
    include_tests: IncludeTests,
    verbosity: Verbosity,
) -> GeigerContext {
    let mut pack_id_to_counters = HashMap::new();
    let packs = packs.get_many(packs.package_ids()).unwrap();
    let pack_paths = find_rs_files_in_packages(&packs);
    for (pack_id, path) in pack_paths {
        let p = &path;
        let scan_counter = rs_files_used.get_mut(p);
        let used_by_build = match scan_counter {
            Some(c) => {
                // TODO: Add proper logging.
                if verbosity == Verbosity::Verbose {
                    println!("Used in build: {}", p.display());
                }
                // This .rs file path was found by intercepting rustc arguments
                // or by parsing the .d files produced by rustc. Here we
                // increase the counter for this path to mark that this file
                // has been scanned. Warnings will be printed for .rs files in
                // this collection with a count of 0 (has not been scanned). If
                // this happens, it could indicate a logic error or some
                // incorrect assumption in cargo-geiger.
                *c += 1;
                true
            }
            None => {
                // This file was not used in the build triggered by
                // cargo-geiger, but it should be scanned anyways to provide
                // both "in build" and "not in build" stats.
                // TODO: Add proper logging.
                if verbosity == Verbosity::Verbose {
                    println!("Not used in build: {}", p.display());
                }
                false
            }
        };
        match find_unsafe_in_file(p, include_tests) {
            Err(e) => match allow_partial_results {
                true => {
                    eprintln!("Failed to parse file: {}, {:?} ", p.display(), e)
                }
                false => {
                    panic!("Failed to parse file: {}, {:?} ", p.display(), e)
                }
            },
            Ok(file_counters) => {
                let pack_counters = pack_id_to_counters
                    .entry(pack_id.clone())
                    .or_insert(PackageCounters::default());
                let target = match used_by_build {
                    true => &mut pack_counters.used,
                    false => &mut pack_counters.not_used,
                };
                *target = target.clone() + file_counters;
            }
        }
    }
    GeigerContext {
        pack_id_to_counters,
        rs_files_used,
    }
}

pub enum Charset {
    Utf8,
    Ascii,
}

#[derive(Clone, Copy)]
pub enum Prefix {
    None,
    Indent,
    Depth,
}

impl FromStr for Charset {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Charset, &'static str> {
        match s {
            "utf8" => Ok(Charset::Utf8),
            "ascii" => Ok(Charset::Ascii),
            _ => Err("invalid charset"),
        }
    }
}

pub struct Symbols {
    down: &'static str,
    tee: &'static str,
    ell: &'static str,
    right: &'static str,
}

pub const UTF8_SYMBOLS: Symbols = Symbols {
    down: "│",
    tee: "├",
    ell: "└",
    right: "─",
};

pub const ASCII_SYMBOLS: Symbols = Symbols {
    down: "|",
    tee: "|",
    ell: "`",
    right: "-",
};

pub struct PrintConfig<'a> {
    /// Don't truncate dependencies that have already been displayed.
    pub all: bool,

    pub verbosity: Verbosity,
    pub direction: EdgeDirection,
    pub prefix: Prefix,

    // Is anyone using this? This is a carry-over from cargo-tree.
    // TODO: Open a github issue to discuss deprecation.
    pub format: &'a Pattern,

    pub symbols: &'a Symbols,
    pub allow_partial_results: bool,
    pub include_tests: IncludeTests,
}

/// Trigger a `cargo clean` + `cargo check` and listen to the cargo/rustc
/// communication to figure out which source files were used by the build.
pub fn resolve_rs_file_deps(
    copt: &CompileOptions,
    ws: &Workspace,
) -> Result<HashMap<PathBuf, u32>, RsResolveError> {
    let config = ws.config();
    // Need to run a cargo clean to identify all new .d deps files.
    // TODO: Figure out how this can be avoided to improve performance, clean
    // Rust builds are __slow__.
    let clean_opt = CleanOptions {
        config: &config,
        spec: vec![],
        target: None,
        release: false,
        doc: false,
    };
    ops::clean(ws, &clean_opt)
        .map_err(|e| RsResolveError::Cargo(e.to_string()))?;
    let inner_arc = Arc::new(Mutex::new(CustomExecutorInnerContext::default()));
    {
        let cust_exec = CustomExecutor {
            cwd: config.cwd().to_path_buf(),
            inner_ctx: inner_arc.clone(),
        };
        let exec: Arc<Executor> = Arc::new(cust_exec);
        ops::compile_with_exec(ws, &copt, &exec)
            .map_err(|e| RsResolveError::Cargo(e.to_string()))?;
    }
    let ws_root = ws.root().to_path_buf();
    let inner_mutex =
        Arc::try_unwrap(inner_arc).map_err(|_| RsResolveError::ArcUnwrap())?;
    let (rs_files, out_dir_args) = {
        let ctx = inner_mutex.into_inner()?;
        (ctx.rs_file_args, ctx.out_dir_args)
    };
    let mut hm = HashMap::<PathBuf, u32>::new();
    for out_dir in out_dir_args {
        for ent in WalkDir::new(&out_dir) {
            let ent = ent.map_err(RsResolveError::Walkdir)?;
            if !is_file_with_ext(&ent, "d") {
                continue;
            }
            let deps = parse_rustc_dep_info(ent.path()).map_err(|e| {
                RsResolveError::DepParse(
                    e.to_string(),
                    ent.path().to_path_buf(),
                )
            })?;
            let canon_paths = deps
                .into_iter()
                .flat_map(|t| t.1)
                .map(PathBuf::from)
                .map(|pb| ws_root.join(pb))
                .map(|pb| {
                    pb.canonicalize().map_err(|e| RsResolveError::Io(e, pb))
                });
            for p in canon_paths {
                hm.insert(p?, 0);
            }
        }
    }
    for pb in rs_files {
        // rs_files must already be canonicalized
        hm.insert(pb, 0);
    }
    Ok(hm)
}

/// Copy-pasted (almost) from the private module cargo::core::compiler::fingerprint.
///
/// TODO: Make a PR to the cargo project to expose this function or to expose
/// the dependency data in some other way.
fn parse_rustc_dep_info(
    rustc_dep_info: &Path,
) -> CargoResult<Vec<(String, Vec<String>)>> {
    let contents = paths::read(rustc_dep_info)?;
    contents
        .lines()
        .filter_map(|l| l.find(": ").map(|i| (l, i)))
        .map(|(line, pos)| {
            let target = &line[..pos];
            let mut deps = line[pos + 2..].split_whitespace();
            let mut ret = Vec::new();
            while let Some(s) = deps.next() {
                let mut file = s.to_string();
                while file.ends_with('\\') {
                    file.pop();
                    file.push(' ');
                    //file.push_str(deps.next().ok_or_else(|| {
                    //internal("malformed dep-info format, trailing \\".to_string())
                    //})?);
                    file.push_str(
                        deps.next()
                            .expect("malformed dep-info format, trailing \\"),
                    );
                }
                ret.push(file);
            }
            Ok((target.to_string(), ret))
        })
        .collect()
}

#[derive(Debug, Default)]
struct CustomExecutorInnerContext {
    /// Stores all lib.rs, main.rs etc. passed to rustc during the build.
    rs_file_args: HashSet<PathBuf>,

    /// Investigate if this needs to be intercepted like this or if it can be
    /// looked up in a nicer way.
    out_dir_args: HashSet<PathBuf>,
}

use std::sync::PoisonError;

/// A cargo Executor to intercept all build tasks and store all ".rs" file
/// paths for later scanning.
///
/// TODO: This is the place(?) to make rustc perform macro expansion to allow
/// scanning of the the expanded code. (incl. code generated by build.rs).
/// Seems to require nightly rust.
#[derive(Debug)]
struct CustomExecutor {
    /// Current work dir
    cwd: PathBuf,

    /// Needed since multiple rustc calls can be in flight at the same time.
    inner_ctx: Arc<Mutex<CustomExecutorInnerContext>>,
}

use std::error::Error;
use std::fmt;

#[derive(Debug)]
enum CustomExecutorError {
    OutDirKeyMissing(String),
    OutDirValueMissing(String),
    InnerContextMutex(String),
    Io(io::Error, PathBuf),
}

impl Error for CustomExecutorError {}

/// Forward Display to Debug. See the crate root documentation.
impl fmt::Display for CustomExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl Executor for CustomExecutor {
    /// In case of an `Err`, Cargo will not continue with the build process for
    /// this package.
    fn exec(
        &self,
        cmd: ProcessBuilder,
        _id: &PackageId,
        _target: &Target,
        _mode: CompileMode,
    ) -> CargoResult<()> {
        let args = cmd.get_args();
        let out_dir_key = OsString::from("--out-dir");
        let out_dir_key_idx =
            args.iter().position(|s| *s == out_dir_key).ok_or_else(|| {
                CustomExecutorError::OutDirKeyMissing(cmd.to_string())
            })?;
        let out_dir = args
            .get(out_dir_key_idx + 1)
            .ok_or_else(|| {
                CustomExecutorError::OutDirValueMissing(cmd.to_string())
            })
            .map(PathBuf::from)?;

        // This can be different from the cwd used to launch the wrapping cargo
        // plugin. Discovered while fixing
        // https://github.com/anderejd/cargo-geiger/issues/19
        let cwd = cmd
            .get_cwd()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.to_owned());

        {
            // Scope to drop and release the mutex before calling rustc.
            let mut ctx = self.inner_ctx.lock().map_err(|e| {
                CustomExecutorError::InnerContextMutex(e.to_string())
            })?;
            for tuple in args
                .iter()
                .map(|s| (s, s.to_string_lossy().to_lowercase()))
                .filter(|t| t.1.ends_with(".rs"))
            {
                let raw_path = cwd.join(tuple.0);
                let p = raw_path
                    .canonicalize()
                    .map_err(|e| CustomExecutorError::Io(e, raw_path))?;
                ctx.rs_file_args.insert(p);
            }
            ctx.out_dir_args.insert(out_dir);
        }
        cmd.exec()?;
        Ok(())
    }

    /// TODO: Investigate if this returns the information we need through
    /// stdout or stderr.
    fn exec_json(
        &self,
        _cmd: ProcessBuilder,
        _id: &PackageId,
        _target: &Target,
        _mode: CompileMode,
        _handle_stdout: &mut FnMut(&str) -> CargoResult<()>,
        _handle_stderr: &mut FnMut(&str) -> CargoResult<()>,
    ) -> CargoResult<()> {
        //cmd.exec_with_streaming(handle_stdout, handle_stderr, false)?;
        //Ok(())
        unimplemented!();
    }

    /// Queried when queuing each unit of work. If it returns true, then the
    /// unit will always be rebuilt, independent of whether it needs to be.
    fn force_rebuild(&self, _unit: &Unit) -> bool {
        true // Overriding the default to force all units to be processed.
    }
}

/// TODO: Write proper documentation for this.
/// This function seems to be looking up the active flags for conditional
/// compilation (cargo::util::Cfg instances).
pub fn get_cfgs(
    config: &Config,
    target: &Option<String>,
    ws: &Workspace,
) -> CargoResult<Option<Vec<Cfg>>> {
    let mut process = util::process(&config.rustc(Some(ws))?.path);
    process.arg("--print=cfg").env_remove("RUST_LOG");
    if let Some(ref s) = *target {
        process.arg("--target").arg(s);
    }
    let output = match process.exec_with_output() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    let output = str::from_utf8(&output.stdout).unwrap();
    let lines = output.lines();
    Ok(Some(
        lines.map(Cfg::from_str).collect::<CargoResult<Vec<_>>>()?,
    ))
}

pub fn workspace(
    config: &Config,
    manifest_path: Option<PathBuf>,
) -> CargoResult<Workspace> {
    let root = match manifest_path {
        Some(path) => path,
        None => important_paths::find_root_manifest_for_wd(config.cwd())?,
    };
    Workspace::new(&root, config)
}

pub fn registry<'a>(
    config: &'a Config,
    package: &Package,
) -> CargoResult<PackageRegistry<'a>> {
    let mut registry = PackageRegistry::new(config)?;
    registry.add_sources(&[package.package_id().source_id().clone()])?;
    Ok(registry)
}

pub fn resolve<'a, 'cfg>(
    registry: &mut PackageRegistry<'cfg>,
    ws: &'a Workspace<'cfg>,
    features: Option<String>,
    all_features: bool,
    no_default_features: bool,
) -> CargoResult<(PackageSet<'a>, Resolve)> {
    let features =
        Method::split_features(&features.into_iter().collect::<Vec<_>>());
    let (packages, resolve) = ops::resolve_ws(ws)?;
    let method = Method::Required {
        dev_deps: true,
        features: &features,
        all_features,
        uses_default_features: !no_default_features,
    };
    let resolve = ops::resolve_with_previous(
        registry,
        ws,
        method,
        Some(&resolve),
        None,
        &[],
        true,
        true,
    )?;
    Ok((packages, resolve))
}

pub struct Node<'a> {
    id: &'a PackageId,
    pack: &'a Package,
}

pub struct Graph<'a> {
    graph: petgraph::Graph<Node<'a>, Kind>,
    nodes: HashMap<&'a PackageId, NodeIndex>,
}

/// Almost unmodified compared to the original in cargo-tree, should be fairly
/// simple to move this and the dependency graph structure out to a library.
/// TODO: Move this to a module to begin with.
pub fn build_graph<'a>(
    resolve: &'a Resolve,
    packages: &'a PackageSet,
    root: &'a PackageId,
    target: Option<&str>,
    cfgs: Option<&[Cfg]>,
    extra_deps: ExtraDeps,
) -> CargoResult<Graph<'a>> {
    let mut graph = Graph {
        graph: petgraph::Graph::new(),
        nodes: HashMap::new(),
    };
    let node = Node {
        id: root,
        pack: packages.get_one(root)?,
    };
    graph.nodes.insert(root, graph.graph.add_node(node));

    let mut pending = vec![root];

    while let Some(pkg_id) = pending.pop() {
        let idx = graph.nodes[&pkg_id];
        let pkg = packages.get_one(pkg_id)?;

        for raw_dep_id in resolve.deps_not_replaced(pkg_id) {
            let it = pkg
                .dependencies()
                .iter()
                .filter(|d| d.matches_id(raw_dep_id))
                .filter(|d| extra_deps.allows(d.kind()))
                .filter(|d| {
                    d.platform()
                        .and_then(|p| target.map(|t| p.matches(t, cfgs)))
                        .unwrap_or(true)
                });
            let dep_id = match resolve.replacement(raw_dep_id) {
                Some(id) => id,
                None => raw_dep_id,
            };
            for dep in it {
                let dep_idx = match graph.nodes.entry(dep_id) {
                    Entry::Occupied(e) => *e.get(),
                    Entry::Vacant(e) => {
                        pending.push(dep_id);
                        let node = Node {
                            id: dep_id,
                            pack: packages.get_one(dep_id)?,
                        };
                        *e.insert(graph.graph.add_node(node))
                    }
                };
                graph.graph.add_edge(idx, dep_idx, dep.kind());
            }
        }
    }

    Ok(graph)
}

pub fn print_tree<'a>(
    root_pack_id: &'a PackageId,
    graph: &Graph<'a>,
    geiger_ctx: &GeigerContext,
    pc: &PrintConfig,
) {
    let mut visited_deps = HashSet::new();
    let mut levels_continue = vec![];
    let node = &graph.graph[graph.nodes[&root_pack_id]];
    print_dependency(
        node,
        &graph,
        &mut visited_deps,
        &mut levels_continue,
        geiger_ctx,
        pc,
    );
}

fn print_dependency<'a>(
    package: &Node<'a>,
    graph: &Graph<'a>,
    visited_deps: &mut HashSet<&'a PackageId>,
    levels_continue: &mut Vec<bool>,
    geiger_ctx: &GeigerContext,
    pc: &PrintConfig,
) {
    let new = pc.all || visited_deps.insert(package.id);
    let treevines = match pc.prefix {
        Prefix::Depth => format!("{} ", levels_continue.len()),
        Prefix::Indent => {
            let mut buf = String::new();
            if let Some((&last_continues, rest)) = levels_continue.split_last()
            {
                for &continues in rest {
                    let c = if continues { pc.symbols.down } else { " " };
                    buf.push_str(&format!("{}   ", c));
                }
                let c = if last_continues {
                    pc.symbols.tee
                } else {
                    pc.symbols.ell
                };
                buf.push_str(&format!("{0}{1}{1} ", c, pc.symbols.right));
            }
            buf
        }
        Prefix::None => "".into(),
    };
    let pack_counters =
        geiger_ctx
            .pack_id_to_counters
            .get(package.id)
            .expect(&format!(
                "Failed to get unsafe counters for package: {}",
                package.id
            )); // TODO: Try to be panic free and use Result everywhere.
    let unsafe_found = pack_counters.used.has_unsafe();
    let colorize = |s: String| {
        if unsafe_found {
            s.red().bold()
        } else {
            s.green()
        }
    };
    let rad = if unsafe_found { "☢" } else { "" };
    let dep_name = colorize(format!(
        "{}",
        pc.format
            .display(package.id, package.pack.manifest().metadata())
    ));
    // TODO: Split up table and tree printing and paint into a backbuffer
    // before writing to stdout?
    let unsafe_info = colorize(table_row(&pack_counters));
    println!("{}  {: <1} {}{}", unsafe_info, rad, treevines, dep_name);
    if !new {
        return;
    }
    let mut normal = vec![];
    let mut build = vec![];
    let mut development = vec![];
    for edge in graph
        .graph
        .edges_directed(graph.nodes[&package.id], pc.direction)
    {
        let dep = match pc.direction {
            EdgeDirection::Incoming => &graph.graph[edge.source()],
            EdgeDirection::Outgoing => &graph.graph[edge.target()],
        };
        match *edge.weight() {
            Kind::Normal => normal.push(dep),
            Kind::Build => build.push(dep),
            Kind::Development => development.push(dep),
        }
    }
    let mut kinds = [
        (Kind::Normal, normal),
        (Kind::Build, build),
        (Kind::Development, development),
    ];
    for (kind, kind_deps) in kinds.iter_mut() {
        print_dependency_kind(
            *kind,
            kind_deps,
            graph,
            visited_deps,
            levels_continue,
            geiger_ctx,
            pc,
        );
    }
}

fn print_dependency_kind<'a>(
    kind: Kind,
    deps: &mut Vec<&Node<'a>>,
    graph: &Graph<'a>,
    visited_deps: &mut HashSet<&'a PackageId>,
    levels_continue: &mut Vec<bool>,
    geiger_ctx: &GeigerContext,
    pc: &PrintConfig,
) {
    if deps.is_empty() {
        return;
    }

    // Resolve uses Hash data types internally but we want consistent output ordering
    deps.sort_by_key(|n| n.id);

    let name = match kind {
        Kind::Normal => None,
        Kind::Build => Some("[build-dependencies]"),
        Kind::Development => Some("[dev-dependencies]"),
    };
    if let Prefix::Indent = pc.prefix {
        if let Some(name) = name {
            print!("{}", table_row_empty());
            for &continues in &**levels_continue {
                let c = if continues { pc.symbols.down } else { " " };
                print!("{}   ", c);
            }

            println!("{}", name);
        }
    }

    let mut it = deps.iter().peekable();
    while let Some(dependency) = it.next() {
        levels_continue.push(it.peek().is_some());
        print_dependency(
            dependency,
            graph,
            visited_deps,
            levels_continue,
            geiger_ctx,
            pc,
        );
        levels_continue.pop();
    }
}

// TODO: use a table library, or factor the tableness out in a smarter way
pub const UNSAFE_COUNTERS_HEADER: [&str; 6] = [
    "Functions ",
    "Expressions ",
    "Impls ",
    "Traits ",
    "Methods ",
    "Dependency",
];

fn table_row_empty() -> String {
    " ".repeat(
        UNSAFE_COUNTERS_HEADER
            .iter()
            .take(5)
            .map(|s| s.len())
            .sum::<usize>()
            + UNSAFE_COUNTERS_HEADER.len()
            + 1,
    )
}

fn table_row(pc: &PackageCounters) -> String {
    let fmt = |used: &Count, not_used: &Count| {
        format!("{}/{}", used.unsafe_, used.unsafe_ + not_used.unsafe_)
    };
    format!(
        "{: <10} {: <12} {: <6} {: <7} {: <7}",
        fmt(&pc.used.functions, &pc.not_used.functions),
        fmt(&pc.used.exprs, &pc.not_used.exprs),
        fmt(&pc.used.item_impls, &pc.not_used.item_impls),
        fmt(&pc.used.item_traits, &pc.not_used.item_traits),
        fmt(&pc.used.methods, &pc.not_used.methods),
    )
}

pub enum ExtraDeps {
    All,
    Build,
    Dev,
    NoMore,
}

impl ExtraDeps {
    fn allows(&self, dep: Kind) -> bool {
        match (self, dep) {
            (_, Kind::Normal) => true,
            (ExtraDeps::All, _) => true,
            (ExtraDeps::Build, Kind::Build) => true,
            (ExtraDeps::Dev, Kind::Development) => true,
            _ => false,
        }
    }
}
