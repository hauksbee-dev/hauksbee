use std::path::Path;

/// Encode a filesystem path as a complete TOML string value.
///
/// Windows paths contain backslashes (and canonical paths may carry the
/// `\\?\` prefix), so interpolating `Path::display()` between quotes creates
/// invalid TOML escape sequences. Let the TOML serializer own the quoting.
pub fn toml_path(path: &Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}
