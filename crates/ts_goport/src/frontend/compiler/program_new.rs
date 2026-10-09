//! Go `internal/compiler/program.go` lines 1 to 444: program options, the
//! `Program` struct, construction, update and the simple accessors.
//! The `Program` type is `NewProgram` (the loader contract name), so it does
//! not clash with the program state in program.rs.

use crate::contentmapper::{Mapper, Project};
use crate::frontend::prelude::*;
use std::cell::OnceCell;

/// Go `compiler.ProgramOptions`.
// PORT: `CreateCheckerPool` is dropped. The checker pool stays in
// program.rs. `Tracing` is the process session (`crate::tracing::get`).
#[derive(Clone)]
pub struct ProgramOptions {
    pub host: Rc<dyn CompilerHost>,
    pub config: Rc<ParsedCommandLine>,
    pub use_source_of_project_reference: bool,
    pub single_threaded: Tristate,
    pub typings_location: String,
    pub project_name: String,
    // ts#64299. PORT: a nil Go func is `None`.
    pub create_module_resolver: Option<Rc<dyn Fn(ResolverOptions) -> Rc<dyn Resolver>>>,
    // SkipModuleResolution avoids all module and type reference resolution while
    // still collecting import metadata needed for emit.
    pub skip_module_resolution: bool,
}

impl ProgramOptions {
    // Go: program.go:52 (*ProgramOptions).canUseProjectReferenceSource
    pub fn can_use_project_reference_source(&self) -> bool {
        self.use_source_of_project_reference
            && !self
                .config
                .compiler_options()
                .disable_source_of_project_reference_redirect
                .is_true()
    }
}

/// Go `lazyValue[T]`.
// PORT: Go `sync.Once` plus `initialized` is a `OnceCell` (single thread).
// Go stores a `*T` and shares it in `tryReuse`; Rust clones the value.
pub struct LazyValue<T> {
    value: OnceCell<T>,
}

impl<T> Default for LazyValue<T> {
    fn default() -> Self {
        Self {
            value: OnceCell::new(),
        }
    }
}

impl<T: Clone> LazyValue<T> {
    // Go: program.go:62 (*lazyValue[T]).getValue
    pub fn get_value(&self, compute: impl FnOnce() -> T) -> &T {
        self.value.get_or_init(compute)
    }

    /// The value when it is computed, else None (Go `initialized.Load()`,
    /// as `tryReuse` reads it).
    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    // Go: program.go:72 (*lazyValue[T]).tryReuse
    pub fn try_reuse(&mut self, from: &LazyValue<T>) {
        if let Some(value) = from.value.get() {
            self.value = OnceCell::from(value.clone());
        }
    }
}

// Go: fileloader.go:78 (*redirectsFile).FileName, fileloader.go:82 (*redirectsFile).Path
// PORT: Go `*redirectsFile` implements `ast.HasFileName`. The inherent
// methods are in file_loader.rs; no other unit adds the trait impl.
impl HasFileName for RedirectsFile {
    fn file_name(&self) -> String {
        RedirectsFile::file_name(self)
    }
    fn path(&self) -> Path {
        RedirectsFile::path(self)
    }
}

/// Go `compiler.Program`.
// PORT: Go embeds `processedFiles`. Rust holds it in `processed_files` and
// derefs to it, so `self.files` works as in Go.
// PORT: dropped fields: `checkerPool` and `compilerCheckerPool` (the pool
// stays in program.rs), `declarationDiagnosticCache` (declaration emit),
// `knownSymlinks`, `packageNames`, `hasTSFile` and `packagesMap` (language
// service and auto-imports). None of them is read during construction.
pub struct NewProgram {
    pub opts: ProgramOptions,
    // Go never sets this field in `NewProgram`; it keeps the zero value.
    pub compare_paths_options: ComparePathsOptions,
    pub processed_files: ProcessedFiles,
    // Go never sets this field in `NewProgram`; it keeps the zero value.
    pub uses_uri_style_node_core_modules: Tristate,
    // Go `commonSourceDirectory` plus `commonSourceDirectoryOnce`.
    pub common_source_directory: OnceCell<String>,
    pub program_diagnostics: Vec<Diagnostic>,
    pub has_emit_blocking_diagnostics: FxHashSet<Path>,
    // Go `sourceFilesToEmit` plus `sourceFilesToEmitOnce`.
    pub source_files_to_emit: OnceCell<Vec<Rc<ParsedSourceFile>>>,
    // Cached unresolved imports for ATA
    pub unresolved_imports: LazyValue<FxHashSet<String>>,
    pub known_symlinks: LazyValue<Rc<KnownSymlinks>>,
    // Used by auto-imports
    pub package_names: LazyValue<Rc<PackageNamesInfo>>,
    // Go `hasTSFileOnce` plus `hasTSFile`.
    pub has_ts_file: OnceCell<bool>,
}

