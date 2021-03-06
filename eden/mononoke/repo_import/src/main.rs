/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![type_length_limit = "4522397"]
use anyhow::{format_err, Error};
use blobrepo::{save_bonsai_changesets, BlobRepo};
use blobrepo_hg::BlobRepoHg;
use bookmarks::{BookmarkName, BookmarkUpdateReason};
use clap::Arg;
use cmdlib::args;
use cmdlib::helpers::block_execute;
use context::CoreContext;
use cross_repo_sync::rewrite_commit;
use derived_data_utils::derived_data_utils;
use fbinit::FacebookInit;
use futures::{
    compat::Future01CompatExt,
    future::TryFutureExt,
    stream::{self, StreamExt, TryStreamExt},
};
use import_tools::{GitimportPreferences, GitimportTarget};
use mercurial_types::{HgChangesetId, MPath};
use mononoke_types::{BonsaiChangeset, ChangesetId};
use movers::DefaultAction;
use serde::{Deserialize, Serialize};
use serde_json;
use slog::info;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use tokio::{process, time};
use topo_sort::sort_topological;

const ARG_GIT_REPOSITORY_PATH: &str = "git-repository-path";
const ARG_DEST_PATH: &str = "dest-path";
const ARG_BATCH_SIZE: &str = "batch-size";
const ARG_BOOKMARK_SUFFIX: &str = "bookmark-suffix";
const ARG_CALL_SIGN: &str = "call-sign";
const ARG_PHAB_CHECK_DISABLED: &str = "disable-phabricator-check";
const ARG_X_REPO_CHECK_DISABLED: &str = "disable-x-repo-check";
const ARG_HG_SYNC_CHECK_DISABLED: &str = "disable-hg-sync-check";
const ARG_SLEEP_TIME: &str = "sleep-time";

#[derive(Deserialize, Clone, Debug)]
struct GraphqlQueryObj {
    differential_commit_query: Vec<GraphqlCommitQueryObj>,
}
#[derive(Deserialize, Clone, Debug)]
struct GraphqlCommitQueryObj {
    results: GraphqlResultsObj,
}
#[derive(Deserialize, Clone, Debug)]
struct GraphqlResultsObj {
    nodes: Vec<GraphqlImportedObj>,
}
#[derive(Deserialize, Clone, Debug)]
struct GraphqlImportedObj {
    imported: bool,
}
#[derive(Debug, Serialize)]
struct GraphqlInputVariables {
    commit: String,
}
#[derive(Debug)]
struct CheckerFlags<'a> {
    phab_check_disabled: bool,
    x_repo_check_disabled: bool,
    hg_sync_check_disabled: bool,
    call_sign: Option<&'a str>,
}

async fn rewrite_file_paths(
    ctx: &CoreContext,
    repo: &BlobRepo,
    path: &Path,
    prefix: &str,
) -> Result<Vec<BonsaiChangeset>, Error> {
    let prefs = GitimportPreferences::default();
    let target = GitimportTarget::FullRepo;
    let import_map = import_tools::gitimport(ctx, repo, path, target, prefs).await?;
    let mut remapped_parents: HashMap<ChangesetId, ChangesetId> = HashMap::new();
    let mover = movers::mover_factory(
        HashMap::new(),
        DefaultAction::PrependPrefix(MPath::new(prefix).unwrap()),
    )?;
    let mut bonsai_changesets = vec![];

    for (_id, (bcs_id, bcs)) in import_map {
        let bcs_mut = bcs.into_mut();
        let rewritten_bcs_opt = rewrite_commit(
            ctx.clone(),
            bcs_mut,
            &remapped_parents,
            mover.clone(),
            repo.clone(),
        )
        .await?;

        if let Some(rewritten_bcs_mut) = rewritten_bcs_opt {
            let rewritten_bcs = rewritten_bcs_mut.freeze()?;
            remapped_parents.insert(bcs_id, rewritten_bcs.get_changeset_id());
            info!(
                ctx.logger(),
                "Remapped {:?} => {:?}",
                bcs_id,
                rewritten_bcs.get_changeset_id(),
            );
            bonsai_changesets.push(rewritten_bcs);
        }
    }
    save_bonsai_changesets(bonsai_changesets.clone(), ctx.clone(), repo.clone())
        .compat()
        .await?;
    Ok(bonsai_changesets)
}

