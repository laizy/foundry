use ethers::{
    providers::Provider,
    solc::{remappings::Remapping, ArtifactOutput, Project, ProjectPathsConfig},
};
use evm_adapters::{
    sputnik::{vicinity, ForkMemoryBackend, PRECOMPILES_MAP},
    FAUCET_ACCOUNT,
};
use regex::Regex;
use sputnik::backend::Backend;
use structopt::StructOpt;

use forge::MultiContractRunnerBuilder;

use ansi_term::Colour;
use ethers::types::U256;

mod forge_opts;
use forge_opts::{EvmType, Opts, Subcommands};

use crate::forge_opts::{Dependency, FullContractInfo};
use std::{collections::HashMap, convert::TryFrom, process::Command, str::FromStr, sync::Arc};

mod cmd;
mod utils;

#[tracing::instrument(err)]
fn main() -> eyre::Result<()> {
    utils::subscriber();

    let opts = Opts::from_args();
    match opts.sub {
        Subcommands::Test {
            opts,
            env,
            json,
            pattern,
            evm_type,
            fork_url,
            fork_block_number,
            initial_balance,
            sender,
            ffi,
            verbosity,
            allow_failure,
        } => {
            // Setup the fuzzer
            // TODO: Add CLI Options to modify the persistence
            let cfg =
                proptest::test_runner::Config { failure_persistence: None, ..Default::default() };
            let fuzzer = proptest::test_runner::TestRunner::new(cfg);

            // Set up the project
            let project = Project::try_from(&opts)?;

            // prepare the test builder
            let builder = MultiContractRunnerBuilder::default()
                .fuzzer(fuzzer)
                .initial_balance(initial_balance)
                .sender(sender);

            // run the tests depending on the chosen EVM
            match evm_type {
                #[cfg(feature = "sputnik-evm")]
                EvmType::Sputnik => {
                    use evm_adapters::sputnik::Executor;
                    use sputnik::backend::MemoryBackend;
                    let mut cfg = opts.evm_version.sputnik_cfg();

                    // We disable the contract size limit by default, because Solidity
                    // test smart contracts are likely to be >24kb
                    cfg.create_contract_limit = None;

                    let vicinity = if let Some(ref url) = fork_url {
                        let provider = Provider::try_from(url.as_str())?;
                        let rt = tokio::runtime::Runtime::new().expect("could not start tokio rt");
                        rt.block_on(vicinity(&provider, fork_block_number))?
                    } else {
                        env.sputnik_state()
                    };
                    let mut backend = MemoryBackend::new(&vicinity, Default::default());
                    // max out the balance of the faucet
                    let faucet =
                        backend.state_mut().entry(*FAUCET_ACCOUNT).or_insert_with(Default::default);
                    faucet.balance = U256::MAX;

                    let backend: Box<dyn Backend> = if let Some(ref url) = fork_url {
                        let provider = Provider::try_from(url.as_str())?;
                        let init_state = backend.state().clone();
                        let backend = ForkMemoryBackend::new(
                            provider,
                            backend,
                            fork_block_number,
                            init_state,
                        );
                        Box::new(backend)
                    } else {
                        Box::new(backend)
                    };
                    let backend = Arc::new(backend);

                    let precompiles = PRECOMPILES_MAP.clone();
                    let evm = Executor::new_with_cheatcodes(
                        backend,
                        env.gas_limit,
                        &cfg,
                        &precompiles,
                        ffi,
                    );

                    test(builder, project, evm, pattern, json, verbosity, allow_failure)?;
                }
                #[cfg(feature = "evmodin-evm")]
                EvmType::EvmOdin => {
                    use evm_adapters::evmodin::EvmOdin;
                    use evmodin::tracing::NoopTracer;

                    let revision = opts.evm_version.evmodin_cfg();

                    // TODO: Replace this with a proper host. We'll want this to also be
                    // provided generically when we add the Forking host(s).
                    let host = env.evmodin_state();

                    let evm = EvmOdin::new(host, env.gas_limit, revision, NoopTracer);
                    test(builder, project, evm, pattern, json, verbosity, allow_failure)?;
                }
            }
        }
        Subcommands::Build { opts } => {
            let project = Project::try_from(&opts)?;
            let output = project.compile()?;
            if output.has_compiler_errors() {
                // return the diagnostics error back to the user.
                eyre::bail!(output.to_string())
            } else if output.is_unchanged() {
                println!("no files changed, compilation skippped.");
            } else {
                println!("success.");
            }
        }
        Subcommands::VerifyContract { contract, address, constructor_args } => {
            let FullContractInfo { path, name } = contract;
            let rt = tokio::runtime::Runtime::new().expect("could not start tokio rt");
            rt.block_on(cmd::verify::run(path, name, address, constructor_args))?;
        }
        Subcommands::Create { contract: _, verify: _ } => {
            unimplemented!("Not yet implemented")
        }
        Subcommands::Update { lib } => {
            // TODO: Should we add some sort of progress bar here? Would be nice
            // but not a requirement.
            // open the repo
            let repo = git2::Repository::open(".")?;

            // if a lib is specified, open it
            if let Some(lib) = lib {
                println!("Updating submodule {:?}", lib);
                repo.find_submodule(
                    &lib.into_os_string().into_string().expect("invalid submodule path"),
                )?
                .update(true, None)?;
            } else {
                Command::new("git")
                    .args(&["submodule", "update", "--init", "--recursive"])
                    .spawn()?
                    .wait()?;
            }
        }
        // TODO: Make it work with updates?
        Subcommands::Install { dependencies } => {
            install(std::env::current_dir()?, dependencies)?;
        }
        Subcommands::Remappings { lib_paths, root } => {
            let root = root.unwrap_or_else(|| std::env::current_dir().unwrap());
            let root = std::fs::canonicalize(root)?;

            let lib_paths = if lib_paths.is_empty() { vec![root.join("lib")] } else { lib_paths };
            let remappings: Vec<_> = lib_paths
                .iter()
                .map(|path| Remapping::find_many(&path).unwrap())
                .flatten()
                .collect();
            remappings.iter().for_each(|x| println!("{}", x));
        }
        Subcommands::Init { root, template } => {
            let root = root.unwrap_or_else(|| std::env::current_dir().unwrap());
            // create the root dir if it does not exist
            if !root.exists() {
                std::fs::create_dir_all(&root)?;
            }
            let root = std::fs::canonicalize(root)?;

            // if a template is provided, then this command is just an alias to `git clone <url>
            // <path>`
            if let Some(ref template) = template {
                println!("Initializing {} from {}...", root.display(), template);
                Command::new("git")
                    .args(&["clone", template, &root.display().to_string()])
                    .spawn()?
                    .wait()?;
            } else {
                println!("Initializing {}...", root.display());

                // make the dirs
                let src = root.join("src");
                let test = src.join("test");
                std::fs::create_dir_all(&test)?;
                let lib = root.join("lib");
                std::fs::create_dir(&lib)?;

                // write the contract file
                let contract_path = src.join("Contract.sol");
                std::fs::write(contract_path, include_str!("../../assets/ContractTemplate.sol"))?;
                // write the tests
                let contract_path = test.join("Contract.t.sol");
                std::fs::write(contract_path, include_str!("../../assets/ContractTemplate.t.sol"))?;

                // sets up git
                Command::new("git").arg("init").current_dir(&root).spawn()?.wait()?;
                Command::new("git").args(&["add", "."]).current_dir(&root).spawn()?.wait()?;
                Command::new("git")
                    .args(&["commit", "-m", "chore: forge init"])
                    .current_dir(&root)
                    .spawn()?
                    .wait()?;

                Dependency::from_str("https://github.com/dapphub/ds-test")
                    .and_then(|dependency| install(root, vec![dependency]))?;
            }

            println!("Done.");
        }
        Subcommands::Completions { shell } => {
            Subcommands::clap().gen_completions_to("forge", shell, &mut std::io::stdout())
        }
        Subcommands::Clean { root } => {
            let root = root.unwrap_or_else(|| std::env::current_dir().unwrap());
            let paths = ProjectPathsConfig::builder().root(&root).build()?;
            let project = Project::builder().paths(paths).build()?;
            project.cleanup()?;
        }
    }

    Ok(())
}

