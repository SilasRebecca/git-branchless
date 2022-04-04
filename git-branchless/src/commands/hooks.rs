//! Callbacks for Git hooks.
//!
//! Git uses "hooks" to run user-defined scripts after certain events. We
//! extensively use these hooks to track user activity and e.g. decide if a
//! commit should be considered obsolete.
//!
//! The hooks are installed by the `branchless init` command. This module
//! contains the implementations for the hooks.

use std::ffi::OsString;
use std::fmt::Write;
use std::io::{stdin, BufRead};
use std::time::SystemTime;

use eyre::Context;
use itertools::Itertools;
use tracing::{error, instrument, warn};

use crate::commands::gc::mark_commit_reachable;
use crate::core::eventlog::{should_ignore_ref_updates, Event, EventLogDb};
use crate::core::formatting::{printable_styled_string, Glyphs, Pluralize};
use crate::git::{CategorizedReferenceName, MaybeZeroOid, Repo};

use crate::core::effects::Effects;
pub use crate::core::rewrite::rewrite_hooks::{
    hook_drop_commit_if_empty, hook_post_rewrite, hook_register_extra_post_rewrite_hook,
    hook_skip_upstream_applied_commit,
};

/// Handle Git's `post-checkout` hook.
///
/// See the man-page for `githooks(5)`.
#[instrument]
pub fn hook_post_checkout(
    effects: &Effects,
    previous_head_oid: &str,
    current_head_oid: &str,
    is_branch_checkout: isize,
) -> eyre::Result<()> {
    if is_branch_checkout == 0 {
        return Ok(());
    }

    let now = SystemTime::now();
    let timestamp = now.duration_since(SystemTime::UNIX_EPOCH)?;
    writeln!(
        effects.get_output_stream(),
        "branchless: processing checkout"
    )?;

    let repo = Repo::from_current_dir()?;
    let conn = repo.get_db_conn()?;
    let mut event_log_db = EventLogDb::new(&conn)?;
    let event_tx_id = event_log_db.make_transaction_id(now, "hook-post-checkout")?;
    event_log_db.add_events(vec![Event::RefUpdateEvent {
        timestamp: timestamp.as_secs_f64(),
        event_tx_id,
        old_oid: previous_head_oid.parse()?,
        new_oid: {
            let oid: MaybeZeroOid = current_head_oid.parse()?;
            oid
        },
        ref_name: OsString::from("HEAD"),
        message: None,
    }])?;
    Ok(())
}

fn hook_post_commit_common(effects: &Effects, hook_name: &str) -> eyre::Result<()> {
    let now = SystemTime::now();
    let glyphs = Glyphs::detect();
    let repo = Repo::from_current_dir()?;
    let conn = repo.get_db_conn()?;
    let mut event_log_db = EventLogDb::new(&conn)?;

    let commit_oid = match repo.get_head_info()?.oid {
        Some(commit_oid) => commit_oid,
        None => {
            // A strange situation, but technically possible.
            warn!(
                "`{}` hook called, but could not determine the OID of `HEAD`",
                hook_name
            );
            return Ok(());
        }
    };

    let commit = repo
        .find_commit_or_fail(commit_oid)
        .wrap_err("Looking up `HEAD` commit")?;
    mark_commit_reachable(&repo, commit_oid)
        .wrap_err("Marking commit as reachable for GC purposes")?;

    let timestamp = commit.get_time().seconds();

    // Potentially lossy conversion. The semantics are to round to the nearest
    // possible float:
    // https://doc.rust-lang.org/reference/expressions/operator-expr.html#semantics.
    // We don't rely on the timestamp's correctness for anything, so this is
    // okay.
    #[allow(clippy::as_conversions)]
    let timestamp = timestamp as f64;

    let event_tx_id = event_log_db.make_transaction_id(now, hook_name)?;
    event_log_db.add_events(vec![Event::CommitEvent {
        timestamp,
        event_tx_id,
        commit_oid: commit.get_oid(),
    }])?;
    writeln!(
        effects.get_output_stream(),
        "branchless: processed commit: {}",
        printable_styled_string(&glyphs, commit.friendly_describe(&glyphs)?)?,
    )?;

    Ok(())
}

