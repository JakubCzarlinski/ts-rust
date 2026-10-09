//! Port of internal/outputpaths/outputpaths_test.go (tsgo#4900).

use ts_goport::frontend::outputpaths::get_source_file_path_in_new_dir;

// Go: outputpaths/outputpaths_test.go:10 TestGetSourceFilePathInNewDirSourceMatchesCommonDirectory
#[test]
fn test_get_source_file_path_in_new_dir_source_matches_common_directory() {
    let actual = get_source_file_path_in_new_dir(
        "/project/src",
        "/project/out",
        "/project",
        "/project/src/",
        true,
    );
    assert_eq!(actual, "/project/src");
}

// Go: outputpaths/outputpaths_test.go:17 TestGetSourceFilePathInNewDirCanonicalizationShrinksCommonDirectory
#[test]
fn test_get_source_file_path_in_new_dir_canonicalization_shrinks_common_directory() {
    // Each Kelvin sign '\u212A' case-folds to the single-byte 'k', so the raw
    // (non-canonicalized) commonSourceDirectory is longer, in bytes, than the source
    // file path it's a case-insensitive prefix of, even though the file path itself
    // is longer overall once its own (already-lowercase) suffix is included.
    // Slicing sourceFilePath by len(commonSourceDirectory) bytes would still panic
    // here ([14:11]); this must clamp per-rune instead, like the reference
    // implementation's substring does.
    let actual = get_source_file_path_in_new_dir(
        "/kkkk/a.ts",
        "/out",
        "/",
        "/\u{212A}\u{212A}\u{212A}\u{212A}/",
        false,
    );
    assert_eq!(actual, "/out/a.ts");
}