fn test<A: ArtifactOutput + 'static, S: Clone, E: evm_adapters::Evm<S>>(
    builder: MultiContractRunnerBuilder,
    project: Project<A>,
    evm: E,
    pattern: Regex,
    json: bool,
    verbosity: u8,
    allow_failure: bool,
) -> eyre::Result<HashMap<String, HashMap<String, forge::TestResult>>> {
    let mut runner = builder.build(project, evm)?;

    let mut exit_code = 0;

    let results = runner.test(pattern)?;

    if json {
        let res = serde_json::to_string(&results)?;
        println!("{}", res);
    } else {
        // Dapptools-style printing of test results
        for (i, (contract_name, tests)) in results.iter().enumerate() {
            if i > 0 {
                println!()
            }
            if !tests.is_empty() {
                let term = if tests.len() > 1 { "tests" } else { "test" };
                println!("Running {} {} for {}", tests.len(), term, contract_name);
            }

            for (name, result) in tests {
                let status = if result.success {
                    Colour::Green.paint("[PASS]")
                } else {
                    // if an error is found, return a -1 exit code
                    exit_code = -1;
                    let txt = match (&result.reason, &result.counterexample) {
                        (Some(ref reason), Some(ref counterexample)) => {
                            format!(
                                "[FAIL. Reason: {}. Counterexample: {}]",
                                reason, counterexample
                            )
                        }
                        (None, Some(ref counterexample)) => {
                            format!("[FAIL. Counterexample: {}]", counterexample)
                        }
                        (Some(ref reason), None) => {
                            format!("[FAIL. Reason: {}]", reason)
                        }
                        (None, None) => "[FAIL]".to_string(),
                    };

                    Colour::Red.paint(txt)
                };
                println!(
                    "{} {} (gas: {})",
                    status,
                    name,
                    result
                        .gas_used
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| "[fuzztest]".to_string())
                );
            }

            if verbosity > 1 {
                println!();

                for (name, result) in tests {
                    let status = if result.success { "Success" } else { "Failure" };
                    println!("{}: {}", status, name);
                    println!();

                    for log in &result.logs {
                        println!("  {}", log);
                    }

                    println!();
                }
            }
        }
    }

    if allow_failure {
        exit_code = 0;
    }
    std::process::exit(exit_code);
}