/// Handle Git's `post-commit` hook.
///
/// See the man-page for `githooks(5)`.
#[instrument]
pub fn hook_post_commit(effects: &Effects) -> eyre::Result<()> {
    hook_post_commit_common(effects, "post-commit")
}

/// Handle Git's `post-merge` hook. It seems that Git doesn't invoke the
/// `post-commit` hook after a merge commit, so we need to handle this case
/// explicitly with another hook.
///
/// See the man-page for `githooks(5)`.
#[instrument]
pub fn hook_post_merge(effects: &Effects, _is_squash_merge: isize) -> eyre::Result<()> {
    hook_post_commit_common(effects, "post-merge")
}

mod reference_transaction {
    use std::collections::HashMap;
    use std::convert::TryInto;
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::{BufRead, BufReader, Cursor};
    use std::str::FromStr;

    use eyre::Context;
    use lazy_static::lazy_static;
    use os_str_bytes::OsStringBytes;
    use tracing::{instrument, warn};

    use crate::git::{MaybeZeroOid, Repo};

    #[instrument]
    fn parse_packed_refs_line(line: &[u8]) -> Option<(OsString, MaybeZeroOid)> {
        if line.is_empty() {
            return None;
        }
        if line[0] == b'#' {
            // The leading `# pack-refs with:` pragma.
            return None;
        }
        if !(b'0'..b'9').contains(&line[0]) && !(b'a'..b'f').contains(&line[0]) {
            // The leading `# pack-refs with:` pragma.
            warn!(?line, "Unrecognized pack-refs line starting character");
            return None;
        }

        lazy_static! {
            static ref RE: regex::bytes::Regex =
                regex::bytes::Regex::new(r"^([^ ]+) (.+)$").unwrap();
        };
        match RE.captures(line) {
            None => {
                warn!(?line, "No regex match for pack-refs line");
                None
            }

            Some(captures) => {
                let oid = &captures[1];
                let oid = match std::str::from_utf8(oid) {
                    Ok(oid) => oid,
                    Err(err) => {
                        warn!(?oid, ?err, "Could not parse OID for pack-refs line");
                        return None;
                    }
                };
                let oid = match MaybeZeroOid::from_str(oid) {
                    Ok(oid) => oid,
                    Err(err) => {
                        warn!(?oid, ?err, "Could not parse OID for pack-refs line");
                        return None;
                    }
                };

                let reference_name = &captures[2];
                let reference_name = match OsStringBytes::from_raw_vec(reference_name.to_vec()) {
                    Ok(reference_name) => reference_name,
                    Err(err) => {
                        warn!(
                            ?reference_name,
                            ?err,
                            "Could not parse reference name for pack-refs line"
                        );
                        return None;
                    }
                };

                Some((reference_name, oid))
            }
        }
    }

    #[cfg(test)]
    #[test]
    fn test_parse_packed_refs_line() {
        use super::*;

        let line: &[u8] = b"1234567812345678123456781234567812345678 refs/foo/bar";
        let name = OsString::from("refs/foo/bar");
        let oid = MaybeZeroOid::from_str("1234567812345678123456781234567812345678").unwrap();
        assert_eq!(parse_packed_refs_line(line), Some((name, oid)));
    }

    #[instrument]
    pub fn read_packed_refs_file(repo: &Repo) -> eyre::Result<HashMap<OsString, MaybeZeroOid>> {
        let packed_refs_file_path = repo.get_packed_refs_path();
        let file = match File::open(&packed_refs_file_path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(err) => return Err(err.into()),
        };

        let reader = BufReader::new(file);
        let mut result = HashMap::new();
        for line in reader.split(b'\n') {
            let line = line.wrap_err("Reading line from packed-refs")?;
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = parse_packed_refs_line(&line) {
                result.insert(k, v);
            }
        }
        Ok(result)
    }

    #[derive(Debug, PartialEq, Eq)]
    pub struct ParsedReferenceTransactionLine {
        pub ref_name: OsString,
        pub old_oid: MaybeZeroOid,
        pub new_oid: MaybeZeroOid,
    }

