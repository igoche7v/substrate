// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

use std::{fs, path::{Path, PathBuf}};

use ansi_term::Style;
use rand::{Rng, distributions::Alphanumeric, rngs::OsRng};
use structopt::StructOpt;

use keystore::{Store as Keystore};
use node_cli::chain_spec::{self, AccountId};
use primitives::{sr25519, crypto::{Public, Ss58Codec}, traits::BareCryptoStore};

/// A utility to easily create a testnet chain spec definition with a given set
/// of authorities and endowed accounts and/or generate random accounts.
#[derive(StructOpt)]
#[structopt(rename_all = "kebab-case")]
enum ChainSpecBuilder {
	/// Create a new chain spec with the given authorities, endowed and sudo
	/// accounts.
	New {
		/// Authority key seed.
		#[structopt(long, short, required = true)]
		authority_seeds: Vec<String>,
		/// Endowed account address (SS58 format).
		#[structopt(long, short)]
		endowed_accounts: Vec<String>,
		/// Sudo account address (SS58 format).
		#[structopt(long, short)]
		sudo_account: String,
		/// The path where the chain spec should be saved.
		#[structopt(long, short, default_value = "./chain_spec.json")]
		chain_spec_path: PathBuf,
	},
	/// Create a new chain spec with the given number of authorities and endowed
	/// accounts. Random keys will be generated as required.
	Generate {
		/// The number of authorities.
		#[structopt(long, short)]
		authorities: usize,
		/// The number of endowed accounts.
		#[structopt(long, short, default_value = "0")]
		endowed: usize,
		/// The path where the chain spec should be saved.
		#[structopt(long, short, default_value = "./chain_spec.json")]
		chain_spec_path: PathBuf,
		/// Path to use when saving generated keystores for each authority.
		///
		/// At this path, a new folder will be created for each authority's
		/// keystore named `auth-$i` where `i` is the authority index, i.e.
		/// `auth-0`, `auth-1`, etc.
		#[structopt(long, short)]
		keystore_path: Option<PathBuf>,
	},
}

impl ChainSpecBuilder {
	/// Returns the path where the chain spec should be saved.
	fn chain_spec_path(&self) -> &Path {
		match self {
			ChainSpecBuilder::New { chain_spec_path, .. } =>
				chain_spec_path.as_path(),
			ChainSpecBuilder::Generate { chain_spec_path, .. } =>
				chain_spec_path.as_path(),
		}
	}
}

fn genesis_constructor(
	authority_seeds: &[String],
	endowed_accounts: &[AccountId],
	sudo_account: &AccountId,
) -> chain_spec::GenesisConfig {
	let authorities = authority_seeds
		.iter()
		.map(AsRef::as_ref)
		.map(chain_spec::get_authority_keys_from_seed)
		.collect::<Vec<_>>();

	let enable_println = true;

	chain_spec::testnet_genesis(
		authorities,
		sudo_account.clone(),
		Some(endowed_accounts.to_vec()),
		enable_println,
	)
}

fn generate_chain_spec(
	authority_seeds: Vec<String>,
	endowed_accounts: Vec<String>,
	sudo_account: String,
) -> Result<String, String> {
	let parse_account = |address: &String| {
		AccountId::from_string(address)
			.map_err(|err| format!("Failed to parse account address: {:?}", err))
	};

	let endowed_accounts = endowed_accounts
		.iter()
		.map(parse_account)
		.collect::<Result<Vec<_>, String>>()?;

	let sudo_account = parse_account(&sudo_account)?;

	let chain_spec = chain_spec::ChainSpec::from_genesis(
		"Custom",
		"custom",
		move || genesis_constructor(&authority_seeds, &endowed_accounts, &sudo_account),
		vec![],
		None,
		None,
		None,
		Default::default(),
	);

	chain_spec.to_json(false).map_err(|err| err.to_string())
}

fn generate_authority_keys_and_store(
	seeds: &[String],
	keystore_path: &Path,
) -> Result<(), String> {
	for (n, seed) in seeds.into_iter().enumerate() {
		let keystore = Keystore::open(
			keystore_path.join(format!("auth-{}", n)),
			None,
		).map_err(|err| err.to_string())?;

		let (_, _, grandpa, babe, im_online) =
			chain_spec::get_authority_keys_from_seed(seed);

		let insert_key = |key_type, public| {
			keystore.write().insert_unknown(
				key_type,
				&format!("//{}", seed),
				public,
			).map_err(|_| format!("Failed to insert key: {}", grandpa))
		};

		insert_key(
			primitives::crypto::key_types::BABE,
			babe.as_slice(),
		)?;

		insert_key(
			primitives::crypto::key_types::GRANDPA,
			grandpa.as_slice(),
		)?;

		insert_key(
			primitives::crypto::key_types::IM_ONLINE,
			im_online.as_slice(),
		)?;
	}

	Ok(())
}

fn print_seeds(
	authority_seeds: &[String],
	endowed_seeds: &[String],
	sudo_seed: &str,
) {
	let header = Style::new().bold().underline();
	let entry = Style::new().bold();

	println!("{}", header.paint("Authority seeds"));

	for (n, seed) in authority_seeds.iter().enumerate() {
		println!("{} //{}",
			entry.paint(format!("auth-{}:", n)),
			seed,
		);
	}

	println!();

	if !endowed_seeds.is_empty() {
		println!("{}", header.paint("Endowed seeds"));
		for (n, seed) in endowed_seeds.iter().enumerate() {
			println!("{} //{}",
				entry.paint(format!("endowed-{}:", n)),
				seed,
			);
		}

		println!();
	}

	println!("{}", header.paint("Sudo seed"));
	println!("//{}", sudo_seed);
}

fn main() -> Result<(), String> {
	let builder = ChainSpecBuilder::from_args();
	let chain_spec_path = builder.chain_spec_path().to_path_buf();

	let (authority_seeds, endowed_accounts, sudo_account) = match builder {
		ChainSpecBuilder::Generate { authorities, endowed, keystore_path, .. } => {
			let authorities = authorities.max(1);
			let rand_str = || -> String {
				OsRng.sample_iter(&Alphanumeric)
					.take(32)
					.collect()
			};

			let authority_seeds = (0..authorities).map(|_| rand_str()).collect::<Vec<_>>();
			let endowed_seeds = (0..endowed).map(|_| rand_str()).collect::<Vec<_>>();
			let sudo_seed = rand_str();

			print_seeds(
				&authority_seeds,
				&endowed_seeds,
				&sudo_seed,
			);

			if let Some(keystore_path) = keystore_path {
				generate_authority_keys_and_store(
					&authority_seeds,
					&keystore_path,
				)?;
			}

			let endowed_accounts = endowed_seeds.iter().map(|seed| {
				chain_spec::get_account_id_from_seed::<sr25519::Public>(seed)
					.to_ss58check()
			}).collect();

			let sudo_account = chain_spec::get_account_id_from_seed::<sr25519::Public>(&sudo_seed)
				.to_ss58check();

			(authority_seeds, endowed_accounts, sudo_account)
		},
		ChainSpecBuilder::New { authority_seeds, endowed_accounts, sudo_account, .. } => {
			(authority_seeds, endowed_accounts, sudo_account)
		},
	};

	let json = generate_chain_spec(
		authority_seeds,
		endowed_accounts,
		sudo_account,
	)?;

	fs::write(chain_spec_path, json).map_err(|err| err.to_string())
}
