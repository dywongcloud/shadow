use std::{
	fs,
	path::PathBuf,
	sync::Arc,
	time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Parser;
use oxc::diagnostics::NamedSource;

use crate::rewriter::NativeRewriter;

mod rewriter;

#[derive(Parser)]
pub struct RewriterOptions {
	/// Module identifier used as the systemjs register name
	#[clap(long, default_value = "module")]
	ident: String,
	/// Name of the async-context holder global emitted around each `await`
	#[clap(long, default_value = "__nw_acf")]
	ctx: String,
	/// Only wrap awaits (CommonJS path); skip SystemJS/ESM lowering
	#[clap(long)]
	awaits_only: bool,
}

#[derive(Parser)]
#[command(version = clap::crate_version!())]
pub enum Cli {
	/// Rewrite a file and print the result
	Rewrite {
		file: PathBuf,
		#[clap(flatten)]
		config: RewriterOptions,
	},
	/// Rewrite a file many times to benchmark the rewriter
	Bench {
		file: PathBuf,
		iterations: u32,
		#[clap(flatten)]
		config: RewriterOptions,
	},
}

fn main() -> Result<()> {
	let args = Cli::parse();

	match args {
		Cli::Rewrite { file, config } => {
			let mut rewriter = NativeRewriter::new();

			let data = fs::read_to_string(&file).context("failed to read file")?;

			let res = if config.awaits_only {
				rewriter.rewrite_awaits(&data, &config.ctx)?
			} else {
				rewriter.rewrite(&data, &config.ident, &config.ctx)?
			};

			let source = Arc::new(
				NamedSource::new(data.clone(), file.to_string_lossy().into_owned())
					.with_language("javascript"),
			);

			eprintln!("rewritten:");
			println!(
				"{}",
				std::str::from_utf8(&res.js).context("failed to parse rewritten js")?
			);

			eprintln!("errors:");
			for err in res.errors {
				eprintln!("{}", err.with_source_code(source.clone()));
			}

			rewriter.reset();
		}
		Cli::Bench {
			file,
			iterations,
			config,
		} => {
			let mut rewriter = NativeRewriter::new();

			let data = fs::read_to_string(&file).context("failed to read file")?;
			let mut duration = Duration::from_secs(0);

			let cnt = iterations * 100;

			for x in 1..=cnt {
				let before = Instant::now();
				if config.awaits_only {
					rewriter
						.rewrite_awaits(&data, &config.ctx)
						.context("failed to rewrite")?;
				} else {
					rewriter
						.rewrite(&data, &config.ident, &config.ctx)
						.context("failed to rewrite")?;
				}
				let after = Instant::now();

				rewriter.reset();

				duration += after - before;

				if x % 100 == 0 {
					println!("{x}...");
				}
			}

			println!("iterations: {cnt}");
			println!("total time: {duration:?}");
			println!("avg time: {:?}", duration / cnt);
		}
	}

	Ok(())
}