async fn derive_bonsais(
    ctx: &CoreContext,
    repo: &BlobRepo,
    shifted_bcs: &[BonsaiChangeset],
) -> Result<(), Error> {
    let derived_data_types = &repo.get_derived_data_config().derived_data_types;

    let len = derived_data_types.len();
    let mut derived_utils = vec![];
    for ty in derived_data_types {
        let utils = derived_data_utils(repo.clone(), ty)?;
        derived_utils.push(utils);
    }

    stream::iter(derived_utils)
        .map(Ok)
        .try_for_each_concurrent(len, |derived_util| async move {
            for bcs in shifted_bcs {
                let csid = bcs.get_changeset_id();
                derived_util
                    .derive(ctx.clone(), repo.clone(), csid)
                    .compat()
                    .map_ok(|_| ())
                    .await?;
            }
            Result::<(), Error>::Ok(())
        })
        .await
}

async fn move_bookmark(
    ctx: &CoreContext,
    repo: &BlobRepo,
    shifted_bcs: &[BonsaiChangeset],
    batch_size: usize,
    bookmark_suffix: &str,
    checker_flags: &CheckerFlags<'_>,
    sleep_time: u64,
) -> Result<(), Error> {
    if shifted_bcs.is_empty() {
        return Err(format_err!("There is no bonsai changeset present"));
    }

    let bookmark = BookmarkName::new(format!("repo_import_{}", bookmark_suffix))?;
    let first_bcs = match shifted_bcs.first() {
        Some(first) => first,
        None => {
            return Err(format_err!("There is no bonsai changeset present"));
        }
    };
    let mut old_csid = first_bcs.get_changeset_id();
    let mut transaction = repo.update_bookmark_transaction(ctx.clone());
    transaction.create(&bookmark, old_csid, BookmarkUpdateReason::ManualMove, None)?;
    if !transaction.commit().await? {
        return Err(format_err!("Logical failure while creating {:?}", bookmark));
    }
    info!(
        ctx.logger(),
        "Created bookmark {:?} pointing to {}", bookmark, old_csid
    );
    for chunk in shifted_bcs.chunks(batch_size) {
        transaction = repo.update_bookmark_transaction(ctx.clone());
        let curr_csid = match chunk.last() {
            Some(bcs) => bcs.get_changeset_id(),
            None => {
                return Err(format_err!("There is no bonsai changeset present"));
            }
        };
        transaction.update(
            &bookmark,
            curr_csid,
            old_csid,
            BookmarkUpdateReason::ManualMove,
            None,
        )?;

        if !transaction.commit().await? {
            return Err(format_err!("Logical failure while setting {:?}", bookmark));
        }
        info!(
            ctx.logger(),
            "Set bookmark {:?} to point to {:?}", bookmark, curr_csid
        );

        // if a check is disabled, we have already passed the check
        let mut passed_phab_check = checker_flags.phab_check_disabled;
        let mut _passed_x_repo_check = checker_flags.x_repo_check_disabled;
        let mut _passed_hg_sync_check = checker_flags.hg_sync_check_disabled;
        let hg_csid = repo
            .get_hg_from_bonsai_changeset(ctx.clone(), curr_csid)
            .compat()
            .await?;
        while !passed_phab_check {
            let call_sign = checker_flags.call_sign.as_ref().unwrap();
            passed_phab_check = phabricator_commit_check(&call_sign, &hg_csid).await?;
            if !passed_phab_check {
                info!(
                    ctx.logger(),
                    "Phabricator hasn't parsed commit: {:?}", hg_csid
                );
                time::delay_for(time::Duration::from_secs(sleep_time)).await;
            }
        }
        old_csid = curr_csid;
    }
    Ok(())
}

async fn phabricator_commit_check(call_sign: &str, hg_csid: &HgChangesetId) -> Result<bool, Error> {
    let commit_id = format!("r{}{}", call_sign, hg_csid);
    let query = "query($commit: String!) {
                    differential_commit_query(query_params:{commits:[$commit]}) {
                        results {
                            nodes {
                                imported
                            }
                        }
                    }
                }";
    let variables = serde_json::to_string(&GraphqlInputVariables { commit: commit_id }).unwrap();
    let output = process::Command::new("jf")
        .arg("graphql")
        .arg("--query")
        .arg(query)
        .arg("--variables")
        .arg(variables)
        .output()
        .await?;
    if !output.status.success() {
        let e = format_err!(
            "Failed to fetch graphql commit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Err(e);
    }
    let query: GraphqlQueryObj = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))?;
    let first_query = match query.differential_commit_query.first() {
        Some(first) => first,
        None => {
            return Err(format_err!(
                "No results were found when checking phabricator"
            ));
        }
    };
    let nodes = &first_query.results.nodes;
    let imported = match nodes.first() {
        Some(imp_obj) => imp_obj.imported,
        None => return Ok(false),
    };
    Ok(imported)
}