impl std::ops::Deref for NewProgram {
    type Target = ProcessedFiles;
    fn deref(&self) -> &ProcessedFiles {
        &self.processed_files
    }
}

impl std::ops::DerefMut for NewProgram {
    fn deref_mut(&mut self) -> &mut ProcessedFiles {
        &mut self.processed_files
    }
}

impl NewProgram {
    // PORT: Go `p.resolver` and `p.projectReferenceFileMapper` are set by
    // `processAllProgramFiles`. `ProcessedFiles` holds them as `Option`, so
    // these read them and panic on the Go nil.
    fn resolver_ref(&self) -> &Rc<dyn Resolver> {
        self.resolver.as_ref().expect("program resolver is not set")
    }

    pub(crate) fn mapper(&self) -> std::cell::Ref<'_, ProjectReferenceFileMapper> {
        self.project_reference_file_mapper
            .as_ref()
            .expect("program project reference file mapper is not set")
            .borrow()
    }

    // Go: program.go:129 (*Program).FileExists
    pub fn file_exists(&self, path: &str) -> bool {
        self.host().fs().file_exists(path)
    }

    // Go: program.go:138 (*Program).ContentMapperProject (tsgo#4712)
    pub fn content_mapper_project(&self) -> Option<Rc<dyn Project>> {
        self.opts.host.content_mapper_project()
    }

    // Go: program.go:134 (*Program).GetCurrentDirectory
    pub fn get_current_directory(&self) -> String {
        self.host().get_current_directory()
    }

    // Go: program.go:143 (*Program).GetGlobalTypingsCacheLocation
    pub fn get_global_typings_cache_location(&self) -> String {
        self.opts.typings_location.clone()
    }

    // Go: program.go:148 (*Program).GetNearestAncestorDirectoryWithPackageJson
    pub fn get_nearest_ancestor_directory_with_package_json(&self, dirname: &str) -> String {
        let scoped = self.resolver_ref().get_package_scope_for_path(dirname);
        if let Some(scoped) = scoped
            && scoped.exists()
        {
            return scoped.package_directory.clone();
        }
        String::new()
    }

    // Go: program.go:157 (*Program).GetPackageJsonInfo
    pub fn get_package_json_info(&self, pkg_json_path: &str) -> Option<Rc<InfoCacheEntry>> {
        let directory = get_directory_path(pkg_json_path);
        let scoped = self.resolver_ref().get_package_scope_for_path(&directory);
        if let Some(scoped) = scoped
            && scoped.exists()
            && scoped.package_directory == directory
        {
            return Some(scoped);
        }
        None
    }

    // Go: program.go:167 (*Program).PackageJsonCacheEntries (tsgo#4301)
    // PackageJsonCacheEntries iterates on all package json cache entries.
    pub fn package_json_cache_entries(
        &self,
        mut f: impl FnMut(&Path, PackageJsonCacheEntry<'_>) -> bool,
    ) {
        self.resolver_ref().package_json_cache_entries(&mut f);
    }

    // Go: program.go:173 (*Program).GetRedirectTargets
    // GetRedirectTargets returns the list of file paths that redirect to the given path.
    // These are files from the same package (same name@version) installed in different locations.
    pub fn get_redirect_targets(&self, path: &Path) -> Vec<String> {
        self.redirect_targets_map
            .as_ref()
            .and_then(|targets| targets.get(path))
            .cloned()
            .unwrap_or_default()
    }

    // Go: program.go:181 (*Program).GetSourceOfProjectReferenceIfOutputIncluded
    // gets the original file that was included in program
    // this returns original source file name when including output of project reference
    // otherwise same name
    pub fn get_source_of_project_reference_if_output_included(
        &self,
        file: &dyn HasFileName,
    ) -> String {
        if let Some(source) = self
            .output_file_to_project_reference_source
            .as_ref()
            .and_then(|sources| sources.get(&file.path()))
        {
            return source.clone();
        }
        file.file_name()
    }

    // Go: program.go:189 (*Program).GetProjectReferenceFromSource
    pub fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<Rc<SourceOutputAndProjectReference>> {
        self.mapper().get_project_reference_from_source(path)
    }

    // Go: program.go:194 (*Program).IsSourceFromProjectReference
    pub fn is_source_from_project_reference(&self, path: &Path) -> bool {
        self.mapper().is_source_from_project_reference(path)
    }

    // Go: program.go:198 (*Program).GetProjectReferenceFromOutputDts
    pub fn get_project_reference_from_output_dts(
        &self,
        path: &Path,
    ) -> Option<Rc<SourceOutputAndProjectReference>> {
        self.mapper().get_project_reference_from_output_dts(path)
    }

    // Go: program.go:202 (*Program).GetResolvedProjectReferenceFor
    pub fn get_resolved_project_reference_for(
        &self,
        path: &Path,
    ) -> (Option<Rc<ParsedCommandLine>>, bool) {
        self.mapper().get_resolved_reference_for(path)
    }

    // Go: program.go:206 (*Program).GetRedirectForResolution
    pub fn get_redirect_for_resolution(
        &self,
        file: &dyn HasFileName,
    ) -> Option<Rc<ParsedCommandLine>> {
        let (redirect, _) = self.mapper().get_redirect_for_resolution(file);
        redirect
    }

    // Go: program.go:211 (*Program).GetParseFileRedirect
    pub fn get_parse_file_redirect(&self, file_name: &str) -> String {
        self.mapper()
            .get_parse_file_redirect(&new_has_file_name(file_name, &self.to_path(file_name)))
    }

    // Go: program.go:215 (*Program).GetResolvedProjectReferences
    pub fn get_resolved_project_references(&self) -> Vec<Option<Rc<ParsedCommandLine>>> {
        self.mapper().get_resolved_project_references()
    }

    // Go: program.go:219 (*Program).RangeResolvedProjectReference
    pub fn range_resolved_project_reference(
        &self,
        f: impl FnMut(
            &Path,
            Option<&Rc<ParsedCommandLine>>,
            Option<&Rc<ParsedCommandLine>>,
            usize,
        ) -> bool,
    ) -> bool {
        self.mapper().range_resolved_project_reference(f)
    }

    // Go: program.go:223 (*Program).RangeResolvedProjectReferenceInChildConfig
    pub fn range_resolved_project_reference_in_child_config(
        &self,
        child_config: &Rc<ParsedCommandLine>,
        f: impl FnMut(
            &Path,
            Option<&Rc<ParsedCommandLine>>,
            Option<&Rc<ParsedCommandLine>>,
            usize,
        ) -> bool,
    ) -> bool {
        self.mapper()
            .range_resolved_project_reference_in_child_config(child_config, f)
    }

    // Go: program.go:231 (*Program).UseCaseSensitiveFileNames
    pub fn use_case_sensitive_file_names(&self) -> bool {
        self.host().fs().use_case_sensitive_file_names()
    }

    // Go: program.go:235 (*Program).UsesUriStyleNodeCoreModules
    pub fn uses_uri_style_node_core_modules(&self) -> Tristate {
        self.uses_uri_style_node_core_modules
    }

    // Go: program.go:242 (*Program).GetSourceFileFromReference
    /** This should have similar behavior to 'processSourceFile' without diagnostics or mutation. */
    pub fn get_source_file_from_reference(
        &self,
        origin: &ParsedSourceFile,
        r: &FileReference,
    ) -> Option<Rc<ParsedSourceFile>> {
        // TODO: The module loader in corsa is fairly different than strada, it should probably be able to expose this functionality at some point,
        // rather than redoing the logic approximately here, since most of the related logic now lives in module.Resolver
        // Still, without the failed lookup reporting that only the loader does, this isn't terribly complicated

        let file_name = resolve_path(&get_directory_path(origin.file_name()), &[&r.file_name]);
        let supported_extensions_base = get_supported_extensions(
            self.options(),
            &self.command_line().content_mapper_extensions(),
        );
        let supported_extensions = get_supported_extensions_with_json_if_resolve_json_module(
            Some(self.options()),
            supported_extensions_base,
        );
        let allow_non_ts_extensions = self.options().allow_non_ts_extensions.is_true();
        if has_extension(&file_name) {
            if !allow_non_ts_extensions {
                let canonical_file_name =
                    get_canonical_file_name(&file_name, self.use_case_sensitive_file_names());
                let mut supported = false;
                for group in &supported_extensions {
                    let group: Vec<&str> = group.iter().map(|ext| &**ext).collect();
                    if file_extension_is_one_of(&canonical_file_name, &group) {
                        supported = true;
                        break;
                    }
                }
                if !supported {
                    return None; // unsupported extensions are forced to fail
                }
            }

            return self.get_source_file_for_resolved_module(&file_name);
        }
        if allow_non_ts_extensions {
            let extensionless = self.get_source_file_for_resolved_module(&file_name);
            if extensionless.is_some() {
                return extensionless;
            }
        }

        // Only try adding extensions from the first supported group (which should be .ts/.tsx/.d.ts)
        for ext in &supported_extensions[0] {
            let result = self.get_source_file_for_resolved_module(&format!("{file_name}{ext}"));
            if result.is_some() {
                return result;
            }
        }
        None
    }
}

