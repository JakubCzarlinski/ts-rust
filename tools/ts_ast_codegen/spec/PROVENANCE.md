# TypeScript Go AST schema provenance

`ast.json` and `ast.schema.json` come from
[`microsoft/typescript-go`](https://github.com/microsoft/typescript-go):

- Commit: `dc37b5249ab60e2bbce936f71b883e6c8136167e`
- Upstream paths: `_scripts/ast.json`, `_scripts/ast.schema.json`
- Retrieved: 2026-06-22
- Upstream `ast.json` SHA-256: `9259791a628105b1ed375a1a69f2002ad478f10e60ae68e01e5527e0fe619546`
- Upstream `ast.schema.json` SHA-256: `c614df46892e8623fcb4ba9d2cbdc4da2537af140674776f3dbb78e96cdf16d2`

`ast.schema.json` is unchanged. It is equal to `tools/scripts/tsc/ast.schema.json` at
microsoft/TypeScript `673a5f17d713` (the current Go pin).

`ast.json` has hand edits for the Rust codegen: 5f68a9705, 2c834772b (nullable object
property type slots) and 0d68d93aa (the optional `ModuleDeclaration.Attributes` member of
ts#63931). Its SHA-256 is now
`60f942171958ac216eb60ac70b01e8882dde3d0d8833164737d95dae211a6516`.

The Go file at the current pin is `tools/scripts/tsc/ast.json` at microsoft/TypeScript
`673a5f17d713`. It is not a drop-in replacement: `ts_ast_codegen` stops on its
`JsxTagNameExpression` alias member `JsxTagNamePropertyAccess`, and it differs from this file
in `brand` fields, `extends` lists and aliases (`DeclarationName`, `HeritageClauseElement`).
`ast_generated.rs` and `syntax_kind.rs` are checked against this file (`check-ast`, `check`).

The upstream repositories are licensed under the Apache License 2.0.
