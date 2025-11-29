//! Move commits and subtrees from one place to another.
//!
//! Under the hood, this makes use of Git's advanced rebase functionality, which
//! is also used to preserve merge commits using the `--rebase-merges` option.

#![warn(missing_docs)]
#![warn(
    clippy::all,
    clippy::as_conversions,
    clippy::clone_on_ref_ptr,
    clippy::dbg_macro
)]
#![allow(clippy::too_many_arguments, clippy::blocks_in_conditions)]

use std::collections::HashMap;
use std::fmt::Write;
use std::time::SystemTime;

use eden_dag::Vertex;
use lib::core::repo_ext::RepoExt;
use lib::util::{ExitCode, EyreExitOr};
use rayon::ThreadPoolBuilder;
use tracing::instrument;

use git_branchless_opts::{MoveOptions, ResolveRevsetOptions, Revset};
use git_branchless_revset::resolve_commits;
use lib::core::config::{
    get_hint_enabled, get_hint_string, get_restack_preserve_timestamps,
    print_hint_suppression_notice, Hint,
};
use lib::core::dag::{sorted_commit_set, union_all, CommitSet, Dag};
use lib::core::effects::Effects;
use lib::core::eventlog::{EventLogDb, EventReplayer};
use lib::core::rewrite::{
    execute_rebase_plan, BuildRebasePlanOptions, ExecuteRebasePlanOptions, ExecuteRebasePlanResult,
    MergeConflictRemediation, RebasePlanBuilder, RebasePlanPermissions, RepoResource,
};
use lib::git::{GitRunInfo, NonZeroOid, Repo};

#[instrument]
fn resolve_base_commit(
    dag: &Dag,
    merge_base_oid: Option<Vertex>,
    oid: NonZeroOid,
) -> eyre::Result<NonZeroOid> {
    let bases = match merge_base_oid {
        Some(merge_base_oid) => {
            let range = dag.query_range(CommitSet::from(merge_base_oid), CommitSet::from(oid))?;
            let roots = dag.query_roots(range.clone())?;
            dag.query_children(roots)?.intersection(&range)
        }
        None => {
            let ancestors = dag.query_ancestors(CommitSet::from(oid))?;
            dag.query_roots(ancestors)?
        }
    };

    match dag.set_first(&bases)? {
        Some(base) => NonZeroOid::try_from(base),
        None => Ok(oid),
    }
}

