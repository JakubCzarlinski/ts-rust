//! Port of Go `internal/lsp/server_projectreference_updates_test.go`.

use ts_goport::ls::lsconv;
use ts_goport::lsp::lsproto;

use super::lsp_server_completion_test::init_completion_client;

child_test! {
    // Go: server_projectreference_updates_test.go:67 TestReferencesAfterAncestorProjectConfigDeletion1
    fn references_after_ancestor_project_config_deletion1() {
        // Go: initMutableLSPClient (server_projectreference_updates_test.go:19) is
        // initCompletionClient with Cwd "/root" and the map FS kept for edits.
        let client = init_completion_client(
            "/root",
            &[
                (
                    "/root/tsconfig.json",
                    r#"{
			"files": [],
			"references": [{ "path": "./project" }]
		}"#,
                ),
                (
                    "/root/project/tsconfig.json",
                    r#"{
			"compilerOptions": { "composite": true },
			"include": ["src/**/*.ts"]
		}"#,
                ),
                ("/root/project/src/main.ts", "export function helloWorld() {}\nhelloWorld()\n"),
            ],
        );
        let fs = super::projecttestutil::current_map_fs_for_test();

        let main_uri = lsconv::file_name_to_document_uri("/root/project/src/main.ts");
        client.send_notification(
            &lsproto::TEXT_DOCUMENT_DID_OPEN_INFO,
            lsproto::DidOpenTextDocumentParams {
                text_document: Some(lsproto::TextDocumentItem {
                    uri: main_uri.clone(),
                    language_id: lsproto::LanguageKind::TYPE_SCRIPT,
                    text: "export function helloWorld() {}\nhelloWorld()\n".to_string(),
                    ..Default::default()
                }),
            },
        );

        // Prime the child project so opening a file creates the ancestor configured-project placeholder.
        let (msg, _) = client.send_request(
            &lsproto::TEXT_DOCUMENT_DOCUMENT_SYMBOL_INFO,
            lsproto::DocumentSymbolParams {
                text_document: lsproto::TextDocumentIdentifier { uri: main_uri.clone() },
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);

        fs.remove("root/tsconfig.json").unwrap();
        client.send_notification(
            &lsproto::WORKSPACE_DID_CHANGE_WATCHED_FILES_INFO,
            lsproto::DidChangeWatchedFilesParams {
                changes: vec![Some(lsproto::FileEvent {
                    uri: lsconv::file_name_to_document_uri("/root/tsconfig.json"),
                    type_: lsproto::FileChangeType::DELETED,
                })],
            },
        );

        let (msg, resp) = client.send_request(
            &lsproto::TEXT_DOCUMENT_REFERENCES_INFO,
            lsproto::ReferenceParams {
                text_document: lsproto::TextDocumentIdentifier { uri: main_uri.clone() },
                position: lsproto::Position { line: 1, character: 3 },
                context: Some(lsproto::ReferenceContext {
                    include_declaration: true,
                }),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let locations = resp.expect("expected response").locations.expect("resp.Locations");
        assert_eq!(locations.len(), 2);
        let location = |sl, sc, el, ec| lsproto::Location {
            uri: main_uri.clone(),
            range: lsproto::Range {
                start: lsproto::Position { line: sl, character: sc },
                end: lsproto::Position { line: el, character: ec },
            },
        };
        assert_eq!(locations, vec![location(0, 16, 0, 26), location(1, 0, 1, 10)]);
    }
}

child_test! {
    // PORT: not in Go. A solution tsconfig (`files: []` and references) is a
    // configured project without a program. Rename and references in a
    // project outside the solution walk the loaded project trees; Go skips a
    // project without a program there (ls/crossproject.go:252). The port read
    // that project's missing host and panicked (editfuzz5 X1).
    fn rename_and_references_skip_project_without_program() {
        const MAIN: &str = "export function bFn(n: number) {\n  return n;\n}\nexport const r = bFn(1);\n";
        let client = init_completion_client(
            "/root",
            &[
                ("/root/tsconfig.json", r#"{"files": [], "references": [{"path": "./a"}]}"#),
                (
                    "/root/a/tsconfig.json",
                    r#"{"compilerOptions": {"composite": true, "strict": true}, "include": ["src"]}"#,
                ),
                ("/root/a/src/lib.ts", "export const aValue = 1;\n"),
                (
                    "/root/b/tsconfig.json",
                    r#"{"compilerOptions": {"strict": true, "noEmit": true}, "include": ["src"]}"#,
                ),
                ("/root/b/src/main.ts", MAIN),
            ],
        );
        let lib_uri = lsconv::file_name_to_document_uri("/root/a/src/lib.ts");
        let main_uri = lsconv::file_name_to_document_uri("/root/b/src/main.ts");
        for (uri, text) in [(&lib_uri, "export const aValue = 1;\n"), (&main_uri, MAIN)] {
            client.send_notification(
                &lsproto::TEXT_DOCUMENT_DID_OPEN_INFO,
                lsproto::DidOpenTextDocumentParams {
                    text_document: Some(lsproto::TextDocumentItem {
                        uri: uri.clone(),
                        language_id: lsproto::LanguageKind::TYPE_SCRIPT,
                        text: text.to_string(),
                        ..Default::default()
                    }),
                },
            );
        }
        let range = |sl, sc, el, ec| lsproto::Range {
            start: lsproto::Position { line: sl, character: sc },
            end: lsproto::Position { line: el, character: ec },
        };
        let bfn_ranges = vec![range(0, 16, 0, 19), range(3, 17, 3, 20)];

        let (msg, resp) = client.send_request(
            &lsproto::TEXT_DOCUMENT_RENAME_INFO,
            lsproto::RenameParams {
                text_document: lsproto::TextDocumentIdentifier { uri: main_uri.clone() },
                position: lsproto::Position { line: 0, character: 17 },
                new_name: "x2".to_string(),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let changes = resp
            .expect("expected response")
            .workspace_edit
            .expect("resp.WorkspaceEdit")
            .changes
            .expect("WorkspaceEdit.Changes");
        assert_eq!(changes.keys().collect::<Vec<_>>(), vec![&main_uri]);
        let edit_ranges: Vec<lsproto::Range> =
            changes[&main_uri].iter().flatten().map(|edit| edit.range).collect();
        assert_eq!(edit_ranges, bfn_ranges);

        let (msg, resp) = client.send_request(
            &lsproto::TEXT_DOCUMENT_REFERENCES_INFO,
            lsproto::ReferenceParams {
                text_document: lsproto::TextDocumentIdentifier { uri: main_uri.clone() },
                position: lsproto::Position { line: 0, character: 17 },
                context: Some(lsproto::ReferenceContext {
                    include_declaration: true,
                }),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let locations = resp.expect("expected response").locations.expect("resp.Locations");
        let reference_ranges: Vec<lsproto::Range> =
            locations.iter().map(|location| location.range).collect();
        assert_eq!(reference_ranges, bfn_ranges);
        assert!(locations.iter().all(|location| location.uri == main_uri));
    }
}

// lschk1: Go searches each project of a references, implementations or
// rename request with a query checker of that project's pool
// (ls/crossproject.go:46 handleCrossProject, project/checkerpool.go:252
// getQueryChecker), so later requests in that project see the state the
// search left. Here the search of `description` in the lib project
// instantiates the members of `EnumOptions<T>` before semanticTokens
// resolves the declared type, so the hover writes the mapped type argument
// `<T>` (checker.go:21090 instantiateSymbol, nodebuilderimpl.go:1036
// lookupTypeParameterNodes). On a fresh checker the form is
// `EnumOptions<T extends object = any>`. The port runs that search on a
// search thread and runs it again on the pool's new query checker when the
// semanticTokens request first needs one. The hover texts are Go N's
// (tsgo-oracle-673a5f17d713, lschk1 repro v9, lschk1-skeptic probes p8, p9
// and s4-cancel-cold-100).

const SEARCHED_REG: &str = "import type { ArgsOptions } from '../types';\n\
    export type Both = ArgsOptions;\n\
    export interface EnumOptions<T extends object = any> {\n  name: string;\n  description?: string;\n}\n\
    export function registerEnumType<T extends object = any>(enumRef: T, options?: EnumOptions<T>) {\n  \
    if (!options || typeof options.name !== 'string') { throw new Error(''); }\n  \
    return { ref: enumRef, description: options.description };\n}\n";
const SEARCHED_TYPES: &str =
    "export type ArgsOptions<T = any> = {\n  name?: string;\n  description?: string;\n};\n";
const SEARCHED_CONFIG: &str =
    r#"{"compilerOptions": {"strict": true, "target": "es2020"}, "include": ["#;
const WARM_HOVER: &str = "(property) EnumOptions<T>.description?: string | undefined";
const FRESH_HOVER: &str =
    "(property) EnumOptions<T extends object = any>.description?: string | undefined";

fn open_file(client: &super::lsptestutil::LspClient, uri: &lsproto::DocumentUri, text: &str) {
    client.send_notification(
        &lsproto::TEXT_DOCUMENT_DID_OPEN_INFO,
        lsproto::DidOpenTextDocumentParams {
            text_document: Some(lsproto::TextDocumentItem {
                uri: uri.clone(),
                language_id: lsproto::LanguageKind::TYPE_SCRIPT,
                text: text.to_string(),
                ..Default::default()
            }),
        },
    );
}

/// A client with the root project (types.ts) and the lib project (lib/reg.ts,
/// which imports types.ts), both files open. Returns the client and the
/// uris of lib/reg.ts and types.ts.
fn searched_lib_client() -> (
    super::lsptestutil::LspClient,
    lsproto::DocumentUri,
    lsproto::DocumentUri,
) {
    let client = init_completion_client(
        "/home/projects",
        &[
            (
                "/home/projects/tsconfig.json",
                format!(r#"{SEARCHED_CONFIG}"types.ts"]}}"#).as_str(),
            ),
            ("/home/projects/types.ts", SEARCHED_TYPES),
            (
                "/home/projects/lib/tsconfig.json",
                format!(r#"{SEARCHED_CONFIG}"*.ts"]}}"#).as_str(),
            ),
            ("/home/projects/lib/reg.ts", SEARCHED_REG),
        ],
    );
    let reg_uri = lsconv::file_name_to_document_uri("/home/projects/lib/reg.ts");
    let types_uri = lsconv::file_name_to_document_uri("/home/projects/types.ts");
    open_file(&client, &reg_uri, SEARCHED_REG);
    open_file(&client, &types_uri, SEARCHED_TYPES);
    (client, reg_uri, types_uri)
}

/// semanticTokens of `reg_uri`, then the hover text on `description` of
/// `options.description` in it.
fn tokens_then_hover(
    client: &super::lsptestutil::LspClient,
    reg_uri: &lsproto::DocumentUri,
) -> String {
    let (msg, _) = client.send_request(
        &lsproto::TEXT_DOCUMENT_SEMANTIC_TOKENS_FULL_INFO,
        lsproto::SemanticTokensParams {
            text_document: lsproto::TextDocumentIdentifier {
                uri: reg_uri.clone(),
            },
            ..Default::default()
        },
    );
    assert!(msg.error.is_none(), "{:?}", msg.error);
    let (msg, resp) = client.send_request(
        &lsproto::TEXT_DOCUMENT_HOVER_INFO,
        lsproto::HoverParams {
            text_document: lsproto::TextDocumentIdentifier {
                uri: reg_uri.clone(),
            },
            position: lsproto::Position {
                line: 8,
                character: 50,
            },
            ..Default::default()
        },
    );
    assert!(msg.error.is_none(), "{:?}", msg.error);
    resp.and_then(|resp| resp.hover)
        .and_then(|hover| hover.contents.markup_content)
        .expect("hover MarkupContent")
        .value
}

/// `description` in types.ts: the lib project includes types.ts, so a
/// search there runs in both projects.
const DESCRIPTION_IN_TYPES: lsproto::Position = lsproto::Position {
    line: 2,
    character: 3,
};

child_test! {
    // PORT: not in Go (see the note above `SEARCHED_REG`).
    fn hover_after_cross_project_references_uses_the_searched_checker() {
        let (client, reg_uri, types_uri) = searched_lib_client();
        let (msg, _) = client.send_request(
            &lsproto::TEXT_DOCUMENT_REFERENCES_INFO,
            lsproto::ReferenceParams {
                text_document: lsproto::TextDocumentIdentifier { uri: types_uri },
                position: DESCRIPTION_IN_TYPES,
                context: Some(lsproto::ReferenceContext {
                    include_declaration: true,
                }),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let text = tokens_then_hover(&client, &reg_uri);
        assert!(text.contains(WARM_HOVER), "{text}");
    }
}

child_test! {
    // PORT: not in Go (see the note above `SEARCHED_REG`): the rename form.
    fn hover_after_cross_project_rename_uses_the_searched_checker() {
        let (client, reg_uri, types_uri) = searched_lib_client();
        let (msg, _) = client.send_request(
            &lsproto::TEXT_DOCUMENT_RENAME_INFO,
            lsproto::RenameParams {
                text_document: lsproto::TextDocumentIdentifier { uri: types_uri },
                position: DESCRIPTION_IN_TYPES,
                new_name: "desc".to_string(),
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let text = tokens_then_hover(&client, &reg_uri);
        assert!(text.contains(WARM_HOVER), "{text}");
    }
}

child_test! {
    // PORT: not in Go (see the note above `SEARCHED_REG`): the
    // implementation form.
    fn hover_after_cross_project_implementation_uses_the_searched_checker() {
        let (client, reg_uri, types_uri) = searched_lib_client();
        let (msg, _) = client.send_request(
            &lsproto::TEXT_DOCUMENT_IMPLEMENTATION_INFO,
            lsproto::ImplementationParams {
                text_document: lsproto::TextDocumentIdentifier { uri: types_uri },
                position: DESCRIPTION_IN_TYPES,
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);
        let text = tokens_then_hover(&client, &reg_uri);
        assert!(text.contains(WARM_HOVER), "{text}");
    }
}

child_test! {
    // PORT: not in Go. In Go a cancel does not stop the search of a project
    // that started (ls/crossproject.go:90 is the only check before it), and
    // every initial item starts at once. So after a cancel either no project
    // was searched (the cancel came before the items started) or every one
    // was, and each searched project keeps its warm query checker
    // (lsp-concurrency D3; lschk1-skeptic s4-cancel-cold-100: Go gives the
    // warm form in all 4 projects). Each project has 6,000 references, so
    // the cancel comes while the searches run.
    fn cross_project_search_after_cancel_leaves_every_project_searched_or_none() {
        const PROJECTS: usize = 4;
        let types = SEARCHED_TYPES;
        let reg = SEARCHED_REG.replace("'../types'", "'../shared/types'");
        let mut big = String::from("import type { ArgsOptions } from '../shared/types';\n");
        for i in 0..3000 {
            big.push_str(&format!(
                "export const v{i}: ArgsOptions = {{ description: 'x{i}' }};\nexport const d{i} = v{i}.description;\n"
            ));
        }
        let root_config = format!(r#"{SEARCHED_CONFIG}"shared/*.ts"]}}"#);
        let project_config = format!(r#"{SEARCHED_CONFIG}"*.ts"]}}"#);
        let mut files: Vec<(String, String)> = vec![
            ("/home/projects/tsconfig.json".to_string(), root_config),
            ("/home/projects/shared/types.ts".to_string(), types.to_string()),
        ];
        for p in 0..PROJECTS {
            files.push((format!("/home/projects/p{p}/tsconfig.json"), project_config.clone()));
            files.push((format!("/home/projects/p{p}/reg.ts"), reg.clone()));
            files.push((format!("/home/projects/p{p}/big.ts"), big.clone()));
        }
        let entries: Vec<(&str, &str)> =
            files.iter().map(|(name, text)| (name.as_str(), text.as_str())).collect();
        let client = init_completion_client("/home/projects", &entries);
        let reg_uris: Vec<lsproto::DocumentUri> = (0..PROJECTS)
            .map(|p| lsconv::file_name_to_document_uri(&format!("/home/projects/p{p}/reg.ts")))
            .collect();
        let types_uri = lsconv::file_name_to_document_uri("/home/projects/shared/types.ts");
        for uri in &reg_uris {
            open_file(&client, uri, &reg);
        }
        open_file(&client, &types_uri, types);
        // The projects are loaded, and the server waits for the next message.
        let (msg, _) = client.send_request(
            &lsproto::TEXT_DOCUMENT_HOVER_INFO,
            lsproto::HoverParams {
                text_document: lsproto::TextDocumentIdentifier { uri: types_uri.clone() },
                position: DESCRIPTION_IN_TYPES,
                ..Default::default()
            },
        );
        assert!(msg.error.is_none(), "{:?}", msg.error);

        let id = client.next_id();
        let req_id = ts_goport::jsonrpc::new_id_int(id);
        let references = client.send_request_message(
            lsproto::TEXT_DOCUMENT_REFERENCES_INFO.new_request_message(
                Some(req_id.clone()),
                lsproto::ReferenceParams {
                    text_document: lsproto::TextDocumentIdentifier { uri: types_uri },
                    position: DESCRIPTION_IN_TYPES,
                    context: Some(lsproto::ReferenceContext {
                        include_declaration: true,
                    }),
                    ..Default::default()
                },
            ),
            req_id,
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
        client.send_notification(
            &lsproto::CANCEL_REQUEST_INFO,
            lsproto::CancelParams {
                id: lsproto::IntegerOrString {
                    integer: Some(id),
                    ..Default::default()
                },
            },
        );
        references
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("the references answer");

        let hovers: Vec<String> = reg_uris
            .iter()
            .map(|uri| tokens_then_hover(&client, uri))
            .collect();
        let warm = hovers.iter().filter(|text| text.contains(WARM_HOVER)).count();
        let fresh = hovers.iter().filter(|text| text.contains(FRESH_HOVER)).count();
        assert!(
            warm == PROJECTS || fresh == PROJECTS,
            "some projects were searched and some not: {hovers:?}"
        );
    }
}
