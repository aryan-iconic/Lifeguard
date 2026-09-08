/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The two phases that turn merged facts into final verdicts.
//!
//! [`ReduceWorkspace`] holds facts that are merged but unresolved, and
//! [`ResolvedCache`] holds them once cross-library resolution has run. The
//! transition consumes the workspace, which is what keeps the states apart.
//!
//! The `LibraryCache` methods here settle import edges against the merged
//! module set, clear errors the merge has verified, and discharge obligations
//! the map could not close. They stay private to this module so callers cannot
//! resolve a `LibraryCache` in place while retaining its unresolved type.

use std::collections::HashMap;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use tracing::debug;

use crate::cache::artifact::CONSTRUCTOR_METHODS;
use crate::cache::artifact::CachedError;
#[cfg(test)]
use crate::cache::artifact::CachedExports;
use crate::cache::artifact::CachedModule;
#[cfg(test)]
use crate::cache::artifact::CachedModuleSafety;
use crate::cache::artifact::CachedReExport;
use crate::cache::artifact::CachedSafety;
use crate::cache::artifact::ConstructorCallees;
use crate::cache::artifact::LibraryCache;
use crate::cache::merge::dedupe_implicit_imports;
use crate::cache::merge::fold_constructor_callees;
use crate::cache::merge::fold_fqn_lists;
use crate::cache::merge::retain_unverified_errors;
use crate::errors::ErrorKind;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::FixedState;
#[cfg(test)]
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::hasher::union_larger;
use crate::imports::ImportGraph;
use crate::imports::resolve_to_known_module;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::mro::c3_linearize;
use crate::pyrefly::sys_info::PythonVersion;
use crate::resolution::ResolutionOutcome;
use crate::resolution::resolve_program;
use crate::resolution::unqualified_index_key;
use crate::traits::ModuleNameExt;

/// Mutable reduce workspace decoded from one or more serialized library artifacts.
pub struct ReduceWorkspace {
    cache: LibraryCache,
    graph_only_stubs: AHashSet<ModuleName>,
    artifact_module_count: usize,
    merged: MergedClassFacts,
}

/// Class facts folded together as dependency caches are consumed, so the
/// un-deduplicated concatenation of every dep's entries never exists.
///
/// This is reduce state, not artifact state: a cache read off disk has none of
/// it, and nothing here is ever written back out.
#[derive(Default)]
pub struct MergedClassFacts {
    pub(super) class_bases: HashMap<ModuleName, Vec<ModuleName>>,
    pub(super) constructor_callees: HashMap<ModuleName, ConstructorCallees>,
}

/// A reduce workspace after all cross-library semantic resolution has completed.
///
/// This intentionally retains the workspace's cache and stub facts: the
/// distinct type prevents output construction before `resolve` has consumed
/// the mutable workspace and completed semantic resolution.
pub struct ResolvedCache {
    cache: LibraryCache,
    graph_only_stubs: AHashSet<ModuleName>,
}

impl ReduceWorkspace {
    /// Wrap an already merged cache and the graph-only stubs injected into it,
    /// bypassing the stub injection that `single` and `merge` perform. Only
    /// tests want that, so the public door is
    /// [`crate::test_lib::reduce_workspace_from_merged`]; this stays crate-private
    /// so no production caller can skip the stub-set invariant.
    pub(crate) fn from_merged(cache: LibraryCache, graph_only_stubs: AHashSet<ModuleName>) -> Self {
        let artifact_module_count = cache
            .modules
            .len()
            .checked_sub(graph_only_stubs.len())
            .expect("graph-only stub count should not exceed cached module count");
        Self {
            cache,
            graph_only_stubs,
            artifact_module_count,
            merged: MergedClassFacts::default(),
        }
    }

    /// Prepare a single serialized cache for reduction by injecting bundled stubs.
    pub fn single(cache: LibraryCache, python_version: PythonVersion) -> Self {
        Self::single_with(cache, python_version, MergedClassFacts::default())
    }

    /// `single`, for a cache whose dependencies have already been folded in.
    fn single_with(
        mut cache: LibraryCache,
        python_version: PythonVersion,
        merged: MergedClassFacts,
    ) -> Self {
        let artifact_module_count = cache.modules.len();
        let graph_only_stubs = cache.inject_bundled_stub_graph(python_version);
        Self {
            cache,
            graph_only_stubs,
            artifact_module_count,
            merged,
        }
    }

    /// Merge a nonempty set of serialized caches and inject bundled stubs.
    pub fn merge(
        mut caches: Vec<LibraryCache>,
        python_version: PythonVersion,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(!caches.is_empty(), "cannot reduce an empty cache set");
        // Preserve the historical merge base and remainder order: duplicate
        // module facts can contain order-sensitive mutation candidates.
        let mut cache = caches.swap_remove(0);
        let merged = if caches.is_empty() {
            MergedClassFacts::default()
        } else {
            cache.merge_dep_caches(caches)
        };
        Ok(Self::single_with(cache, python_version, merged))
    }

