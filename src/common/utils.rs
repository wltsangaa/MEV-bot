use crate::sandwich::simulation::Sandwich;
use anyhow::Result;
use ethers::core::rand::thread_rng;
use ethers::prelude::*;

use ethers::{
    self,
    types::{
        transaction::eip2930::{AccessList, AccessListItem},
        U256,
        U64,
    },
};
use fern::colors::{Color, ColoredLevelConfig};
use foundry_evm_mini::evm::utils::{b160_to_h160, h160_to_b160, ru256_to_u256, u256_to_ru256};
use log::LevelFilter;
use rand::Rng;
use revm::primitives::{B160, U256 as rU256};
use std::str::FromStr;
use std::sync::Arc;
use std::path::{PathBuf, Path};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use chrono::Utc;
use std::fs::{self};



use crate::common::constants::*;

pub const DEFAULT_LOG_DIR: &str = "logs";
pub const SANDWICH_LOG_FILENAME: &str = "mev_opportunity.csv";


pub fn setup_logger() -> Result<()> {
    let colors = ColoredLevelConfig {
        trace: Color::Cyan,
        debug: Color::Magenta,
        info: Color::Green,
        warn: Color::Red,
        error: Color::BrightRed,
        ..ColoredLevelConfig::new()
    };

    fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{}[{}] {}",
                chrono::Local::now().format("[%H:%M:%S]"),
                colors.color(record.level()),
                message
            ))
        })
        .chain(std::io::stdout())
        .level(log::LevelFilter::Error)
        .level_for(PROJECT_NAME, LevelFilter::Info)
        .apply()?;

    Ok(())
}

pub fn calculate_next_block_base_fee(
    gas_used: U256,
    gas_limit: U256,
    base_fee_per_gas: U256,
) -> U256 {
    let gas_used = gas_used;

    let mut target_gas_used = gas_limit / 2;
    target_gas_used = if target_gas_used == U256::zero() {
        U256::one()
    } else {
        target_gas_used
    };

    let new_base_fee = {
        if gas_used > target_gas_used {
            base_fee_per_gas
                + ((base_fee_per_gas * (gas_used - target_gas_used)) / target_gas_used)
                    / U256::from(8u64)
        } else {
            base_fee_per_gas
                - ((base_fee_per_gas * (target_gas_used - gas_used)) / target_gas_used)
                    / U256::from(8u64)
        }
    };

    let seed = rand::thread_rng().gen_range(0..9);
    new_base_fee + seed
}

pub fn access_list_to_ethers(access_list: Vec<(B160, Vec<rU256>)>) -> AccessList {
    AccessList::from(
        access_list
            .into_iter()
            .map(|(address, slots)| AccessListItem {
                address: b160_to_h160(address),
                storage_keys: slots
                    .into_iter()
                    .map(|y| H256::from_uint(&ru256_to_u256(y)))
                    .collect(),
            })
            .collect::<Vec<AccessListItem>>(),
    )
}

pub fn access_list_to_revm(access_list: AccessList) -> Vec<(B160, Vec<rU256>)> {
    access_list
        .0
        .into_iter()
        .map(|x| {
            (
                h160_to_b160(x.address),
                x.storage_keys
                    .into_iter()
                    .map(|y| u256_to_ru256(y.0.into()))
                    .collect(),
            )
        })
        .collect()
}

abigen!(
    IERC20,
    r#"[
        function balanceOf(address) external view returns (uint256)
    ]"#,
);

pub async fn get_token_balance(
    provider: Arc<Provider<Ws>>,
    owner: H160,
    token: H160,
) -> Result<U256> {
    let contract = IERC20::new(token, provider);
    let token_balance = contract.balance_of(owner).call().await?;
    Ok(token_balance)
}

pub fn create_new_wallet() -> (LocalWallet, H160) {
    let wallet = LocalWallet::new(&mut thread_rng());
    let address = wallet.address();
    (wallet, address)
}

pub fn to_h160(str_address: &'static str) -> H160 {
    H160::from_str(str_address).unwrap()
}

pub fn is_weth(token_address: H160) -> bool {
    token_address == to_h160(WETH)
}

pub fn is_main_currency(token_address: H160) -> bool {
    let main_currencies = vec![to_h160(WETH), to_h160(USDT), to_h160(USDC)];
    main_currencies.contains(&token_address)
}

#[derive(Debug, Clone)]
pub enum MainCurrency {
    WETH,
    USDT,
    USDC,

    Default, // Pairs that aren't WETH/Stable pairs. Default to WETH for now
}

impl MainCurrency {
    pub fn new(address: H160) -> Self {
        if address == to_h160(WETH) {
            MainCurrency::WETH
        } else if address == to_h160(USDT) {
            MainCurrency::USDT
        } else if address == to_h160(USDC) {
            MainCurrency::USDC
        } else {
            MainCurrency::Default
        }
    }