    #[instrument]
    pub fn parse_reference_transaction_line(
        line: &[u8],
    ) -> eyre::Result<ParsedReferenceTransactionLine> {
        let cursor = Cursor::new(line);
        let fields = {
            let mut fields = Vec::new();
            for field in cursor.split(b' ') {
                let field = field.wrap_err("Reading reference-transaction field")?;
                let field = OsString::from_raw_vec(field)
                    .wrap_err("Decoding reference-transaction field")?;
                fields.push(field);
            }
            fields
        };
        match fields.as_slice() {
            [old_value, new_value, ref_name] => Ok(ParsedReferenceTransactionLine {
                ref_name: ref_name.clone(),
                old_oid: old_value.as_os_str().try_into()?,
                new_oid: {
                    let oid: MaybeZeroOid = new_value.as_os_str().try_into()?;
                    oid
                },
            }),
            _ => {
                eyre::bail!(
                    "Unexpected number of fields in reference-transaction line: {:?}",
                    &line
                )
            }
        }
    }

    #[cfg(test)]
    #[test]
    fn test_parse_reference_transaction_line() -> eyre::Result<()> {
        use crate::core::eventlog::should_ignore_ref_updates;

        let line = b"123abc 456def refs/heads/mybranch";
        assert_eq!(
            parse_reference_transaction_line(line)?,
            ParsedReferenceTransactionLine {
                old_oid: "123abc".parse()?,
                new_oid: {
                    let oid: MaybeZeroOid = "456def".parse()?;
                    oid
                },
                ref_name: OsString::from("refs/heads/mybranch"),
            }
        );

        {
            let line = b"123abc 456def ORIG_HEAD";
            let parsed_line = parse_reference_transaction_line(line)?;
            assert_eq!(
                parsed_line,
                ParsedReferenceTransactionLine {
                    old_oid: "123abc".parse()?,
                    new_oid: {
                        let oid: MaybeZeroOid = "456def".parse()?;
                        oid
                    },
                    ref_name: OsString::from("ORIG_HEAD"),
                }
            );
            assert!(should_ignore_ref_updates(&parsed_line.ref_name));
        }

        let line = b"there are not three fields here";
        assert!(parse_reference_transaction_line(line).is_err());

        Ok(())
    }

    /// As per the discussion at
    /// https://public-inbox.org/git/CAKjfCeBcuYC3OXRVtxxDGWRGOxC38Fb7CNuSh_dMmxpGVip_9Q@mail.gmail.com/,
    /// the OIDs passed to the reference transaction can't actually be trusted
    /// when dealing with packed references, so we need to look up their actual
    /// values on disk again. See https://git-scm.com/docs/git-pack-refs for
    /// details about packed references.
    ///
    /// Supposing we have a ref named `refs/heads/foo` pointing to an OID
    /// `abc123`, when references are packed, we'll first see a transaction like
    /// this:
    ///
    /// ```text
    /// 000000 abc123 refs/heads/foo
    /// ```
    ///
    /// And immediately afterwards see a transaction like this:
    ///
    /// ```text
    /// abc123 000000 refs/heads/foo
    /// ```
    ///
    /// If considered naively, this would suggest that the reference was created
    /// (even though it already exists!) and then deleted (even though it still
    /// exists!).
    #[instrument]
    pub fn fix_packed_reference_oid(
        repo: &Repo,
        packed_references: &HashMap<OsString, MaybeZeroOid>,
        parsed_line: ParsedReferenceTransactionLine,
    ) -> ParsedReferenceTransactionLine {
        match parsed_line {
            ParsedReferenceTransactionLine {
                ref_name,
                old_oid: MaybeZeroOid::Zero,
                new_oid,
            } if packed_references.get(&ref_name) == Some(&new_oid) => {
                // The reference claims to have been created, but it appears to
                // already be in the `packed-refs` file with that OID. Most
                // likely it was being packed in this operation.
                ParsedReferenceTransactionLine {
                    ref_name,
                    old_oid: new_oid,
                    new_oid,
                }
            }

            ParsedReferenceTransactionLine {
                ref_name,
                old_oid,
                new_oid: MaybeZeroOid::Zero,
            } if packed_references.get(&ref_name) == Some(&old_oid) => {
                // The reference claims to have been deleted, but it's still in
                // the `packed-refs` file with that OID. Most likely it was
                // being packed in this operation.
                ParsedReferenceTransactionLine {
                    ref_name,
                    old_oid,
                    new_oid: old_oid,
                }
            }

            other => other,
        }
    }
}

