/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! read_res -- Read the content of EdenAPI responses
//!
//! This program allows querying the contents of
//! EdenAPI CBOR data and history responses.

#![deny(warnings)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{prelude::*, stdin, stdout};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde_cbor::Deserializer;
use structopt::StructOpt;

use edenapi_types::{DataEntry, DataError, HistoryResponseChunk, WireHistoryEntry};
use types::{Key, Parents, RepoPathBuf};

#[derive(Debug, StructOpt)]
#[structopt(name = "read_res", about = "Read the content of EdenAPI responses")]
enum Args {
    Data(DataArgs),
    History(HistoryArgs),
}

#[derive(Debug, StructOpt)]
#[structopt(about = "Read the content of a CBOR data response")]
enum DataArgs {
    Ls(DataLsArgs),
    Cat(DataCatArgs),
    Check(DataCheckArgs),
}

#[derive(Debug, StructOpt)]
#[structopt(about = "List the data entries in the response")]
struct DataLsArgs {
    #[structopt(help = "Input CBOR file (stdin is used if omitted)")]
    input: Option<PathBuf>,
    #[structopt(long, short, help = "Only look at the first N entries")]
    limit: Option<usize>,
}

#[derive(Debug, StructOpt)]
#[structopt(about = "Get the content of a data entry")]
struct DataCatArgs {
    #[structopt(help = "Input CBOR file (stdin used if omitted)")]
    input: Option<PathBuf>,
    #[structopt(long, short, help = "Output file (stdout used if omitted)")]
    output: Option<PathBuf>,
    #[structopt(long, short, help = "Path of desired data entry")]
    path: String,
    #[structopt(long, short, help = "Node hash of desired data entry")]
    hgid: String,
    #[structopt(long, short, help = "Only look at the first N entries")]
    limit: Option<usize>,
}

#[derive(Debug, StructOpt)]
#[structopt(about = "Check the validity of node hashes for all entries")]
struct DataCheckArgs {
    #[structopt(help = "Input CBOR file (stdin is used if omitted)")]
    input: Option<PathBuf>,
    #[structopt(long, short, help = "Only look at the first N entries")]
    limit: Option<usize>,
}

#[derive(Debug, StructOpt)]
#[structopt(about = "Read the content of a CBOR history response")]
enum HistoryArgs {
    Ls(HistLsArgs),
    Show(HistShowArgs),
}

#[derive(Debug, StructOpt)]
#[structopt(about = "List files in this history response")]
struct HistLsArgs {
    #[structopt(help = "Input CBOR file (stdin is used if omitted)")]
    input: Option<PathBuf>,
    #[structopt(long, short, help = "Only look at the first N entries")]
    limit: Option<usize>,
}

#[derive(Debug, StructOpt)]
#[structopt(about = "Show history for a single file")]
struct HistShowArgs {
    #[structopt(help = "Input CBOR file (stdin is used if omitted)")]
    input: Option<PathBuf>,
    #[structopt(long, short, help = "Only show entries for given file")]
    file: Option<String>,
    #[structopt(long, short, help = "Only show number of entries per file")]
    count: bool,
    #[structopt(long, short, help = "Only look at the first N entries")]
    limit: Option<usize>,
}

fn main() -> Result<()> {
    match Args::from_args() {
        Args::Data(args) => cmd_data(args),
        Args::History(args) => cmd_history(args),
    }
}

fn cmd_data(args: DataArgs) -> Result<()> {
    match args {
        DataArgs::Ls(args) => cmd_data_ls(args),
        DataArgs::Cat(args) => cmd_data_cat(args),
        DataArgs::Check(args) => cmd_data_check(args),
    }
}

fn cmd_data_ls(args: DataLsArgs) -> Result<()> {
    let entries: Vec<DataEntry> = read_input(args.input, args.limit)?;
    for entry in entries {
        println!("{}", entry.key());
    }
    Ok(())
}

fn cmd_data_cat(args: DataCatArgs) -> Result<()> {
    let path = RepoPathBuf::from_string(args.path)?;
    let hgid = args.hgid.parse()?;
    let key = Key::new(path, hgid);

    let entries: Vec<DataEntry> = read_input(args.input, args.limit)?;
    let entry = entries
        .into_iter()
        .find(|entry| entry.key() == &key)
        .ok_or_else(|| anyhow!("Key not found"))?;

    write_output(args.output, &entry.data()?)
}