    pub fn decimals(&self) -> u8 {
        match self {
            MainCurrency::WETH => WETH_DECIMALS,
            MainCurrency::USDT => USDC_DECIMALS,
            MainCurrency::USDC => USDC_DECIMALS,
            MainCurrency::Default => WETH_DECIMALS,
        }
    }

    pub fn balance_slot(&self) -> i32 {
        match self {
            MainCurrency::WETH => WETH_BALANCE_SLOT,
            MainCurrency::USDT => USDT_BALANCE_SLOT,
            MainCurrency::USDC => USDC_BALANCE_SLOT,
            MainCurrency::Default => WETH_BALANCE_SLOT,
        }
    }

    /*
    We score the currencies by importance
    WETH has the highest importance, and USDT, USDC in the following order
    */
    pub fn weight(&self) -> u8 {
        match self {
            MainCurrency::WETH => 3,
            MainCurrency::USDT => 2,
            MainCurrency::USDC => 1,
            MainCurrency::Default => 3, // default is WETH
        }
    }
}

pub fn return_main_and_target_currency(token0: H160, token1: H160) -> Option<(H160, H160)> {
    let token0_supported = is_main_currency(token0);
    let token1_supported = is_main_currency(token1);

    if !token0_supported && !token1_supported {
        return None;
    }

    if token0_supported && token1_supported {
        let mc0 = MainCurrency::new(token0);
        let mc1 = MainCurrency::new(token1);

        let token0_weight = mc0.weight();
        let token1_weight = mc1.weight();

        if token0_weight > token1_weight {
            return Some((token0, token1));
        } else {
            return Some((token1, token0));
        }
    }

    if token0_supported {
        return Some((token0, token1));
    } else {
        return Some((token1, token0));
    }
}


pub fn log_sandwich_data_to_csv(
    file_path: &str,
    block_number: U64,
    sandwiches_count: usize,
    bundle_id: &str,
    base_fee: U256,
    priority_fee: U256,
    max_fee: U256,
    bribe_amount: U256,
    revenue: i128,
    profit: i128,
    gas_cost: i128,
    front_gas: u64,
    back_gas: u64,
    sandwiches: &Vec<Sandwich>,  // Changed to accept Vec<Sandwich>
    bundle_request_json: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(file_path);
    
    // Ensure the parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Check if the file exists
    let file_exists = path.exists();

    // Open the file with options to append and create if it doesn't exist
    let file = OpenOptions::new()
        .write(true)
        .append(true)
        .create(true)
        .open(file_path)?;

    // Create a CSV writer with headers if the file didn't exist
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(!file_exists)
        .from_writer(file);

    // Write headers if the file didn't exist
    if !file_exists {
        wtr.write_record(&[
            "timestamp",
            "block_number",
            "sandwiches_count",
            "bundle_id",
            "base_fee",
            "priority_fee",
            "max_fee",
            "bribe_amount",
            "revenue",
            "profit",
            "gas_cost",
            "front_gas",
            "back_gas",
            "sandwiches",
            "bundle_request_json"
        ])?;
    }

    // Convert sandwiches to a JSON string
    let sandwiches_json = json!({
        "sandwiches": sandwiches.iter().map(|sandwich| {
            json!({
                "victim_tx": {
                    "tx_hash": format!("{:?}", sandwich.victim_tx.tx_hash),
                    "from": format!("{:?}", sandwich.victim_tx.from),
                    "to": format!("{:?}", sandwich.victim_tx.to),
                    "value": sandwich.victim_tx.value.to_string(),
                    "gas_price": sandwich.victim_tx.gas_price.to_string(),
                    "gas_limit": sandwich.victim_tx.gas_limit.map(|g| g.to_string()).unwrap_or_default()
                },
                "amount_in": sandwich.amount_in.to_string(),
                "swap_info": {
                    "target_pair": format!("{:?}", sandwich.swap_info.target_pair),
                    "main_currency": format!("{:?}", sandwich.swap_info.main_currency)
                }
            })
        }).collect::<Vec<Value>>()
    }).to_string();

    // Write the data record
    wtr.write_record(&[
        Utc::now().to_rfc3339(),
        block_number.to_string(),
        sandwiches_count.to_string(),
        bundle_id.to_string(),
        base_fee.to_string(),
        priority_fee.to_string(),
        max_fee.to_string(),
        bribe_amount.to_string(),
        revenue.to_string(),
        profit.to_string(),
        gas_cost.to_string(),
        front_gas.to_string(),
        back_gas.to_string(),
        sandwiches_json,
        bundle_request_json.to_owned(),
    ])?;

    // Flush the writer to ensure all data is written to the file
    wtr.flush()?;
    Ok(())
}

pub fn get_default_sandwich_log_path() -> PathBuf {
    PathBuf::from(DEFAULT_LOG_DIR).join(SANDWICH_LOG_FILENAME)
}