// Go: program.go:285 NewProgram
pub fn new_program(opts: ProgramOptions) -> NewProgram {
    let _trace = crate::tracing::get().map(|tr| {
        tr.push(
            crate::tracing::Phase::Program,
            "createProgram",
            vec![(
                "configFilePath",
                opts.config
                    .compiler_options()
                    .config_file_path
                    .clone()
                    .into(),
            )],
            true,
        )
    });
    // PORT: Go builds `p` with a zero `processedFiles` and then calls
    // `p.SingleThreaded()`. `ProcessedFiles` has no zero value, so the files
    // are processed first. `SingleThreaded` reads only `opts`.
    let processed_files = process_all_program_files(opts.clone(), single_threaded(&opts));
    let mut p = NewProgram {
        opts,
        compare_paths_options: ComparePathsOptions::default(),
        processed_files,
        uses_uri_style_node_core_modules: Tristate::default(),
        common_source_directory: OnceCell::new(),
        program_diagnostics: Vec::new(),
        has_emit_blocking_diagnostics: FxHashSet::default(),
        source_files_to_emit: OnceCell::new(),
        unresolved_imports: LazyValue::default(),
        known_symlinks: LazyValue::default(),
        package_names: LazyValue::default(),
        has_ts_file: OnceCell::new(),
    };
    p.init_checker_pool();
    p.verify_compiler_options();
    p
}

