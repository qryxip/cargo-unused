//! List actually used crates.
//!
//! ```no_run
//! use cargo::ops::Packages;
//! use cargo_linked::LinkedPackages;
//!
//! use std::path::PathBuf;
//!
//! let jobs: Option<String> = unimplemented!();
//! let lib: bool = unimplemented!();
//! let bin: Option<String> = unimplemented!();
//! let example: Option<String> = unimplemented!();
//! let test: Option<String> = unimplemented!();
//! let bench: Option<String> = unimplemented!();
//! let release: bool = unimplemented!();
//! let features: Vec<String> = unimplemented!();
//! let all_features: bool = unimplemented!();
//! let no_default_features: bool = unimplemented!();
//! let manifest_path: Option<PathBuf> = unimplemented!();
//! let color: Option<String> = unimplemented!();
//! let frozen: bool = unimplemented!();
//! let locked: bool = unimplemented!();
//! let offline: bool = unimplemented!();
//!
//! let mut config = cargo::Config::default()?;
//!
//! cargo_linked::util::Configure {
//!     manifest_path: &manifest_path,
//!     color: &color,
//!     frozen,
//!     locked,
//!     offline,
//!     modify_target_dir: |d| d.join("cargo_linked").join("target"),
//! }
//! .configure(&mut config)?;
//!
//! let ws = cargo_linked::util::workspace(&config, &manifest_path)?;
//!
//! let (packages, resolve) = Packages::All.to_package_id_specs(&ws).and_then(|specs| {
//!     cargo::ops::resolve_ws_precisely(
//!         &ws,
//!         &features,
//!         all_features,
//!         no_default_features,
//!         &specs,
//!     )
//! })?;
//!
//! let (compile_options, target) = cargo_linked::util::CompileOptionsForSingleTarget {
//!     ws: &ws,
//!     jobs: &jobs,
//!     lib,
//!     bin: &bin,
//!     example: &example,
//!     test: &test,
//!     bench: &bench,
//!     release,
//!     features: &features,
//!     all_features,
//!     no_default_features,
//!     manifest_path: &manifest_path,
//! }
//! .compile_options_for_single_target()?;
//!
//! let LinkedPackages { used, unused } =
//!     LinkedPackages::find(&ws, &packages, &resolve, &compile_options, target)?;
//! # failure::Fallible::Ok(())
//! ```

macro_rules! lazy_regex {
    ($regex:expr $(,)?) => {
        ::once_cell::sync::Lazy::new(|| ::regex::Regex::new($regex).unwrap())
    };
}

pub mod util;

mod error;
mod fs;
mod parse;
mod process;
mod ser;

#[doc(inline)]
pub use crate::error::{Error, ErrorKind};

pub use cargo as cargo_0_39;
pub use miniserde as miniserde_0_1;
pub use serde as serde_1;

use crate::fs::JsonFileLock;
use crate::process::Rustc;

use ansi_term::Colour;
use cargo::core::compiler::{CompileMode, Executor, Unit};
use cargo::core::manifest::{Target, TargetKind};
use cargo::core::{dependency, Package, PackageId, PackageSet, Resolve, Workspace};
use cargo::ops::{CompileOptions, Packages};
use cargo::util::process_builder::ProcessBuilder;
use cargo::CargoResult;
use failure::ResultExt as _;
use fixedbitset::FixedBitSet;
use if_chain::if_chain;
use maplit::{btreemap, btreeset, hashset};
use once_cell::sync::Lazy;
use regex::Regex;

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::ops::Index;
use std::sync::{Arc, Mutex};

/// Result.
pub type Result<T> = std::result::Result<T, crate::Error>;

#[derive(Default, serde::Deserialize)]
#[serde(transparent)]
struct Cache(Vec<CacheValue>);

impl Cache {
    fn take_or_default(&mut self, key: CacheKey) -> BTreeMap<PackageId, CacheUsedPackages> {
        (0..self.0.len())
            .find(|&i| self.0[i].key == key)
            .map(|i| self.0.remove(i).used_packages)
            .unwrap_or_default()
    }

