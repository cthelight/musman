//! Per-run action counters and summary reporting.

/// Aggregated outcome of one reconciliation run.
#[derive(Debug, Default)]
pub struct Stats {
    /// Regular files seen in the source tree (managed or not).
    pub regular_files: usize,
    /// Source files whose extension is a managed format.
    pub managed_files: usize,
    /// Distinct tracks (relative paths minus extension) among managed files.
    pub tracks: usize,
    /// New hard links created.
    pub links_created: usize,
    /// Existing links re-pointed at the correct source file.
    pub links_replaced: usize,
    /// Links already correct, left untouched.
    pub links_up_to_date: usize,
    /// Files removed because the desired state does not want them.
    pub files_removed: usize,
    /// Empty directories removed.
    pub dirs_removed: usize,
    /// Non-fatal problems, one formatted line each.
    pub errors: Vec<String>,
}

impl Stats {
    /// True when any operation failed (drives the exit code).
    pub fn had_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// A short human-readable summary of the run.
    pub fn summary(&self) -> String {
        let mut lines = vec![
            format!(
                "scanned {} regular files: {} managed, {} tracks",
                self.regular_files, self.managed_files, self.tracks
            ),
            format!(
                "links: {} created, {} replaced, {} up to date",
                self.links_created, self.links_replaced, self.links_up_to_date
            ),
        ];
        if self.files_removed > 0 {
            lines.push(format!("files removed: {}", self.files_removed));
        }
        if self.dirs_removed > 0 {
            lines.push(format!("empty directories removed: {}", self.dirs_removed));
        }
        if !self.errors.is_empty() {
            lines.push(format!("errors: {}", self.errors.len()));
            for err in &self.errors {
                lines.push(format!("  error: {err}"));
            }
        }
        lines.join("\n")
    }
}
