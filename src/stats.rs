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
    /// Files copied to the sync target.
    pub files_copied: usize,
    /// Sync-target files already matching the source, left untouched.
    pub files_up_to_date: usize,
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

    /// Merge another run's counters into this one.
    ///
    /// Source-scan counters are replaced (a `link`-then-`sync` run scans the
    /// same source twice); action counters are summed.
    pub fn add(&mut self, other: Stats) {
        self.regular_files = other.regular_files;
        self.managed_files = other.managed_files;
        self.tracks = other.tracks;
        self.links_created += other.links_created;
        self.links_replaced += other.links_replaced;
        self.links_up_to_date += other.links_up_to_date;
        self.files_copied += other.files_copied;
        self.files_up_to_date += other.files_up_to_date;
        self.files_removed += other.files_removed;
        self.dirs_removed += other.dirs_removed;
        self.errors.extend(other.errors);
    }

    /// A short human-readable summary of the run.
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "scanned {} regular files: {} managed, {} tracks",
            self.regular_files, self.managed_files, self.tracks
        )];
        if self.links_created > 0 || self.links_replaced > 0 || self.links_up_to_date > 0 {
            lines.push(format!(
                "links: {} created, {} replaced, {} up to date",
                self.links_created, self.links_replaced, self.links_up_to_date
            ));
        }
        if self.files_copied > 0 || self.files_up_to_date > 0 {
            lines.push(format!(
                "copies: {} copied, {} up to date",
                self.files_copied, self.files_up_to_date
            ));
        }
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