    fn insert(&mut self, key: CacheKey, value: BTreeMap<PackageId, CacheUsedPackages>) {
        self.0.push(CacheValue {
            key,
            used_packages: value,
        });
        self.0.sort_by_key(|v| v.key);
    }
}

impl Index<CacheKey> for Cache {
    type Output = BTreeMap<PackageId, CacheUsedPackages>;

    fn index(&self, index: CacheKey) -> &BTreeMap<PackageId, CacheUsedPackages> {
        self.0
            .iter()
            .find(|v| v.key == index)
            .map(|CacheValue { used_packages, .. }| used_packages)
            .unwrap_or_else(|| panic!("no value found for {:?}", index))
    }
}

#[derive(serde::Deserialize)]
struct CacheValue {
    key: CacheKey,
    used_packages: BTreeMap<PackageId, CacheUsedPackages>,
}

#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Deserialize, miniserde::Serialize,
)]
struct CacheKey {
    release: bool,
}

impl CacheKey {
    fn new(release: bool) -> Self {
        Self { release }
    }
}

#[derive(Default, Debug, serde::Deserialize)]
struct CacheUsedPackages {
    lib: Option<BTreeSet<PackageId>>,
    bin: BTreeMap<String, BTreeSet<PackageId>>,
    test: BTreeMap<String, BTreeSet<PackageId>>,
    bench: BTreeMap<String, BTreeSet<PackageId>>,
    example_lib: BTreeMap<String, BTreeSet<PackageId>>,
    example_bin: BTreeMap<String, BTreeSet<PackageId>>,
    custom_build: Option<BTreeSet<PackageId>>,
}

impl CacheUsedPackages {
    fn get<'a>(&'a self, target: &Target) -> Option<&'a BTreeSet<PackageId>> {
        match target.kind() {
            TargetKind::Lib(_) => self.lib.as_ref(),
            TargetKind::Bin => self.bin.get(&target.name().to_owned()),
            TargetKind::Test => self.test.get(&target.name().to_owned()),
            TargetKind::Bench => self.bench.get(&target.name().to_owned()),
            TargetKind::ExampleLib(_) => self.example_lib.get(&target.name().to_owned()),
            TargetKind::ExampleBin => self.example_bin.get(&target.name().to_owned()),
            TargetKind::CustomBuild => self.custom_build.as_ref(),
        }
    }

    fn insert<'a, I: IntoIterator<Item = P>, P: Borrow<PackageId>>(
        &'a mut self,
        target: &Target,
        packages: I,
    ) {
        let key = target.name().to_owned();
        let val = packages.into_iter().map(|p| *p.borrow()).collect();
        match target.kind() {
            TargetKind::Lib(_) => self.lib = Some(val),
            TargetKind::Bin => {
                self.bin.insert(key, val);
            }
            TargetKind::Test => {
                self.test.insert(key, val);
            }
            TargetKind::Bench => {
                self.bench.insert(key, val);
            }
            TargetKind::ExampleLib(_) => {
                self.example_lib.insert(key, val);
            }
            TargetKind::ExampleBin => {
                self.example_bin.insert(key, val);
            }
            TargetKind::CustomBuild => self.custom_build = Some(val),
        }
    }
}

#[derive(Debug, Default)]
pub struct LinkedPackages {
    pub used: BTreeSet<PackageId>,
    pub unused: LinkedPackagesUnused,
}

