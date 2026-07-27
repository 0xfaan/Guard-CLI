#![no_std]
use soroban_sdk::{contract, contractimpl, Address, Env, Map};

#[contract]
pub struct StdImportsSafe;

#[contractimpl]
impl StdImportsSafe {
    /// Safe: uses Soroban SDK collections instead of importing from `std`.
    pub fn record(env: Env, account: Address, amount: i128) -> i128 {
        let mut balances: Map<Address, i128> = Map::new(&env);
        balances.set(account.clone(), amount);
        balances.get(account).unwrap_or(0)
    }
}