fn install(root: impl AsRef<std::path::Path>, dependencies: Vec<Dependency>) -> eyre::Result<()> {
    let libs = std::path::Path::new("lib");

    dependencies.iter().try_for_each(|dep| -> eyre::Result<_> {
        let path = libs.join(&dep.name);
        println!("Installing {} in {:?}, (url: {}, tag: {:?})", dep.name, path, dep.url, dep.tag);

        // install the dep
        Command::new("git")
            .args(&["submodule", "add", &dep.url, &path.display().to_string()])
            .current_dir(&root)
            .spawn()?
            .wait()?;

        // call update on it
        Command::new("git")
            .args(&["submodule", "update", "--init", "--recursive", &path.display().to_string()])
            .current_dir(&root)
            .spawn()?
            .wait()?;

        // checkout the tag if necessary
        let message = if let Some(ref tag) = dep.tag {
            Command::new("git")
                .args(&["checkout", "--recurse-submodules", tag])
                .current_dir(&path)
                .spawn()?
                .wait()?;

            Command::new("git").args(&["add", &path.display().to_string()]).spawn()?.wait()?;

            format!("forge install: {}\n\n{}", dep.name, tag)
        } else {
            format!("forge install: {}", dep.name)
        };

        Command::new("git").args(&["commit", "-m", &message]).current_dir(&root).spawn()?.wait()?;

        Ok(())
    })
}