/// Move a subtree from one place to another.
#[instrument]
/// Move commits or ranges in the commit graph to a new destination.
/// This function orchestrates the logic for moving, inserting, or fixing up commits,
/// handling various modes and options, and performing safety checks and planning.
/// It interacts with the DAG, resolves revsets, builds and executes a rebase plan,
/// and manages user feedback and error handling.
pub fn r#move(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    sources: Vec<Revset>,
    dest: Option<Revset>,
    bases: Vec<Revset>,
    exacts: Vec<Revset>,
    resolve_revset_options: &ResolveRevsetOptions,
    move_options: &MoveOptions,
    fixup: bool,
    insert: bool,
    dry_run: bool,
) -> EyreExitOr<()> {
    // Flags to track which arguments were provided by the user.
    let sources_provided = !sources.is_empty();
    let bases_provided = !bases.is_empty();
    let exacts_provided = !exacts.is_empty();
    let dest_provided = dest.is_some();
    // If no sources, bases, or exacts are provided, default sources to HEAD.
    let should_sources_default_to_head = !sources_provided && !bases_provided && !exacts_provided;

    // Open the repository from the current directory.
    let repo = Repo::from_current_dir()?;
    // Get the OID (object ID) of HEAD, if available.
    let head_oid = repo.get_head_info()?.oid;

    // Determine the destination revset.
    // If not provided, default to HEAD if possible, otherwise error.
    let dest = match dest {
        Some(dest) => dest,
        None => match head_oid {
            Some(oid) => Revset(oid.to_string()),
            None => {
                writeln!(effects.get_output_stream(), "No --dest argument was provided, and no OID for HEAD is available as a default")?;
                return Ok(Err(ExitCode(1)));
            }
        },
    };

    // Prepare repository state and DAG for planning.
    let references_snapshot = repo.get_references_snapshot()?;
    let conn = repo.get_db_conn()?;
    let event_log_db = EventLogDb::new(&conn)?;
    let event_replayer = EventReplayer::from_event_log_db(effects, &repo, &event_log_db)?;
    let event_cursor = event_replayer.make_default_cursor();
    let mut dag = Dag::open_and_sync(
        effects,
        &repo,
        &event_replayer,
        event_cursor,
        &references_snapshot,
    )?;

    // Resolve source, base, and exact commit sets from revsets.
    let source_oids: CommitSet =
        match resolve_commits(effects, &repo, &mut dag, &sources, resolve_revset_options) {
            Ok(commit_sets) => union_all(&commit_sets),
            Err(err) => {
                err.describe(effects)?;
                return Ok(Err(ExitCode(1)));
            }
        };
    let base_oids: CommitSet =
        match resolve_commits(effects, &repo, &mut dag, &bases, resolve_revset_options) {
            Ok(commit_sets) => union_all(&commit_sets),
            Err(err) => {
                err.describe(effects)?;
                return Ok(Err(ExitCode(1)));
            }
        };
    // Resolve exact components, ensuring each has exactly one root and one parent.
    let exact_components = match resolve_commits(
        effects,
        &repo,
        &mut dag,
        &exacts,
        resolve_revset_options,
    ) {
        Ok(commit_sets) => {
            let exact_oids = union_all(&commit_sets);
            let mut components: HashMap<NonZeroOid, CommitSet> = HashMap::new();

            for component in dag.get_connected_components(&exact_oids)?.into_iter() {
                let component_roots = dag.query_roots(component.clone())?;
                let component_root = match dag.commit_set_to_vec(&component_roots)?.as_slice() {
                    [only_commit_oid] => *only_commit_oid,
                    _ => {
                        writeln!(
                            effects.get_error_stream(),
                            "The --exact flag can only be used to move ranges with exactly 1 root.\n\
                             Received range with {} roots: {:?}",
                            dag.set_count(&component_roots)?,
                            component_roots
                        )?;
                        return Ok(Err(ExitCode(1)));
                    }
                };

                let component_parents = dag.query_parents(CommitSet::from(component_root))?;
                if dag.set_count(&component_parents)? != 1 {
                    writeln!(
                        effects.get_output_stream(),
                        "The --exact flag can only be used to move ranges or commits with exactly 1 parent.\n\
                         Received range with {} parents: {:?}",
                        dag.set_count(&component_parents)?,
                        component_parents
                    )?;
                    return Ok(Err(ExitCode(1)));
                };

                components.insert(component_root, component);
            }

            components
        }
        Err(err) => {
            err.describe(effects)?;
            return Ok(Err(ExitCode(1)));
        }
    };

    // Resolve the destination OID, ensuring it refers to exactly one commit.
    let dest_oid: NonZeroOid = match resolve_commits(
        effects,
        &repo,
        &mut dag,
        std::slice::from_ref(&dest),
        resolve_revset_options,
    ) {
        Ok(commit_sets) => match dag.commit_set_to_vec(&commit_sets[0])?.as_slice() {
            [only_commit_oid] => *only_commit_oid,
            other => {
                let Revset(expr) = dest;
                writeln!(
                    effects.get_error_stream(),
                    "Expected revset to expand to exactly 1 commit (got {}): {}",
                    other.len(),
                    expr,
                )?;
                return Ok(Err(ExitCode(1)));
            }
        },
        Err(err) => {
            err.describe(effects)?;
            return Ok(Err(ExitCode(1)));
        }
    };

    // If no sources or bases provided, default base to HEAD.
    let base_oids = if should_sources_default_to_head {
        match head_oid {
            Some(head_oid) => CommitSet::from(head_oid),
            None => {
                writeln!(effects.get_output_stream(), "No --source or --base arguments were provided, and no OID for HEAD is available as a default")?;
                return Ok(Err(ExitCode(1)));
            }
        }
    } else {
        base_oids
    };
    // For each base OID, resolve the correct base commit using merge base logic.
    let base_oids = {
        let mut result = Vec::new();
        for base_oid in dag.commit_set_to_vec(&base_oids)? {
            let merge_base_oid =
                dag.query_gca_one(vec![base_oid, dest_oid].into_iter().collect::<CommitSet>())?;
            let base_commit_oid = resolve_base_commit(&dag, merge_base_oid, base_oid)?;
            result.push(CommitSet::from(base_commit_oid))
        }
        union_all(&result)
    };
    // Union sources and bases for the set of commits to move.
    let source_oids = source_oids.union(&base_oids);

    // Print hints to the user if they provided redundant arguments that default to HEAD.
    if let Some(head_oid) = head_oid {
        if get_hint_enabled(&repo, Hint::MoveImplicitHeadArgument)? {
            let should_warn_base = !sources_provided
                && bases_provided
                && dag.set_contains(&base_oids, head_oid)?
                && dag.set_count(&base_oids)? == 1;
            if should_warn_base {
                writeln!(
                    effects.get_output_stream(),
                    "{}: you can omit the --base flag in this case, as it defaults to HEAD",
                    effects.get_glyphs().render(get_hint_string())?,
                )?;
            }

            let should_warn_dest = dest_provided && dest_oid == head_oid;
            if should_warn_dest {
                writeln!(
                    effects.get_output_stream(),
                    "{}: you can omit the --dest flag in this case, as it defaults to HEAD",
                    effects.get_glyphs().render(get_hint_string())?,
                )?;
            }

            if should_warn_base || should_warn_dest {
                print_hint_suppression_notice(effects, Hint::MoveImplicitHeadArgument)?;
            }
        }
    }
    // base_oids is no longer needed.
    drop(base_oids);

    // Unpack move options and prepare for rebase planning.
    let MoveOptions {
        force_rewrite_public_commits,
        force_in_memory,
        force_on_disk,
        detect_duplicate_commits_via_patch_id,
        resolve_merge_conflicts,
        dump_rebase_constraints,
        dump_rebase_plan,
        reparent,
    } = *move_options;
    // Get current time for event logging.
    let now = SystemTime::now();
    let event_tx_id = event_log_db.make_transaction_id(now, "move")?;
    // Create thread pool and repo pool for parallel operations.
    let pool = ThreadPoolBuilder::new().build()?;
    let repo_pool = RepoResource::new_pool(&repo)?;
    // Build the rebase plan for moving commits.
    let rebase_plan = {
        let build_options = BuildRebasePlanOptions {
            force_rewrite_public_commits,
            dump_rebase_constraints,
            dump_rebase_plan,
            detect_duplicate_commits_via_patch_id,
        };
        // Verify permissions for rewriting the set of commits to move.
        let permissions = {
            let commits_to_move = &source_oids;
            let commits_to_move = commits_to_move.union(&union_all(
                &exact_components.values().cloned().collect::<Vec<_>>(),
            ));
            let commits_to_move = if insert || fixup {
                commits_to_move.union(&dag.query_children(CommitSet::from(dest_oid))?)
            } else {
                commits_to_move
            };

            match RebasePlanPermissions::verify_rewrite_set(&dag, build_options, &commits_to_move)?
            {
                Ok(permissions) => permissions,
                Err(err) => {
                    err.describe(effects, &repo, &dag)?;
                    return Ok(Err(ExitCode(1)));
                }
            }
        };
        // Initialize the rebase plan builder.
        let mut builder = RebasePlanBuilder::new(&dag, permissions);

        // For each root of the source commits, add move or fixup operations to the plan.
        let source_roots = dag.query_roots(source_oids.clone())?;
        for source_root in dag.commit_set_to_vec(&source_roots)? {
            if fixup {
                // If fixup mode, fix up all descendants of the source root onto the destination.
                let commits = dag.query_descendants(CommitSet::from(source_root))?;
                let commits = dag.commit_set_to_vec(&commits)?;
                for commit in commits.iter() {
                    builder.fixup_commit(*commit, dest_oid)?;
                }
            } else {
                // Otherwise, move the subtree rooted at source_root to the destination.
                builder.move_subtree(source_root, vec![dest_oid])?;
            }

            if reparent {
                builder.reparent_subtree(source_root, vec![dest_oid], repo)?;
            }
        }

        // Handle exact components: sort roots topologically and process each.
        let component_roots: CommitSet = exact_components.keys().cloned().collect();
        let component_roots: Vec<NonZeroOid> = sorted_commit_set(&repo, &dag, &component_roots)?
            .iter()
            .map(|commit| commit.get_oid())
            .collect();
        for component_root in component_roots.iter().cloned() {
            let component = exact_components.get(&component_root).unwrap();

            // Find the non-inclusive ancestor components of the current root.
            // This determines where the current component should be moved.
            let mut possible_destinations: Vec<NonZeroOid> = vec![];
            for root in component_roots.iter().cloned() {
                let component = exact_components.get(&root).unwrap();
                if !dag.set_contains(component, component_root)?
                    && dag.query_is_ancestor(root, component_root)?
                {
                    possible_destinations.push(root);
                }
            }

            // Determine the destination OID for the current component.
            // If there are multiple possible destinations, ensure a single lineage.
            let component_dest_oid = if possible_destinations.is_empty() {
                dest_oid
            } else {
                // If there was a merge commit somewhere outside of the selected
                // components, then it's possible that the current component
                // could have multiple possible parents.
                //
                // To check for this, we can confirm that the nearest
                // destination component is an ancestor of the previous (ie next
                // nearest). This works because possible_destinations is made
                // from component_roots, which has been sorted topologically; so
                // each included component should "come after" the previous
                // component.
                for i in 1..possible_destinations.len() {
                    if !dag
                        .query_is_ancestor(possible_destinations[i - 1], possible_destinations[i])?
                    {
                        writeln!(
                            effects.get_output_stream(),
                            "This operation cannot be completed because the {} at {}\n\
                              has multiple possible parents also being moved. Please retry this operation\n\
                              without this {}, or with only 1 possible parent.",
                            if dag.set_count(component)? == 1 {
                                "commit"
                            } else {
                                "range of commits rooted"
                            },
                            component_root,
                            if dag.set_count(component)? == 1 {
                                "commit"
                            } else {
                                "range of commits"
                            },
                        )?;
                        return Ok(Err(ExitCode(1)));
                    }
                }

                let nearest_component = exact_components
                    .get(&possible_destinations[possible_destinations.len() - 1])
                    .unwrap();
                // The current component could be descended from any commit
                // in nearest_component, not just its head.
                let dest_ancestor = dag
                    .query_ancestors(CommitSet::from(component_root))?
                    .intersection(nearest_component);
                match dag.set_first(&dag.query_heads(dest_ancestor.clone())?)? {
                    Some(head) => NonZeroOid::try_from(head)?,
                    None => dest_oid,
                }
            };

            // Each component has exactly one parent (already checked).
            let component_parent = NonZeroOid::try_from(
                dag.set_first(&dag.query_parents(CommitSet::from(component_root))?)?
                    .unwrap(),
            )?;
            // Find children of the component that are not part of the component itself.
            let component_children: CommitSet =
                dag.query_children(component.clone())?.difference(component);
            let component_children = dag.filter_visible_commits(component_children)?;

            for component_child in dag.commit_set_to_vec(&component_children)? {
                // If the range being extracted has any child commits, then we
                // need to move each of those subtrees up to the parent commit
                // of the range. If, however, we're inserting the range and the
                // destination commit is in one of those subtrees, then we
                // should only move the commits from the root of that child
                // subtree up to (and including) the destination commit.
                if insert && dag.query_is_ancestor(component_child, component_dest_oid)? {
                    builder.move_range(component_child, component_dest_oid, component_parent)?;
                } else {
                    builder.move_subtree(component_child, vec![component_parent])?;
                }
            }

            // Add fixup or move operations for the component root itself.
            if fixup {
                let commits = dag.commit_set_to_vec(component)?;
                for commit in commits.iter() {
                    builder.fixup_commit(*commit, dest_oid)?;
                }
            } else {
                builder.move_subtree(component_root, vec![component_dest_oid])?;
            }

            if reparent {
                builder.reparent_subtree(component_root, vec![component_dest_oid], repo)?;
            }
        }

        // If insert mode is enabled, move children of the destination to the new source head.
        if insert {
            let source_head = {
                let exact_head = if component_roots.is_empty() {
                    CommitSet::empty()
                } else {
                    // As long as component_roots has been sorted topologically,
                    // we only need to compare adjacent elements to confirm a
                    // single lineage.
                    for i in 1..component_roots.len() {
                        if !dag.query_is_ancestor(component_roots[i - 1], component_roots[i])? {
                            writeln!(
                                effects.get_output_stream(),
                                "The --insert and --exact flags can only be used together when moving commits or\n\
                                 ranges that form a single lineage, but {} is not an ancestor of {}.",
                                component_roots[i - 1],
                                component_roots[i]
                            )?;
                            return Ok(Err(ExitCode(1)));
                        }
                    }

                    let head_component = exact_components
                        .get(&component_roots[component_roots.len() - 1])
                        .unwrap()
                        .clone();
                    dag.query_heads(head_component)?
                };
                let source_heads: CommitSet = dag
                    .query_heads(dag.query_descendants(source_oids.clone())?)?
                    .union(&exact_head);
                match dag.commit_set_to_vec(&source_heads)?.as_slice() {
                    [oid] => *oid,
                    _ => {
                        writeln!(
                            effects.get_output_stream(),
                            "The --insert flag cannot be used when moving subtrees or ranges with multiple heads."
                        )?;
                        return Ok(Err(ExitCode(1)));
                    }
                }
            };

            let exact_components = exact_components
                .values()
                .cloned()
                .collect::<Vec<CommitSet>>();
            let exact_oids = union_all(&exact_components);
            // Children of dest_oid that are not themselves being moved.
            let dest_children: CommitSet = dag
                .query_children(CommitSet::from(dest_oid))?
                .difference(&source_oids)
                .difference(&exact_oids);
            let dest_children = dag.filter_visible_commits(dest_children)?;

            for dest_child in dag.commit_set_to_vec(&dest_children)? {
                builder.move_subtree(dest_child, vec![source_head])?;
                if reparent {
                    builder.reparent_subtree(dest_child, vec![source_head], &repo)?;
                }
            }
        }
        // If reparent mode is enabled, reparent moved commits to their original parents.
        // This is similar to amend.rs logic.
        if reparent {
            let mut moved_commits = source_oids.clone();
            for component in exact_components.values() {
                moved_commits = moved_commits.union(component);
            }
            let moved_commits_vec = dag.commit_set_to_vec(&moved_commits)?;
            for commit_oid in moved_commits_vec {
                let parents_set = dag.query_parents(CommitSet::from(commit_oid))?;
                let parent_oids = dag.commit_set_to_vec(&parents_set)?;
                builder.reparent_subtree(commit_oid, parent_oids, &repo)?;
            }
        }
        // Build the final rebase plan.
        builder.build(effects, &pool, &repo_pool)?
    };
    // Execute the rebase plan, or report if nothing needs to be done.
    let result = match rebase_plan {
        Ok(None) => {
            writeln!(effects.get_output_stream(), "Nothing to do.")?;
            return Ok(Ok(()));
        }
        Ok(Some(rebase_plan)) => {
            let options = ExecuteRebasePlanOptions {
                now,
                event_tx_id,
                preserve_timestamps: get_restack_preserve_timestamps(&repo)?,
                force_in_memory,
                force_on_disk,
                dry_run,
                resolve_merge_conflicts,
                check_out_commit_options: Default::default(),
            };
            execute_rebase_plan(
                effects,
                git_run_info,
                &repo,
                &event_log_db,
                &rebase_plan,
                &options,
            )?
        }
        Err(err) => {
            err.describe(effects, &repo, &dag)?;
            return Ok(Err(ExitCode(1)));
        }
    };

    // Handle the result of executing the rebase plan.
    match result {
        // Success: commits were moved.
        ExecuteRebasePlanResult::Succeeded { rewritten_oids: _ } => Ok(Ok(())),

        // Dry-run: report that no commits were moved.
        ExecuteRebasePlanResult::WouldSucceed if dry_run => {
            writeln!(effects.get_output_stream(), "(This was a dry-run; no commits were moved. Re-run without --dry-run to actually move commits.)")?;
            Ok(Ok(()))
        }

        // Should not happen unless dry-run.
        ExecuteRebasePlanResult::WouldSucceed => {
            unreachable!("WouldSucceed should only apply to dry runs")
        }

        // Merge conflicts occurred: describe and report error.
        ExecuteRebasePlanResult::DeclinedToMerge { failed_merge_info } => {
            failed_merge_info.describe(effects, &repo, MergeConflictRemediation::Retry)?;
            Ok(Err(ExitCode(1)))
        }

        // Other failure: propagate exit code.
        ExecuteRebasePlanResult::Failed { exit_code } => Ok(Err(exit_code)),
    }
}