impl NewProgram {
    // Go: program.go:305 (*Program).UpdateProgram
    // Return an updated program for which it is known that only the file with the given path has changed.
    // In addition to a new program, return a boolean indicating whether the data of the old program was reused.
    // The returned source file is the changed file as acquired through newHost; it is None
    // only if the host cannot locate the file (e.g. it was deleted).
    // PORT: the `createCheckerPool` parameter is dropped with the option.
    // PORT: a nil `createModuleResolver` is `None` (ts#64299).
    pub fn update_program(
        &self,
        changed_file_path: &Path,
        new_host: Rc<dyn CompilerHost>,
        create_module_resolver: Option<Rc<dyn Fn(ResolverOptions) -> Rc<dyn Resolver>>>,
    ) -> (NewProgram, Option<Rc<ParsedSourceFile>>, bool) {
        match self.reuse_program(
            changed_file_path,
            new_host.clone(),
            create_module_resolver.clone(),
        ) {
            (Some(result), new_file, true) => (result, new_file, true),
            (_, new_file, _) => {
                let mut new_opts = self.opts.clone();
                new_opts.host = new_host;
                if create_module_resolver.is_some() {
                    new_opts.create_module_resolver = create_module_resolver;
                }
                (new_program(new_opts), new_file, false)
            }
        }
    }

