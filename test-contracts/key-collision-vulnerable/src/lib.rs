#![no_std]
use soroban_sdk::{contract, contractimpl, symbol_short, Env};

#[contract]
pub struct KeyCollisionVulnerable;

#[contractimpl]
impl KeyCollisionVulnerable {
    /// Vulnerable: stores a balance under `data`.
    pub fn set_balance(env: Env, balance: i128) {
        env.storage()
            .instance()
            .set(&symbol_short!("data"), &balance);
    }

    /// Vulnerable: stores a pause flag under the same `data` key.
    pub fn set_paused(env: Env, paused: bool) {
        env.storage().instance().set(&symbol_short!("data"), &paused);
    }
}