    /// Return the total number of modules, including injected bundled stubs.
    pub fn module_count(&self) -> usize {
        self.cache.modules.len()
    }

    /// Return the number of modules contributed by serialized cache artifacts.
    pub fn artifact_module_count(&self) -> usize {
        self.artifact_module_count
    }

    /// Resolve cross-library errors and consume the mutable reduce workspace.
    pub fn resolve(mut self) -> ResolvedCache {
        self.cache.resolve_cross_library_errors(self.merged);
        ResolvedCache {
            cache: self.cache,
            graph_only_stubs: self.graph_only_stubs,
        }
    }
}

impl ResolvedCache {
    pub(crate) fn resolved_cache(&self) -> &LibraryCache {
        &self.cache
    }

    pub(crate) fn modules(&self) -> &[CachedModule] {
        &self.cache.modules
    }

    pub(crate) fn graph_only_stubs(&self) -> &AHashSet<ModuleName> {
        &self.graph_only_stubs
    }

    pub(crate) fn re_exports(&self) -> &[CachedReExport] {
        &self.cache.exports.re_exports
    }

    pub(crate) fn build_import_graph(&self) -> ImportGraph {
        self.cache.to_import_graph()
    }
}

/// Resolve `name` against the merged module set and, when it resolves to a module
/// other than `from`, add it to `imports` as a real edge. Self-edges are skipped
/// to mirror the whole-program builder's `try_add_edge` (which rejects them), so a
/// module resolving a missing or ambiguous submodule import to itself never
/// becomes its own dependency. Returns the resolved target — including a
/// self-resolution, which callers still record for cross-library error clearing —
/// or `None` when `name` does not resolve.
fn resolve_and_add_import_edge(
    imports: &mut AHashSet<ModuleName>,
    from: ModuleName,
    name: &ModuleName,
    module_names: &AHashSet<ModuleName>,
) -> Option<ModuleName> {
    let resolved = resolve_to_known_module(name, module_names)?;
    if resolved != from {
        imports.insert(resolved);
    }
    Some(resolved)
}

impl LibraryCache {
    /// Resolve ambiguous imports: `from X import Y` where X was in the library
    /// but X.Y was not. If X.Y resolves to a module in the merged set, it's a
    /// submodule — add it as a real import edge.
    /// Returns a map of module → newly resolved targets for downstream error clearing.
    fn resolve_ambiguous_imports(
        &mut self,
        module_names: &AHashSet<ModuleName>,
    ) -> AHashMap<ModuleName, AHashSet<ModuleName>> {
        self.modules
            .par_iter_mut()
            .filter_map(|module| {
                let mut resolved = AHashSet::new();
                for ambiguous in module.ambiguous_imports.drain() {
                    if let Some(target) = resolve_and_add_import_edge(
                        &mut module.imports,
                        module.name,
                        &ambiguous,
                        module_names,
                    ) {
                        resolved.insert(target);
                    }
                }
                (!resolved.is_empty()).then_some((module.name, resolved))
            })
            .collect()
    }

    /// Clear cached errors verified safe by the completed resolution outcome.
    /// General errors require positive resolution evidence; decorator errors
    /// can be verified from static verdicts alone.
    fn finalize_resolution(
        &mut self,
        module_names: &AHashSet<ModuleName>,
        func_safety_by_module: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
        outcome: &ResolutionOutcome,
        class_bases: &HashMap<ModuleName, Vec<ModuleName>>,
        constructor_callees: &HashMap<ModuleName, ConstructorCallees>,
    ) {
        let decorator_scan_cache: DashMap<String, bool, FixedState> = DashMap::default();
        if !outcome.promoted.is_empty() || outcome.resolved_to_safe {
            // With positive evidence (a promotion or a mutation candidate now
            // `Safe`), clear every verified-safe error kind.
            let resolver = SafetyResolver::with_safe_index(
                module_names,
                func_safety_by_module,
                &outcome.globally_safe,
            )
            .with_decorator_cache(&decorator_scan_cache)
            .with_class_bases(class_bases)
            .with_constructor_callees(constructor_callees);
            self.clear_errors_where(|caller, error| resolver.clears_error(caller, error, |_| true));
        } else {
            // Without promotion evidence, clear only static-safe kinds:
            // `UnsafeDecoratorCall` via the general verdict, plus (always, inside
            // `clears_error`) constructor-shaped `UnsafeFunctionCall` and
            // class-decorator calls, whose safety follows from static verdicts alone.
            // These checks ignore the globally-safe index, so an empty one suffices.
            let empty = AHashSet::new();
            let resolver =
                SafetyResolver::with_safe_index(module_names, func_safety_by_module, &empty)
                    .with_decorator_cache(&decorator_scan_cache)
                    .with_class_bases(class_bases)
                    .with_constructor_callees(constructor_callees);
            self.clear_errors_where(|caller, error| {
                resolver.clears_error(caller, error, |kind| kind == ErrorKind::UnsafeDecoratorCall)
            });
        }
        debug!("{} functions promoted", outcome.promoted.len());
    }