    // ReuseProgram attempts to produce a new program by replacing only
    // changedFilePath in place, reusing the rest of p. It returns
    // (newProgram, newFile, true) on success, or (nil, newFile, false) when the
    // file cannot be replaced in place. Unlike UpdateProgram, it never constructs a
    // full fallback program, so callers that build their own fallback (e.g. with a
    // different host) do not pay for a discarded program build.
    // Go: program.go:332 (*Program).ReuseProgram
    // PORT: Go nil `*Program` is `None`. The `createCheckerPool` parameter is
    // dropped with the option.
    // PORT: tsgo#4712 Go copies `contentMapperOptionDiagnostics` into the new
    // program. `NewProgram` has no field for them: the program version keeps
    // them (`program/go_frontend.rs` `content_mapper_option_diagnostics_of`).
    pub fn reuse_program(
        &self,
        changed_file_path: &Path,
        new_host: Rc<dyn CompilerHost>,
        create_module_resolver: Option<Rc<dyn Fn(ResolverOptions) -> Rc<dyn Resolver>>>,
    ) -> (Option<NewProgram>, Option<Rc<ParsedSourceFile>>, bool) {
        let mut new_opts = self.opts.clone();
        new_opts.host = new_host.clone();
        if create_module_resolver.is_some() {
            new_opts.create_module_resolver = create_module_resolver;
        }

        // PORT: Go dereferences a nil old file and panics; so does this.
        let old_file = self
            .files_by_path
            .get(changed_file_path)
            .cloned()
            .expect("changed file is not in the program");
        // tsgo#4712
        let new_file;
        let mut old_supplemental_files: Vec<Rc<ParsedSourceFile>> = Vec::new();
        let mut new_supplemental_files: Vec<Rc<ParsedSourceFile>> = Vec::new();
        if !old_file.content_mapper().is_empty() {
            // Content-mapped files are produced by running an external transform, which a plain reparse can't
            // reproduce. Re-run the transform through the host; any failure (or a missing file) falls back to
            // a full rebuild so the file loader's failure policy runs.
            // PORT: Go passes a nil mapper to the host when the new config
            // has none for the file, and the transform then fails; here that
            // is the same fallback.
            let Some(mapper) = new_opts
                .config
                .get_content_mapper_for_file_name(old_file.file_name())
            else {
                return (None, None, false);
            };
            match new_host.get_content_mapped_source_files(old_file.parse_options(), &mapper) {
                Ok(files) => {
                    new_file = files.canonical;
                    old_supplemental_files = old_file.supplemental_source_files();
                    new_supplemental_files = files.supplemental;
                }
                Err(_) => return (None, None, false),
            }
        } else {
            new_file = new_host.get_source_file(old_file.parse_options());
        }

        // If this file is part of a package redirect group (same package installed in multiple
        // node_modules locations), we need to rebuild the program because the redirect targets
        // might need recalculation.
        let in_redirect_files = self
            .redirect_files_by_path
            .as_ref()
            .is_some_and(|redirects| redirects.contains_key(changed_file_path));
        let is_redirect_target = self
            .redirect_targets_map
            .as_ref()
            .is_some_and(|targets| targets.contains_key(changed_file_path));
        if in_redirect_files || is_redirect_target {
            return (None, new_file, false);
        }

        // #4792: `canReplaceFileInProgram` is a program method.
        if !self.can_replace_file_in_program(&old_file, new_file.as_deref()) {
            return (None, new_file, false);
        }
        let new_file = new_file.expect("checked by can_replace_file_in_program");
        // Cloning does not recompute synthetic helper or JSX-runtime import bookkeeping. Fall back to a full
        // build whenever either version requires those imports.
        // tsgo#4712
        if self.has_import_helpers_import_specifier(old_file.path())
            || self.needs_import_helpers_import_specifier(&new_file)
        {
            return (None, Some(new_file), false);
        }
        if self.has_jsx_runtime_import_specifier(old_file.path())
            || !self.jsx_runtime_import_specifier(&new_file).is_empty()
        {
            return (None, Some(new_file), false);
        }
        if old_supplemental_files.len() != new_supplemental_files.len() {
            return (None, Some(new_file), false);
        }
        for (old_supplemental, new_supplemental) in
            old_supplemental_files.iter().zip(&new_supplemental_files)
        {
            if old_supplemental.path() != new_supplemental.path()
                || !self.can_replace_file_in_program(old_supplemental, Some(&**new_supplemental))
            {
                return (None, Some(new_file), false);
            }
            if self.has_import_helpers_import_specifier(old_supplemental.path())
                || self.needs_import_helpers_import_specifier(new_supplemental)
            {
                return (None, Some(new_file), false);
            }
            if self.has_jsx_runtime_import_specifier(old_supplemental.path())
                || !self
                    .jsx_runtime_import_specifier(new_supplemental)
                    .is_empty()
            {
                return (None, Some(new_file), false);
            }
        }
        // TODO: reverify compiler options when config has changed?
        // PORT: Go copies the `processedFiles` struct and shares its maps.
        // Rust clones it.
        let mut result = NewProgram {
            opts: new_opts,
            compare_paths_options: self.compare_paths_options.clone(),
            processed_files: self.processed_files.clone(),
            uses_uri_style_node_core_modules: self.uses_uri_style_node_core_modules,
            common_source_directory: OnceCell::new(),
            program_diagnostics: self.program_diagnostics.clone(),
            has_emit_blocking_diagnostics: self.has_emit_blocking_diagnostics.clone(),
            source_files_to_emit: OnceCell::new(),
            unresolved_imports: LazyValue::default(),
            known_symlinks: LazyValue::default(),
            package_names: LazyValue::default(),
            has_ts_file: OnceCell::new(),
        };
        result
            .unresolved_imports
            .try_reuse(&self.unresolved_imports);
        result.known_symlinks.try_reuse(&self.known_symlinks);
        result.package_names.try_reuse(&self.package_names);
        result.init_checker_pool();
        // PORT: Go `core.FindIndex` returns -1 and the index panics; so does this.
        let index = result
            .files
            .iter()
            .position(|file| file.path() == new_file.path())
            .expect("changed file is not in the file list");
        result.processed_files.files[index] = new_file.clone();
        result
            .processed_files
            .files_by_path
            .insert(new_file.path().clone(), new_file.clone());
        // tsgo#4712
        for (old_supplemental, new_supplemental) in
            old_supplemental_files.iter().zip(&new_supplemental_files)
        {
            // PORT: Go `core.FindIndex` returns -1 and the index panics; so does this.
            let supplemental_index = result
                .files
                .iter()
                .position(|file| Rc::ptr_eq(file, old_supplemental))
                .expect("supplemental file is not in the file list");
            result.processed_files.files[supplemental_index] = new_supplemental.clone();
            result
                .processed_files
                .files_by_path
                .insert(new_supplemental.path().clone(), new_supplemental.clone());
        }
        update_file_include_processor(&mut result);
        (Some(result), Some(new_file), true)
    }

