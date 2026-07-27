#![no_std]
use soroban_sdk::{contract, contractimpl, Env};
use std::collections::HashMap;

#[contract]
pub struct StdImportsVulnerable;

#[contractimpl]
impl StdImportsVulnerable {
    /// Vulnerable: Soroban contracts should not import from `std`.
    pub fn count(env: Env) -> u32 {
        let values: HashMap<u32, u32> = HashMap::new();
        let _ = env;
        values.len() as u32
    }
}