    /// Collect error names that can use the global unqualified fallback; qualified
    /// names resolve through module-specific safety maps instead.
    fn unqualified_error_names(&self) -> AHashSet<String> {
        self.modules
            .par_iter()
            .filter_map(|module| match &module.safety {
                CachedSafety::Ok(safety) => Some(safety),
                CachedSafety::AnalysisError { .. } => None,
            })
            .fold(AHashSet::new, |mut names, safety| {
                for error in &safety.errors {
                    let Some(name) = unqualified_index_key(&error.metadata) else {
                        continue;
                    };
                    if !names.contains(name) {
                        names.insert(name.to_owned());
                    }
                }
                names
            })
            .reduce(AHashSet::new, union_larger)
    }

    /// Drop every error `should_clear` admits, in parallel. Returns whether any
    /// error was removed.
    fn clear_errors_where(
        &mut self,
        should_clear: impl Fn(ModuleName, &CachedError) -> bool + Sync,
    ) -> bool {
        self.modules
            .par_iter_mut()
            .map(|module| {
                let caller = module.name;
                let CachedSafety::Ok(ref mut safety) = module.safety else {
                    return false;
                };
                retain_unverified_errors(safety, |error| should_clear(caller, error))
            })
            .reduce(|| false, |any_cleared, cleared| any_cleared || cleared)
    }

    /// Resolve missing imports against the merged cache and selectively clear
    /// false errors using per-function safety verdicts.
    pub fn resolve_cross_library_errors(&mut self, merged: MergedClassFacts) {
        let module_names: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let ambiguous_resolved = self.resolve_ambiguous_imports(&module_names);

        let mut class_bases = merged.class_bases;
        fold_fqn_lists(&mut class_bases, std::mem::take(&mut self.class_bases));
        let mut constructor_callees = merged.constructor_callees;
        fold_constructor_callees(
            &mut constructor_callees,
            std::mem::take(&mut self.constructor_callees),
        );

        self.propagate_re_export_safety();

        let mut func_safety_by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            self.modules
                .iter_mut()
                .map(|m| (m.name, std::mem::take(&mut m.function_safety)))
                .collect();

        self.modules.par_iter_mut().for_each(|module| {
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                dedupe_implicit_imports(&mut safety.implicit_imports);
            }

            let from_ambiguous = ambiguous_resolved.get(&module.name);

            if module.missing_imports.is_empty() && from_ambiguous.is_none() {
                return;
            }

            let mut still_missing: AHashSet<ModuleName> =
                AHashSet::with_capacity(module.missing_imports.len());
            let mut resolved_modules: AHashSet<ModuleName> =
                AHashSet::with_capacity(module.missing_imports.len());

            if let Some(from_ambiguous) = from_ambiguous {
                resolved_modules.extend(from_ambiguous.iter().copied());
            }

            for missing in module.missing_imports.drain() {
                match resolve_and_add_import_edge(
                    &mut module.imports,
                    module.name,
                    &missing,
                    &module_names,
                ) {
                    Some(resolved) => {
                        resolved_modules.insert(resolved);
                    }
                    None => {
                        still_missing.insert(missing);
                    }
                }
            }

            module.missing_imports = still_missing;

            let caller = module.name;
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                let resolver = SafetyResolver::new(&resolved_modules, &func_safety_by_module)
                    .with_class_bases(&class_bases)
                    .with_constructor_callees(&constructor_callees);
                // Same decision as the final clear: this pass has no promotion
                // evidence to gate on, but a recorded constructor callee still
                // has to outrank the class's aggregate verdict here, or the
                // error is gone before `finalize_resolution` ever sees it.
                retain_unverified_errors(safety, |error| {
                    resolver.clears_error(caller, error, |_| true)
                });
            }
        });

        let needed_unqualified = self.unqualified_error_names();
        let mut module_errors: HashMap<ModuleName, Vec<String>> = HashMap::new();
        let outcome = resolve_program(
            &module_names,
            &mut func_safety_by_module,
            self.modules
                .iter()
                .map(|module| (module.name, module.mutation_candidates.as_slice())),
            needed_unqualified,
            |module_name, metadata| {
                module_errors.entry(module_name).or_default().push(metadata);
            },
        );
        for module in &mut self.modules {
            let Some(errors) = module_errors.get(&module.name) else {
                continue;
            };
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                safety
                    .errors
                    .extend(errors.iter().map(|metadata| CachedError {
                        kind: ErrorKind::ImportedVarArgument,
                        metadata: metadata.clone(),
                        parameterized_decorator: false,
                    }));
            }
        }

        self.finalize_resolution(
            &module_names,
            &func_safety_by_module,
            &outcome,
            &class_bases,
            &constructor_callees,
        );

        // Return the verdicts taken at the top; resolution needed them in one flat
        // map to do cross-module lookups while `self.modules` was borrowed mutably.
        for module in &mut self.modules {
            if let Some(fs) = func_safety_by_module.remove(&module.name) {
                module.function_safety = fs;
            }
        }
    }
}