    // Go: program.go:435 (*Program).initCheckerPool
    // PORT: the checker pool stays in program.rs, so only the check is kept.
    pub fn init_checker_pool(&mut self) {
        if !self.finished_processing {
            panic!("Program must finish processing files before initializing checker pool");
        }
    }

    // Go: program.go:454 (*Program).canReplaceFileInProgram
    // #4792: a method, so each import also compares its resolution mode.
    pub fn can_replace_file_in_program(
        &self,
        file1: &ParsedSourceFile,
        file2: Option<&ParsedSourceFile>,
    ) -> bool {
        let Some(file2) = file2 else {
            return false;
        };
        file1.parse_options() == file2.parse_options()
            // tsgo#4712
            && file1.script_kind == file2.script_kind
            && is_external_or_common_js_module_of(file1)
                == is_external_or_common_js_module_of(file2)
            && file1.uses_uri_style_node_core_modules == file2.uses_uri_style_node_core_modules
            && slices_equal_func(&file1.imports, &file2.imports, |n1, n2| {
                equal_module_specifiers(*n1, *n2)
                    && self.get_mode_for_usage_location(file1, *n1)
                        == self.get_mode_for_usage_location(file2, *n2)
            })
            && slices_equal_func(
                &file1.module_augmentations,
                &file2.module_augmentations,
                |n1, n2| equal_module_augmentation_names(*n1, *n2),
            )
            && file1.ambient_module_names == file2.ambient_module_names
            && slices_equal_func(
                &file1.referenced_files,
                &file2.referenced_files,
                equal_file_references,
            )
            && slices_equal_func(
                &file1.type_reference_directives,
                &file2.type_reference_directives,
                equal_file_references,
            )
            && slices_equal_func(
                &file1.lib_reference_directives,
                &file2.lib_reference_directives,
                equal_file_references,
            )
            && equal_check_js_directives(
                file1.check_js_directive.as_ref(),
                file2.check_js_directive.as_ref(),
            )
    }
}

/// Go `ast.IsExternalOrCommonJSModule(file)` for a file of the loader
/// (tsgo#4712 `canReplaceFileInProgram`).
// PORT: the binder sets Go `SourceFile.CommonJSModuleIndicator`. A
// published file reads the indicators from its `source_file_info`, which has
// the bind result once the file is bound (nil before, as in Go). A file that
// is not published yet is not bound; its parse has both indicators.
fn is_external_or_common_js_module_of(file: &ParsedSourceFile) -> bool {
    if crate::ast::is_published(file.store) {
        return is_external_or_common_js_module(file.root);
    }
    file.external_module_indicator.is_some() || file.common_js_module_indicator.is_some()
}

