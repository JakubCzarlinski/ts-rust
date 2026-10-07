//! The program reads of the cross-project search: the Go
//! `*compiler.Program` methods that `provideSymbolsAndEntries`,
//! `forEachOriginalDefinitionLocation` and the `symbolAndEntriesTo*`
//! functions of references, implementations and rename call.
//!
//! PORT: Go runs the search of each project on its own goroutine, and every
//! goroutine reads the shared `*compiler.Program`. `NewProgram` is not
//! `Sync` (it holds `Rc` and `RefCell` values), so a search thread
//! (`search_thread.rs`) cannot read it. The search code is generic over
//! `ProgramView` instead. `NewProgram` implements it on the dispatch
//! thread. `SearchView` implements it on a search thread with a copy of the
//! data it reads, and asks the dispatch thread for the rare reads that the
//! copy does not hold. `LanguageService<P>` has the type parameter
//! `P = NewProgram`, so code outside the search path does not change. The
//! search code takes the program as `&P`.

use crate::ls::prelude::*;

/// The program of a language service (see the module comment).
pub trait ProgramView {
    /// A key for the per-program caches of a thread (`import_tracker.rs`,
    /// `search_thread.rs`): the program version id. It is not the address,
    /// because a freed program's address can be used again.
    fn identity(&self) -> usize;

    // Go: compiler/program.go:127 (*Program).GetCurrentDirectory
    fn get_current_directory(&self) -> String;

    /// Go `program.GetSourceFile(fileName)` as the file root, or
    /// `Node::NIL` for nil.
    fn source_file_root(&self, file_name: &str) -> Node;

    /// Go `program.GetSourceFiles()` as file roots, in program order.
    fn source_file_roots(&self) -> Vec<Node>;

    /// The index of `file` in `program.GetSourceFiles()`, or -1.
    fn source_file_index(&self, file: Node) -> i32;

    // Go: compiler/program.go:178 (*Program).IsSourceFromProjectReference
    fn is_source_from_project_reference(&self, path: &tspath::Path) -> bool;

    // Go: compiler/program.go (*Program).IsSourceFileDefaultLibrary
    fn is_source_file_default_library(&self, path: &tspath::Path) -> bool;

    /// Go `program.GetJSXRuntimeImportSpecifier(path)`, the specifier node.
    fn jsx_runtime_import_specifier(&self, path: &tspath::Path) -> Node;

    // Go: compiler/program.go:1922 (*Program).GetImportHelpersImportSpecifier
    fn import_helpers_import_specifier(&self, path: &tspath::Path) -> Node;

    /// The `<reference path>` directives, then the `<reference types>`
    /// directives, of `referencing_file` that resolve to the file `target`
    /// (the reference part of Go `findModuleReferences`).
    fn references_to_file(&self, referencing_file: Node, target: Node) -> Vec<FileReference>;

    /// Go `getReferenceAtPosition(sourceFile, position, program)`.
    fn reference_at_position(&self, source_file: Node, position: i32) -> Option<RefInfo>;

    /// Go `program.GetTypeChecker(ctx)`.
    fn get_type_checker(&self, ctx: &Context) -> (Rc<RefCell<Checker>>, ls_program::Release);

    /// Go `program.GetTypeCheckerForFile(ctx, file)`. A view with one
    /// checker gives that one.
    fn get_type_checker_for_file(
        &self,
        ctx: &Context,
        _file: Node,
    ) -> (Rc<RefCell<Checker>>, ls_program::Release) {
        self.get_type_checker(ctx)
    }
}

impl ProgramView for compiler::NewProgram {
    fn identity(&self) -> usize {
        ls_program::program_version(self).id as usize
    }

    fn get_current_directory(&self) -> String {
        compiler::NewProgram::get_current_directory(self)
    }

    fn source_file_root(&self, file_name: &str) -> Node {
        self.get_source_file(file_name)
            .map_or(Node::NIL, |file| file.root)
    }

    fn source_file_roots(&self) -> Vec<Node> {
        self.get_source_files()
            .iter()
            .map(|file| file.root)
            .collect()
    }

    fn source_file_index(&self, file: Node) -> i32 {
        self.source_files()
            .iter()
            .position(|f| f.root == file)
            .map_or(-1, |i| i as i32)
    }

    fn is_source_from_project_reference(&self, path: &tspath::Path) -> bool {
        compiler::NewProgram::is_source_from_project_reference(self, path)
    }

    fn is_source_file_default_library(&self, path: &tspath::Path) -> bool {
        compiler::NewProgram::is_source_file_default_library(self, path)
    }

    fn jsx_runtime_import_specifier(&self, path: &tspath::Path) -> Node {
        self.get_jsx_runtime_import_specifier(path).1
    }

    fn import_helpers_import_specifier(&self, path: &tspath::Path) -> Node {
        self.get_import_helpers_import_specifier(path)
    }

    // Go: ls/importTracker.go:716 findModuleReferences (the `<reference>` part)
    // PORT: Go passes the `*ast.SourceFile` to the program methods.
    // `NewProgram` takes its parsed file, found here by the file's path, or
    // in another program that has the file (`ls_program::parsed_source_file`).
    fn references_to_file(&self, referencing_file: Node, target: Node) -> Vec<FileReference> {
        let mut refs: Vec<FileReference> = Vec::new();
        let referencing_parsed_file = self
            .get_source_file_by_path(&tspath::Path(
                source_file_info(referencing_file).path.clone(),
            ))
            .filter(|parsed| parsed.root == referencing_file)
            .or_else(|| ls_program::parsed_source_file(referencing_file))
            // PORT: a port-only lookup (Go passes the file itself), so a miss
            // is a port panic, not a Go nil read.
            .expect("invalid memory address or nil pointer dereference");

        // Check <reference path> directives
        for ref_ in &referencing_parsed_file.referenced_files {
            if self
                .get_source_file_from_reference(&referencing_parsed_file, ref_)
                .is_some_and(|file| file.root == target)
            {
                refs.push(ref_.clone());
            }
        }

        // Check <reference types> directives
        for ref_ in &referencing_parsed_file.type_reference_directives {
            let referenced = self
                .get_resolved_type_reference_directive_from_type_reference_directive(
                    ref_,
                    &referencing_parsed_file,
                );
            if referenced.is_some_and(|referenced| {
                referenced.resolved_file_name == source_file_file_name(target)
            }) {
                refs.push(ref_.clone());
            }
        }
        refs
    }

    fn reference_at_position(&self, source_file: Node, position: i32) -> Option<RefInfo> {
        get_reference_at_position(source_file, position, self)
    }

    fn get_type_checker(&self, ctx: &Context) -> (Rc<RefCell<Checker>>, ls_program::Release) {
        ls_program::get_type_checker(self, ctx)
    }

    fn get_type_checker_for_file(
        &self,
        ctx: &Context,
        file: Node,
    ) -> (Rc<RefCell<Checker>>, ls_program::Release) {
        ls_program::get_type_checker_for_file(self, ctx, file)
    }
}