/// Whether `local_name` is cached `Safe` in `fs`.
/// Inherited methods resolve up the class MRO in the resolver
/// (`SafetyResolver::mro_method_verdict`).
fn lookup_in_safety_map(local_name: &str, fs: &AHashMap<String, FunctionSafetyInfo>) -> bool {
    fs.get(local_name)
        .is_some_and(|info| info.verdict.is_safe())
}

/// The merged per-function verdicts plus the module set they resolve against —
/// shared context for reduce-time error clearing and promotion.
///
/// A qualified name (`mod.sub.func`) is split at its longest module prefix and
/// looked up there; an unqualified name (`helper`) uses `globally_safe` if
/// present, else scans `modules`.
#[derive(Clone, Copy)]
struct SafetyResolver<'a> {
    modules: &'a AHashSet<ModuleName>,
    by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    /// `Some` enables O(1) unqualified lookups; `None` scans `modules`.
    globally_safe: Option<&'a AHashSet<String>>,
    /// Caches `scan_unqualified_decorator_safe` by name, so its O(modules) scan
    /// runs once per distinct decorator instead of once per call site.
    decorator_scan_cache: Option<&'a DashMap<String, bool, FixedState>>,
    /// Class FQN -> base FQNs, enabling MRO resolution of inherited
    /// `Class.method` calls when there is no exact method verdict.
    class_bases: Option<&'a HashMap<ModuleName, Vec<ModuleName>>>,
    /// Map-phase-resolved constructor callees, keyed by class FQN. When present
    /// for a class, these replace re-deriving its constructor method set.
    constructor_callees: Option<&'a HashMap<ModuleName, ConstructorCallees>>,
}

impl<'a> SafetyResolver<'a> {
    /// No prebuilt indices — the unqualified fallback scans `modules`.
    fn new(
        modules: &'a AHashSet<ModuleName>,
        by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    ) -> Self {
        SafetyResolver {
            modules,
            by_module,
            globally_safe: None,
            decorator_scan_cache: None,
            class_bases: None,
            constructor_callees: None,
        }
    }

    /// Backed by the prebuilt globally-safe index for O(1) unqualified lookups.
    fn with_safe_index(
        modules: &'a AHashSet<ModuleName>,
        by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
        globally_safe: &'a AHashSet<String>,
    ) -> Self {
        SafetyResolver {
            modules,
            by_module,
            globally_safe: Some(globally_safe),
            decorator_scan_cache: None,
            class_bases: None,
            constructor_callees: None,
        }
    }

    fn with_decorator_cache(mut self, cache: &'a DashMap<String, bool, FixedState>) -> Self {
        self.decorator_scan_cache = Some(cache);
        self
    }

    /// Attach class base edges.
    fn with_class_bases(mut self, class_bases: &'a HashMap<ModuleName, Vec<ModuleName>>) -> Self {
        self.class_bases = Some(class_bases);
        self
    }

    /// Attach the map phase's resolved constructor callees.
    fn with_constructor_callees(
        mut self,
        constructor_callees: &'a HashMap<ModuleName, ConstructorCallees>,
    ) -> Self {
        self.constructor_callees = Some(constructor_callees);
        self
    }

    /// Resolve `local` = `Class.method` (or `Outer.Inner.method`) up the MRO of
    /// `module`.`Class`: return the verdict of the first ancestor, in C3 method
    /// resolution order, that defines an exact `Base.method` entry, or `None` if
    /// no reachable ancestor defines it. `Class` itself is skipped — its own
    /// method is checked by the caller before falling back to the MRO.
    fn mro_method_verdict(&self, module: &ModuleName, local: &str) -> Option<FunctionSafety> {
        let class_bases = self.class_bases?;
        let (class_local, method) = local.rsplit_once('.')?;
        let class_fqn = module.append_str(class_local);
        for ancestor in c3_linearize(class_bases, &class_fqn).iter().skip(1) {
            let candidate = ancestor.append_str(method);
            if let Some((bmod, blocal)) = self.split_at_module(candidate.as_str()) {
                if let Some(info) = self.by_module.get(&bmod).and_then(|fs| fs.get(blocal)) {
                    return Some(info.verdict);
                }
            }
        }
        None
    }