fn is_valid_bookmark_suffix(bookmark_suffix: &str) -> bool {
    let spec_chars = "./-_";
    bookmark_suffix
        .chars()
        .all(|c| c.is_alphanumeric() || spec_chars.contains(c))
}

fn sort_bcs(shifted_bcs: &[BonsaiChangeset]) -> Result<Vec<BonsaiChangeset>, Error> {
    let mut bcs_parents = HashMap::new();
    let mut id_bcs = HashMap::new();
    for bcs in shifted_bcs {
        let parents: Vec<_> = bcs.parents().collect();
        let bcs_id = bcs.get_changeset_id();
        bcs_parents.insert(bcs_id, parents);
        id_bcs.insert(bcs_id, bcs);
    }

    let sorted_commits = sort_topological(&bcs_parents).expect("loop in commit chain!");
    let mut sorted_bcs: Vec<BonsaiChangeset> = vec![];
    for csid in sorted_commits {
        match id_bcs.get(&csid) {
            Some(&bcs) => sorted_bcs.push(bcs.clone()),
            _ => {
                return Err(format_err!(
                    "Could not find mapping for changeset id {}",
                    csid
                ))
            }
        }
    }
    Ok(sorted_bcs)
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let app = args::MononokeApp::new("Import Repository")
        .with_advanced_args_hidden()
        .build()
        .version("0.0.0")
        .about("Automating repository imports")
        .arg(
            Arg::with_name(ARG_GIT_REPOSITORY_PATH)
                .required(true)
                .help("Path to a git repository to import"),
        )
        .arg(
            Arg::with_name(ARG_DEST_PATH)
                .long(ARG_DEST_PATH)
                .required(true)
                .takes_value(true)
                .help("Path to the destination folder we import to"),
        )
        .arg(
            Arg::with_name(ARG_BATCH_SIZE)
                .long(ARG_BATCH_SIZE)
                .takes_value(true)
                .default_value("100")
                .help("Number of commits we make visible when moving the bookmark"),
        )
        .arg(
            Arg::with_name(ARG_BOOKMARK_SUFFIX)
                .long(ARG_BOOKMARK_SUFFIX)
                .required(true)
                .takes_value(true)
                .help("Suffix of the bookmark (repo_import_<suffix>)"),
        )
        .arg(
            Arg::with_name(ARG_CALL_SIGN)
                .long(ARG_CALL_SIGN)
                .takes_value(true)
                .help("Call sign to get commit info from Phabricator. e.g. FBS for fbsource"),
        )
        .arg(
            Arg::with_name(ARG_PHAB_CHECK_DISABLED)
                .long(ARG_PHAB_CHECK_DISABLED)
                .takes_value(false)
                .help("Disable waiting for Phabricator to parse commits."),
        )
        .arg(
            Arg::with_name(ARG_X_REPO_CHECK_DISABLED)
                .long(ARG_X_REPO_CHECK_DISABLED)
                .takes_value(false)
                .help("Disable x_repo sync check after moving the bookmark"),
        )
        .arg(
            Arg::with_name(ARG_HG_SYNC_CHECK_DISABLED)
                .long(ARG_HG_SYNC_CHECK_DISABLED)
                .takes_value(false)
                .help("Disable hg sync check after moving the bookmark"),
        )
        .arg(
            Arg::with_name(ARG_SLEEP_TIME)
                .long(ARG_SLEEP_TIME)
                .takes_value(true)
                .default_value("1")
                .help(
                    "Sleep time, if we fail dependent system (phabricator, hg_sync ...) checkers",
                ),
        );

    let matches = app.get_matches();

    let path = Path::new(matches.value_of(ARG_GIT_REPOSITORY_PATH).unwrap());
    let prefix = matches.value_of(ARG_DEST_PATH).unwrap();
    let bookmark_suffix = matches.value_of(ARG_BOOKMARK_SUFFIX).unwrap();
    let batch_size = matches.value_of(ARG_BATCH_SIZE).unwrap();
    let batch_size = batch_size.parse::<NonZeroUsize>()?.get();
    if !is_valid_bookmark_suffix(&bookmark_suffix) {
        return Err(format_err!(
            "The bookmark suffix contains invalid character(s).
            You can only use alphanumeric and \"./-_\" characters"
        ));
    }

    let phab_check_disabled = matches.is_present(ARG_PHAB_CHECK_DISABLED);
    let x_repo_check_disabled = matches.is_present(ARG_X_REPO_CHECK_DISABLED);
    let hg_sync_check_disabled = matches.is_present(ARG_HG_SYNC_CHECK_DISABLED);
    let call_sign = matches.value_of(ARG_CALL_SIGN);
    if !phab_check_disabled && call_sign.is_none() {
        return Err(format_err!("Call sign was not specified"));
    }
    let checker_flags = CheckerFlags {
        phab_check_disabled,
        x_repo_check_disabled,
        hg_sync_check_disabled,
        call_sign,
    };
    let sleep_time = matches.value_of(ARG_SLEEP_TIME).unwrap();
    let sleep_time = sleep_time.parse::<u64>()?;

    args::init_cachelib(fb, &matches, None);

    let logger = args::init_logging(fb, &matches);
    let ctx = CoreContext::new_with_logger(fb, logger.clone());
    let repo = args::create_repo(fb, &logger, &matches);
    block_execute(
        async {
            let repo = repo.compat().await?;
            let mut shifted_bcs = rewrite_file_paths(&ctx, &repo, &path, &prefix).await?;
            shifted_bcs = sort_bcs(&shifted_bcs)?;
            derive_bonsais(&ctx, &repo, &shifted_bcs).await?;
            move_bookmark(
                &ctx,
                &repo,
                &shifted_bcs,
                batch_size,
                &bookmark_suffix,
                &checker_flags,
                sleep_time,
            )
            .await
        },
        fb,
        "repo_import",
        &logger,
        &matches,
        cmdlib::monitoring::AliveService,
    )
}