/// Go `slices.EqualFunc`.
fn slices_equal_func<T>(s1: &[T], s2: &[T], eq: impl Fn(&T, &T) -> bool) -> bool {
    s1.len() == s2.len() && s1.iter().zip(s2).all(|(v1, v2)| eq(v1, v2))
}

impl NewProgram {
    // Go: program.go:472 (*Program).needsImportHelpersImportSpecifier
    pub fn needs_import_helpers_import_specifier(&self, file: &ParsedSourceFile) -> bool {
        let (redirect, _) = self.mapper().get_redirect_for_resolution(file);
        let options_for_file = get_compiler_options_with_redirect(
            self.opts.config.compiler_options(),
            redirect
                .as_deref()
                .map(|r| r as &dyn ModuleResolvedProjectReference),
        );
        if !options_for_file.import_helpers.is_true() {
            return false;
        }
        // PORT: Go `ast.IsSourceFileJS` and `ast.IsExternalModule` take the
        // source file. `prog()` is not set during program construction, so
        // this reads the `ParsedSourceFile` fields.
        let is_java_script_file = file.is_js();
        let is_external_module_file = file.external_module_indicator.is_some();
        if !is_java_script_file
            && (file.is_declaration_file
                || (!options_for_file.get_isolated_modules() && !is_external_module_file))
        {
            return false;
        }
        true
    }

    /// Go `p.importHelpersImportSpecifiers[path] != nil`.
    fn has_import_helpers_import_specifier(&self, path: &Path) -> bool {
        self.import_helpers_import_specifiers
            .as_ref()
            .and_then(|specifiers| specifiers.get(path))
            .is_some_and(|specifier| specifier.is_some())
    }

    /// Go `p.jsxRuntimeImportSpecifiers[path] != nil`.
    fn has_jsx_runtime_import_specifier(&self, path: &Path) -> bool {
        self.jsx_runtime_import_specifiers
            .as_ref()
            .is_some_and(|specifiers| specifiers.contains_key(path))
    }

    // Go: program.go:486 (*Program).jsxRuntimeImportSpecifier (tsgo#4712)
    // PORT: as `needs_import_helpers_import_specifier`, this reads the
    // `ParsedSourceFile` fields (`get_jsx_implicit_import_base_of_file`).
    pub fn jsx_runtime_import_specifier(&self, file: &ParsedSourceFile) -> String {
        if !file.is_js() && file.script_kind != ScriptKind::TSX {
            return String::new();
        }
        let (redirect, _) = self.mapper().get_redirect_for_resolution(file);
        let options_for_file = get_compiler_options_with_redirect(
            self.opts.config.compiler_options(),
            redirect
                .as_deref()
                .map(|r| r as &dyn ModuleResolvedProjectReference),
        );
        get_jsx_runtime_import(
            &super::file_loader::get_jsx_implicit_import_base_of_file(&options_for_file, file),
            &options_for_file,
        )
    }
}

// Go: program.go:495 equalModuleSpecifiers
pub fn equal_module_specifiers(n1: Node, n2: Node) -> bool {
    n1.kind() == n2.kind() && (!is_string_literal(n1) || n1.text() == n2.text())
}

// Go: program.go:499 equalModuleAugmentationNames
pub fn equal_module_augmentation_names(n1: Node, n2: Node) -> bool {
    n1.kind() == n2.kind() && n1.text() == n2.text()
}

// Go: program.go:503 equalFileReferences
pub fn equal_file_references(f1: &FileReference, f2: &FileReference) -> bool {
    f1.file_name == f2.file_name
        && f1.resolution_mode == f2.resolution_mode
        && f1.preserve == f2.preserve
}

// Go: program.go:507 equalCheckJSDirectives
pub fn equal_check_js_directives(
    d1: Option<&CheckJsDirective>,
    d2: Option<&CheckJsDirective>,
) -> bool {
    match (d1, d2) {
        (None, None) => true,
        (Some(d1), Some(d2)) => d1.enabled == d2.enabled,
        _ => false,
    }
}

impl NewProgram {
    // Go: program.go:511 (*Program).SourceFiles
    pub fn source_files(&self) -> &[Rc<ParsedSourceFile>] {
        &self.files
    }

