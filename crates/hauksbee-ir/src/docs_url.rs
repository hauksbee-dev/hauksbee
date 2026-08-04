//! The one place a repo-relative doc path becomes a public URL.
//!
//! Every user-facing message that points at documentation renders the pointer
//! through [`docs_url`], so the transform lives in exactly one function and
//! the published site's URL contract can be tested against it (see
//! `tests/docs_url_contract.rs`, which validates every doc path the binaries
//! emit against the site's machine-published `url-contract.json`).
//!
//! The contract (published at
//! `https://hauksbee-docs.eoghancollins0.workers.dev/url-contract.json`) is:
//! route = `"/"` + lowercase(repo-relative path minus its `.md` suffix).
//! No other transform. Paths whose published route diverges from that rule
//! (the site's special cases, e.g. the repo root `README.md`) must not be
//! passed here; the contract test enforces that.

/// Render the public documentation URL for a repo-relative doc path.
///
/// `repo_path` is the path as it appears in the repository, e.g.
/// `docs/cosim/MCU.md`; the result is
/// `https://docs.hauksbee.dev/docs/cosim/mcu`.
pub fn docs_url(repo_path: &str) -> String {
    let trimmed = repo_path.strip_suffix(".md").unwrap_or(repo_path);
    format!("https://docs.hauksbee.dev/{}", trimmed.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::docs_url;

    #[test]
    fn lowercases_and_strips_md() {
        assert_eq!(
            docs_url("docs/cosim/MCU.md"),
            "https://docs.hauksbee.dev/docs/cosim/mcu"
        );
    }

    #[test]
    fn no_md_suffix_is_passed_through() {
        assert_eq!(
            docs_url("docs/checks"),
            "https://docs.hauksbee.dev/docs/checks"
        );
    }
}
