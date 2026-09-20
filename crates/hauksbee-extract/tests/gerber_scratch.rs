//! Shared scratch-directory lifecycle for gerber extraction tests: clear the
//! dir, let the caller populate it, extract, then remove it again. Every
//! write and `.expect(..)` message a caller uses stays at the call site;
//! only the create/extract/remove lifecycle lives here.

use std::path::{Path, PathBuf};

use hauksbee_extract::gerber::{from_gerber_dir, GerberExtraction};
use hauksbee_extract::ExtractError;

pub fn extract_fresh(
    dir: PathBuf,
    build: impl FnOnce(&Path),
) -> Result<GerberExtraction, ExtractError> {
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    build(&dir);
    let result = from_gerber_dir(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}