    /// The longest prefix of `func_name` naming a module in `self.modules`,
    /// paired with the remaining local name; `None` if unqualified.
    fn split_at_module<'n>(&self, func_name: &'n str) -> Option<(ModuleName, &'n str)> {
        let fqn = ModuleName::from_str(func_name);
        fqn.iter_parents()
            .find(|(parent, _)| self.modules.contains(parent))
            .map(|(parent, dot_pos)| (parent, &func_name[dot_pos + 1..]))
    }

    /// Whether an unqualified name is verified safe: the index when present,
    /// else a scan of `modules`.
    fn unqualified_safe(&self, func_name: &str) -> bool {
        if let Some(index) = self.globally_safe {
            return index.contains(func_name);
        }
        self.modules
            .iter()
            .filter_map(|m| self.by_module.get(m))
            .filter_map(|fs| fs.get(func_name))
            .any(|info| info.verdict.is_safe())
    }

    /// `module`'s own verdict for `local`, or `None` when it has no such entry.
    /// The MRO must only be walked when the class itself does not define the method.
    fn own_verdict(&self, module: &ModuleName, local: &str) -> Option<FunctionSafety> {
        Some(self.by_module.get(module)?.get(local)?.verdict)
    }

    /// Whether `module` has an own entry for `local` verified `Safe`.
    fn own_call_safe(&self, module: &ModuleName, local: &str) -> bool {
        self.own_verdict(module, local).is_some_and(|v| v.is_safe())
    }

    /// Whether a plain function call is found and verified `Safe`.
    fn is_call_verified_safe(&self, func_name: &str) -> bool {
        match self.split_at_module(func_name) {
            Some((module, local)) => match self.own_verdict(&module, local) {
                Some(verdict) => verdict.is_safe(),
                None => self
                    .mro_method_verdict(&module, local)
                    .is_some_and(|v| v.is_safe()),
            },
            None => self.unqualified_safe(func_name),
        }
    }

    /// Like `is_call_verified_safe`, but only an own entry under a
    /// module-qualified name clears. Both restrictions follow from an `Unknown*`
    /// call target meaning `func_name` is a best-effort textual name rather than a
    /// proven callee:
    /// - an unqualified name must not clear on a same-named safe function in
    ///   some resolved module (or in the global index);
    /// - the MRO fallback does not apply, since walking a class hierarchy for a
    ///   name that was never bound to that class is speculative.
    fn is_call_verified_safe_no_unqualified(&self, func_name: &str) -> bool {
        self.split_at_module(func_name)
            .is_some_and(|(module, local)| self.own_call_safe(&module, local))
    }

    /// Whether a parameterized-decorator call is safe: the factory AND every
    /// immediate nested function must be `Safe`, since the factory runs its
    /// returned wrapper at decoration time. Never consults `globally_safe`
    /// (own-verdict only).
    fn is_decorator_call_verified_safe(&self, func_name: &str) -> bool {
        if let Some((module, local)) = self.split_at_module(func_name) {
            return self
                .by_module
                .get(&module)
                .is_some_and(|fs| lookup_decorator_in_safety_map(local, fs));
        }
        let Some(cache) = self.decorator_scan_cache else {
            return self.scan_unqualified_decorator_safe(func_name);
        };
        if let Some(cached) = cache.get(func_name) {
            return *cached;
        }
        let result = self.scan_unqualified_decorator_safe(func_name);
        cache.insert(func_name.to_owned(), result);
        result
    }

    /// Whether any module has `func_name` as a decorator-verified-safe function.
    /// O(modules); callers should memoize by name (see `decorator_scan_cache`).
    fn scan_unqualified_decorator_safe(&self, func_name: &str) -> bool {
        self.modules
            .iter()
            .filter_map(|m| self.by_module.get(m))
            .any(|fs| lookup_decorator_in_safety_map(func_name, fs))
    }

    /// The combined verdict of the constructor callees the map phase recorded for
    /// `class_fqn`, or `None` when it recorded none (i.e. no visible constructor methods).
    fn recorded_constructor_verdict(&self, class_fqn: &ModuleName) -> Option<FunctionSafety> {
        let recorded = self.constructor_callees?.get(class_fqn)?;
        let derived = recorded
            .iter(*class_fqn)
            .map(|(owner, method)| self.callee_verdict(&owner, method));
        let extra = recorded
            .extra
            .iter()
            .map(|callee| self.recorded_callee_verdict(callee));
        derived.chain(extra).reduce(|acc, verdict| acc | verdict)
    }

    /// The verdict of a callee recorded by its full FQN, resolved the same way
    /// `callee_verdict` resolves a derived one.
    fn recorded_callee_verdict(&self, callee: &ModuleName) -> FunctionSafety {
        self.split_at_module(callee.as_str())
            .and_then(|(module, local)| self.own_verdict(&module, local))
            .unwrap_or(FunctionSafety::Unsafe)
    }

    /// A recorded callee's verdict, treating one that no longer resolves as
    /// `Unsafe`: the map phase saw it run, so losing sight of it is not evidence
    /// that it is safe. The callee's own FQN is never built as a `ModuleName`,
    /// since interning it would outlive the lookup.
    fn callee_verdict(&self, owner: &ModuleName, method: &str) -> FunctionSafety {
        self.split_at_module(owner.as_str())
            .and_then(|(module, local)| self.own_verdict(&module, &format!("{local}.{method}")))
            .unwrap_or(FunctionSafety::Unsafe)
    }

    /// Whether a constructor verdict lets its call clear. `UnsafeIfImported`
    /// means safe only within the defining module, so it clears only when the
    /// caller is that module.
    fn constructor_verdict_clears(
        &self,
        verdict: FunctionSafety,
        caller_module: &ModuleName,
        class_fqn: &ModuleName,
    ) -> bool {
        if verdict == FunctionSafety::Safe {
            return true;
        }
        // Only the module that defines the class may clear an `UnsafeIfImported`
        // constructor. Matching any ancestor package would let the importing
        // module clear it too, which is the opposite of what the verdict means.
        verdict == FunctionSafety::UnsafeIfImported
            && self
                .split_at_module(class_fqn.as_str())
                .is_some_and(|(defining, _)| &defining == caller_module)
    }

    /// Dispatch a cached error to the right verified-safe check by kind. The
    /// callee `metadata` may render with trailing `()` suffixes; strip them here.
    ///
    /// `UnknownFunctionCall` / `UnknownMethodCall` couldn't bind the call target,
    /// so they additionally skip the unqualified fallback: an unbound short name
    /// must not clear on a same-named safe function elsewhere.
    fn is_error_verified_safe(&self, error: &CachedError) -> bool {
        let func_name = error.metadata.trim_end_matches("()");
        match error.kind {
            ErrorKind::UnsafeDecoratorCall | ErrorKind::UnknownDecoratorCall
                if error.parameterized_decorator =>
            {
                self.is_decorator_call_verified_safe(func_name)
            }
            ErrorKind::UnknownFunctionCall | ErrorKind::UnknownMethodCall => {
                self.is_call_verified_safe_no_unqualified(func_name)
            }
            _ => self.is_call_verified_safe(func_name),
        }
    }

    /// Whether `error` in `caller` may be dropped.
    ///
    /// A call to a class the map phase recorded constructor callees for is
    /// decided by those callees alone. Such a call also does not consult `kinds`;
    /// its answer follows from static verdicts, with no promotion evidence needed.
    ///
    /// Every other error clears only when `kinds` admits it and the general
    /// verdict verifies it.
    fn clears_error(
        &self,
        caller: ModuleName,
        error: &CachedError,
        kinds: impl Fn(ErrorKind) -> bool,
    ) -> bool {
        if let Some(cleared) = self.recorded_constructor_clears(caller, error) {
            return cleared;
        }
        kinds(error.kind) && self.is_error_verified_safe(error)
    }

    /// `Some(cleared)` when `error` is a call to a class with recorded
    /// constructor callees, `None` when no record applies and the general path
    /// decides.
    ///
    /// `Unknown*` kinds are included because they are what the map emits
    /// for a class it could not bind -- the cross-library instantiation the
    /// recorded callees exist to answer. The lookup is an exact match on a
    /// recorded class FQN, so an unbound short name still cannot clear here.
    fn recorded_constructor_clears(&self, caller: ModuleName, error: &CachedError) -> Option<bool> {
        match error.kind {
            ErrorKind::UnsafeFunctionCall
            | ErrorKind::UnknownFunctionCall
            | ErrorKind::UnsafeDecoratorCall
            | ErrorKind::UnknownDecoratorCall => {
                let func_name = error.metadata.trim_end_matches("()");
                let fqn = ModuleName::from_str(func_name);
                let verdict = self.recorded_constructor_verdict(&fqn)?;
                Some(self.constructor_verdict_clears(verdict, &caller, &fqn))
            }
            _ => None,
        }
    }
}