/// Handle Git's `reference-transaction` hook.
///
/// See the man-page for `githooks(5)`.
#[instrument]
pub fn hook_reference_transaction(effects: &Effects, transaction_state: &str) -> eyre::Result<()> {
    use reference_transaction::{
        fix_packed_reference_oid, parse_reference_transaction_line, read_packed_refs_file,
        ParsedReferenceTransactionLine,
    };

    if transaction_state != "committed" {
        return Ok(());
    }
    let now = SystemTime::now();

    let repo = Repo::from_current_dir()?;
    let conn = repo.get_db_conn()?;
    let mut event_log_db = EventLogDb::new(&conn)?;
    let event_tx_id = event_log_db.make_transaction_id(now, "reference-transaction")?;

    let packed_references = read_packed_refs_file(&repo)?;

    let parsed_lines: Vec<ParsedReferenceTransactionLine> = stdin()
        .lock()
        .split(b'\n')
        .filter_map(|line| {
            let line = match line {
                Ok(line) => line,
                Err(_) => return None,
            };
            match parse_reference_transaction_line(line.as_slice()) {
                Ok(line) => Some(line),
                Err(err) => {
                    error!(?err, "Could not parse reference-transaction-line");
                    None
                }
            }
        })
        .filter(
            |ParsedReferenceTransactionLine {
                 ref_name,
                 old_oid: _,
                 new_oid: _,
             }| !should_ignore_ref_updates(ref_name),
        )
        .map(|parsed_line| fix_packed_reference_oid(&repo, &packed_references, parsed_line))
        .collect();
    if parsed_lines.is_empty() {
        return Ok(());
    }

    let num_reference_updates = Pluralize {
        determiner: None,
        amount: parsed_lines.len(),
        unit: ("update", "updates"),
    };
    writeln!(
        effects.get_output_stream(),
        "branchless: processing {}: {}",
        num_reference_updates,
        parsed_lines
            .iter()
            .map(
                |ParsedReferenceTransactionLine {
                     ref_name,
                     old_oid: _,
                     new_oid: _,
                 }| { CategorizedReferenceName::new(ref_name).friendly_describe() }
            )
            .map(|description| format!("{}", console::style(description).green()))
            .sorted()
            .collect::<Vec<_>>()
            .join(", ")
    )?;

    let timestamp = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .wrap_err("Calculating timestamp")?
        .as_secs_f64();
    let events = parsed_lines
        .into_iter()
        .map(
            |ParsedReferenceTransactionLine {
                 ref_name,
                 old_oid,
                 new_oid,
             }| {
                Event::RefUpdateEvent {
                    timestamp,
                    event_tx_id,
                    ref_name,
                    old_oid,
                    new_oid,
                    message: None,
                }
            },
        )
        .collect::<Vec<Event>>();
    event_log_db.add_events(events)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::testing::{make_git, GitRunOptions};

    #[test]
    fn test_is_rebase_underway() -> eyre::Result<()> {
        let git = make_git()?;

        git.init_repo()?;
        let repo = git.get_repo()?;
        assert!(!repo.is_rebase_underway()?);

        let oid1 = git.commit_file_with_contents("test", 1, "foo")?;
        git.run(&["checkout", "HEAD^"])?;
        git.commit_file_with_contents("test", 1, "bar")?;
        git.run_with_options(
            &["rebase", &oid1.to_string()],
            &GitRunOptions {
                expected_exit_code: 1,
                ..Default::default()
            },
        )?;
        assert!(repo.is_rebase_underway()?);

        Ok(())
    }
}