impl LinkedPackages {
    pub fn find(
        ws: &Workspace,
        packages: &PackageSet,
        resolve: &Resolve,
        compile_opts: &CompileOptions,
        target: &Target,
    ) -> crate::Result<Self> {
        let current = ws.current().with_context(|_| crate::ErrorKind::Cargo)?;

        let all_ids = cargo::ops::resolve_ws(ws)
            .map(|(ps, _)| ps.package_ids().collect::<HashSet<_>>())
            .with_context(|_| crate::ErrorKind::Cargo)?;

        let packages = packages
            .get_many(packages.package_ids())
            .with_context(|_| crate::ErrorKind::Cargo)?
            .into_iter()
            .map(|p| (p.package_id(), p))
            .collect::<BTreeMap<_, _>>();

        let target = target.clone();

        let unnecessary_dev_deps =
            if target.is_test() || target.is_example() || target.is_custom_build() {
                hashset!()
            } else {
                let mut dev_removed = hashset!(&current);
                let mut cur = dev_removed.clone();
                loop {
                    let mut next = hashset!();
                    for from_pkg in cur {
                        for (to_id, deps) in resolve.deps(from_pkg.package_id()) {
                            if deps
                                .iter()
                                .any(|d| d.kind() != dependency::Kind::Development)
                            {
                                let to_pkg = &packages[&to_id];
                                if dev_removed.insert(to_pkg) {
                                    next.insert(to_pkg);
                                }
                            }
                        }
                    }
                    cur = next;
                    if cur.is_empty() {
                        break;
                    }
                }
                packages
                    .values()
                    .cloned()
                    .filter(|p| !dev_removed.contains(p))
                    .map(Package::package_id)
                    .collect()
            };

        let extern_crate_names = packages
            .values()
            .map(|from_pkg| {
                let resolve_names = |filter: fn(_) -> _| -> CargoResult<HashMap<_, _>> {
                    resolve
                        .deps(from_pkg.package_id())
                        .flat_map(|(to_id, deps)| deps.iter().map(move |d| (d, to_id)))
                        .filter(|&(d, _)| filter(d.kind()))
                        .map(|(_, to_id)| {
                            let to_lib = packages
                                .get(&to_id)
                                .unwrap_or_else(|| panic!("could not find `{}`", to_id))
                                .targets()
                                .iter()
                                .find(|t| t.is_lib())
                                .unwrap_or_else(|| {
                                    panic!("`{}` does not have any `lib` target", to_id)
                                });
                            let extern_crate_name =
                                resolve.extern_crate_name(from_pkg.package_id(), to_id, to_lib)?;
                            Ok((to_id, extern_crate_name))
                        })
                        .collect()
                };

                let normal_dev = resolve_names(|k| k != dependency::Kind::Build)?;
                let build = resolve_names(|k| k == dependency::Kind::Build)?;
                let self_lib_name = from_pkg
                    .targets()
                    .iter()
                    .find(|t| t.is_lib())
                    .map(|lib| {
                        let id = from_pkg.package_id();
                        resolve.extern_crate_name(id, id, lib)
                    })
                    .transpose()?;

                let extern_crate_names = from_pkg
                    .targets()
                    .iter()
                    .map(|from_target| {
                        let mut extern_crate_names = if from_target.is_custom_build() {
                            build.clone()
                        } else {
                            normal_dev.clone()
                        };
                        if !(from_target.is_lib() || from_target.is_custom_build()) {
                            if let Some(self_lib_name) = self_lib_name.clone() {
                                extern_crate_names.insert(from_pkg.package_id(), self_lib_name);
                            }
                        }
                        (from_target.clone(), extern_crate_names)
                    })
                    .collect::<HashMap<_, _>>();

                Ok((from_pkg.package_id(), extern_crate_names))
            })
            .collect::<CargoResult<HashMap<_, _>>>()
            .with_context(|_| crate::ErrorKind::Cargo)?;

        let cache_file = ws
            .target_dir()
            .join("..")
            .open_rw("cache.json", ws.config(), "msg?")
            .with_context(|_| crate::ErrorKind::Cargo)?;
        let mut cache_file = JsonFileLock::<Cache>::from(cache_file);
        let mut cache = cache_file.read()?;
        let cache_key = CacheKey::new(compile_opts.build_config.release);

        let store = Arc::new(Mutex::new(ExecStore::new(cache.take_or_default(cache_key))));
        let exec: Arc<dyn Executor + 'static> = Arc::new(Exec {
            target: target.clone(),
            extern_crate_names,
            supports_color: ws.config().shell().supports_color(),
            store: store.clone(),
        });
        cargo::ops::compile_with_exec(ws, compile_opts, &exec)
            .with_context(|_| crate::ErrorKind::Cargo)?;
        drop(exec);

        let ExecStore {
            used_packages,
            all_targets,
        } = Arc::try_unwrap(store)
            .unwrap_or_else(|s| panic!("{:?} has other references", s))
            .into_inner()
            .unwrap();

        cache.insert(cache_key, used_packages);
        cache_file.write(&cache)?;

        let mut outcome = Self::default();
        outcome.used = {
            let mut used = cache[cache_key]
                .get(&current.package_id())
                .unwrap()
                .bin
                .get(&target.name().to_owned())
                .unwrap()
                .clone();
            let mut cur = used.clone();
            while !cur.is_empty() {
                let mut next = btreeset!();
                for cur in cur {
                    for dep in cache[cache_key][&cur]
                        .lib
                        .as_ref()
                        .unwrap()
                        .iter()
                        .cloned()
                        .chain(
                            cache[cache_key][&cur]
                                .custom_build
                                .clone()
                                .unwrap_or_default(),
                        )
                    {
                        if used.insert(dep) {
                            next.insert(dep);
                        }
                    }
                }
                cur = next;
            }
            used
        };

        outcome.unused.trivial = all_ids
            .iter()
            .cloned()
            .filter(|id| {
                !outcome.used.contains(id)
                    && (!all_targets.contains_key(id) || unnecessary_dev_deps.contains(id))
            })
            .collect();
        outcome.unused.maybe_obsolete = all_ids
            .iter()
            .cloned()
            .filter(|id| !(outcome.used.contains(id) || outcome.unused.trivial.contains(id)))
            .collect();

        Ok(outcome)
    }
}