/// Whether a plain function call can be verified as safe using cached
/// per-function safety verdicts from the resolved modules.
#[doc(hidden)]
pub fn is_call_verified_safe(
    func_name: &str,
    resolved_modules: &AHashSet<ModuleName>,
    func_safety_by_module: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> bool {
    SafetyResolver::new(resolved_modules, func_safety_by_module).is_call_verified_safe(func_name)
}

/// Whether a decorator is safe: safe itself AND every immediate (one level deep)
/// nested function is safe. For `deco`, `deco.builder` is checked; `deco.b.inner`
/// and `deco_helper` are not.
fn lookup_decorator_in_safety_map(
    local_name: &str,
    fs: &AHashMap<String, FunctionSafetyInfo>,
) -> bool {
    if !lookup_in_safety_map(local_name, fs) {
        return false;
    }
    // A class decorator returns the class, so its constructor methods (not
    // arbitrary nested defs) govern import-time safety; the aggregate-safe
    // factory verdict already reflects them.
    if is_class_like_entry(local_name, fs) {
        return true;
    }
    fs.iter().all(|(name, info)| {
        let is_immediate_child = name
            .strip_prefix(local_name)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|child| !child.contains('.'));
        !is_immediate_child || info.verdict == FunctionSafety::Safe
    })
}