fn cmd_data_check(args: DataCheckArgs) -> Result<()> {
    let entries: Vec<DataEntry> = read_input(args.input, args.limit)?;
    for entry in entries {
        match entry.data() {
            Ok(_) => {}
            Err(DataError::Redacted(..)) => {
                println!("{} [Contents redacted]", entry.key());
            }
            Err(DataError::MaybeHybridManifest(e)) => {
                println!("{} [Possible flat manifest hash] {}", entry.key(), e);
            }
            Err(DataError::Corrupt(e)) => {
                println!("{} [Invalid hash] {}", entry.key(), e);
            }
        }
    }
    Ok(())
}

fn cmd_history(args: HistoryArgs) -> Result<()> {
    match args {
        HistoryArgs::Ls(args) => cmd_history_ls(args),
        HistoryArgs::Show(args) => cmd_history_show(args),
    }
}

fn cmd_history_ls(args: HistLsArgs) -> Result<()> {
    let chunks: Vec<HistoryResponseChunk> = read_input(args.input, args.limit)?;
    // Deduplicate and sort paths.
    let mut paths = BTreeSet::new();
    for chunk in chunks {
        paths.insert(chunk.path.into_string());
    }
    for path in paths {
        println!("{}", path);
    }
    Ok(())
}

fn cmd_history_show(args: HistShowArgs) -> Result<()> {
    let chunks: Vec<HistoryResponseChunk> = read_input(args.input, args.limit)?;
    let map = make_history_map(chunks);
    match args.file {
        Some(ref path) => match map.get(path) {
            Some(entries) => print_history(path, entries, args.count),
            None => println!("Path not found in input: {}", path),
        },
        None => {
            for (path, entries) in &map {
                print_history(path, entries, args.count);
            }
        }
    }
    Ok(())
}

fn make_history_map(
    chunks: impl IntoIterator<Item = HistoryResponseChunk>,
) -> BTreeMap<String, Vec<WireHistoryEntry>> {
    let mut map = BTreeMap::new();
    for chunk in chunks {
        map.entry(chunk.path.into_string())
            .or_insert_with(Vec::new)
            .extend_from_slice(&chunk.entries);
    }
    map
}

fn print_history(path: &str, entries: &[WireHistoryEntry], counts_only: bool) {
    if counts_only {
        println!("{}: {}", path, entries.len());
    } else {
        println!("{}:", path);
        for entry in entries {
            println!("  node: {}", entry.node);
            let parents = match entry.parents {
                Parents::Two(p1, p2) => format!("{} {}", p1, p2),
                Parents::One(p1) => format!("{}", p1),
                Parents::None => "None".to_string(),
            };
            println!("  parents: {}", parents);
            println!("  linknode: {}", entry.linknode);
            if let Some(path) = &entry.copyfrom {
                println!("  copyfrom: {}", path);
            }
            println!()
        }
        println!()
    }
}

fn read_input<T: DeserializeOwned>(path: Option<PathBuf>, limit: Option<usize>) -> Result<Vec<T>> {
    Ok(match path {
        Some(path) => {
            eprintln!("Reading from file: {:?}", &path);
            let file = File::open(&path)?;
            Deserializer::from_reader(file)
                .into_iter()
                .take(limit.unwrap_or(usize::MAX))
                .collect::<Result<Vec<_>, _>>()?
        }
        None => {
            eprintln!("Reading from stdin");
            Deserializer::from_reader(stdin())
                .into_iter()
                .take(limit.unwrap_or(usize::MAX))
                .collect::<Result<Vec<_>, _>>()?
        }
    })
}

fn write_output(path: Option<PathBuf>, content: &[u8]) -> Result<()> {
    match path {
        Some(path) => {
            eprintln!("Writing to file: {:?}", &path);
            let mut file = File::create(&path)?;
            file.write_all(content)?;
        }
        None => {
            stdout().write_all(content)?;
        }
    }
    Ok(())
}