#[derive(Default, Debug, serde::Deserialize)]
pub struct LinkedPackagesUnused {
    pub trivial: BTreeSet<PackageId>,
    pub maybe_obsolete: BTreeSet<PackageId>,
}

#[derive(Debug)]
struct Exec {
    target: Target,
    extern_crate_names: HashMap<PackageId, HashMap<Target, HashMap<PackageId, String>>>,
    supports_color: bool,
    store: Arc<Mutex<ExecStore>>,
}

impl Executor for Exec {
    fn exec(
        &self,
        cmd: ProcessBuilder,
        id: PackageId,
        target: &Target,
        _: CompileMode,
        on_stdout_line: &mut dyn FnMut(&str) -> CargoResult<()>,
        on_stderr_line: &mut dyn FnMut(&str) -> CargoResult<()>,
    ) -> CargoResult<()> {
        static E0432_SINGLE_MOD: Lazy<Regex> =
            lazy_regex!(r"\Aunresolved import `([a-zA-Z0-9_]+)`\z");
        static E0433_SINGLE_MOD: Lazy<Regex> =
            lazy_regex!(r"\Afailed to resolve [a-z ]+`([a-zA-Z0-9_]+)`( in `\{\{root\}\}`)?\z");
        static E0463_SINGLE_MOD: Lazy<Regex> =
            lazy_regex!(r"\Acan't find crate for `([a-zA-Z0-9_]+)`\z");

        let mut cmd = Rustc::new(cmd, id, target)?;
        let mut exclude = FixedBitSet::with_capacity(cmd.externs().len());
        let uses = crate::parse::find_uses_lossy(
            target.src_path(),
            &cmd.externs().iter().map(|e| e.name()).collect(),
        );
        let uses = match uses {
            Ok(uses) => uses,
            Err(err) => {
                let mut msg = "".to_owned();
                for (i, cause) in err.as_fail().iter_chain().enumerate() {
                    let head = if i == 0 && err.as_fail().cause().is_none() {
                        "warning:"
                    } else if i == 0 {
                        "  warning:"
                    } else {
                        "caused by:"
                    };
                    if self.supports_color {
                        write!(msg, "{} ", Colour::Yellow.bold().paint(head)).unwrap();
                    } else {
                        write!(msg, "{} ", head).unwrap();
                    }
                    for (i, line) in cause.to_string().lines().enumerate() {
                        if i > 0 {
                            (0..=head.len()).for_each(|_| msg.push(' '));
                        }
                        msg += line;
                        msg.push('\n');
                    }
                }
                on_stderr_line(msg.trim_end())?;
                hashset!()
            }
        };
        for (i, r#extern) in cmd.externs().iter().enumerate() {
            if !uses.contains(r#extern.name()) {
                exclude.insert(i);
            }
        }

        let needs_exclude_one_by_one = loop {
            if let Some(errors) =
                cmd.capture_error_messages(&exclude, on_stdout_line, on_stderr_line)?
            {
                let mut updated = false;
                let mut num_e0432 = 0;
                let mut num_e0433 = 0;
                let mut num_e0463 = 0;
                let mut num_others = 0;

                for error in errors {
                    if_chain! {
                        if let Some(code) = &error.code;
                        if let Some(regex) = match &*code.code {
                            "E0432" => {
                                num_e0432 += 1;
                                Some(&E0432_SINGLE_MOD)
                            }
                            "E0433" => {
                                num_e0433 += 1;
                                Some(&E0433_SINGLE_MOD)
                            }
                            "E0463" => {
                                num_e0463 += 1;
                                Some(&E0463_SINGLE_MOD)
                            }
                            _ => {
                                num_others += 1;
                                None
                            }
                        };
                        if let Some(caps) = regex.captures(&error.message);
                        if let Some(pos) = cmd
                            .externs()
                            .iter()
                            .position(|e| *e.name() == caps[1]);
                        then {
                            updated |= exclude[pos];
                            exclude.set(pos, false);
                        }
                    }
                }

                on_stderr_line(&format!(
                    "E0432: {}, E0433: {}, E0483: {}, other error(s): {}",
                    num_e0432, num_e0433, num_e0463, num_others,
                ))?;

                if !updated {
                    break true;
                }
            } else {
                break false;
            }
        };

        if needs_exclude_one_by_one {
            let prev = exclude;
            exclude = FixedBitSet::with_capacity(cmd.externs().len());
            let mut success = true;
            for i in 0..cmd.externs().len() {
                if prev[i] {
                    exclude.insert(i);
                    success = cmd
                        .capture_error_messages(&exclude, on_stdout_line, on_stderr_line)?
                        .is_none();
                    exclude.set(i, success);
                }
            }
            if !success {
                exclude.set(cmd.externs().len() - 1, false);
                cmd.run(&exclude, on_stdout_line, on_stderr_line)?;
            }
        }

        let used = cmd
            .externs()
            .iter()
            .enumerate()
            .filter(|&(i, _)| !exclude[i])
            .map(|(_, e)| e.name())
            .collect::<HashSet<_>>();
        let used = self
            .extern_crate_names
            .get(&id)
            .and_then(|extern_crate_names| extern_crate_names.get(target))
            .expect("`extern_crate_names` should contain all of the targets")
            .iter()
            .filter(|(_, name)| used.contains(name.as_str()))
            .map(|(&id, _)| id);

        self.store
            .lock()
            .unwrap()
            .used_packages
            .entry(id)
            .or_insert_with(CacheUsedPackages::default)
            .insert(target, used);
        Ok(())
    }

    fn force_rebuild(&self, unit: &Unit) -> bool {
        let mut store = self.store.lock().unwrap();
        store
            .all_targets
            .entry((*unit).pkg.package_id())
            .or_insert_with(BTreeSet::new)
            .insert((*unit).target.clone());
        store
            .used_packages
            .get(&(*unit).pkg.package_id())
            .map_or(true, |v| v.get(&(*unit).target).is_none())
    }
}

#[derive(Debug)]
struct ExecStore {
    used_packages: BTreeMap<PackageId, CacheUsedPackages>,
    all_targets: BTreeMap<PackageId, BTreeSet<Target>>,
}

impl ExecStore {
    fn new(used_packages: BTreeMap<PackageId, CacheUsedPackages>) -> Self {
        Self {
            used_packages,
            all_targets: btreemap!(),
        }
    }
}