/// The `function_safety` entry names of `local_name`'s constructor methods.
fn constructors(local_name: &str) -> impl Iterator<Item = String> + '_ {
    CONSTRUCTOR_METHODS
        .into_iter()
        .map(move |method| format!("{local_name}.{method}"))
}

/// Whether `local_name` names a class: it has a cached `__init__`/`__new__`.
fn is_class_like_entry(local_name: &str, fs: &AHashMap<String, FunctionSafetyInfo>) -> bool {
    constructors(local_name).any(|method| fs.contains_key(&method))
}

#[cfg(test)]
mod tests {
    use rayon::ThreadPoolBuilder;

    use super::*;
    use crate::effects::ImportedArgs;
    use crate::module_safety::MutationCandidate;
    use crate::module_safety::MutationCandidateSite;

    #[test]
    fn reduce_workspace_rejects_empty_cache_set() {
        assert!(ReduceWorkspace::merge(Vec::new(), PythonVersion::default()).is_err());
    }

    #[test]
    #[should_panic(expected = "graph-only stub count should not exceed cached module count")]
    fn reduce_workspace_rejects_inconsistent_stub_count() {
        let cache = LibraryCache {
            modules: Vec::new(),
            exports: CachedExports {
                re_exports: Vec::new(),
            },
            ..Default::default()
        };
        let graph_only_stubs = AHashSet::from_iter([ModuleName::from_str("missing_stub")]);

        ReduceWorkspace::from_merged(cache, graph_only_stubs);
    }

    #[test]
    fn reduce_workspace_merge_preserves_historical_cache_order() {
        fn cache_with_candidate(module: ModuleName, call: &str) -> LibraryCache {
            let mut cached_module = CachedModule::empty(module);
            cached_module.mutation_candidates.push(MutationCandidate {
                callee: ModuleName::from_str("dependency.mutate"),
                site: MutationCandidateSite::ModuleScope {
                    call: ModuleName::from_str(call),
                },
                arg_offset: 0,
                imported_args: ImportedArgs::default(),
            });
            LibraryCache {
                modules: vec![cached_module],
                exports: CachedExports {
                    re_exports: Vec::new(),
                },
                ..Default::default()
            }
        }

        let module = ModuleName::from_str("pkg.module");
        let workspace = ReduceWorkspace::merge(
            vec![
                cache_with_candidate(module, "first"),
                cache_with_candidate(module, "middle"),
                cache_with_candidate(module, "last"),
            ],
            PythonVersion::default(),
        )
        .expect("nonempty caches should merge");
        let merged = workspace
            .cache
            .modules
            .iter()
            .find(|cached| cached.name == module)
            .expect("merged cache should contain the input module");
        let calls: Vec<&str> = merged
            .mutation_candidates
            .iter()
            .map(|candidate| match &candidate.site {
                MutationCandidateSite::ModuleScope { call } => call.as_str(),
                _ => panic!("expected module-scope mutation candidate"),
            })
            .collect();

        assert_eq!(calls, ["first", "last", "middle"]);
    }