#[cfg(test)]
mod tests {
    use crate::{move_bookmark, sort_bcs, CheckerFlags};

    use anyhow::Result;
    use blobstore::Loadable;
    use bookmarks::{BookmarkName, BookmarkUpdateLog, BookmarkUpdateReason, Freshness};
    use context::CoreContext;
    use fbinit::FacebookInit;
    use futures::stream::TryStreamExt;
    use tests_utils::drawdag::create_from_dag;

    #[fbinit::compat_test]
    async fn move_bookmark_test(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let blob_repo = blobrepo_factory::new_memblob_empty(None)?;
        let batch_size: usize = 2;
        let call_sign = Some("FBS");
        let checker_flags = CheckerFlags {
            phab_check_disabled: true,
            x_repo_check_disabled: true,
            hg_sync_check_disabled: true,
            call_sign,
        };
        let sleep_time = 1;
        let changesets = create_from_dag(
            &ctx,
            &blob_repo,
            r##"
                A-B-C-D-E-F-G
            "##,
        )
        .await?;
        let mut bonsais = vec![];
        for (_, csid) in &changesets {
            bonsais.push(csid.load(ctx.clone(), &blob_repo.get_blobstore()).await?);
        }
        bonsais = sort_bcs(&bonsais)?;
        move_bookmark(
            &ctx,
            &blob_repo,
            &bonsais,
            batch_size,
            "test_repo",
            &checker_flags,
            sleep_time,
        )
        .await?;
        // Check the bookmark moves created BookmarkLogUpdate entries
        let entries = blob_repo
            .attribute_expected::<dyn BookmarkUpdateLog>()
            .list_bookmark_log_entries(
                ctx.clone(),
                BookmarkName::new("repo_import_test_repo")?,
                5,
                None,
                Freshness::MostRecent,
            )
            .map_ok(|(cs, rs, _ts)| (cs, rs)) // dropping timestamps
            .try_collect::<Vec<_>>()
            .await?;

        assert_eq!(
            entries,
            vec![
                (Some(changesets["G"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["F"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["D"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["B"]), BookmarkUpdateReason::ManualMove),
                (Some(changesets["A"]), BookmarkUpdateReason::ManualMove),
            ]
        );
        Ok(())
    }
}