    // Go: program.go:512 (*Program).DuplicateSourceFiles
    pub fn duplicate_source_files(&self) -> &[DuplicateSourceFile] {
        &self.duplicate_source_files
    }

    // Go: program.go:513 (*Program).Options
    pub fn options(&self) -> &CompilerOptions {
        self.opts.config.compiler_options()
    }

    // Go: program.go:517 (*Program).GetContentMapper (tsgo#4712)
    // GetContentMapper returns the content mapper that produced the given source file, or nil if the
    // file was not produced by a content mapper.
    pub fn get_content_mapper(&self, file: &ParsedSourceFile) -> Option<Rc<Mapper>> {
        if file.content_mapper().is_empty() {
            return None;
        }
        let mapper = self
            .opts
            .config
            .get_content_mapper_for_file_name(file.file_name())?;
        if mapper.identity() == file.content_mapper() {
            return Some(mapper);
        }
        None
    }

    // Go: program.go:528 (*Program).ContentMapperExtensions (tsgo#4712)
    pub fn content_mapper_extensions(&self) -> Vec<String> {
        self.opts.config.content_mapper_extensions()
    }

    // Go: program.go:529 (*Program).CommandLine
    pub fn command_line(&self) -> &Rc<ParsedCommandLine> {
        &self.opts.config
    }

    // Go: program.go:530 (*Program).Host
    pub fn host(&self) -> &Rc<dyn CompilerHost> {
        &self.opts.host
    }

    /// Empties the caches of the resolver of this program's load
    /// (`module::Caches::release`). `ls_program` calls it when no live
    /// program shares this load's processed files: Go `UpdateProgram`
    /// shares them, resolver included, with each clone.
    // PORT: not in Go. Go's GC frees the resolver with its last program; the
    // port keeps the program shell (multiprog M2).
    pub fn release_resolver_caches(&self) {
        if let Some(resolver) = &self.resolver {
            resolver.release_caches();
        }
    }

    // Go: program.go:531 (*Program).Tracing
    // PORT: the session is the process global `crate::tracing::get`.

    // Go: program.go:532 (*Program).GetConfigFileParsingDiagnostics
    pub fn get_config_file_parsing_diagnostics(&self) -> Vec<Diagnostic> {
        self.opts
            .config
            .get_config_file_parsing_diagnostics()
            .to_vec()
    }

    // Go: program.go:538 (*Program).GetUnresolvedImports
    // GetUnresolvedImports returns the unresolved imports for this program.
    // The result is cached and computed only once.
    pub fn get_unresolved_imports(&self) -> &FxHashSet<String> {
        self.unresolved_imports
            .get_value(|| self.extract_unresolved_imports())
    }

    // Go: program.go:542 (*Program).extractUnresolvedImports
    fn extract_unresolved_imports(&self) -> FxHashSet<String> {
        let mut unresolved_set = FxHashSet::default();

        for source_file in &self.files {
            let unresolved_imports = self.extract_unresolved_imports_from_source_file(source_file);
            for imp in unresolved_imports {
                unresolved_set.insert(imp);
            }
        }

        unresolved_set
    }

    // Go: program.go:555 (*Program).extractUnresolvedImportsFromSourceFile
    fn extract_unresolved_imports_from_source_file(&self, file: &ParsedSourceFile) -> Vec<String> {
        let mut unresolved_imports = Vec::new();

        if let Some(resolved_modules) = self.resolved_modules.get(file.path()) {
            for (cache_key, resolution) in resolved_modules {
                let resolved = resolution.is_resolved();
                if (!resolved
                    || !extension_is_one_of(
                        &resolution.extension,
                        SUPPORTED_TS_EXTENSIONS_WITH_JSON_FLAT,
                    ))
                    && !is_external_module_name_relative(&cache_key.name)
                {
                    unresolved_imports.push(cache_key.name.clone());
                }
            }
        }

        unresolved_imports
    }

    // Go: program.go:570 (*Program).SingleThreaded
    pub fn single_threaded(&self) -> bool {
        single_threaded(&self.opts)
    }

    // Go: program.go:574 (*Program).BindSourceFiles
    // PORT: binding stays in program.rs (`bind_all`); not ported here.
}

// PORT: the body of Go `(*Program).SingleThreaded`. It reads only `opts`, so
// `new_program` can call it before the program exists.
fn single_threaded(opts: &ProgramOptions) -> bool {
    opts.single_threaded
        .default_if_unknown(opts.config.compiler_options().single_threaded)
        .is_true()
}