    #[test]
    fn mro_resolves_inherited_method_to_base_verdict() {
        let modules: AHashSet<ModuleName> =
            [ModuleName::from_str("base"), ModuleName::from_str("sub")]
                .into_iter()
                .collect();
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::new();
        by_module.insert(
            ModuleName::from_str("base"),
            [(
                "Base.method".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
        );
        // `sub.Sub` inherits `method` from `base.Base`; it has no own entry.
        by_module.insert(ModuleName::from_str("sub"), AHashMap::new());
        let class_bases: HashMap<ModuleName, Vec<ModuleName>> = [(
            ModuleName::from_str("sub.Sub"),
            vec![ModuleName::from_str("base.Base")],
        )]
        .into_iter()
        .collect();

        let resolver = SafetyResolver::new(&modules, &by_module).with_class_bases(&class_bases);
        assert!(
            resolver.is_call_verified_safe("sub.Sub.method"),
            "an inherited method resolves to the defining base's Safe verdict via the MRO",
        );

        let no_mro = SafetyResolver::new(&modules, &by_module);
        assert!(
            !no_mro.is_call_verified_safe("sub.Sub.method"),
            "without MRO data an inherited method is not verified (no class fallback)",
        );
    }

    #[test]
    fn mro_own_unsafe_override_shadows_safe_base() {
        let modules: AHashSet<ModuleName> =
            [ModuleName::from_str("base"), ModuleName::from_str("sub")]
                .into_iter()
                .collect();
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::new();
        by_module.insert(
            ModuleName::from_str("base"),
            [(
                "Base.method".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
        );
        // `sub.Sub` overrides the inherited `method` with an `Unsafe` one.
        by_module.insert(
            ModuleName::from_str("sub"),
            [(
                "Sub.method".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Unsafe),
            )]
            .into_iter()
            .collect(),
        );
        let class_bases: HashMap<ModuleName, Vec<ModuleName>> = [(
            ModuleName::from_str("sub.Sub"),
            vec![ModuleName::from_str("base.Base")],
        )]
        .into_iter()
        .collect();

        let resolver = SafetyResolver::new(&modules, &by_module).with_class_bases(&class_bases);
        assert!(
            !resolver.is_call_verified_safe("sub.Sub.method"),
            "an own Unsafe override shadows the base's Safe verdict; the MRO must not be walked",
        );
    }

    #[test]
    fn mro_diamond_prefers_right_branch_override_over_shared_ancestor() {
        let module = ModuleName::from_str("m");
        let modules: AHashSet<ModuleName> = [module].into_iter().collect();
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::new();
        by_module.insert(
            module,
            [
                (
                    "A.method".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                ),
                (
                    "C.method".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Unsafe),
                ),
            ]
            .into_iter()
            .collect(),
        );
        // D(B, C); B(A); C(A). `method` is Safe on the shared ancestor A and
        // overridden Unsafe on the right branch C. C3 MRO of D is [D, B, C, A],
        // so D.method resolves to C (Unsafe); a depth-first walk would wrongly
        // reach A (Safe) first and clear the call.
        let class_bases: HashMap<ModuleName, Vec<ModuleName>> = [
            (
                ModuleName::from_str("m.D"),
                vec![ModuleName::from_str("m.B"), ModuleName::from_str("m.C")],
            ),
            (
                ModuleName::from_str("m.B"),
                vec![ModuleName::from_str("m.A")],
            ),
            (
                ModuleName::from_str("m.C"),
                vec![ModuleName::from_str("m.A")],
            ),
        ]
        .into_iter()
        .collect();

        let resolver = SafetyResolver::new(&modules, &by_module).with_class_bases(&class_bases);
        assert!(
            !resolver.is_call_verified_safe("m.D.method"),
            "diamond method resolves via C3 to the Unsafe right-branch override, not the Safe ancestor",
        );
    }

    #[test]
    fn clear_verified_errors_processes_every_module() {
        // Callees are module-qualified so the conservative `Unknown*` path can
        // bind them per module; an unqualified short name is never cleared.
        let module_a = ModuleName::from_str("test.module_a");
        let module_b = ModuleName::from_str("test.module_b");

        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: module_a,
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "test.module_a.helper()".to_owned(),
                            parameterized_decorator: false,
                        }],
                        force_imports_eager_overrides: Vec::new(),
                        implicit_imports: Vec::new(),
                    }),
                    imports: AHashSet::new(),
                    missing_imports: AHashSet::new(),
                    ambiguous_imports: AHashSet::new(),
                    side_effect_imports: AHashSet::new(),
                    function_safety: AHashMap::new(),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: module_b,
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "test.module_b.helper()".to_owned(),
                            parameterized_decorator: false,
                        }],
                        force_imports_eager_overrides: Vec::new(),
                        implicit_imports: Vec::new(),
                    }),
                    imports: AHashSet::new(),
                    missing_imports: AHashSet::new(),
                    ambiguous_imports: AHashSet::new(),
                    side_effect_imports: AHashSet::new(),
                    function_safety: AHashMap::new(),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: CachedExports {
                re_exports: Vec::new(),
            },
            ..Default::default()
        };

        let module_names: AHashSet<ModuleName> = [module_a, module_b].into_iter().collect();
        let func_safety_by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> = [
            (
                module_a,
                [(
                    "helper".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                )]
                .into_iter()
                .collect(),
            ),
            (
                module_b,
                [(
                    "helper".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                )]
                .into_iter()
                .collect(),
            ),
        ]
        .into_iter()
        .collect();
        let globally_safe_funcs: AHashSet<String> = ["helper".to_owned()].into_iter().collect();

        let resolver = SafetyResolver::with_safe_index(
            &module_names,
            &func_safety_by_module,
            &globally_safe_funcs,
        );
        let cleared = ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("should build test thread pool")
            .install(|| {
                cache.clear_errors_where(|caller, error| {
                    resolver.clears_error(caller, error, |_| true)
                })
            });

        assert!(
            cleared,
            "expected at least one verified error to be removed"
        );
        assert!(
            cache.modules.iter().all(CachedModule::is_safe),
            "all modules should have their verified errors cleared",
        );
    }
}
